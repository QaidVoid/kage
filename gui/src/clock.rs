//! The wall clock, read the way each platform provides it.
//!
//! `std::time::SystemTime::now` panics on `wasm32-unknown-unknown`, so any
//! view that formats a relative time cannot call it directly: the browser
//! answers with `Date.now`. This is the one place the split lives, so the
//! views stay free of target gates.

/// Seconds since the Unix epoch, as `SystemTime::now` would report on a
/// native target and `Date.now` does in a browser.
#[cfg(not(target_arch = "wasm32"))]
pub fn unix_seconds() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| i64::try_from(since.as_secs()).unwrap_or(0))
        .unwrap_or(0)
}

/// Seconds since the Unix epoch, read from the browser clock.
#[cfg(target_arch = "wasm32")]
pub fn unix_seconds() -> i64 {
    (js_sys::Date::now() / 1_000.0) as i64
}

/// The local time of day of a Unix timestamp, as `17:27`.
#[must_use]
pub fn time_of_day(unix: i64) -> String {
    use chrono::TimeZone as _;

    chrono::Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|at| at.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// A short span label: `<1s` under a second, then as [`duration`].
#[must_use]
pub fn span(took: std::time::Duration) -> String {
    if took.as_secs() == 0 {
        "<1s".to_owned()
    } else {
        duration(i64::try_from(took.as_secs()).unwrap_or(i64::MAX))
    }
}

/// The elapsed label the topbar's working pill carries: seconds under
/// a minute, then minutes with seconds, then hours with minutes, as
/// the web client's duration formatter reads.
#[must_use]
pub fn duration(seconds: i64) -> String {
    let s = seconds.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3_600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3_600, (s % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::{duration, span, time_of_day, unix_seconds};

    #[test]
    fn reads_a_plausible_epoch_time() {
        // Any correct platform clock lands well past 2023 and well before
        // the year the test stops being meaningful.
        assert!(unix_seconds() > 1_700_000_000);
        assert!(unix_seconds() < 4_000_000_000);
    }

    #[test]
    fn spans_under_a_second_read_as_less_than_one() {
        assert_eq!(span(std::time::Duration::from_millis(300)), "<1s");
        assert_eq!(span(std::time::Duration::from_millis(2_400)), "2s");
    }

    #[test]
    fn a_time_of_day_is_hours_and_minutes() {
        let label = time_of_day(unix_seconds());
        assert_eq!(label.len(), 5, "{label}");
        assert_eq!(&label[2..3], ":");
    }

    #[test]
    fn durations_read_seconds_minutes_then_hours() {
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(60), "1m 0s");
        assert_eq!(duration(754), "12m 34s");
        assert_eq!(duration(3_600), "1h 0m");
        assert_eq!(duration(4_567), "1h 16m");
        assert_eq!(duration(-5), "0s", "a stamp ahead of the clock clamps");
    }
}
