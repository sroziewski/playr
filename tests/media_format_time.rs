//! `playr::media::format_time`: `M:SS` under an hour, `H:MM:SS` from an hour on, and `0:00`
//! for anything that is not a positive finite number of seconds.

use playr::media::format_time;

#[test]
fn formats_minutes_and_seconds_below_one_hour() {
    let cases = [
        (0.0, "0:00"),
        (0.999, "0:00"),
        (1.0, "0:01"),
        (9.0, "0:09"),
        (59.0, "0:59"),
        (59.999, "0:59"),
        (60.0, "1:00"),
        (61.0, "1:01"),
        (754.0, "12:34"),
        (3599.0, "59:59"),
        (3599.999, "59:59"),
    ];
    for (secs, expected) in cases {
        assert_eq!(format_time(secs), expected, "format_time({secs})");
    }
}

#[test]
fn formats_hours_minutes_and_seconds_from_one_hour_on() {
    let cases = [
        (3600.0, "1:00:00"),
        (3661.0, "1:01:01"),
        (5025.0, "1:23:45"),
        (37230.0, "10:20:30"),
        (86399.0, "23:59:59"),
        (360_000.0, "100:00:00"),
    ];
    for (secs, expected) in cases {
        assert_eq!(format_time(secs), expected, "format_time({secs})");
    }
}

#[test]
fn renders_negative_and_non_finite_input_as_zero() {
    for secs in [-0.5, -5.0, -3600.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(format_time(secs), "0:00", "format_time({secs})");
    }
}
