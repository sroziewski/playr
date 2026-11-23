//! Decisions the window makes, taken out of the UI code: what ⏮ does, and which History
//! entries can be clicked.

use playr::controls::{Back, RESTART_AFTER_SECS, back_action, history_item_enabled};

#[test]
fn back_restarts_a_seekable_entry_past_three_seconds_else_goes_to_the_previous_entry() {
    assert_eq!(RESTART_AFTER_SECS, 3.0);
    let just_past = 3.0 + 1e-9;
    let cases = [
        ("exactly 3 s, later entry", 3.0, true, 2, Back::Previous),
        ("just past 3 s, later entry", just_past, true, 2, Back::Restart),
        ("just past 3 s, first entry", just_past, true, 0, Back::Restart),
        ("exactly 3 s, first entry", 3.0, true, 0, Back::Disabled),
        ("early in a later entry", 2.5, true, 1, Back::Previous),
        ("start of the first entry", 0.0, true, 0, Back::Disabled),
        ("not seekable, late, later entry", 40.0, false, 3, Back::Previous),
        ("not seekable, late, first entry", 40.0, false, 0, Back::Disabled),
        ("position unknown (NaN), later entry", f64::NAN, true, 1, Back::Previous),
    ];
    let mut bad = Vec::new();
    for (case, position, seekable, entry, expected) in cases {
        let actual = back_action(position, seekable, entry);
        if actual != expected {
            bad.push(format!("{case}: back_action({position}, {seekable}, {entry}) = {actual:?}, expected {expected:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn a_history_entry_is_disabled_only_for_a_local_file_known_to_be_gone() {
    let cases = [
        ("address, even if reported gone", "https://radio.example/live", Some(false), true),
        ("upper-case scheme", "HTTP://radio.example/live", Some(false), true),
        ("address after spaces", "  https://radio.example/live", Some(false), true),
        ("file that exists", "C:\\Music\\a.mp3", Some(true), true),
        ("file not checked yet", "C:\\Music\\a.mp3", None, true),
        ("file that is gone", "C:\\Music\\a.mp3", Some(false), false),
        ("file URL that is gone", "file:///C:/Music/a.mp3", Some(false), false),
    ];
    let mut bad = Vec::new();
    for (case, target, exists, expected) in cases {
        let actual = history_item_enabled(target, exists);
        if actual != expected {
            bad.push(format!("{case}: history_item_enabled({target:?}, {exists:?}) = {actual}, expected {expected}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
