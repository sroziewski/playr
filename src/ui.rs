//! The window: video surface, overlay controls, keyboard shortcuts, drag-and-drop.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, Key, RichText, ViewportCommand};
use eframe::egui_glow;

use crate::media::format_time;
use crate::net;
use crate::player::{Player, State, Status};
use crate::video_gl::VideoRenderer;

/// Stations offered in the Radio menu (name, address).
pub const STATIONS: &[(&str, &str)] = &[
    ("SomaFM Groove Salad (MP3)", "https://ice1.somafm.com/groovesalad-128-mp3"),
    ("SomaFM Groove Salad (AAC)", "https://ice1.somafm.com/groovesalad-128-aac"),
    ("SomaFM Indie Pop Rocks", "https://ice6.somafm.com/indiepop-128-mp3"),
    ("SomaFM Drone Zone", "https://ice1.somafm.com/dronezone-128-mp3"),
    ("BBC World Service", "http://stream.live.vc.bbcmedia.co.uk/bbc_world_service"),
];

/// File types the open dialog offers. The installers in `install/` register playr for exactly
/// these with the desktop ("Open with"); `tests/installers.rs` keeps the lists equal.
pub const MEDIA_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "webm", "avi", "mp3", "flac", "aac", "m4a", "ogg", "oga", "wav", "m3u", "m3u8", "pls",
];
const CONTROLS_HIDE_AFTER: Duration = Duration::from_millis(2500);
const SEEK_STEP: f64 = 5.0;

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
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<String>) -> Self {
        let ctx = cc.egui_ctx.clone();
        let player = Player::new(Box::new(move || ctx.request_repaint()));
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

    fn seek_by(&self, status: &Status, delta: f64) {
        if status.seekable {
            self.player.seek(self.player.shared.position() + delta);
        }
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
        let pressed = |key: Key| ctx.input(|i| i.key_pressed(key));
        if pressed(Key::Space) {
            self.player.toggle_pause();
        }
        if pressed(Key::ArrowLeft) {
            self.seek_by(status, -SEEK_STEP);
        }
        if pressed(Key::ArrowRight) {
            self.seek_by(status, SEEK_STEP);
        }
        if pressed(Key::ArrowUp) {
            self.set_volume(self.volume + 0.05);
        }
        if pressed(Key::ArrowDown) {
            self.set_volume(self.volume - 0.05);
        }
        if pressed(Key::M) {
            let audio = &self.player.shared.audio;
            audio.set_muted(!audio.is_muted());
        }
        if pressed(Key::O) {
            self.open_dialog();
        }
        if pressed(Key::U) {
            self.url_open = true;
        }
        let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
        if pressed(Key::F) {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
        }
        if pressed(Key::Escape) && fullscreen {
            ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        }
    }

    fn draw_stage(&mut self, ui: &mut egui::Ui, status: &Status) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, Color32::BLACK);
        let response = ui.interact(rect, ui.scope_id().with("stage"), egui::Sense::click());
        if response.double_clicked() {
            let fullscreen = ui.ctx().input(|i| i.viewport().fullscreen.unwrap_or(false));
            ui.ctx().send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
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
                self.player.seek(value);
            } else if slider.changed() && !slider.dragged() {
                self.player.seek(value);
            }
            ui.monospace(tail);
        });
    }

    fn button_row(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, status: &Status) {
        ui.horizontal(|ui| {
            let shared = self.player.shared.clone();
            let playing = status.state == State::Playing && !shared.is_paused();
            let can_play = matches!(status.state, State::Playing | State::Ended);
            if ui.add_enabled(can_play, egui::Button::new(if playing { "⏸" } else { "▶" })).clicked() {
                self.player.toggle_pause();
            }
            let muted = shared.audio.is_muted();
            if ui.button(if muted { "🔇" } else { "🔊" }).clicked() {
                shared.audio.set_muted(!muted);
            }
            ui.spacing_mut().slider_width = 90.0;
            let mut volume = self.volume;
            if ui.add(egui::Slider::new(&mut volume, 0.0..=1.0).show_value(false)).changed() {
                self.set_volume(volume);
            }
            ui.separator();
            if ui.button("Open").clicked() {
                self.open_dialog();
            }
            if ui.button("URL").clicked() {
                self.url_open = true;
            }
            ui.menu_button("Radio", |ui| {
                for (name, url) in STATIONS {
                    if ui.button(*name).clicked() {
                        self.player.open(*url);
                        ui.close();
                    }
                }
            });
            if ui.button("⛶").clicked() {
                let fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fullscreen));
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

    fn draw_url_dialog(&mut self, ctx: &egui::Context) {
        let mut open = self.url_open;
        let mut submit = false;
        egui::Window::new("Open URL")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("Radio stream, playlist (.m3u / .pls) or media file address:");
                let edit = ui.add(egui::TextEdit::singleline(&mut self.url_text).desired_width(420.0).hint_text("https://…"));
                edit.request_focus();
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                ui.horizontal(|ui| {
                    submit = ui.button("Play").clicked() || enter;
                });
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
        }
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
            let wait = next_in.unwrap_or(0.25).clamp(0.001, 0.25);
            ctx.request_repaint_after(Duration::from_secs_f64(wait));
        }

        self.draw_stage(ui, &status);

        let idle = self.last_motion.elapsed();
        // Never hide the controls from under a slider being dragged or an open menu.
        let interacting = self.seek_drag.is_some() || ctx.egui_is_using_pointer() || ctx.any_popup_open();
        let auto_hide = status.video.is_some() && playing && !self.url_open && !interacting;
        if !auto_hide || idle < CONTROLS_HIDE_AFTER {
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
