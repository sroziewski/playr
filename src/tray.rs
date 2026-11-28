//! Hiding the window to the notification area ("tray") while playing goes on.
//!
//! On Windows the tray's handlers, called on the main thread by the message loop winit pumps,
//! show and close the window through Win32 at once, rather than asking the app to on its next
//! frame: a hidden window's frames come slowly and may not come at all.
//! On Linux the icon is a StatusNotifierItem on D-Bus (KDE, and GNOME with the AppIndicator
//! extension Ubuntu turns on), whose handlers run on a thread of their own: they ask the window
//! to show or close through egui and wake it, and eframe carries such requests out for a
//! hidden window too. X11 only: Wayland does not let a window hide itself.
//! Playing needs no frames: sound, the clock and the playlist run on the player's threads.

use std::sync::Arc;

use crate::player::{Shared, State};

/// What a click on the tray icon or an entry of its menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Show,
    TogglePause,
    Quit,
}

/// The longest tooltip Windows shows, in UTF-16 units (the field holds 128 with its NUL).
pub const TOOLTIP_MAX: usize = 127;

/// The action of a click on the icon: a left button released, or a left double click, shows
/// the window. The right button opens the menu (done by the tray itself), so it does nothing.
pub fn click_action(left: bool, released: bool, double: bool) -> Option<Action> {
    (left && (released || double)).then_some(Action::Show)
}

/// The tooltip for `title` (the window title, "Song — playr"): cut to [`TOOLTIP_MAX`] UTF-16
/// units on a character boundary, ending in "…" when cut.
pub fn tooltip(title: &str) -> String {
    let title = title.trim();
    let title = if title.is_empty() { "playr" } else { title };
    if title.encode_utf16().count() <= TOOLTIP_MAX {
        return title.to_string();
    }
    let mut out = String::new();
    let mut units = 0;
    for c in title.chars() {
        if units + c.len_utf16() > TOOLTIP_MAX - 1 {
            break;
        }
        units += c.len_utf16();
        out.push(c);
    }
    out.push('…');
    out
}

/// Play/pause from the tray menu. Media that has ended stays as it is: restarting it needs a
/// seek, which only the window's own controls do.
pub fn toggle_pause(shared: &Shared) {
    if shared.status().state != State::Ended {
        shared.set_paused(!shared.is_paused());
    }
}

/// The action of the tray menu entry `id`, given the ids of the entries Show, Play / Pause and
/// Quit, in that order.
pub fn menu_choice<T: PartialEq>(id: &T, [show, pause, quit]: [&T; 3]) -> Option<Action> {
    [(show, Action::Show), (pause, Action::TogglePause), (quit, Action::Quit)]
        .into_iter()
        .find_map(|(item, action)| (item == id).then_some(action))
}

/// What [`Tray::hide`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Hidden {
    /// The window is hidden and the icon shows in the tray.
    ToTray,
    /// There is no tray here (or it failed, with the reason): the caller minimizes instead.
    Unavailable(String),
}

/// What the window does after [`Tray::hide`] gave `hidden`: nothing more when it went to the
/// tray; otherwise it minimizes, with an on-screen message saying why there is no tray.
pub fn when_hidden(hidden: Hidden) -> Option<(String, eframe::egui::ViewportCommand)> {
    match hidden {
        Hidden::ToTray => None,
        Hidden::Unavailable(why) => Some((format!("Minimized: {why}"), eframe::egui::ViewportCommand::Minimized(true))),
    }
}

#[cfg(windows)]
pub use windows_tray::{Tray, act};

