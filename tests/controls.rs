//! Skip steps, skip targets and the skip chain of `playr::controls`, plus a headless egui frame
//! that checks where `paint_icon` draws.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Key, Pos2, Rect, Shape, pos2, vec2};
use playr::controls::{
    CHAIN_WINDOW, DEFAULT_STEP, Icon, SKIP_STEPS, Shortcut, SkipChain, icon_button, no_skip_reason, paint_icon, shortcut,
    skip_label, skip_target, step_after, step_label,
};

const MINUS: char = '\u{2212}';

#[test]
fn the_six_steps_are_10_s_20_s_30_s_1_min_5_min_10_min() {
    let labels: Vec<String> = SKIP_STEPS.iter().map(|&s| step_label(s)).collect();

    assert_eq!(SKIP_STEPS, [10.0, 20.0, 30.0, 60.0, 300.0, 600.0]);
    assert_eq!(labels, ["10 s", "20 s", "30 s", "1 min", "5 min", "10 min"]);
}

#[test]
fn the_default_step_is_30_s() {
    assert_eq!(SKIP_STEPS[DEFAULT_STEP], 30.0);
}

#[test]
fn the_chain_window_is_1500_ms() {
    assert_eq!(CHAIN_WINDOW, Duration::from_millis(1500));
}

