//! The corner player: the window shrunk to a share of the screen and tucked into one of its
//! corners, on top of other windows, like a picture-in-picture view.

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub const ALL: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];

    pub fn label(self) -> &'static str {
        match self {
            Corner::TopLeft => "↖ Top left",
            Corner::TopRight => "↗ Top right",
            Corner::BottomLeft => "↙ Bottom left",
            Corner::BottomRight => "↘ Bottom right",
        }
    }
}

/// Shares of the work area's width the window may take.
pub const MIN_SHARE: f32 = 0.2;
pub const MAX_SHARE: f32 = 0.6;
/// What the window took when the user placed it by hand in a corner (720 of 1920 points).
pub const DEFAULT_SHARE: f32 = 0.375;
/// Gap between the window and the edges of the work area.
pub const MARGIN: f32 = 12.0;
/// Below this the picture and the control bar no longer fit.
pub const MIN_INNER_WIDTH: f32 = 240.0;
/// The smallest window in the corner, and the window's usual minimum otherwise.
pub const CORNER_MIN_INNER: Vec2 = vec2(MIN_INNER_WIDTH, 135.0);
pub const NORMAL_MIN_INNER: Vec2 = vec2(420.0, 260.0);
/// A corner player stays above the window the user works in unless told otherwise.
pub const KEEP_ON_TOP: bool = true;

/// A window in full screen or maximised has no title bar to measure, so it is first returned to
/// a normal window and placed on a later frame.
pub fn defer_placement(fullscreen: bool, maximized: bool) -> bool {
    fullscreen || maximized
}

/// The aspect to re-place the window for, when the picture's shape changed from `applied`.
/// Between list entries there is briefly no picture (`None`); that keeps the window as it is
/// instead of jumping to 16:9 and back.
pub fn needs_replace(applied: f32, picture: Option<f32>) -> Option<f32> {
    picture.filter(|a| a.is_finite() && *a > 0.0 && (a - applied).abs() > 0.01)
}

/// How far the window's outer rectangle reaches beyond its inner (client) one on each side:
/// the title bar and borders, measured from the live window.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Frame {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Frame {
    pub fn between(outer: Rect, inner: Rect) -> Frame {
        Frame {
            left: (inner.min.x - outer.min.x).max(0.0),
            top: (inner.min.y - outer.min.y).max(0.0),
            right: (outer.max.x - inner.max.x).max(0.0),
            bottom: (outer.max.y - inner.max.y).max(0.0),
        }
    }
}

/// Where the window goes and how large its inner area is: `share` of the work area's width
/// (clamped to [`MIN_SHARE`]..[`MAX_SHARE`]), the picture's `aspect` (width / height) kept,
/// shrunk to fit the work area's height, never narrower than [`MIN_INNER_WIDTH`].
pub fn placement(work: Rect, corner: Corner, share: f32, aspect: f32, frame: Frame) -> (Pos2, Vec2) {
    let aspect = if aspect.is_finite() && aspect > 0.1 { aspect } else { 16.0 / 9.0 };
    let share = if share.is_finite() { share.clamp(MIN_SHARE, MAX_SHARE) } else { DEFAULT_SHARE };
    let room_w = (work.width() - 2.0 * MARGIN - frame.left - frame.right).max(1.0);
    let room_h = (work.height() - 2.0 * MARGIN - frame.top - frame.bottom).max(1.0);
    let mut inner_w = work.width() * share - frame.left - frame.right;
    let mut inner_h = inner_w / aspect;
    if inner_h > room_h {
        inner_h = room_h;
        inner_w = inner_h * aspect;
    }
    inner_w = inner_w.min(room_w).max(MIN_INNER_WIDTH.min(room_w));
    inner_h = (inner_w / aspect).min(room_h);
    let outer = vec2(inner_w + frame.left + frame.right, inner_h + frame.top + frame.bottom);
    let x = match corner {
        Corner::TopLeft | Corner::BottomLeft => work.min.x + MARGIN,
        Corner::TopRight | Corner::BottomRight => work.max.x - MARGIN - outer.x,
    };
    let y = match corner {
        Corner::TopLeft | Corner::TopRight => work.min.y + MARGIN,
        Corner::BottomLeft | Corner::BottomRight => work.max.y - MARGIN - outer.y,
    };
    (pos2(x, y), vec2(inner_w, inner_h))
}

/// The usable part of the monitor around `point` (physical pixels): without the taskbar.
#[cfg(windows)]
pub fn work_area_px(point: Pos2) -> Option<Rect> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint};
    let pt = POINT { x: point.x.round() as i32, y: point.y.round() as i32 };
    // SAFETY: plain Win32 calls; `info` is a properly sized, initialised MONITORINFO.
    unsafe {
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let r = info.rcWork;
        Some(Rect::from_min_max(pos2(r.left as f32, r.top as f32), pos2(r.right as f32, r.bottom as f32)))
    }
}

#[cfg(not(windows))]
pub fn work_area_px(_point: Pos2) -> Option<Rect> {
    None
}
