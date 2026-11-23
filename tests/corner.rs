//! The corner player's geometry: `playr::corner`. Expected positions and sizes are worked out
//! by hand from the placement rule (share of the work area's width, aspect kept, fitted to the
//! room left after the margins and the window frame, never narrower than `MIN_INNER_WIDTH`).

use eframe::egui::{Pos2, Rect, Vec2, pos2, vec2};
use playr::corner::{
    CORNER_MIN_INNER, Corner, DEFAULT_SHARE, Frame, KEEP_ON_TOP, MARGIN, MAX_SHARE, MIN_INNER_WIDTH, MIN_SHARE, NORMAL_MIN_INNER, defer_placement, needs_replace,
    placement, work_area_px,
};

const SIXTEEN_NINE: f32 = 16.0 / 9.0;

fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
    Rect::from_min_size(pos2(x, y), vec2(w, h))
}

fn close(actual: (Pos2, Vec2), expected: (Pos2, Vec2)) -> bool {
    let near = |a: f32, b: f32| (a - b).abs() <= 1e-3;
    near(actual.0.x, expected.0.x) && near(actual.0.y, expected.0.y) && near(actual.1.x, expected.1.x) && near(actual.1.y, expected.1.y)
}

fn check(bad: &mut Vec<String>, case: &str, actual: (Pos2, Vec2), expected: (Pos2, Vec2)) {
    if !close(actual, expected) {
        bad.push(format!("{case}: got {actual:?}, expected {expected:?}"));
    }
}

fn finish(bad: Vec<String>) {
    assert!(bad.is_empty(), "{} expectation(s) failed:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn the_constants_are_the_agreed_ones() {
    assert_eq!((MIN_SHARE, MAX_SHARE, DEFAULT_SHARE, MARGIN, MIN_INNER_WIDTH), (0.2, 0.6, 0.375, 12.0, 240.0));
}

#[test]
fn the_minimum_window_sizes_and_keep_on_top_are_the_agreed_ones() {
    assert_eq!((CORNER_MIN_INNER, NORMAL_MIN_INNER, KEEP_ON_TOP), (vec2(240.0, 135.0), vec2(420.0, 260.0), true));
}

#[test]
fn placing_waits_a_frame_when_the_window_is_full_screen_or_maximised() {
    let cases = [((false, false), false), ((true, false), true), ((false, true), true), ((true, true), true)];
    let actual: Vec<((bool, bool), bool)> = cases.iter().map(|&((f, m), _)| ((f, m), defer_placement(f, m))).collect();

    assert_eq!(actual, cases);
}

#[test]
fn the_window_is_placed_again_only_when_the_pictures_shape_really_changes() {
    let applied = 16.0 / 9.0;
    let cases = [
        ("no picture between list entries", None, None),
        ("same shape", Some(applied), None),
        ("within 0.01", Some(applied + 0.009), None),
        ("just over 0.01 wider", Some(applied + 0.011), Some(applied + 0.011)),
        ("just over 0.01 narrower", Some(applied - 0.011), Some(applied - 0.011)),
        ("4:3", Some(4.0 / 3.0), Some(4.0 / 3.0)),
        ("portrait", Some(0.5625), Some(0.5625)),
        ("zero", Some(0.0), None),
        ("negative", Some(-1.5), None),
        ("NaN", Some(f32::NAN), None),
        ("infinite", Some(f32::INFINITY), None),
    ];
    let mut bad = Vec::new();
    for (case, picture, expected) in cases {
        let actual = needs_replace(applied, picture);
        if actual != expected {
            bad.push(format!("{case}: needs_replace({applied}, {picture:?}) = {actual:?}, expected {expected:?}"));
        }
    }
    finish(bad);
}

#[test]
fn the_corners_come_in_reading_order_with_their_labels() {
    let labels: Vec<(Corner, &str)> = Corner::ALL.iter().map(|&c| (c, c.label())).collect();

    assert_eq!(
        labels,
        [
            (Corner::TopLeft, "↖ Top left"),
            (Corner::TopRight, "↗ Top right"),
            (Corner::BottomLeft, "↙ Bottom left"),
            (Corner::BottomRight, "↘ Bottom right"),
        ]
    );
}

#[test]
fn frame_between_measures_each_side_and_never_goes_below_zero() {
    let title_bar = Frame::between(Rect::from_min_max(pos2(10.0, 20.0), pos2(810.0, 640.0)), Rect::from_min_max(pos2(17.0, 51.0), pos2(803.0, 633.0)));
    let overhanging = Frame::between(Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0)), Rect::from_min_max(pos2(-5.0, 10.0), pos2(103.0, 96.0)));

    assert_eq!(title_bar, Frame { left: 7.0, top: 31.0, right: 7.0, bottom: 7.0 });
    assert_eq!(overhanging, Frame { left: 0.0, top: 10.0, right: 0.0, bottom: 4.0 });
}

