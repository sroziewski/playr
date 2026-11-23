//! A click on the picture: `playr::controls::stage_click` / `stage_input_allowed`, and what a
//! sequence of clicks does when egui itself turns pointer events into clicks and double-clicks.
//!
//! The egui test drives a headless `egui::Context` with pointer events and runs each frame
//! through the same steps as `App::draw_stage` (input allowed only without a dialog or popup,
//! `last_click_paused` updated on every raw click), applied to a model of the player's pause
//! and full-screen state. It does not run `App` itself, which needs a window and a GL context.

use eframe::egui::{self, Event, Modifiers, PointerButton, Pos2, RawInput, Rect, Sense, pos2, vec2};
use playr::controls::{StageClick, stage_click, stage_input_allowed, stage_step};

#[test]
fn stage_click_follows_its_truth_table() {
    use StageClick::*;
    let cases = [
        // (clicked, double_clicked, playing, last_click_paused) -> outcome
        ((false, false, false, false), Nothing),
        ((false, false, false, true), Nothing),
        ((false, false, true, false), Nothing),
        ((false, false, true, true), Nothing),
        ((true, false, false, false), Nothing),
        ((true, false, false, true), Nothing),
        ((true, false, true, false), TogglePause),
        ((true, false, true, true), TogglePause),
        ((false, true, false, false), Fullscreen),
        ((false, true, false, true), Fullscreen),
        ((false, true, true, false), Fullscreen),
        ((false, true, true, true), FullscreenAndUndoPause),
        ((true, true, false, false), Fullscreen),
        ((true, true, false, true), Fullscreen),
        ((true, true, true, false), Fullscreen),
        ((true, true, true, true), FullscreenAndUndoPause),
    ];
    let mut bad = Vec::new();
    for ((clicked, double_clicked, playing, last_click_paused), expected) in cases {
        let actual = stage_click(clicked, double_clicked, playing, last_click_paused);
        if actual != expected {
            bad.push(format!("stage_click({clicked}, {double_clicked}, {playing}, {last_click_paused}) = {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn stage_input_is_allowed_only_without_the_url_dialog_and_without_a_popup() {
    let cases = [((false, false), true), ((true, false), false), ((false, true), false), ((true, true), false)];
    let actual: Vec<((bool, bool), bool)> = cases.iter().map(|&((url, popup), _)| ((url, popup), stage_input_allowed(url, popup))).collect();

    assert_eq!(actual, cases);
}

#[test]
fn stage_step_remembers_whether_the_last_raw_click_paused() {
    use StageClick::*;
    let cases = [
        // (case, last before, clicked, double_clicked, allowed, playing) -> (action, last after)
        ("a click that pauses", false, true, false, true, true, TogglePause, true),
        ("a click that pauses again", true, true, false, true, true, TogglePause, true),
        ("a click while nothing plays", true, true, false, true, false, Nothing, false),
        ("a click a popup swallows", true, true, false, false, true, Nothing, false),
        ("the double-click after a pausing click", true, true, true, true, true, FullscreenAndUndoPause, false),
        ("the double-click after an ignored click", false, true, true, true, true, Fullscreen, false),
        ("a double-click a popup swallows", true, true, true, false, true, Nothing, false),
        ("a frame without a click keeps it", true, false, false, true, true, Nothing, true),
        ("a double-click report without the raw click keeps it", true, false, true, true, true, FullscreenAndUndoPause, true),
    ];
    let mut bad = Vec::new();
    for (case, before, clicked, double_clicked, allowed, playing, action, after) in cases {
        let mut last = before;
        let actual = stage_step(&mut last, clicked, double_clicked, allowed, playing);
        if (actual, last) != (action, after) {
            bad.push(format!("{case}: ({actual:?}, {last}), expected ({action:?}, {after})"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The part of the player and window a click can change.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Stage {
    paused: bool,
    fullscreen: bool,
}

impl Stage {
    fn apply(&mut self, click: StageClick) {
        match click {
            StageClick::Nothing => {}
            StageClick::TogglePause => self.paused = !self.paused,
            StageClick::Fullscreen => self.fullscreen = !self.fullscreen,
            StageClick::FullscreenAndUndoPause => {
                self.paused = !self.paused;
                self.fullscreen = !self.fullscreen;
            }
        }
    }
}

const SPOT: Pos2 = pos2(400.0, 300.0);

/// A button press at `at` seconds (released 50 ms later), with `popup` telling whether a popup
/// menu is open during it.
#[derive(Clone, Copy)]
struct Press {
    at: f64,
    popup: bool,
}

const fn press(at: f64) -> Press {
    Press { at, popup: false }
}

/// Feeds `presses` to a stage covering the whole screen and returns the state it ends in.
fn run(presses: &[Press], playing: bool) -> Stage {
    let ctx = egui::Context::default();
    let mut stage = Stage::default();
    let mut last_click_paused = false;
    let button = |pressed| Event::PointerButton { pos: SPOT, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    let mut frames: Vec<(f64, Vec<Event>, bool)> = vec![(0.0, vec![Event::PointerMoved(SPOT)], false)];
    for p in presses {
        frames.push((p.at, vec![button(true)], p.popup));
        frames.push((p.at + 0.05, vec![button(false)], p.popup));
    }
    frames.push((presses.last().map_or(0.0, |p| p.at) + 1.0, vec![], false));
    for (time, events, popup) in frames {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(800.0, 600.0))), time: Some(time), events, ..Default::default() };
        let output = ctx.run_ui(input, |ui| {
            let response = ui.interact(ui.max_rect(), ui.scope_id().with("stage"), Sense::click());
            let allowed = stage_input_allowed(false, popup);
            stage.apply(stage_step(&mut last_click_paused, response.clicked(), response.double_clicked(), allowed, playing));
        });
        output.drop_without_applying_deltas();
    }
    stage
}

#[test]
fn click_sequences_on_the_picture() {
    let paused = Stage { paused: true, fullscreen: false };
    let fullscreen = Stage { paused: false, fullscreen: true };
    let cases: [(&str, Vec<Press>, bool, Stage); 6] = [
        ("one click while playing pauses", vec![press(0.1)], true, paused),
        ("a double-click while playing goes full screen and keeps playing", vec![press(0.1), press(0.25)], true, fullscreen),
        ("two clicks further apart than a double-click pause and resume", vec![press(0.1), press(1.5)], true, Stage::default()),
        ("one click with nothing playing does nothing", vec![press(0.1)], false, Stage::default()),
        ("a double-click with nothing playing goes full screen", vec![press(0.1), press(0.25)], false, fullscreen),
        (
            "a double-click whose first click closed a popup goes full screen without touching pause",
            vec![Press { at: 0.1, popup: true }, press(0.25)],
            true,
            fullscreen,
        ),
    ];
    let mut bad = Vec::new();
    for (case, presses, playing, expected) in cases {
        let actual = run(&presses, playing);
        if actual != expected {
            bad.push(format!("{case}: {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
