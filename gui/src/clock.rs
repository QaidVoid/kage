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

#[cfg(test)]
mod tests {
    use super::unix_seconds;

    #[test]
    fn reads_a_plausible_epoch_time() {
        // Any correct platform clock lands well past 2023 and well before
        // the year the test stops being meaningful.
        assert!(unix_seconds() > 1_700_000_000);
        assert!(unix_seconds() < 4_000_000_000);
    }
}