#[test]
fn each_corner_takes_its_share_of_the_width_inside_the_margins() {
    let windows_frame = Frame { left: 7.0, top: 31.0, right: 7.0, bottom: 7.0 };
    // 1920 x 1032 work area, 0.375 share: outer width 720, inner 706 x 706/(16/9) = 397.125,
    // outer height 397.125 + 38 = 435.125.
    let full_hd = rect(0.0, 0.0, 1920.0, 1032.0);
    // Work area off the origin (a second monitor): 0.5 of 1600 = outer 800, inner 786 x 786*3/4
    // = 589.5, outer height 589.5 + 34 = 623.5; right edge 1700, bottom edge 950.
    let offset = rect(100.0, 50.0, 1600.0, 900.0);
    let uneven_frame = Frame { left: 8.0, top: 30.0, right: 6.0, bottom: 4.0 };
    let cases = [
        ("full HD, top left", full_hd, Corner::TopLeft, 0.375, SIXTEEN_NINE, windows_frame, (pos2(12.0, 12.0), vec2(706.0, 397.125))),
        ("full HD, top right", full_hd, Corner::TopRight, 0.375, SIXTEEN_NINE, windows_frame, (pos2(1188.0, 12.0), vec2(706.0, 397.125))),
        ("full HD, bottom left", full_hd, Corner::BottomLeft, 0.375, SIXTEEN_NINE, windows_frame, (pos2(12.0, 584.875), vec2(706.0, 397.125))),
        ("full HD, bottom right", full_hd, Corner::BottomRight, 0.375, SIXTEEN_NINE, windows_frame, (pos2(1188.0, 584.875), vec2(706.0, 397.125))),
        ("offset, top left", offset, Corner::TopLeft, 0.5, 4.0 / 3.0, uneven_frame, (pos2(112.0, 62.0), vec2(786.0, 589.5))),
        ("offset, top right", offset, Corner::TopRight, 0.5, 4.0 / 3.0, uneven_frame, (pos2(888.0, 62.0), vec2(786.0, 589.5))),
        ("offset, bottom left", offset, Corner::BottomLeft, 0.5, 4.0 / 3.0, uneven_frame, (pos2(112.0, 314.5), vec2(786.0, 589.5))),
        ("offset, bottom right", offset, Corner::BottomRight, 0.5, 4.0 / 3.0, uneven_frame, (pos2(888.0, 314.5), vec2(786.0, 589.5))),
    ];
    let mut bad = Vec::new();
    for (case, work, corner, share, aspect, frame, expected) in cases {
        check(&mut bad, case, placement(work, corner, share, aspect, frame), expected);
    }
    finish(bad);
}

#[test]
fn a_picture_too_tall_for_the_work_area_takes_its_full_height_and_the_width_follows() {
    // 0.6 of 2560 = 1536 wide would be 864 high; the room is 600 - 24 = 576, so 1024 x 576.
    let unframed = placement(rect(0.0, 0.0, 2560.0, 600.0), Corner::TopLeft, 0.6, SIXTEEN_NINE, Frame::default());
    // With a frame: room 600 - 24 - 38 = 538 high, width 538 * 16/9 = 956.444, outer 970.444 x
    // 576, so the bottom right corner is at x 2560 - 12 - 970.444 and y 600 - 12 - 576.
    let framed = placement(rect(0.0, 0.0, 2560.0, 600.0), Corner::BottomRight, 0.6, SIXTEEN_NINE, Frame { left: 7.0, top: 31.0, right: 7.0, bottom: 7.0 });

    let mut bad = Vec::new();
    check(&mut bad, "unframed", unframed, (pos2(12.0, 12.0), vec2(1024.0, 576.0)));
    check(&mut bad, "framed", framed, (pos2(1577.5555, 12.0), vec2(956.4444, 538.0)));
    finish(bad);
}

