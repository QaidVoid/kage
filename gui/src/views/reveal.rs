//! Paced reveal of streamed text.
//!
//! Some providers deliver a reply in bursts: nothing for seconds, then
//! hundreds of characters at once. The transcript shows such text at a
//! steady pace instead, spreading each burst over the time the bursts
//! have been arriving apart, so the text keeps moving between them. A
//! steady stream arrives in small, close deltas and stays close behind.

use std::collections::HashMap;

use web_time::Instant;

/// The shortest time a burst is spread over, in seconds. A steady
/// stream trails by about this much.
const MIN_SPREAD: f64 = 0.25;
/// The longest time a burst is spread over, in seconds.
const MAX_SPREAD: f64 = 2.5;
/// The spread of the first burst, before any gap is known.
const FIRST_SPREAD: f64 = 1.2;
/// The slowest pace, in characters per second, so a short backlog does
/// not crawl.
const MIN_RATE: f64 = 60.0;
/// How much of a new gap moves the running estimate.
const GAP_WEIGHT: f64 = 0.3;

/// The pace of one item's text.
#[derive(Debug, Clone)]
struct Pace {
    /// Characters shown, fractional between frames.
    shown: f64,
    /// Characters arrived when last looked at.
    seen: usize,
    /// Characters per second until the next arrival.
    rate: f64,
    /// The running estimate of the time between arrivals, in seconds.
    gap: Option<f64>,
    /// When text last arrived.
    arrived: Instant,
    /// When the pace last advanced.
    tick: Instant,
}

/// The paced items of one transcript, by item index.
#[derive(Debug, Default)]
pub(crate) struct Reveal {
    items: HashMap<usize, Pace>,
}

impl Reveal {
    /// Paces item `ix` from its first character, unless it already is.
    pub(crate) fn track(&mut self, ix: usize, now: Instant) {
        self.items.entry(ix).or_insert(Pace {
            shown: 0.0,
            seen: 0,
            rate: MIN_RATE,
            gap: None,
            arrived: now,
            tick: now,
        });
    }

    /// Whether item `ix` is paced.
    pub(crate) fn tracks(&self, ix: usize) -> bool {
        self.items.contains_key(&ix)
    }

    /// How many of the `len` characters item `ix` holds show at `now`,
    /// or `None` when the item is not paced. An item that caught up
    /// and is no longer `live` stops being paced.
    pub(crate) fn advance(
        &mut self,
        ix: usize,
        len: usize,
        live: bool,
        now: Instant,
    ) -> Option<usize> {
        let pace = self.items.get_mut(&ix)?;
        if len > pace.seen {
            let since = now.saturating_duration_since(pace.arrived).as_secs_f64();
            if pace.seen > 0 {
                pace.gap = Some(
                    pace.gap
                        .map_or(since, |gap| gap + (since - gap) * GAP_WEIGHT),
                );
            }
            pace.seen = len;
            pace.arrived = now;
            let spread = pace
                .gap
                .map_or(FIRST_SPREAD, |gap| gap.clamp(MIN_SPREAD, MAX_SPREAD));
            pace.rate = ((len as f64 - pace.shown) / spread).max(MIN_RATE);
        }
        let step = now.saturating_duration_since(pace.tick).as_secs_f64();
        pace.tick = now;
        pace.shown = (pace.shown + pace.rate * step).min(len as f64);
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "shown is clamped to 0..=len"
        )]
        let shown = pace.shown as usize;
        if shown >= len && !live {
            self.items.remove(&ix);
        }
        Some(shown)
    }

    /// Whether some paced text still trails what arrived, so another
    /// frame moves it.
    pub(crate) fn behind(&self) -> bool {
        self.items
            .values()
            .any(|pace| (pace.shown as usize) < pace.seen)
    }

    /// Forgets every item, for another session.
    pub(crate) fn clear(&mut self) {
        self.items.clear();
    }
}

/// The first `chars` characters of `text`.
#[must_use]
pub(crate) fn prefix(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(at, _)| &text[..at])
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// A duration as seconds, for tests that step a clock.
    fn secs(value: f64) -> Duration {
        Duration::from_secs_f64(value)
    }

    #[test]
    fn a_burst_spreads_over_the_first_spread() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        reveal.track(0, start);
        assert_eq!(reveal.advance(0, 1200, true, start), Some(0));
        let part = reveal.advance(0, 1200, true, start + secs(0.5));
        assert_eq!(part, Some(500));
        assert!(reveal.behind());
        assert_eq!(
            reveal.advance(0, 1200, true, start + secs(FIRST_SPREAD)),
            Some(1200)
        );
        assert!(!reveal.behind());
    }

    #[test]
    fn bursts_spread_over_the_time_between_them() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        reveal.track(0, start);
        reveal.advance(0, 300, true, start);
        reveal.advance(0, 300, true, start + secs(2.0));
        reveal.advance(0, 600, true, start + secs(2.0));
        let later = reveal.advance(0, 600, true, start + secs(3.0));
        assert_eq!(later, Some(450), "the second burst spreads over the 2s gap");
    }

    #[test]
    fn a_finished_item_stops_being_paced_once_caught_up() {
        let start = Instant::now();
        let mut reveal = Reveal::default();
        reveal.track(3, start);
        reveal.advance(3, 10, false, start);
        assert_eq!(reveal.advance(3, 10, false, start + secs(1.0)), Some(10));
        assert_eq!(reveal.advance(3, 10, false, start + secs(2.0)), None);
        assert_eq!(reveal.advance(9, 10, true, start), None, "never tracked");
    }

    #[test]
    fn a_prefix_counts_characters() {
        assert_eq!(prefix("h\u{e9}llo", 2), "h\u{e9}");
        assert_eq!(prefix("hi", 5), "hi");
    }
}