#[cfg(windows)]
mod windows_tray {
    use std::cell::RefCell;

    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, PostMessageW, SW_HIDE, SW_SHOW, SetForegroundWindow, ShowWindow, WM_CLOSE,
    };

    use super::{Action, Arc, Hidden, Shared, click_action, menu_choice, toggle_pause, tooltip};

    /// The state the tray's handlers act on. The handlers are process-wide and can be set
    /// only once, so they read it from here rather than capturing it. The icon is made once
    /// and then only shown and hidden: dropping it from inside its own handler would destroy
    /// the window that is still handling the click.
    struct Target {
        hwnd: isize,
        shared: Arc<Shared>,
        icon: TrayIcon,
        items: [MenuId; 3],
    }

    // The handlers are called by the main thread's message loop, the thread that made the icon.
    thread_local! {
        static TARGET: RefCell<Option<Target>> = const { RefCell::new(None) };
    }

    /// Does `action` as a click on the tray icon or its menu would. Nothing happens before the
    /// first [`Tray::hide`] on this thread.
    pub fn act(action: Action) {
        // The borrow ends before the window is touched: showing it makes Windows call the
        // window's procedure at once, and a frame drawn there may hide to the tray again.
        let Some((hwnd, shared)) = TARGET.with_borrow(|t| {
            let t = t.as_ref()?;
            if action != Action::TogglePause {
                let _ = t.icon.set_visible(false);
            }
            Some((t.hwnd as HWND, t.shared.clone()))
        }) else {
            return;
        };
        match action {
            Action::TogglePause => toggle_pause(&shared),
            Action::Show => unsafe {
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            },
            Action::Quit => unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            },
        }
    }

    fn install_handlers() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
                let action = match event {
                    TrayIconEvent::Click { button, button_state, .. } => {
                        click_action(button == MouseButton::Left, button_state == MouseButtonState::Up, false)
                    }
                    TrayIconEvent::DoubleClick { button, .. } => click_action(button == MouseButton::Left, false, true),
                    _ => None,
                };
                if let Some(action) = action {
                    act(action);
                }
            }));
            MenuEvent::set_event_handler(Some(Arc::new(|event: MenuEvent| {
                let action = TARGET.with_borrow(|t| {
                    let [show, pause, quit] = &t.as_ref()?.items;
                    menu_choice(&event.id, [show, pause, quit])
                });
                if let Some(action) = action {
                    act(action);
                }
            })));
        });
    }

    fn build(title: &str) -> Result<(TrayIcon, [MenuId; 3]), String> {
        let show = MenuItem::new("Show playr", true, None);
        let pause = MenuItem::new("Play / Pause", true, None);
        let quit = MenuItem::new("Quit", true, None);
        let menu = Menu::new();
        menu.append_items(&[&show, &pause, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;
        let picture = crate::ui::window_icon();
        let icon = tray_icon::Icon::from_rgba(picture.rgba, picture.width, picture.height).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_icon(icon)
            .with_tooltip(tooltip(title))
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;
        Ok((tray, [show.id().clone(), pause.id().clone(), quit.id().clone()]))
    }

    /// Whether a taskbar is running to hold the icon. Without one (Explorer not running) the
    /// tray still reports success while showing nothing, and the hidden window would be lost.
    fn taskbar_running() -> bool {
        let class: Vec<u16> = "Shell_TrayWnd\0".encode_utf16().collect();
        !unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }.is_null()
    }

    /// The tray of playr's window.
    pub struct Tray {
        hwnd: Option<isize>,
        shared: Arc<Shared>,
    }

    impl Tray {
        /// `hwnd` is the window's handle, `None` when it could not be had.
        pub fn new(hwnd: Option<isize>, shared: Arc<Shared>) -> Self {
            Tray { hwnd, shared }
        }

        /// Hides the window and its taskbar button, showing an icon in the tray whose
        /// tooltip is `title`.
        pub fn hide(&mut self, title: &str) -> Hidden {
            let Some(hwnd) = self.hwnd else { return Hidden::Unavailable("the window handle is unknown".into()) };
            if !taskbar_running() {
                return Hidden::Unavailable("there is no taskbar to hold the tray icon".into());
            }
            install_handlers();
            let shown = TARGET.with_borrow_mut(|target| match target {
                Some(t) => t
                    .icon
                    .set_tooltip(Some(tooltip(title)))
                    .and_then(|_| t.icon.set_visible(true))
                    .map_err(|e| format!("cannot show the tray icon: {e}")),
                None => build(title).map_err(|e| format!("cannot add the tray icon: {e}")).map(|(icon, items)| {
                    *target = Some(Target { hwnd, shared: self.shared.clone(), icon, items });
                }),
            });
            if let Err(e) = shown {
                return Hidden::Unavailable(e);
            }
            unsafe {
                ShowWindow(hwnd as HWND, SW_HIDE);
            }
            Hidden::ToTray
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux_tray::{Tray, act, argb_icon};

#[cfg(target_os = "linux")]
mod linux_tray {
    use std::sync::Mutex;

    use eframe::egui::{Context, ViewportCommand};
    use ksni::blocking::{Handle, TrayMethods};
    use ksni::menu::{MenuItem, StandardItem};

    use super::{Action, Arc, Hidden, Shared, toggle_pause, tooltip};

    /// What the tray's handlers act on. They run on the tray's own thread, and the icon is
    /// made on each hide and taken down on each show, so it lives here rather than in [`Tray`].
    struct Target {
        ctx: Context,
        shared: Arc<Shared>,
        icon: Option<Handle<Item>>,
    }

    static TARGET: Mutex<Option<Target>> = Mutex::new(None);

    /// Does `action` as a click on the tray icon or its menu would. Nothing happens before the
    /// first [`Tray::hide`].
    pub fn act(action: Action) {
        let (ctx, shared, icon) = {
            let mut target = TARGET.lock().unwrap_or_else(|p| p.into_inner());
            let Some(t) = target.as_mut() else { return };
            let icon = if action == Action::TogglePause { None } else { t.icon.take() };
            (t.ctx.clone(), t.shared.clone(), icon)
        };
        // Called from the icon's own click handler, the icon must first answer the click: taken
        // down at once, it would leave the desktop's call without a reply.
        if let Some(icon) = icon {
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(200));
                icon.shutdown().wait();
            });
        }
        match action {
            Action::TogglePause => toggle_pause(&shared),
            Action::Show => {
                ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
            }
            Action::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
        }
        // A hidden window draws no frames of its own accord: wake it to carry the request out.
        ctx.request_repaint();
    }

    /// `picture` (RGBA rows) as the ARGB32, network byte order, a StatusNotifierItem carries.
    pub fn argb_icon(rgba: &[u8]) -> Vec<u8> {
        rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect()
    }

    struct Item {
        tooltip: String,
        icon: ksni::Icon,
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            "playr".into()
        }

        fn title(&self) -> String {
            "playr".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![self.icon.clone()]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: self.tooltip.clone(), ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            act(Action::Show);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let item = |label: &str, action: Action| {
                StandardItem { label: label.into(), activate: Box::new(move |_: &mut Self| act(action)), ..Default::default() }
                    .into()
            };
            vec![
                item("Show playr", Action::Show),
                item("Play / Pause", Action::TogglePause),
                MenuItem::Separator,
                item("Quit", Action::Quit),
            ]
        }

        /// The desktop stopped showing tray icons (its tray extension was turned off, or the
        /// shell restarted): bring the window back rather than leave it out of reach.
        fn watcher_offline(&self, _reason: ksni::OfflineReason) -> bool {
            act(Action::Show);
            false
        }
    }

    fn reason(e: &ksni::Error) -> String {
        match e {
            ksni::Error::Dbus(e) => format!("cannot reach the desktop's message bus (D-Bus): {e}"),
            // No watcher, no host to show the icon, and any reason a later ksni adds.
            _ => "the desktop shows no tray icons (on GNOME, turn on the AppIndicator extension)".into(),
        }
    }

    /// The tray of playr's window.
    pub struct Tray {
        window: Option<isize>,
        shared: Arc<Shared>,
        ctx: Option<Context>,
        wayland: bool,
    }

    impl Tray {
        /// `window` is the window's X11 id, `None` when it could not be had.
        pub fn new(window: Option<isize>, shared: Arc<Shared>) -> Self {
            Tray { window, shared, ctx: None, wayland: false }
        }

        /// The window's egui context, through which it is hidden and shown, and whether it is
        /// on Wayland.
        pub fn attach(&mut self, ctx: &Context, wayland: bool) {
            self.ctx = Some(ctx.clone());
            self.wayland = wayland;
        }

        /// Hides the window, showing an icon in the tray whose tooltip is `title`.
        pub fn hide(&mut self, title: &str) -> Hidden {
            if self.wayland {
                return Hidden::Unavailable("on Wayland a window cannot hide itself".into());
            }
            if self.window.is_none() {
                return Hidden::Unavailable("the window handle is unknown".into());
            }
            let Some(ctx) = self.ctx.clone() else { return Hidden::Unavailable("the window is not ready".into()) };
            let picture = crate::ui::window_icon();
            let icon = ksni::Icon { width: picture.width as i32, height: picture.height as i32, data: argb_icon(&picture.rgba) };
            // Registering with the desktop's tray takes a D-Bus round trip or two.
            let handle = match (Item { tooltip: tooltip(title), icon }).spawn() {
                Ok(h) => h,
                Err(e) => return Hidden::Unavailable(reason(&e)),
            };
            let old = TARGET.lock().unwrap_or_else(|p| p.into_inner()).replace(Target {
                ctx: ctx.clone(),
                shared: self.shared.clone(),
                icon: Some(handle),
            });
            if let Some(icon) = old.and_then(|t| t.icon) {
                icon.shutdown();
            }
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            Hidden::ToTray
        }
    }
}

/// Elsewhere there is no tray: the window is minimized instead.
#[cfg(not(any(windows, target_os = "linux")))]
pub struct Tray;

#[cfg(not(any(windows, target_os = "linux")))]
impl Tray {
    pub fn new(_hwnd: Option<isize>, _shared: Arc<Shared>) -> Self {
        Tray
    }

    pub fn hide(&mut self, _title: &str) -> Hidden {
        Hidden::Unavailable("there is no tray on this system".into())
    }
}
