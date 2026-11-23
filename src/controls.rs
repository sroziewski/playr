//! Transport controls: skipping back and forward by a chosen step, and the painted icons of
//! the control bar (the emoji font has no reliable glyphs for skip or full screen).

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, pos2, vec2};

/// The skip steps offered, in seconds, shortest first.
pub const SKIP_STEPS: [f64; 6] = [10.0, 20.0, 30.0, 60.0, 300.0, 600.0];
/// The step selected when playr starts (30 s).
pub const DEFAULT_STEP: usize = 2;
/// Skips this close together add up (see [`SkipChain`]).
pub const CHAIN_WINDOW: Duration = Duration::from_millis(1500);

pub const SPEEDS: [f64; 11] = [0.25, 0.5, 0.75, 1.0, 1.05, 1.1, 1.15, 1.25, 1.5, 1.75, 2.0];

pub fn speed_label(speed: f64) -> String {
    format!("{}×", (speed * 100.0).round() / 100.0)
}

pub fn speed_after(current: f64, delta: isize) -> f64 {
    const EPS: f64 = 1e-9;
    let mut speed = current;
    for _ in 0..delta.unsigned_abs() {
        let next = if delta > 0 {
            SPEEDS.iter().copied().find(|&s| s > speed + EPS)
        } else {
            SPEEDS.iter().rev().copied().find(|&s| s < speed - EPS)
        };
        match next {
            Some(s) => speed = s,
            None => break,
        }
    }
    if delta == 0 { current } else { speed.clamp(SPEEDS[0], SPEEDS[SPEEDS.len() - 1]) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageClick {
    Nothing,
    TogglePause,
    Fullscreen,
    FullscreenAndUndoPause,
}

pub fn stage_click(clicked: bool, double_clicked: bool, playing: bool, last_click_paused: bool) -> StageClick {
    match (double_clicked, clicked && playing) {
        (true, _) if playing && last_click_paused => StageClick::FullscreenAndUndoPause,
        (true, _) => StageClick::Fullscreen,
        (false, true) => StageClick::TogglePause,
        (false, false) => StageClick::Nothing,
    }
}

pub fn stage_step(
    last_click_paused: &mut bool,
    clicked: bool,
    double_clicked: bool,
    allowed: bool,
    playing: bool,
) -> StageClick {
    let action = stage_click(clicked && allowed, double_clicked && allowed, playing, *last_click_paused);
    if clicked {
        *last_click_paused = action == StageClick::TogglePause;
    }
    action
}

pub fn stage_input_allowed(url_dialog_open: bool, popup_open: bool) -> bool {
    !url_dialog_open && !popup_open
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Back {
    Restart,
    Previous,
    Disabled,
}

pub const RESTART_AFTER_SECS: f64 = 3.0;

pub fn back_action(position: f64, seekable: bool, entry: usize) -> Back {
    if seekable && position > RESTART_AFTER_SECS {
        Back::Restart
    } else if entry > 0 {
        Back::Previous
    } else {
        Back::Disabled
    }
}

pub fn history_item_enabled(target: &str, exists: Option<bool>) -> bool {
    crate::net::is_url(target) || exists != Some(false)
}

pub fn pasted_target(text: &str) -> Option<String> {
    let text = text.trim().trim_matches('"').trim();
    (!text.is_empty() && (crate::net::is_url(text) || std::path::Path::new(text).is_file())).then(|| text.to_string())
}

/// "10 s", "1 min", "10 min".
pub fn step_label(secs: f64) -> String {
    if secs >= 60.0 && secs % 60.0 == 0.0 { format!("{} min", secs as u64 / 60) } else { format!("{} s", secs as u64) }
}

/// "+30 s", "−1 min" (a real minus sign, as on the buttons).
pub fn skip_label(delta: f64) -> String {
    format!("{}{}", if delta < 0.0 { '−' } else { '+' }, step_label(delta.abs()))
}

/// The index of the step `delta` places away from `index`, stopping at the shortest and the
/// longest step.
pub fn step_after(index: usize, delta: isize) -> usize {
    index.saturating_add_signed(delta).min(SKIP_STEPS.len() - 1)
}

/// Why skipping is unavailable, for the tooltip and the on-screen message.
pub fn no_skip_reason(live: bool) -> &'static str {
    if live { "Live stream: there is nothing to skip" } else { "This media cannot be skipped through" }
}

/// What a key does in the player window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shortcut {
    PlayPause,
    SkipBack,
    SkipForward,
    ShorterStep,
    LongerStep,
    VolumeUp,
    VolumeDown,
    Mute,
    OpenFile,
    OpenUrl,
    ToggleFullscreen,
    LeaveFullscreen,
    Slower,
    Faster,
    ToggleCaptions,
}

/// The shortcut bound to `key`. While a text field has focus the keys belong to the text
/// (typing "f" in the station search must not go full screen), so there is none.
pub fn shortcut(key: Key, text_field_focused: bool) -> Option<Shortcut> {
    if text_field_focused {
        return None;
    }
    Some(match key {
        Key::Space => Shortcut::PlayPause,
        Key::ArrowLeft => Shortcut::SkipBack,
        Key::ArrowRight => Shortcut::SkipForward,
        Key::OpenBracket => Shortcut::ShorterStep,
        Key::CloseBracket => Shortcut::LongerStep,
        Key::ArrowUp => Shortcut::VolumeUp,
        Key::ArrowDown => Shortcut::VolumeDown,
        Key::M => Shortcut::Mute,
        Key::O => Shortcut::OpenFile,
        Key::U => Shortcut::OpenUrl,
        Key::F | Key::F12 => Shortcut::ToggleFullscreen,
        Key::Escape => Shortcut::LeaveFullscreen,
        Key::Comma => Shortcut::Slower,
        Key::Period => Shortcut::Faster,
        Key::C => Shortcut::ToggleCaptions,
        _ => return None,
    })
}

/// Where a skip of `delta` seconds from `from` lands: never before the start, and with a known
/// duration never past the end.
pub fn skip_target(from: f64, delta: f64, duration: Option<f64>) -> f64 {
    let to = (from + delta).max(0.0);
    match duration {
        Some(d) if d.is_finite() && d >= 0.0 => to.min(d),
        _ => to,
    }
}

/// Makes quick repeated skips add up.
///
/// The player reports its new position only after the pipeline has carried out a seek, so
/// three clicks on "+10 s" in a row would each start from the old position and move 10 s in
/// total. Within [`CHAIN_WINDOW`] of the previous skip, the next one starts from where the
/// previous one was headed instead.
#[derive(Debug, Default, Clone)]
pub struct SkipChain {
    last: Option<(f64, Instant)>,
}

impl SkipChain {
    /// The seek target for a skip of `delta` requested at `now` while playback is at `position`.
    pub fn skip(&mut self, position: f64, delta: f64, duration: Option<f64>, now: Instant) -> f64 {
        let from = match self.last {
            Some((target, at)) if now.saturating_duration_since(at) < CHAIN_WINDOW => target,
            _ => position,
        };
        let to = skip_target(from, delta, duration);
        self.last = Some((to, now));
        to
    }

    /// Forgets the previous skip, for when the position changed some other way (a new file, the
    /// seek bar).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Play,
    Pause,
    SkipBack,
    SkipForward,
    EnterFullscreen,
    ExitFullscreen,
}

/// A button showing `icon` (and `text` after it, if any). A disabled one ignores clicks and
/// shows the `on_disabled_hover_text` of its response instead of the `on_hover_text`.
pub fn icon_button(ui: &mut Ui, icon: Icon, text: &str, enabled: bool) -> Response {
    ui.add_enabled_ui(enabled, |ui| paint_button(ui, icon, text)).inner
}

fn paint_button(ui: &mut Ui, icon: Icon, text: &str) -> Response {
    let enabled = ui.is_enabled();
    let icon_size = 14.0;
    let galley = (!text.is_empty()).then(|| {
        ui.painter().layout_no_wrap(text.to_string(), egui::TextStyle::Button.resolve(ui.style()), Color32::PLACEHOLDER)
    });
    let pad = ui.spacing().button_padding;
    let gap = if galley.is_some() { 4.0 } else { 0.0 };
    let text_w = galley.as_ref().map_or(0.0, |g| g.size().x);
    let height = ui.spacing().interact_size.y.max(icon_size + 2.0 * pad.y);
    let size = vec2(pad.x * 2.0 + icon_size + gap + text_w, height);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = if enabled { ui.style().interact(&response) } else { &ui.visuals().widgets.noninteractive };
        let painter = ui.painter();
        if enabled {
            painter.rect(rect, visuals.corner_radius, visuals.weak_bg_fill, visuals.bg_stroke, egui::StrokeKind::Inside);
        }
        let color = if enabled { visuals.fg_stroke.color } else { ui.visuals().weak_text_color() };
        let icon_rect = Rect::from_center_size(pos2(rect.left() + pad.x + icon_size / 2.0, rect.center().y), Vec2::splat(icon_size));
        paint_icon(painter, icon_rect, icon, color);
        if let Some(galley) = galley {
            let at = pos2(icon_rect.right() + gap, rect.center().y - galley.size().y / 2.0);
            painter.galley(at, galley, color);
        }
    }
    response
}