#[test]
fn step_label_uses_minutes_only_for_whole_minutes() {
    let cases = [
        (10.0, "10 s"),
        (45.0, "45 s"),
        (59.0, "59 s"),
        (60.0, "1 min"),
        (90.0, "90 s"),
        (120.0, "2 min"),
        (300.0, "5 min"),
        (600.0, "10 min"),
    ];
    let mut bad = Vec::new();
    for (secs, expected) in cases {
        let actual = step_label(secs);
        if actual != expected {
            bad.push(format!("step_label({secs}) = {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn skip_label_has_a_plus_or_a_real_minus_sign() {
    let cases = [
        (10.0, "+10 s".to_string()),
        (30.0, "+30 s".to_string()),
        (600.0, "+10 min".to_string()),
        (-10.0, format!("{MINUS}10 s")),
        (-60.0, format!("{MINUS}1 min")),
        (-300.0, format!("{MINUS}5 min")),
    ];
    let mut bad = Vec::new();
    for (delta, expected) in cases {
        let actual = skip_label(delta);
        if actual != expected {
            bad.push(format!("skip_label({delta}) = {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    assert_eq!(skip_label(-60.0).chars().next(), Some('\u{2212}'), "U+2212, not the ASCII hyphen");
}

#[test]
fn skip_target_clamps_to_the_start_and_to_a_known_end() {
    // (from, delta, duration, expected, case)
    let cases: [(f64, f64, Option<f64>, f64, &str); 15] = [
        (100.0, 30.0, Some(500.0), 130.0, "forward inside the file"),
        (100.0, -30.0, Some(500.0), 70.0, "back inside the file"),
        (30.0, -30.0, Some(500.0), 0.0, "back exactly to the start"),
        (20.0, -30.0, Some(500.0), 0.0, "back past the start"),
        (-5.0, 3.0, Some(500.0), 0.0, "negative from still lands before the start"),
        (-5.0, 10.0, Some(500.0), 5.0, "negative from moved past the start"),
        (470.0, 30.0, Some(500.0), 500.0, "forward exactly to the end"),
        (480.0, 30.0, Some(500.0), 500.0, "forward past the end"),
        (480.0, 600.0, Some(500.0), 500.0, "a whole step past the end"),
        (0.0, 10.0, Some(0.0), 0.0, "duration 0"),
        (480.0, 30.0, None, 510.0, "unknown duration: no upper clamp"),
        (20.0, -30.0, None, 0.0, "unknown duration: still clamped at the start"),
        (480.0, 30.0, Some(f64::INFINITY), 510.0, "infinite duration"),
        (480.0, 30.0, Some(f64::NAN), 510.0, "NaN duration counts as unknown"),
        (480.0, 30.0, Some(-5.0), 510.0, "negative duration counts as unknown"),
    ];
    let mut bad = Vec::new();
    for (from, delta, duration, expected, case) in cases {
        let actual = skip_target(from, delta, duration);
        if actual != expected {
            bad.push(format!("{case}: skip_target({from}, {delta}, {duration:?}) = {actual}, expected {expected}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

enum Step {
    /// Skip by `delta` at `t0 + at_ms` while playback reports `position`; expect `target`.
    Skip { at_ms: u64, position: f64, delta: f64, duration: Option<f64>, target: f64 },
    Reset,
}

fn skip(at_ms: u64, position: f64, delta: f64, duration: Option<f64>, target: f64) -> Step {
    Step::Skip { at_ms, position, delta, duration, target }
}

#[test]
fn skip_chain_cases() {
    let window = CHAIN_WINDOW.as_millis() as u64;
    let cases: Vec<(&str, Vec<Step>)> = vec![
        (
            "three +10 s within the window accumulate from 100 to 130",
            vec![skip(0, 100.0, 10.0, None, 110.0), skip(400, 100.0, 10.0, None, 120.0), skip(800, 100.0, 10.0, None, 130.0)],
        ),
        (
            "1499 ms after the previous skip still chains",
            vec![skip(0, 100.0, 10.0, None, 110.0), skip(window - 1, 104.0, 10.0, None, 120.0)],
        ),
        (
            "1500 ms after the previous skip starts from the reported position",
            vec![skip(0, 100.0, 10.0, None, 110.0), skip(window, 104.0, 10.0, None, 114.0)],
        ),
        (
            "the window counts from the latest skip, not the first of the chain",
            vec![
                skip(0, 100.0, 10.0, None, 110.0),
                skip(1400, 101.0, 10.0, None, 120.0),
                skip(2800, 102.0, 10.0, None, 130.0),
            ],
        ),
        (
            "the first skip of a fresh chain starts from the reported position",
            vec![skip(250, 42.0, 20.0, Some(500.0), 62.0)],
        ),
        (
            "reset forgets the chain",
            vec![skip(0, 100.0, 10.0, None, 110.0), Step::Reset, skip(100, 104.0, 10.0, None, 114.0)],
        ),
        (
            "mixed directions accumulate: +60 then -10 is +50",
            vec![skip(0, 100.0, 60.0, Some(1000.0), 160.0), skip(300, 100.0, -10.0, Some(1000.0), 150.0)],
        ),
        (
            "a chain past the end stays at the end and the next -10 goes from the end",
            vec![
                skip(0, 180.0, 10.0, Some(200.0), 190.0),
                skip(200, 180.0, 10.0, Some(200.0), 200.0),
                skip(400, 180.0, 10.0, Some(200.0), 200.0),
                skip(600, 180.0, -10.0, Some(200.0), 190.0),
            ],
        ),
        (
            "a chain past the start stays at 0 and the next +10 goes from 0",
            vec![
                skip(0, 15.0, -10.0, Some(200.0), 5.0),
                skip(200, 15.0, -10.0, Some(200.0), 0.0),
                skip(400, 15.0, 10.0, Some(200.0), 10.0),
            ],
        ),
    ];

    let t0 = Instant::now();
    let mut bad = Vec::new();
    for (case, steps) in cases {
        let mut chain = SkipChain::default();
        for (n, step) in steps.iter().enumerate() {
            match *step {
                Step::Reset => chain.reset(),
                Step::Skip { at_ms, position, delta, duration, target } => {
                    let actual = chain.skip(position, delta, duration, t0 + Duration::from_millis(at_ms));
                    if actual != target {
                        bad.push(format!("{case}: step {n} (at {at_ms} ms, position {position}, delta {delta}) -> {actual}, expected {target}"));
                    }
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn step_after_moves_through_the_steps_and_stops_at_both_ends() {
    let last = SKIP_STEPS.len() - 1;
    // (index, delta, expected)
    let cases: [(usize, isize, usize); 15] = [
        (2, 1, 3),
        (2, -1, 1),
        (3, 0, 3),
        (1, -1, 0),
        (0, 1, 1),
        (0, -1, 0),
        (4, 1, 5),
        (5, -1, 4),
        (last, 1, last),
        (3, 100, last),
        (3, -100, 0),
        (5, -5, 0),
        (2, isize::MAX, last),
        (2, isize::MIN, 0),
        (9, 0, last),
    ];
    let mut bad = Vec::new();
    for (index, delta, expected) in cases {
        let actual = step_after(index, delta);
        if actual != expected {
            bad.push(format!("step_after({index}, {delta}) = {actual}, expected {expected}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn no_skip_reason_tells_a_live_stream_from_other_unseekable_media() {
    assert_eq!(no_skip_reason(true), "Live stream: there is nothing to skip");
    assert_eq!(no_skip_reason(false), "This media cannot be skipped through");
}

const BOUND_KEYS: [(Key, Shortcut); 13] = [
    (Key::Space, Shortcut::PlayPause),
    (Key::ArrowLeft, Shortcut::SkipBack),
    (Key::ArrowRight, Shortcut::SkipForward),
    (Key::OpenBracket, Shortcut::ShorterStep),
    (Key::CloseBracket, Shortcut::LongerStep),
    (Key::ArrowUp, Shortcut::VolumeUp),
    (Key::ArrowDown, Shortcut::VolumeDown),
    (Key::M, Shortcut::Mute),
    (Key::O, Shortcut::OpenFile),
    (Key::U, Shortcut::OpenUrl),
    (Key::F, Shortcut::ToggleFullscreen),
    (Key::F12, Shortcut::ToggleFullscreen),
    (Key::Escape, Shortcut::LeaveFullscreen),
];

#[test]
fn every_bound_key_gives_its_shortcut() {
    let mut bad = Vec::new();
    for (key, expected) in BOUND_KEYS {
        let actual = shortcut(key, false);
        if actual != Some(expected) {
            bad.push(format!("{key:?}: {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn every_other_key_gives_no_shortcut() {
    let unbound: Vec<Key> = Key::ALL.iter().copied().filter(|k| !BOUND_KEYS.iter().any(|(b, _)| b == k)).collect();
    let bound_hits: Vec<(Key, Shortcut)> = unbound.iter().filter_map(|&k| shortcut(k, false).map(|s| (k, s))).collect();

    assert_eq!(unbound.len(), Key::ALL.len() - 13, "13 distinct keys are bound");
    for key in [Key::A, Key::Enter, Key::F11, Key::Tab, Key::Num5] {
        assert!(unbound.contains(&key), "{key:?} must be among the unbound keys tested");
    }
    assert_eq!(bound_hits, Vec::<(Key, Shortcut)>::new());
}

#[test]
fn no_key_is_a_shortcut_while_a_text_field_has_focus() {
    let hits: Vec<(Key, Shortcut)> = Key::ALL.iter().filter_map(|&k| shortcut(k, true).map(|s| (k, s))).collect();

    assert!(Key::ALL.contains(&Key::F) && Key::ALL.contains(&Key::F12) && Key::ALL.contains(&Key::Space));
    assert_eq!(hits, Vec::<(Key, Shortcut)>::new());
}

/// Lays out one `icon_button` and clicks its centre: press in one frame, release in the next.
/// Returns `enabled()` as seen in every frame and whether any frame reported `clicked()`.
fn click_icon_button(enabled: bool) -> (Vec<bool>, bool) {
    let ctx = egui::Context::default();
    let screen = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0));
    let mut seen_enabled = Vec::new();
    let mut clicked = false;
    let mut frame = |time: f64, events: Vec<egui::Event>| -> Rect {
        let input = egui::RawInput { screen_rect: Some(screen), time: Some(time), events, ..Default::default() };
        let mut rect = Rect::NOTHING;
        let _ = ctx.run_ui(input, |ui| {
            let response = icon_button(ui, Icon::SkipForward, "+30 s", enabled);
            rect = response.rect;
            seen_enabled.push(response.enabled());
            clicked |= response.clicked();
        });
        rect
    };
    let rect = frame(0.0, vec![]);
    assert!(rect.width() > 0.0 && rect.height() > 0.0, "the button has no area: {rect:?}");
    let at = rect.center();
    let button = |pressed| egui::Event::PointerButton {
        pos: at,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    frame(0.1, vec![egui::Event::PointerMoved(at), button(true)]);
    frame(0.2, vec![button(false)]);
    frame(0.3, vec![]);
    (seen_enabled, clicked)
}

#[test]
fn an_enabled_icon_button_reports_a_click() {
    assert_eq!(click_icon_button(true), (vec![true; 4], true));
}

#[test]
fn a_disabled_icon_button_is_disabled_and_ignores_a_click() {
    assert_eq!(click_icon_button(false), (vec![false; 4], false));
}

// ---- paint_icon in a headless egui frame ----

/// A colour nothing else in the frame uses, so the icon's shapes can be told apart.
const INK: Color32 = Color32::from_rgb(201, 17, 93);
/// Not at the origin and not a round size, so a hard-coded rectangle would show.
fn icon_rect() -> Rect {
    Rect::from_min_size(pos2(37.0, 51.0), vec2(18.0, 18.0))
}

#[derive(Debug)]
enum Drawn {
    Polygon(Vec<Pos2>),
    Rect(Rect),
    Line([Pos2; 2], f32),
}

fn flatten(shape: Shape, out: &mut Vec<Shape>) {
    match shape {
        Shape::Vec(shapes) => shapes.into_iter().for_each(|s| flatten(s, out)),
        s => out.push(s),
    }
}

/// The shapes `paint_icon` adds for `icon` inside [`icon_rect`] in [`INK`].
fn painted(icon: Icon) -> Vec<Drawn> {
    let ctx = egui::Context::default();
    let output = ctx.run_ui(egui::RawInput::default(), |ui| {
        paint_icon(ui.painter(), icon_rect(), icon, INK);
    });
    let mut shapes = Vec::new();
    for clipped in output.shapes {
        flatten(clipped.shape, &mut shapes);
    }
    shapes
        .into_iter()
        .filter_map(|shape| match shape {
            Shape::Path(p) if p.fill == INK => Some(Drawn::Polygon(p.points)),
            Shape::Rect(r) if r.fill == INK => Some(Drawn::Rect(r.rect)),
            Shape::LineSegment { points, stroke } if stroke.color == INK => Some(Drawn::Line(points, stroke.width)),
            _ => None,
        })
        .collect()
}

const ALL_ICONS: [Icon; 6] =
    [Icon::Play, Icon::Pause, Icon::SkipBack, Icon::SkipForward, Icon::EnterFullscreen, Icon::ExitFullscreen];

#[test]
fn every_icon_draws_in_its_colour_inside_its_rect() {
    let rect = icon_rect();
    let mut bad = Vec::new();
    for icon in ALL_ICONS {
        let drawn = painted(icon);
        if drawn.is_empty() {
            bad.push(format!("{icon:?}: nothing drawn in the given colour"));
        }
        for d in &drawn {
            // A stroked line may reach half its width past its end points.
            let (points, slack): (Vec<Pos2>, f32) = match d {
                Drawn::Polygon(p) => (p.clone(), 0.0),
                Drawn::Rect(r) => (vec![r.min, r.max], 0.0),
                Drawn::Line(p, w) => (p.to_vec(), w / 2.0),
            };
            let allowed = rect.expand(slack + 0.01);
            for p in points {
                if !allowed.contains(p) {
                    bad.push(format!("{icon:?}: {p:?} of {d:?} lies outside {rect:?}"));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The point of a skip arrowhead is its vertex at the vertical centre; the other two form its
/// back edge.
fn arrow_directions(icon: Icon) -> Vec<&'static str> {
    let mid = icon_rect().center().y;
    painted(icon)
        .into_iter()
        .map(|d| match d {
            Drawn::Polygon(p) => {
                assert_eq!(p.len(), 3, "{icon:?}: an arrowhead is a triangle, got {p:?}");
                let tip = p.iter().find(|q| (q.y - mid).abs() < 0.01).unwrap_or_else(|| panic!("{icon:?}: no tip in {p:?}"));
                let back = p.iter().filter(|q| (q.y - mid).abs() >= 0.01).map(|q| q.x).sum::<f32>() / 2.0;
                if tip.x > back { "right" } else { "left" }
            }
            other => panic!("{icon:?}: expected arrowheads, got {other:?}"),
        })
        .collect()
}

#[test]
fn skip_icons_are_two_arrowheads_pointing_the_way_of_the_skip() {
    assert_eq!(arrow_directions(Icon::SkipForward), ["right", "right"]);
    assert_eq!(arrow_directions(Icon::SkipBack), ["left", "left"]);
}

#[test]
fn play_is_one_triangle_pointing_right() {
    assert_eq!(arrow_directions(Icon::Play), ["right"]);
}

#[test]
fn pause_is_two_filled_bars_side_by_side() {
    let bars: Vec<Rect> = painted(Icon::Pause)
        .into_iter()
        .map(|d| match d {
            Drawn::Rect(r) => r,
            other => panic!("Pause: expected filled bars, got {other:?}"),
        })
        .collect();

    assert_eq!(bars.len(), 2, "{bars:?}");
    assert!(bars[0].right() < bars[1].left(), "the bars overlap: {bars:?}");
    assert!(bars.iter().all(|b| b.height() > b.width()), "bars are taller than wide: {bars:?}");
}

#[test]
fn enter_fullscreen_brackets_sit_in_the_corners_and_exit_brackets_do_not() {
    let rect = icon_rect();
    let corners = [rect.left_top(), rect.right_top(), rect.left_bottom(), rect.right_bottom()];
    let touched = |icon: Icon| -> Vec<Pos2> {
        let ends: Vec<Pos2> = painted(icon)
            .into_iter()
            .flat_map(|d| match d {
                Drawn::Line(p, _) => p.to_vec(),
                other => panic!("{icon:?}: expected bracket lines, got {other:?}"),
            })
            .collect();
        corners.iter().copied().filter(|c| ends.iter().any(|e| e.distance(*c) < 0.01)).collect()
    };

    assert_eq!(touched(Icon::EnterFullscreen), corners.to_vec());
    assert_eq!(touched(Icon::ExitFullscreen), Vec::<Pos2>::new());
}