#[test]
fn the_share_is_clamped_and_a_share_that_is_not_a_number_is_the_default() {
    // 2000 x 1000, no frame, aspect 2: the inner width is share * 2000, the height half that.
    let work = rect(0.0, 0.0, 2000.0, 1000.0);
    let cases = [
        ("below the minimum", 0.05, 400.0),
        ("the minimum", 0.2, 400.0),
        ("between", 0.5, 1000.0),
        ("the maximum", 0.6, 1200.0),
        ("above the maximum", 0.9, 1200.0),
        ("NaN", f32::NAN, 750.0),
        ("infinity", f32::INFINITY, 750.0),
        ("minus infinity", f32::NEG_INFINITY, 750.0),
    ];
    let mut bad = Vec::new();
    for (case, share, width) in cases {
        check(&mut bad, case, placement(work, Corner::TopLeft, share, 2.0, Frame::default()), (pos2(12.0, 12.0), vec2(width, width / 2.0)));
    }
    finish(bad);
}

#[test]
fn a_bad_aspect_is_taken_as_sixteen_to_nine() {
    // 2000 x 1000, share 0.5: 1000 wide; 16:9 gives 562.5 high. The room is 976 x 1976.
    let work = rect(0.0, 0.0, 2000.0, 1000.0);
    let cases = [
        ("NaN", f32::NAN, vec2(1000.0, 562.5)),
        ("infinity", f32::INFINITY, vec2(1000.0, 562.5)),
        ("zero", 0.0, vec2(1000.0, 562.5)),
        ("negative", -2.0, vec2(1000.0, 562.5)),
        ("exactly 0.1", 0.1, vec2(1000.0, 562.5)),
        // Kept: 1000 / 0.5 = 2000 high is too tall, so 976 high and 488 wide.
        ("portrait 0.5", 0.5, vec2(488.0, 976.0)),
        // Kept: 976 * 0.11 = 107.36 wide is raised to 240, and 240 / 0.11 is capped at 976.
        ("just above 0.1", 0.11, vec2(240.0, 976.0)),
    ];
    let mut bad = Vec::new();
    for (case, aspect, size) in cases {
        check(&mut bad, case, placement(work, Corner::TopLeft, 0.5, aspect, Frame::default()), (pos2(12.0, 12.0), size));
    }
    finish(bad);
}

#[test]
fn the_window_is_never_narrower_than_the_minimum_unless_the_work_area_is_narrower() {
    let cases = [
        // 0.2 of 800 = 160 is raised to 240; 240 / (16/9) = 135.
        ("small screen", rect(0.0, 0.0, 800.0, 600.0), 0.2, Corner::TopLeft, (pos2(12.0, 12.0), vec2(240.0, 135.0))),
        // Room 200 - 24 = 176 wide: 75 is raised to 176 only; 176 / (16/9) = 99 fits 126.
        ("room narrower than the minimum", rect(0.0, 0.0, 200.0, 150.0), 0.375, Corner::TopRight, (pos2(12.0, 12.0), vec2(176.0, 99.0))),
        // Same width, room only 56 high: the height is capped there.
        ("and too low as well", rect(0.0, 0.0, 200.0, 80.0), 0.375, Corner::BottomLeft, (pos2(12.0, 12.0), vec2(176.0, 56.0))),
    ];
    let mut bad = Vec::new();
    for (case, work, share, corner, expected) in cases {
        check(&mut bad, case, placement(work, corner, share, SIXTEEN_NINE, Frame::default()), expected);
    }
    finish(bad);
}

#[test]
#[cfg(windows)]
fn the_work_area_near_the_top_left_of_the_primary_monitor_is_a_real_rectangle_there() {
    let area = work_area_px(pos2(1.0, 1.0)).expect("a monitor near (1, 1)");
    let far_away = work_area_px(pos2(1.0e6, -1.0e6)).expect("the nearest monitor to a point off every screen");

    assert!(area.width() > 0.0 && area.height() > 0.0, "{area:?}");
    // A taskbar along the top or left edge moves the work area off (1, 1) by its own size.
    assert!(area.expand(200.0).contains(pos2(1.0, 1.0)), "{area:?}");
    assert!(far_away.width() > 0.0 && far_away.height() > 0.0, "{far_away:?}");
}

#[test]
#[cfg(not(windows))]
fn there_is_no_work_area_outside_windows() {
    assert_eq!(work_area_px(pos2(1.0, 1.0)), None);
}
