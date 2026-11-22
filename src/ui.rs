//! The window: video surface, overlay controls, keyboard shortcuts, drag-and-drop.

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, Key, RichText, ViewportCommand};
use eframe::egui_glow;

use crate::cookies;
use crate::controls::{
    self, DEFAULT_STEP, Icon, SKIP_STEPS, SPEEDS, Shortcut, SkipChain, icon_button, skip_label, speed_label, step_label,
};
use crate::media::format_time;
use crate::net;
use crate::player::{Player, State, Status};
use crate::radio::{self, Station};
use crate::subtitles::Subtitles;
use crate::video_gl::VideoRenderer;
use crate::youtube::{self, Caption, CaptionKind, Choice, Video};

/// File types the open dialog offers. The installers in `install/` register playr for exactly
/// these with the desktop ("Open with"); `tests/installers.rs` keeps the lists equal.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "mp3", "flac", "aac", "m4a", "ogg", "oga", "wav", "m3u", "m3u8", "pls",
];
const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);
const OSD_FOR: Duration = Duration::from_millis(1200);

pub struct App {
    player: Player,
    renderer: Arc<Mutex<VideoRenderer>>,
    frame_size: Option<(u32, u32)>,
    last_source: String,
    last_title: String,
    last_motion: Instant,
    seek_drag: Option<f64>,
    volume: f32,
    url_open: bool,
    url_text: String,
    skip_step: usize,
    skip_chain: SkipChain,
    osd: Option<(String, Instant)>,
    radio_query: String,
    captions: Captions,
    cookie_note: Option<String>,
}