/// Draws `icon` inside `rect` (a square) in `color`.
pub fn paint_icon(painter: &egui::Painter, rect: Rect, icon: Icon, color: Color32) {
    let (l, r, t, b) = (rect.left(), rect.right(), rect.top(), rect.bottom());
    let (w, h) = (rect.width(), rect.height());
    let triangle = |points: Vec<Pos2>| egui::Shape::convex_polygon(points, color, Stroke::NONE);
    match icon {
        Icon::Play => {
            painter.add(triangle(vec![pos2(l + w * 0.2, t), pos2(r - w * 0.05, rect.center().y), pos2(l + w * 0.2, b)]));
        }
        Icon::Pause => {
            let bar = w * 0.28;
            painter.rect_filled(Rect::from_min_max(pos2(l + w * 0.12, t), pos2(l + w * 0.12 + bar, b)), 1.0, color);
            painter.rect_filled(Rect::from_min_max(pos2(r - w * 0.12 - bar, t), pos2(r - w * 0.12, b)), 1.0, color);
        }
        Icon::SkipBack | Icon::SkipForward => {
            let (y0, y1, ym) = (t + h * 0.12, b - h * 0.12, rect.center().y);
            let half = w / 2.0;
            for start in [l, l + half] {
                let points = if icon == Icon::SkipForward {
                    vec![pos2(start, y0), pos2(start + half, ym), pos2(start, y1)]
                } else {
                    vec![pos2(start + half, y0), pos2(start + half, y1), pos2(start, ym)]
                };
                painter.add(triangle(points));
            }
        }
        Icon::EnterFullscreen | Icon::ExitFullscreen => {
            // Four corner brackets: pointing out to enter full screen, in to leave it.
            let stroke = Stroke::new(1.8, color);
            let arm = w * 0.36;
            for (cx, cy, dx, dy) in [(l, t, 1.0, 1.0), (r, t, -1.0, 1.0), (l, b, 1.0, -1.0), (r, b, -1.0, -1.0)] {
                let (corner, ax, ay) = if icon == Icon::EnterFullscreen {
                    (pos2(cx, cy), pos2(cx + dx * arm, cy), pos2(cx, cy + dy * arm))
                } else {
                    let c = pos2(cx + dx * arm, cy + dy * arm);
                    (c, pos2(c.x - dx * arm, c.y), pos2(c.x, c.y - dy * arm))
                };
                painter.line_segment([ax, corner], stroke);
                painter.line_segment([corner, ay], stroke);
            }
        }
    }
}
