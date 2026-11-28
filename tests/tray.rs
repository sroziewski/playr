//! "To tray" in `playr::tray`: which clicks on the icon and which menu entries do what, the
//! icon's tooltip, Play / Pause from the tray menu on a real `Player`, and hiding a window
//! (none, and on Windows a real one of this test's own). Clicks on the real icon are not driven.

mod common;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use common::fixture;
use playr::player::{Player, State, Status};
use eframe::egui::ViewportCommand;
use playr::tray::{Action, Hidden, TOOLTIP_MAX, Tray, click_action, menu_choice, toggle_pause, tooltip, when_hidden};

const ELLIPSIS: char = '…';

// Each `Player` opens the default audio device; one at a time, at volume zero.
static AUDIO_DEVICE: Mutex<()> = Mutex::new(());

struct Session {
    player: Player,
    _device: MutexGuard<'static, ()>,
}

fn start() -> Session {
    let device = AUDIO_DEVICE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let player = Player::new(Box::new(|| {}));
    player.shared.audio.set_volume(0.0);
    Session { player, _device: device }
}

mod menu {
    use super::*;

    #[test]
    fn each_entry_gives_its_own_action_and_an_unknown_id_none() {
        let (show, pause, quit) = ("show-7", "pause-8", "quit-9");
        let cases: [(&str, &str, Option<Action>); 5] = [
            ("Show playr", show, Some(Action::Show)),
            ("Play / Pause", pause, Some(Action::TogglePause)),
            ("Quit", quit, Some(Action::Quit)),
            ("an id of no entry", "other-10", None),
            ("an empty id", "", None),
        ];
        let mut bad = Vec::new();
        for (name, id, expected) in cases {
            let actual = menu_choice(&id, [&show, &pause, &quit]);
            if actual != expected {
                bad.push(format!("{name}: {actual:?}, expected {expected:?}"));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}

mod click {
    use super::*;

    #[test]
    fn only_a_left_release_or_a_left_double_click_shows_the_window() {
        // (left, released, double) -> action; the right button opens the menu, which the tray
        // does by itself, and a left press that is not yet released does nothing.
        let cases: [(&str, bool, bool, bool, Option<Action>); 8] = [
            ("left pressed", true, false, false, None),
            ("left released", true, true, false, Some(Action::Show)),
            ("left double click", true, false, true, Some(Action::Show)),
            ("left released as part of a double click", true, true, true, Some(Action::Show)),
            ("right pressed", false, false, false, None),
            ("right released", false, true, false, None),
            ("right double click", false, false, true, None),
            ("right released as part of a double click", false, true, true, None),
        ];
        let mut bad = Vec::new();
        for (name, left, released, double, expected) in cases {
            let actual = click_action(left, released, double);
            if actual != expected {
                bad.push(format!("{name}: {actual:?}, expected {expected:?}"));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}

mod tooltip_text {
    use super::*;

    fn units(s: &str) -> usize {
        s.encode_utf16().count()
    }

    #[test]
    fn the_limit_is_127_utf16_units() {
        assert_eq!(TOOLTIP_MAX, 127);
    }

    #[test]
    fn a_title_that_fits_is_kept_trimmed_and_a_blank_one_becomes_playr() {
        let a127 = "a".repeat(127);
        // 'ż' is one UTF-16 unit but two UTF-8 bytes: 127 of them fit, though they take 254 bytes.
        let z127 = "ż".repeat(127);
        let cases: Vec<(&str, String, String)> = vec![
            ("short title", "Song — playr".into(), "Song — playr".into()),
            ("leading and trailing whitespace", "  \tSong — playr \n".into(), "Song — playr".into()),
            ("empty", "".into(), "playr".into()),
            ("blank", " \t\r\n ".into(), "playr".into()),
            ("exactly 127 ascii", a127.clone(), a127.clone()),
            ("exactly 127 two-byte chars", z127.clone(), z127.clone()),
            ("127 ascii once the padding is trimmed", format!("  {a127}  "), a127.clone()),
            ("an emoji pair that ends exactly at 127", format!("{}😀", "a".repeat(125)), format!("{}😀", "a".repeat(125))),
        ];
        let mut bad = Vec::new();
        for (name, title, expected) in cases {
            let actual = tooltip(&title);
            if actual != expected {
                bad.push(format!("{name}: {actual:?}, expected {expected:?}"));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }

    #[test]
    fn a_longer_title_is_cut_on_a_char_boundary_ending_in_an_ellipsis_within_127_units() {
        // A cut keeps as much as fits in 126 units, leaving one for "…" (a single unit).
        let cases: Vec<(&str, String, String)> = vec![
            ("128 ascii", "a".repeat(128), format!("{}{ELLIPSIS}", "a".repeat(126))),
            ("many ascii", "b".repeat(1000), format!("{}{ELLIPSIS}", "b".repeat(126))),
            ("128 two-byte chars", "ż".repeat(128), format!("{}{ELLIPSIS}", "ż".repeat(126))),
            (
                // 125 + 2 would be 127 units: the pair does not fit before "…" and is not split.
                "a surrogate pair straddling the cut",
                format!("{}😀b", "a".repeat(125)),
                format!("{}{ELLIPSIS}", "a".repeat(125)),
            ),
            (
                // 124 + 2 = 126 units: the pair fits whole, then "…" makes 127.
                "a surrogate pair ending right at the cut",
                format!("{}😀bc", "a".repeat(124)),
                format!("{}😀{ELLIPSIS}", "a".repeat(124)),
            ),
            ("only surrogate pairs", "😀".repeat(64), format!("{}{ELLIPSIS}", "😀".repeat(63))),
            ("whitespace trimmed before cutting", format!("   {}   ", "c".repeat(130)), format!("{}{ELLIPSIS}", "c".repeat(126))),
        ];
        let mut bad = Vec::new();
        for (name, title, expected) in cases {
            let actual = tooltip(&title);
            if actual != expected {
                bad.push(format!("{name}: {actual:?}, expected {expected:?}"));
            }
            if units(&actual) > TOOLTIP_MAX {
                bad.push(format!("{name}: {} UTF-16 units", units(&actual)));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}

mod play_pause {
    use super::*;

    fn path_of(name: &str) -> String {
        fixture(name).to_str().expect("utf-8 path").to_string()
    }

    impl Session {
        fn wait_for_state(&self, state: State, timeout: Duration) -> Status {
            let started = Instant::now();
            loop {
                let status = self.player.shared.status();
                if status.state == state {
                    return status;
                }
                if started.elapsed() > timeout {
                    panic!("timed out after {timeout:?} waiting for {state:?}; status {status:?}");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }

        /// `is_paused` after each of two presses of Play / Pause in the tray, and the state after them.
        fn press_twice(&self) -> (bool, bool, State) {
            toggle_pause(&self.player.shared);
            let first = self.player.shared.is_paused();
            toggle_pause(&self.player.shared);
            let second = self.player.shared.is_paused();
            (first, second, self.player.shared.status().state)
        }
    }

    #[test]
    fn pauses_and_resumes_while_playing() {
        let s = start();
        s.player.open(path_of("sine.wav"));
        s.wait_for_state(State::Playing, Duration::from_secs(20));
        assert!(!s.player.shared.is_paused(), "playing starts unpaused");

        assert_eq!(s.press_twice(), (true, false, State::Playing));
    }

    #[test]
    fn toggles_with_nothing_open() {
        let s = start();
        assert_eq!(s.player.shared.status().state, State::Idle);
        assert!(!s.player.shared.is_paused(), "a new player starts unpaused");

        assert_eq!(s.press_twice(), (true, false, State::Idle));
    }

    #[test]
    fn toggles_after_a_failure() {
        let s = start();
        let missing = fixture("sine.wav").with_file_name("no-such-file-for-the-tray.wav");
        s.player.open(missing.to_str().expect("utf-8 path"));
        s.wait_for_state(State::Failed, Duration::from_secs(20));
        assert!(s.player.shared.is_paused(), "a failure pauses");

        assert_eq!(s.press_twice(), (false, true, State::Failed));
    }

    #[test]
    fn leaves_ended_media_as_it_is() {
        let s = start();
        s.player.open(path_of("sine.wav"));
        s.wait_for_state(State::Ended, Duration::from_secs(20));
        assert!(s.player.shared.is_paused(), "the clock is held at the end");
        let position = s.player.shared.position();

        toggle_pause(&s.player.shared);

        assert!(s.player.shared.is_paused(), "Play / Pause at the end must not unpause");
        assert_eq!(s.player.shared.status().state, State::Ended);
        // The audio device may still report the last microseconds heard, so not exact; a restart
        // would put the clock back near 0 s.
        let after = s.player.shared.position();
        assert!((after - position).abs() < 0.05, "Play / Pause at the end moved the clock from {position} to {after}");
    }
}

mod hide {
    use super::*;

    #[test]
    fn without_a_window_handle_there_is_no_tray_and_the_reason_is_given() {
        let s = start();
        let mut tray = Tray::new(None, s.player.shared.clone());

        let results: Vec<Hidden> = ["Song — playr", ""].into_iter().map(|title| tray.hide(title)).collect();

        let why = "the window handle is unknown".to_string();
        assert_eq!(results, [Hidden::Unavailable(why.clone()), Hidden::Unavailable(why)]);
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    #[test]
    fn elsewhere_there_is_no_tray_even_with_a_window_handle() {
        let s = start();
        let mut tray = Tray::new(Some(0x5a17), s.player.shared.clone());

        assert_eq!(tray.hide("Song — playr"), Hidden::Unavailable("there is no tray on this system".into()));
    }

    #[cfg(windows)]
    mod windows_window {
        use super::*;
        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, FindWindowW, IsWindowVisible, MSG, PM_REMOVE,
            PeekMessageW, RegisterClassW, SW_SHOW, ShowWindow, WM_CLOSE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        };
        use playr::tray::act;

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain([0]).collect()
        }

        /// A plain top-level window of this test, shown, destroyed on drop.
        struct TestWindow(HWND);

        impl TestWindow {
            fn new() -> Self {
                let class = wide("playr-tray-test-window");
                let title = wide("playr tray test");
                unsafe {
                    let instance = GetModuleHandleW(std::ptr::null());
                    let wc = WNDCLASSW {
                        style: 0,
                        lpfnWndProc: Some(DefWindowProcW),
                        cbClsExtra: 0,
                        cbWndExtra: 0,
                        hInstance: instance,
                        hIcon: std::ptr::null_mut(),
                        hCursor: std::ptr::null_mut(),
                        hbrBackground: std::ptr::null_mut(),
                        lpszMenuName: std::ptr::null(),
                        lpszClassName: class.as_ptr(),
                    };
                    // 0 also when a previous run in this process registered it already.
                    RegisterClassW(&wc);
                    let hwnd = CreateWindowExW(
                        0,
                        class.as_ptr(),
                        title.as_ptr(),
                        WS_OVERLAPPEDWINDOW,
                        CW_USEDEFAULT,
                        CW_USEDEFAULT,
                        320,
                        200,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        instance,
                        std::ptr::null(),
                    );
                    assert!(!hwnd.is_null(), "CreateWindowExW failed");
                    TestWindow(hwnd)
                }
            }

            fn show(&self) {
                unsafe { ShowWindow(self.0, SW_SHOW) };
            }

            fn visible(&self) -> bool {
                unsafe { IsWindowVisible(self.0) != 0 }
            }
        }

        impl Drop for TestWindow {
            fn drop(&mut self) {
                unsafe { DestroyWindow(self.0) };
            }
        }

        fn taskbar_running() -> bool {
            let class = wide("Shell_TrayWnd");
            !unsafe { FindWindowW(class.as_ptr(), std::ptr::null()) }.is_null()
        }

        #[test]
        fn a_real_window_is_hidden_to_the_tray_and_again_after_being_shown() {
            let s = start();
            let window = TestWindow::new();
            window.show();
            assert!(window.visible(), "test precondition: the window is shown");
            let mut tray = Tray::new(Some(window.0 as isize), s.player.shared.clone());

            let first = tray.hide("First song — playr");
            let hidden_first = !window.visible();
            window.show();
            let shown_between = window.visible();
            let second = tray.hide("Second song — playr");
            let hidden_second = !window.visible();

            if !taskbar_running() {
                // Without a taskbar (no Explorer) the window must stay where it is.
                let why = || Hidden::Unavailable("there is no taskbar to hold the tray icon".into());
                assert_eq!((first, hidden_first, second, hidden_second), (why(), false, why(), false));
                return;
            }
            assert_eq!((first, hidden_first, shown_between, second, hidden_second), (Hidden::ToTray, true, true, Hidden::ToTray, true));

            // Play / Pause from the menu: the player toggles, the window stays in the tray.
            let paused_before = s.player.shared.is_paused();
            act(Action::TogglePause);
            let after_toggle = (s.player.shared.is_paused(), window.visible());
            act(Action::TogglePause);
            let after_second_toggle = (s.player.shared.is_paused(), window.visible());
            assert_eq!(
                (after_toggle, after_second_toggle),
                ((!paused_before, false), (paused_before, false)),
                "(paused, window visible) after each Play / Pause"
            );

            act(Action::Show);
            assert!(window.visible(), "Show brings the window back");

            assert_eq!(tray.hide("Third song — playr"), Hidden::ToTray);
            assert!(!window.visible(), "hidden again before Quit");
            assert_eq!(close_requests(&window), 0, "test precondition: no WM_CLOSE queued yet");
            act(Action::Quit);
            assert_eq!(close_requests(&window), 1, "Quit posts one WM_CLOSE to the window");
        }

        /// How many WM_CLOSE messages were queued for `window`, taking them off the queue.
        fn close_requests(window: &TestWindow) -> usize {
            let mut count = 0;
            loop {
                let mut msg: MSG = unsafe { std::mem::zeroed() };
                if unsafe { PeekMessageW(&mut msg, window.0, WM_CLOSE, WM_CLOSE, PM_REMOVE) } == 0 {
                    return count;
                }
                assert_eq!((msg.message, msg.hwnd), (WM_CLOSE, window.0), "PeekMessageW filtered on WM_CLOSE for this window");
                count += 1;
            }
        }

        #[test]
        fn before_the_first_hide_on_a_thread_a_tray_action_does_nothing() {
            // A thread of its own: the tray's state belongs to the thread that hid a window.
            std::thread::spawn(|| {
                let window = TestWindow::new();
                assert!(!window.visible(), "test precondition: a new window is not shown");

                act(Action::Show);
                act(Action::Quit);

                assert_eq!((window.visible(), close_requests(&window)), (false, 0), "(visible, WM_CLOSE queued)");
            })
            .join()
            .expect("the thread's assertions");
        }
    }

    /// Linux: the icon is a StatusNotifierItem on the session D-Bus, and the window is hidden
    /// and shown through egui viewport commands, here on an egui context of the test's own.
    #[cfg(target_os = "linux")]
    mod linux_window {
        use super::*;
        use eframe::egui::{Context, RawInput, ViewportId};
        use playr::tray::{act, argb_icon};

        /// The viewport commands `ctx` was asked for since the last call, without the theme egui
        /// sets on its own first pass.
        fn commands(ctx: &Context) -> Vec<ViewportCommand> {
            let out = ctx.run_ui(RawInput::default(), |_| {});
            let all = out.viewport_output.get(&ViewportId::ROOT).map(|v| v.commands.clone()).unwrap_or_default();
            all.into_iter().filter(|c| !matches!(c, ViewportCommand::SetTheme(_))).collect()
        }

        const CHILD: &str = "PLAYR_TEST_CHILD";

        /// Runs `test` alone in a copy of this binary with `env` added and returns false; in that
        /// copy returns true, and the caller runs the test body.
        fn in_child(test: &str, env: &[(&str, &str)]) -> bool {
            if std::env::var_os(CHILD).is_some() {
                return true;
            }
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test, "--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD, "1")
                .envs(env.iter().copied())
                .output()
                .expect("start the child test");
            let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success(), "{test} failed in the child process:\n{stdout}\n{stderr}");
            assert!(stdout.contains("test result: ok. 1 passed"), "{test} did not run in the child process:\n{stdout}");
            false
        }

        #[test]
        fn the_icon_is_the_window_icon_in_argb_network_order() {
            let rgba = [0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xff];
            assert_eq!(argb_icon(&rgba), [0x44, 0x11, 0x22, 0x33, 0xff, 0xaa, 0xbb, 0xcc]);
        }

        #[test]
        fn on_wayland_the_window_cannot_hide_and_is_left_alone() {
            let s = start();
            let ctx = Context::default();
            let mut tray = Tray::new(None, s.player.shared.clone());
            tray.attach(&ctx, true);

            let hidden = tray.hide("Song — playr");

            assert_eq!(hidden, Hidden::Unavailable("on Wayland a window cannot hide itself".into()));
            assert_eq!(commands(&ctx), []);
        }

        #[test]
        fn before_the_window_is_attached_there_is_no_tray() {
            let s = start();
            let mut tray = Tray::new(Some(0x5a17), s.player.shared.clone());

            assert_eq!(tray.hide("Song — playr"), Hidden::Unavailable("the window is not ready".into()));
        }

        #[test]
        fn without_a_message_bus_the_reason_is_given_and_the_window_stays() {
            let test = "hide::linux_window::without_a_message_bus_the_reason_is_given_and_the_window_stays";
            if !in_child(test, &[("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/playr-test-bus")]) {
                return;
            }
            let s = start();
            let ctx = Context::default();
            let mut tray = Tray::new(Some(0x5a17), s.player.shared.clone());
            tray.attach(&ctx, false);

            let hidden = tray.hide("Song — playr");

            let Hidden::Unavailable(why) = &hidden else { panic!("hidden without a tray: {hidden:?}") };
            assert!(why.starts_with("cannot reach the desktop's message bus (D-Bus)"), "{why}");
            assert_eq!(commands(&ctx), []);
        }

        /// Needs a desktop that shows tray icons (KDE, or GNOME with the AppIndicator extension);
        /// elsewhere it checks only that the window is left alone. The icon shows for a moment.
        #[test]
        fn hiding_shows_the_icon_and_the_icon_shows_the_window_and_quits() {
            let s = start();
            let ctx = Context::default();
            let mut tray = Tray::new(Some(0x5a17), s.player.shared.clone());
            tray.attach(&ctx, false);

            match tray.hide("Song — playr") {
                Hidden::Unavailable(why) => {
                    eprintln!("no tray on this desktop ({why}); only the fallback is checked");
                    assert_eq!(commands(&ctx), []);
                }
                Hidden::ToTray => {
                    assert_eq!(commands(&ctx), [ViewportCommand::Visible(false)]);
                    act(Action::Show);
                    assert_eq!(commands(&ctx), [ViewportCommand::Visible(true), ViewportCommand::Focus]);
                    act(Action::Quit);
                    assert_eq!(commands(&ctx), [ViewportCommand::Close]);
                }
            }
        }
    }
}

#[test]
fn when_hidden_to_the_tray_nothing_else_happens_otherwise_the_window_minimizes_saying_why() {
    assert_eq!(when_hidden(Hidden::ToTray), None);
    assert_eq!(
        when_hidden(Hidden::Unavailable("there is no taskbar to hold the tray icon".into())),
        Some(("Minimized: there is no taskbar to hold the tray icon".to_string(), ViewportCommand::Minimized(true)))
    );
    assert_eq!(
        when_hidden(Hidden::Unavailable("the window handle is unknown".into())),
        Some(("Minimized: the window handle is unknown".to_string(), ViewportCommand::Minimized(true)))
    );
}