#[derive(Default)]
struct Captions {
    track: Option<Caption>,
    last: Option<Caption>,
    loaded: Option<Subtitles>,
    pending: Option<Receiver<Result<Subtitles, String>>>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<String>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let player = Player::new(Box::new(move || ctx.request_repaint()));
        if let Some(saved) = cookies::saved_path() {
            let _ = player.set_cookies(Some(saved));
        }
        if let Some(target) = initial {
            player.open(target);
        }
        Self {
            player,
            renderer: Arc::new(Mutex::new(VideoRenderer::default())),
            frame_size: None,
            last_source: String::new(),
            last_title: String::new(),
            last_motion: Instant::now(),
            seek_drag: None,
            volume: 1.0,
            url_open: false,
            url_text: String::new(),
            skip_step: DEFAULT_STEP,
            skip_chain: SkipChain::default(),
            osd: None,
            radio_query: String::new(),
            captions: Captions::default(),
            cookie_note: None,
        }
    }

    fn open_dialog(&mut self) {
        let picked = rfd::FileDialog::new()
            .add_filter("Media", MEDIA_EXTENSIONS)
            .add_filter("All files", &["*"])
            .pick_file();
        if let Some(path) = picked {
            self.player.open(path.to_string_lossy().into_owned());
        }
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.player.shared.audio.set_volume(self.volume);
    }

    fn skip(&mut self, status: &Status, delta: f64) {
        if !status.seekable {
            self.osd = Some((controls::no_skip_reason(status.live).to_string(), Instant::now()));
            return;
        }
        let to = self.skip_chain.skip(self.player.shared.position(), delta, status.duration, Instant::now());
        self.player.seek(to);
        self.osd = Some((format!("{}   →   {}", skip_label(delta), format_time(to)), Instant::now()));
    }

    fn set_skip_step(&mut self, step: usize) {
        self.skip_step = step;
        self.osd = Some((format!("Skip step: {}", step_label(SKIP_STEPS[self.skip_step])), Instant::now()));
    }

    fn browse_cookies(&mut self) {
        let picked = rfd::FileDialog::new()
            .set_title("Choose a cookies file (Netscape cookies.txt format)")
            .add_filter("Cookies (Netscape format)", &["txt"])
            .add_filter("All files", &["*"])
            .pick_file();
        if let Some(path) = picked {
            let note = match self.player.set_cookies(Some(path.clone())) {
                Ok(n) => format!("Using {n} cookies from {}", path.file_name().map_or_else(|| path.display().to_string(), |f| f.to_string_lossy().into_owned())),
                Err(e) => format!("Not a cookies file: {e}"),
            };
            self.show_osd(note.clone());
            self.cookie_note = Some(note);
        }
    }

    fn forget_cookies(&mut self) {
        let note = match self.player.set_cookies(None) {
            Ok(_) => "No cookies file".to_string(),
            Err(e) => format!("Could not forget the cookies file: {e}"),
        };
        self.show_osd(note.clone());
        self.cookie_note = Some(note);
    }

    fn cookie_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let current = self.player.shared.cookies_path();
            let name = current.as_ref().and_then(|p| p.file_name()).map(|f| f.to_string_lossy().into_owned());
            ui.label(format!("🍪 Cookies: {}", name.as_deref().unwrap_or("none")))
                .on_hover_text("A Netscape cookies.txt file exported from your browser. Only its spotify.com cookies go to Spotify and only its youtube.com / google.com cookies to YouTube.");
            if ui.button("Browse…").clicked() {
                self.browse_cookies();
            }
            if current.is_some() && ui.button("Forget").clicked() {
                self.forget_cookies();
            }
        });
        if let Some(note) = &self.cookie_note {
            ui.weak(note);
        }
    }

    fn playlist_controls(&mut self, ui: &mut egui::Ui, status: &Status) {
        let Some(list) = status.playlist.clone() else { return };
        let count = list.entries.len();
        let at = status.entry.min(count.saturating_sub(1));
        if ui.add_enabled(at > 0, egui::Button::new("⏮")).on_hover_text("Previous in the list").clicked() {
            self.player.open_entry(status.source.clone(), at - 1);
        }
        ui.menu_button(format!("📃 {}/{}", at + 1, count), |ui| {
            ui.strong(&list.name);
            ui.separator();
            egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                for (i, item) in list.entries.iter().enumerate() {
                    let label = format!("{}. {}", i + 1, item.label());
                    if ui.add(egui::Button::selectable(i == at, label)).clicked() && i != at {
                        self.player.open_entry(status.source.clone(), i);
                        ui.close();
                    }
                }
            });
        })
        .response
        .on_hover_text("Spotify list (played from YouTube, sound only)");
        if ui.add_enabled(at + 1 < count, egui::Button::new("⏭")).on_hover_text("Next in the list").clicked() {
            self.player.open_entry(status.source.clone(), at + 1);
        }
    }

    fn show_osd(&mut self, text: impl Into<String>) {
        self.osd = Some((text.into(), Instant::now()));
    }

    fn set_speed(&mut self, speed: f64) {
        self.player.set_speed(speed);
        self.show_osd(format!("Speed {}", speed_label(self.player.shared.speed())));
    }

    fn reopen_youtube(&mut self, status: &Status, choice: Choice, what: &str) {
        let position = self.player.shared.position();
        self.player.open_youtube(status.source.clone(), choice);
        self.player.seek(position);
        self.show_osd(format!("Switching to {what}…"));
    }

    fn select_caption(&mut self, ctx: &egui::Context, caption: Option<Caption>, headers: Vec<(String, String)>) {
        self.captions.loaded = None;
        self.captions.pending = None;
        if let Some(track) = self.captions.track.take() {
            self.captions.last = Some(track);
        }
        let Some(caption) = caption else {
            self.show_osd("Captions off");
            return;
        };
        let (tx, rx) = channel();
        let fetched = caption.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(youtube::fetch_caption(&fetched, &headers));
            repaint.request_repaint();
        });
        self.show_osd(format!("Captions: {}", caption.label));
        self.captions.track = Some(caption);
        self.captions.pending = Some(rx);
    }

    fn toggle_captions(&mut self, ctx: &egui::Context, status: &Status) {
        if self.captions.track.is_some() {
            self.select_caption(ctx, None, Vec::new());
            return;
        }
        let Some(video) = &status.youtube else {
            self.show_osd("No captions for this media");
            return;
        };
        let language = video.pick(&status.choice).1.and_then(|a| a.language.clone());
        let caption = self.captions.last.clone().or_else(|| video.default_caption(language.as_deref()).cloned());
        match caption {
            Some(c) => self.select_caption(ctx, Some(c), video.headers.clone()),
            None => self.show_osd("This video has no captions"),
        }
    }

    fn poll_captions(&mut self) {
        let Some(rx) = &self.captions.pending else { return };
        match rx.try_recv() {
            Ok(Ok(subs)) => {
                self.captions.loaded = Some(subs);
                self.captions.pending = None;
            }
            Ok(Err(e)) => {
                self.captions.pending = None;
                self.captions.track = None;
                self.show_osd(format!("Captions unavailable: {e}"));
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.captions.pending = None,
        }
    }

    fn open_pasted(&mut self, text: &str) {
        if let Some(target) = controls::pasted_target(text) {
            self.player.open(target);
        } else {
            self.show_osd("The clipboard holds no address or file to play");
        }
    }

    fn toggle_fullscreen(ctx: &egui::Context) {
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
    }

    fn handle_input(&mut self, ctx: &egui::Context, status: &Status) {
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.player.open(path.to_string_lossy().into_owned());
        }
        if ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO || i.pointer.any_pressed()) {
            self.last_motion = Instant::now();
        }
        // Shortcuts must not fire while the address field has focus.
        if self.url_open {
            return;
        }
        let text_focused = ctx.text_edit_focused();
        let pasted = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Paste(text) => Some(text.clone()),
                _ => None,
            })
        });
        if let Some(text) = pasted.filter(|_| !text_focused) {
            self.open_pasted(&text);
        }
        let keys: Vec<Key> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, .. } => Some(*key),
                    _ => None,
                })
                .collect()
        });
        let step = SKIP_STEPS[self.skip_step];
        for shortcut in keys.into_iter().filter_map(|key| controls::shortcut(key, text_focused)) {
            match shortcut {
                Shortcut::PlayPause => self.player.toggle_pause(),
                Shortcut::SkipBack => self.skip(status, -step),
                Shortcut::SkipForward => self.skip(status, step),
                Shortcut::ShorterStep => self.set_skip_step(controls::step_after(self.skip_step, -1)),
                Shortcut::LongerStep => self.set_skip_step(controls::step_after(self.skip_step, 1)),
                Shortcut::VolumeUp => self.set_volume(self.volume + 0.05),
                Shortcut::VolumeDown => self.set_volume(self.volume - 0.05),
                Shortcut::Mute => {
                    let audio = &self.player.shared.audio;
                    audio.set_muted(!audio.is_muted());
                }
                Shortcut::OpenFile => self.open_dialog(),
                Shortcut::OpenUrl => self.url_open = true,
                Shortcut::ToggleFullscreen => Self::toggle_fullscreen(ctx),
                Shortcut::LeaveFullscreen => {
                    if ctx.input(|i| i.viewport().fullscreen.unwrap_or(false)) {
                        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
                    }
                }
                Shortcut::Slower if status.seekable => self.set_speed(controls::speed_after(self.player.shared.speed(), -1)),
                Shortcut::Faster if status.seekable => self.set_speed(controls::speed_after(self.player.shared.speed(), 1)),
                Shortcut::Slower | Shortcut::Faster => {}
                Shortcut::ToggleCaptions => self.toggle_captions(ctx, status),
            }
        }
    }

    fn draw_stage(&mut self, ui: &mut egui::Ui, status: &Status, controls_shown: bool) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        let response = ui.interact(rect, ui.scope_id().with("stage"), egui::Sense::click());
        if response.double_clicked() {
            Self::toggle_fullscreen(ui.ctx());
        }

        let size = self.frame_size.or(status.video).filter(|&(w, h)| w > 0 && h > 0);
        if let (Some((w, h)), true) = (size, status.video.is_some()) {
            // Largest rectangle with the picture's aspect ratio that fits the window.
            let scale = (rect.width() / w as f32).min(rect.height() / h as f32);
            let fitted = egui::Rect::from_center_size(rect.center(), egui::vec2(w as f32 * scale, h as f32 * scale));
            let renderer = self.renderer.clone();
            ui.painter().add(egui::PaintCallback {
                rect: fitted,
                callback: Arc::new(egui_glow::CallbackFn::new(move |_info, painter| {
                    renderer.lock().unwrap().paint(painter.gl());
                })),
            });
        } else {
            let (headline, detail) = match status.state {
                State::Idle => ("playr".to_string(), "Drop a file here, press O to open one, or U for a URL".to_string()),
                State::Loading => ("Opening…".to_string(), status.title.clone()),
                State::Failed => ("Cannot play this".to_string(), status.error.clone().unwrap_or_default()),
                _ => (status.title.clone(), status.now_playing.clone().unwrap_or_else(|| status.info.clone())),
            };
            let center = rect.center();
            let painter = ui.painter();
            painter.text(center - egui::vec2(0.0, 14.0), Align2::CENTER_BOTTOM, headline, egui::FontId::proportional(26.0), Color32::WHITE);
            painter.text(center + egui::vec2(0.0, 6.0), Align2::CENTER_TOP, detail, egui::FontId::proportional(15.0), Color32::from_gray(170));
        }

        let mut y = rect.top() + 10.0;
        let mut lines: Vec<(String, Color32)> = Vec::new();
        if status.video.is_some()
            && let Some(e) = &status.error {
                lines.push((e.clone(), Color32::from_rgb(255, 120, 120)));
            }
        if let Some(e) = self.renderer.lock().unwrap().error() {
            lines.push((e.to_string(), Color32::from_rgb(255, 120, 120)));
        }
        lines.extend(status.notes.iter().map(|n| (n.clone(), Color32::from_rgb(255, 200, 110))));
        for (text, color) in lines {
            let galley = ui.painter().text(egui::pos2(rect.left() + 12.0, y), Align2::LEFT_TOP, text, egui::FontId::proportional(13.0), color);
            y += galley.height() + 4.0;
        }
        if status.state == State::Playing || status.state == State::Ended {
            self.draw_captions(ui, rect, controls_shown);
        }
        self.draw_osd(ui, rect);
    }

    fn draw_captions(&self, ui: &mut egui::Ui, rect: egui::Rect, controls_shown: bool) {
        let Some(subs) = &self.captions.loaded else { return };
        let lines = subs.lines_at(self.player.shared.position());
        if lines.is_empty() {
            return;
        }
        let painter = ui.painter();
        let font = egui::FontId::proportional((rect.height() * 0.045).clamp(16.0, 44.0));
        let mut bottom = rect.bottom() - if controls_shown { 96.0 } else { 36.0 };
        for line in lines.iter().rev() {
            let galley = painter.layout(line.to_string(), font.clone(), Color32::WHITE, rect.width() * 0.9);
            let size = galley.size();
            let top_left = egui::pos2(rect.center().x - size.x / 2.0, bottom - size.y);
            painter.rect_filled(egui::Rect::from_min_size(top_left, size).expand2(egui::vec2(8.0, 2.0)), 4.0, Color32::from_black_alpha(185));
            painter.galley(top_left, galley, Color32::WHITE);
            bottom -= size.y + 4.0;
        }
    }

    fn draw_osd(&mut self, ui: &mut egui::Ui, rect: egui::Rect) {
        let Some((text, at)) = &self.osd else { return };
        let shown = at.elapsed();
        if shown >= OSD_FOR {
            self.osd = None;
            return;
        }
        let painter = ui.painter();
        let galley = painter.layout_no_wrap(text.clone(), egui::FontId::proportional(22.0), Color32::WHITE);
        let center = egui::pos2(rect.center().x, rect.top() + 28.0 + galley.size().y / 2.0);
        let frame = egui::Rect::from_center_size(center, galley.size() + egui::vec2(28.0, 14.0));
        painter.rect_filled(frame, 8.0, Color32::from_black_alpha(170));
        painter.galley(center - galley.size() / 2.0, galley, Color32::WHITE);
        ui.ctx().request_repaint_after(OSD_FOR - shown);
    }

    fn draw_controls(&mut self, ctx: &egui::Context, status: &Status, position: f64) {
        let width = (ctx.content_rect().width() - 24.0).max(200.0);
        egui::Area::new(egui::Id::unique("playr-controls"))
            .anchor(Align2::CENTER_BOTTOM, egui::vec2(0.0, -10.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_width(width);
                    self.seek_row(ui, status, position);
                    self.button_row(ui, ctx, status);
                });
            });
    }

    fn seek_row(&mut self, ui: &mut egui::Ui, status: &Status, position: f64) {
        ui.horizontal(|ui| {
            let Some(duration) = status.duration.filter(|d| *d > 0.0) else {
                let label = if status.live && status.state == State::Playing { "LIVE" } else { "" };
                ui.label(format!("{}  {label}", format_time(position)));
                return;
            };
            let shown = self.seek_drag.unwrap_or(position).clamp(0.0, duration);
            ui.monospace(format_time(shown));
            let tail = format_time(duration);
            // Leave room for the trailing duration label.
            ui.spacing_mut().slider_width = (ui.available_width() - 12.0 - 9.0 * tail.len() as f32).max(60.0);
            let mut value = shown;
            let slider = ui.add_enabled(status.seekable, egui::Slider::new(&mut value, 0.0..=duration).show_value(false));
            if slider.dragged() {
                self.seek_drag = Some(value);
            }
            if slider.drag_stopped() {
                self.seek_drag = None;
                self.skip_chain.reset();
                self.player.seek(value);
            } else if slider.changed() && !slider.dragged() {
                self.skip_chain.reset();
                self.player.seek(value);
            }
            ui.monospace(tail);
        });
    }

    fn button_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, status: &Status) {
        ui.horizontal_wrapped(|ui| {
            let shared = self.player.shared.clone();
            let playing = status.state == State::Playing && !shared.is_paused();
            let can_play = matches!(status.state, State::Playing | State::Ended);
            let step = SKIP_STEPS[self.skip_step];
            let no_skip = controls::no_skip_reason(status.live);

            let back = icon_button(ui, Icon::SkipBack, &step_label(step), status.seekable)
                .on_hover_text(format!("Back {} (←)", step_label(step)))
                .on_disabled_hover_text(no_skip);
            if back.clicked() {
                self.skip(status, -step);
            }
            let (icon, tip) = if playing { (Icon::Pause, "Pause (Space)") } else { (Icon::Play, "Play (Space)") };
            if icon_button(ui, icon, "", can_play).on_hover_text(tip).clicked() {
                self.player.toggle_pause();
            }
            let forward = icon_button(ui, Icon::SkipForward, &step_label(step), status.seekable)
                .on_hover_text(format!("Forward {} (→)", step_label(step)))
                .on_disabled_hover_text(no_skip);
            if forward.clicked() {
                self.skip(status, step);
            }
            ui.add_enabled_ui(status.seekable, |ui| {
                ui.spacing_mut().item_spacing.x = 2.0;
                for (i, secs) in SKIP_STEPS.iter().enumerate() {
                    let chip = ui
                        .add(egui::Button::selectable(i == self.skip_step, step_label(*secs)).small())
                        .on_hover_text("Skip step for the skip buttons and the arrow keys ([ and ] change it)");
                    if chip.clicked() {
                        self.skip_step = i;
                    }
                }
            });
            ui.separator();
            let muted = shared.audio.is_muted();
            if ui.button(if muted { "🔇" } else { "🔊" }).on_hover_text("Mute (M)").clicked() {
                shared.audio.set_muted(!muted);
            }
            ui.spacing_mut().slider_width = 90.0;
            let mut volume = self.volume;
            if ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false)).on_hover_text("Volume (↑ ↓)").changed() {
                self.set_volume(volume);
            }
            ui.separator();
            if ui.button("Open").on_hover_text("Open a file (O)").clicked() {
                self.open_dialog();
            }
            if ui.button("URL").on_hover_text("Open an address: YouTube, Spotify, radio, file (U)").clicked() {
                self.url_open = true;
            }
            let cookie_tip = match self.player.shared.cookies_path() {
                Some(p) => format!("Cookies file: {}\nClick to choose another", p.display()),
                None => "Choose a browser cookies file (Netscape cookies.txt) for Spotify and YouTube".to_string(),
            };
            if ui.button("🍪").on_hover_text(cookie_tip).clicked() {
                self.browse_cookies();
            }
            let radio_label = if radio::find_by_url(&status.source).is_some() { "📻 Radio ●" } else { "📻 Radio" };
            ui.menu_button(radio_label, |ui| self.radio_menu(ui, status));
            ui.separator();
            let speed = shared.speed();
            ui.add_enabled_ui(status.seekable, |ui| {
                ui.menu_button(speed_label(speed), |ui| {
                    for s in SPEEDS {
                        let label = if s == 1.0 { "1× (normal)".to_string() } else { speed_label(s) };
                        if ui.add(egui::Button::selectable((s - speed).abs() < 1e-9, label)).clicked() {
                            self.set_speed(s);
                            ui.close();
                        }
                    }
                })
                .response
                .on_hover_text("Playback speed (< and >)")
                .on_disabled_hover_text(controls::no_skip_reason(status.live));
            });
            if let Some(video) = status.youtube.clone() {
                self.youtube_menus(ui, ctx, status, &video);
            }
            self.playlist_controls(ui, status);
            let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            let (icon, tip) = if fullscreen {
                (Icon::ExitFullscreen, "Exit full screen (F12, F, Esc)")
            } else {
                (Icon::EnterFullscreen, "Full screen (F12, F, double-click)")
            };
            if icon_button(ui, icon, "", true).on_hover_text(tip).clicked() {
                Self::toggle_fullscreen(ctx);
            }
            ui.separator();
            let text = match (&status.now_playing, status.title.is_empty()) {
                (Some(now), _) => format!("{} — {now}", status.title),
                (None, false) => format!("{}   {}", status.title, status.info),
                (None, true) => String::new(),
            };
            ui.add(egui::Label::new(RichText::new(text).weak()).truncate());
        });
    }

    fn youtube_menus(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, status: &Status, video: &Video) {
        let (current_video, current_audio) = video.pick(&status.choice);
        let mut audio_only = self.player.shared.youtube_audio_only();
        let tick = ui
            .checkbox(&mut audio_only, "🎧 Audio only")
            .on_hover_text("Play YouTube as sound only, like radio: no picture is downloaded or decoded");
        if tick.changed() {
            self.player.set_youtube_audio_only(audio_only);
            let what = if audio_only { "sound only" } else { "video and sound" };
            self.reopen_youtube(status, status.choice.clone(), what);
        }
        if !video.video.is_empty() && !audio_only {
            let label = current_video.map_or_else(|| "Quality".to_string(), |f| format!("⚙ {}", f.label()));
            ui.menu_button(label, |ui| {
                for format in &video.video {
                    let selected = current_video.is_some_and(|c| c.id == format.id);
                    if ui.add(egui::Button::selectable(selected, format.label())).clicked() && !selected {
                        let choice = Choice { video: Some(format.id.clone()), ..status.choice.clone() };
                        self.reopen_youtube(status, choice, &format.label());
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text("Picture quality");
        }
        if video.audio.len() > 1 {
            let label = current_audio.map_or_else(|| "Audio".to_string(), |a| format!("🗣 {}", a.language.clone().unwrap_or_else(|| a.label.clone())));
            ui.menu_button(label, |ui| {
                egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                    for track in &video.audio {
                        let selected = current_audio.is_some_and(|c| c.id == track.id);
                        if ui.add(egui::Button::selectable(selected, &track.label)).clicked() && !selected {
                            let choice = Choice { audio: Some(track.id.clone()), ..status.choice.clone() };
                            self.reopen_youtube(status, choice, &track.label);
                            ui.close();
                        }
                    }
                });
            })
            .response
            .on_hover_text("Audio track");
        }
        let on = self.captions.track.is_some();
        let cc = if on { "CC ●" } else { "CC" };
        ui.menu_button(cc, |ui| {
            if video.captions.is_empty() {
                ui.weak("This video has no captions");
                return;
            }
            if ui.add(egui::Button::selectable(!on, "Off")).clicked() {
                self.select_caption(ctx, None, Vec::new());
                ui.close();
            }
            let current = self.captions.track.as_ref().map(|c| c.code.clone());
            let mut picked = None;
            for caption in video.captions.iter().filter(|c| c.kind != CaptionKind::Translated) {
                if ui.add(egui::Button::selectable(current.as_deref() == Some(&caption.code), &caption.label)).clicked() {
                    picked = Some(caption.clone());
                }
            }
            let translated: Vec<&Caption> = video.captions.iter().filter(|c| c.kind == CaptionKind::Translated).collect();
            if !translated.is_empty() {
                ui.menu_button("Auto-translate", |ui| {
                    egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                        for caption in translated {
                            if ui.add(egui::Button::selectable(current.as_deref() == Some(&caption.code), &caption.label)).clicked() {
                                picked = Some(caption.clone());
                            }
                        }
                    });
                });
            }
            if let Some(caption) = picked {
                self.select_caption(ctx, Some(caption), video.headers.clone());
                ui.close();
            }
        })
        .response
        .on_hover_text("Subtitles / captions (C)");
    }

    fn radio_menu(&mut self, ui: &mut egui::Ui, status: &Status) {
        ui.set_min_width(280.0);
        ui.add(egui::TextEdit::singleline(&mut self.radio_query).hint_text("Search stations or genres…").desired_width(260.0));
        ui.separator();
        let playing = status.source.as_str();
        if self.radio_query.trim().is_empty() {
            for section in radio::sections() {
                if std::ptr::eq(section, &radio::RUSSIAN) {
                    ui.separator();
                }
                let current = section.contains(playing);
                let label = if current { format!("● {}", section.name) } else { section.name.to_string() };
                ui.menu_button(label, |ui| {
                    for station in section.stations {
                        self.station_button(ui, station, None, playing);
                    }
                });
            }
            return;
        }
        let found = radio::search(&self.radio_query);
        if found.is_empty() {
            ui.weak("No station matches");
            return;
        }
        egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
            for (section, station) in found {
                self.station_button(ui, station, Some(section.name), playing);
            }
        });
    }

    fn station_button(&self, ui: &mut egui::Ui, station: &Station, section: Option<&str>, playing: &str) {
        let current = station.url == playing;
        let mut job = egui::text::LayoutJob::default();
        let name = if current { RichText::new(station.name).strong() } else { RichText::new(station.name) };
        name.append_to(&mut job, ui.style(), egui::FontSelection::Default, egui::Align::Center);
        if let Some(section) = section {
            RichText::new(format!("   {section}")).weak().small().append_to(&mut job, ui.style(), egui::FontSelection::Default, egui::Align::Center);
        }
        if ui.add(egui::Button::selectable(current, job)).on_hover_text(station.url).clicked() {
            self.player.open(station.url);
            ui.close();
        }
    }

    fn draw_url_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.url_open;
        let mut submit = false;
        egui::Window::new("Open URL")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("YouTube or Spotify link, radio stream, playlist (.m3u / .pls) or media file address:");
                let edit = ui.add(egui::TextEdit::singleline(&mut self.url_text).desired_width(420.0).hint_text("https://…   (Ctrl+V to paste)"));
                edit.request_focus();
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                ui.horizontal(|ui| {
                    submit = ui.button("Play").clicked() || enter;
                });
                ui.separator();
                self.cookie_row(ui);
            });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            open = false;
        }
        if submit {
            let url = self.url_text.trim().to_string();
            if net::is_url(&url) {
                self.player.open(url);
                open = false;
            }
        }
        self.url_open = open;
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let shared = self.player.shared.clone();
        let status = shared.status();

        if status.source != self.last_source {
            self.last_source = status.source.clone();
            self.frame_size = None;
            self.renderer.lock().unwrap().clear();
            self.skip_chain.reset();
            self.captions = Captions::default();
        }
        self.poll_captions();
        let title = if status.title.is_empty() { "playr".to_string() } else { format!("{} — playr", status.title) };
        if title != self.last_title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.last_title = title;
        }

        self.handle_input(&ctx, &status);

        let (frame, next_in) = shared.take_due_frame();
        if let Some(frame) = frame {
            self.frame_size = Some((frame.width, frame.height));
            self.renderer.lock().unwrap().set_frame(frame);
        }
        let playing = status.state == State::Playing && !shared.is_paused();
        if playing {
            // Wake exactly when the next picture is due; with no picture queued, the pipeline
            // wakes us, and the slow tick only keeps the time display moving.
            let wait = (next_in.unwrap_or(0.25) / shared.speed()).clamp(0.001, 0.25);
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        }

        let idle = self.last_motion.elapsed();
        // Never hide the controls from under a slider being dragged or an open menu.
        let interacting = self.seek_drag.is_some() || ctx.egui_is_using_pointer() || ctx.any_popup_open();
        let auto_hide = status.video.is_some() && playing && !self.url_open && !interacting;
        let controls_shown = !auto_hide || idle < CONTROLS_HIDE_AFTER;
        self.draw_stage(ui, &status, controls_shown);

        if controls_shown {
            let position = match status.duration {
                Some(d) => shared.position().clamp(0.0, d),
                None => shared.position().max(0.0),
            };
            self.draw_controls(&ctx, &status, position);
            if auto_hide {
                ctx.request_repaint_after(CONTROLS_HIDE_AFTER - idle);
            }
        } else {
            ctx.set_cursor_icon(egui::CursorIcon::None);
        }
        if self.url_open {
            self.draw_url_dialog(&ctx);
        }
    }
}

/// The window and taskbar icon, decoded from `assets/playr.png`.
pub fn window_icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(include_bytes!("../assets/playr.png")).expect("assets/playr.png is a valid PNG")
}

pub fn run(initial: Option<String>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("playr")
            // On Linux the window is matched to playr.desktop (and its icon) by this app id.
            .with_app_id("playr")
            .with_icon(std::sync::Arc::new(window_icon()))
            .with_inner_size([960.0, 600.0])
            .with_min_inner_size([420.0, 260.0])
            .with_drag_and_drop(true),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native("playr", options, Box::new(|cc| Ok(Box::new(App::new(cc, initial)))))
}
