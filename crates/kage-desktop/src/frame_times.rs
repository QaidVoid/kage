//! Frame-time instrumentation behind `KAGE_FRAME_TIMES`.
//!
//! NFR-01 asks that 30 Hz streaming appends hold 299 of 300 ticks over
//! ten seconds, which is a statement about the cost of a frame. This
//! module measures it, and it measures two different things, because
//! they are two different things:
//!
//! - **draw** is the time inside the app's own frame, taken around
//!   the draw call. That is the number a budget is about.
//! - **idle** is everything else: the time egui spent with no repaint
//!   scheduled. It grows without bound while the app waits, so it says
//!   nothing about frame cost.
//!
//! The first version of this harness measured only the gap between
//! frames and reported a windowed smoke run as having one-second
//! frames. The process used 0.22 s of CPU over a ten second run: it
//! was waiting, not computing. The summary reports both figures now,
//! so that reading cannot go wrong again, and
//! [`FrameTimes::end_frame`] is wrapped around the draw rather than
//! around the whole frame loop.
//!
//! Recording is off unless `KAGE_FRAME_TIMES` is set, so a shipped
//! run pays one branch per frame and allocates nothing.
//! `KAGE_FRAME_SLOW_MS=<ms>` traces each frame longer than that as it
//! lands, which is what tells a one-off stall from a periodic one.

use std::time::Duration;

use web_time::Instant;

/// The environment flag that turns recording on.
pub const FLAG: &str = "KAGE_FRAME_TIMES";

/// The environment flag that traces frames slower than its
/// milliseconds, as they land, with their index.
pub const SLOW_FLAG: &str = "KAGE_FRAME_SLOW_MS";

/// The trace threshold, when the flag carries one.
fn trace_slow_ms() -> Option<u32> {
    std::env::var(SLOW_FLAG)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|limit| *limit > 0)
}

#[derive(Debug)]
struct Inner {
    /// The clock the first frame began on, set by `begin_frame`. The
    /// wall time is measured from here, not from construction: the
    /// gap between building the window and its first frame is the
    /// window's startup, not part of any frame.
    started: Option<Instant>,
    /// Where the frame in flight began. The draw is measured from
    /// here to `end_frame`.
    frame_start: Instant,
    /// Where the previous frame ended. The interval is measured from
    /// here, so it survives `begin_frame` overwriting the start.
    last_end: Instant,
    /// Frames slower than this are traced as they land.
    trace_slow_ms: Option<u32>,
    /// One draw duration per recorded frame, in order.
    draws: Vec<Duration>,
    /// One interval per recorded frame, in order.
    intervals: Vec<Duration>,
    /// Whether the summary has been printed, so a close that records
    /// one more frame does not print it twice.
    reported: bool,
}

impl Inner {
    /// The `q` quantile of `sorted`, which the caller sorted.
    fn quantile(sorted: &[Duration], q: f32) -> Duration {
        let ix = ((sorted.len() - 1) as f32 * q).round() as usize;
        sorted[ix]
    }

    /// The largest of `samples`, or zero when there are none.
    fn worst(samples: &[Duration]) -> Duration {
        samples.iter().copied().max().unwrap_or_default()
    }

    /// The index, in recorded order, of the first frame that drew as
    /// long as the worst one. Ties take the earliest, so the index
    /// names when the cost appeared rather than where it sorted.
    fn worst_at(ordered: &[Duration], sorted: &[Duration]) -> usize {
        let worst = Self::worst(sorted);
        ordered
            .iter()
            .position(|sample| *sample == worst)
            .unwrap_or_default()
    }
}

/// The per-frame times, kept between frames.
#[derive(Debug)]
pub struct FrameTimes {
    /// The recorder, or `None` while the flag is unset.
    inner: Option<Inner>,
}

impl FrameTimes {
    /// Reads the flag and starts recording when it is set.
    #[must_use]
    pub fn from_env() -> Self {
        let on = std::env::var_os(FLAG).is_some_and(|value| !value.is_empty());
        Self::new(on)
    }

    /// A recorder that is `on` or off without reading the
    /// environment. [`FrameTimes::from_env`] is this behind the flag;
    /// the tests use it directly because the crate forbids unsafe and
    /// `set_var` is unsafe under that rule.
    #[must_use]
    pub fn new(on: bool) -> Self {
        Self {
            inner: on.then(|| Inner {
                started: None,
                frame_start: Instant::now(),
                last_end: Instant::now(),
                trace_slow_ms: trace_slow_ms(),
                intervals: Vec::new(),
                draws: Vec::new(),
                reported: false,
            }),
        }
    }

    /// Whether the flag is on.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.inner.is_some()
    }

    /// Starts a frame. The matching [`FrameTimes::end_frame`] records
    /// what the draw cost.
    pub fn begin_frame(&mut self, now: Instant) {
        if let Some(inner) = self.inner.as_mut() {
            inner.started.get_or_insert(now);
            inner.frame_start = now;
        }
    }

    /// Records a frame: `drawn` is the time the app's own draw took,
    /// and `now` is when the frame ended. Returns whether a frame was
    /// recorded.
    ///
    /// Both are passed because a clock alone cannot tell them apart.
    /// egui calls the app once per frame and does not say how long ago
    /// that was, so a recorder that measured `begin_frame` to
    /// `end_frame` would charge the wait between frames to the frame:
    /// an app that used 0.22 s of CPU over ten seconds would report
    /// one-second frames. The caller wraps only its own draw and
    /// passes that, which puts the gap in the interval and idle
    /// figures where they belong.
    pub fn end_frame(&mut self, now: Instant, drawn: Duration) -> bool {
        let Some(inner) = self.inner.as_mut() else {
            return false;
        };
        let index = inner.draws.len();
        inner.draws.push(drawn);
        // The interval runs from the previous frame's end, which is a
        // different field from this frame's start: measuring it from
        // the start would make every interval equal its own draw.
        inner
            .intervals
            .push(now.saturating_duration_since(inner.last_end));
        inner.last_end = now;
        if let Some(limit) = inner.trace_slow_ms
            && drawn.as_secs_f64() * 1000.0 > f64::from(limit)
        {
            println!(
                "slow frame {index}: {:.2} ms drawn",
                drawn.as_secs_f64() * 1000.0
            );
        }
        true
    }

    /// The distribution of the run, or `None` while nothing was
    /// recorded. The summary is one line an automated run can grep,
    /// and it carries the frame count so a short run cannot be
    /// mistaken for a fast one. The worst frame's index is in the
    /// line because *when* a stall lands decides what it is: one long
    /// frame at startup is a lazy first use (a font face, a syntax
    /// set), one in the steady state is work the budget must absorb.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        let inner = self.inner.as_ref()?;
        if inner.draws.is_empty() {
            return Some("frames: 0 (no frames drawn)".to_owned());
        }
        let mut draws = inner.draws.clone();
        draws.sort_unstable();
        let mut intervals = inner.intervals.clone();
        intervals.sort_unstable();
        let wall = inner.started.map_or(Duration::ZERO, |start| {
            inner.last_end.saturating_duration_since(start)
        });
        let drawn: Duration = draws.iter().sum();
        let idle = wall.saturating_sub(drawn);
        Some(format!(
            "frames: {count} over {wall_ms:.0} ms  \
             draw p50 {p50:.2} ms  p95 {p95:.2} ms  p99 {p99:.2} ms  \
             max {max:.2} ms at frame {worst_at}  \
             interval p50 {ip50:.2} ms  max {imax:.2} ms  \
             drawn {drawn_ms:.0} ms  idle {idle_ms:.0} ms ({idle_pct:.0}%)",
            count = draws.len(),
            wall_ms = wall.as_secs_f64() * 1000.0,
            p50 = Inner::quantile(&draws, 0.50).as_secs_f64() * 1000.0,
            p95 = Inner::quantile(&draws, 0.95).as_secs_f64() * 1000.0,
            p99 = Inner::quantile(&draws, 0.99).as_secs_f64() * 1000.0,
            max = Inner::worst(&draws).as_secs_f64() * 1000.0,
            worst_at = Inner::worst_at(&inner.draws, &draws),
            ip50 = Inner::quantile(&intervals, 0.50).as_secs_f64() * 1000.0,
            imax = Inner::worst(&intervals).as_secs_f64() * 1000.0,
            drawn_ms = drawn.as_secs_f64() * 1000.0,
            idle_ms = idle.as_secs_f64() * 1000.0,
            idle_pct = idle.as_secs_f64() / wall.as_secs_f64().max(f64::MIN_POSITIVE) * 100.0,
        ))
    }

    /// Prints the summary once, on the way out.
    pub fn report(&mut self) {
        let Some(inner) = self.inner.as_mut() else {
            return;
        };
        if inner.reported {
            return;
        }
        inner.reported = true;
        if let Some(line) = self.summary() {
            println!("{line}");
        }
    }
}

/// A fixed-rate ticker for the `--stream` measurement.
///
/// NFR-01 asks for 299 of 300 ticks over ten seconds. A ticker that
/// resets its clock to "now" on every tick pushes each one out by a
/// frame, and the rate drifts below its budget however fast the frames
/// themselves are: at 60 fps that drift costs about half the ticks. So
/// the clock advances by exactly one period per tick, which keeps the
/// phase and makes the tick count a statement about the frame cost
/// alone. When a frame is longer than a period the tick fires once and
/// the debt is dropped, so a stall reads as a missed tick rather than
/// as a burst of catch-up appends.
#[derive(Debug)]
pub struct Ticker {
    /// The next tick is due at this instant.
    next: Instant,
    /// The gap between ticks.
    period: Duration,
    /// Ticks that fired.
    fired: usize,
}

impl Ticker {
    /// A ticker that fires every `period` from `start`. The first tick
    /// is one whole period out, so it does not fire on the frame that
    /// built it.
    #[must_use]
    pub fn new(start: Instant, period: Duration) -> Self {
        Self {
            next: start + period,
            period,
            fired: 0,
        }
    }

    /// A 30 Hz ticker, the rate NFR-01 measures.
    #[must_use]
    pub fn hz30(start: Instant) -> Self {
        Self::new(start, Duration::from_secs_f64(1.0 / 30.0))
    }

    /// The gap between ticks.
    #[must_use]
    pub fn period(&self) -> Duration {
        self.period
    }

    /// How many ticks have fired.
    #[must_use]
    pub fn fired(&self) -> usize {
        self.fired
    }

    /// Fires a tick when one is due at `now`, at most one per call.
    /// Returns whether a tick fired, so the caller does its work once.
    ///
    /// The clock always advances by whole periods. When a caller
    /// arrives late the tick still fires once and the missed periods
    /// are dropped rather than paid back as a burst, but the phase is
    /// never shortened: `next` is a fixed grid, so a frame that lands
    /// one microsecond past a boundary does not make every later tick
    /// early. A ticker that reset to `now` on a late frame would
    /// shorten its own period every time it was late, and the rate
    /// would drift down without any frame being slow.
    pub fn tick(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.fired += 1;
        // Advance past every boundary that has passed, then park the
        // next tick on the grid. The loop is bounded by how late the
        // caller is; a caller that is many periods late skips the debt
        // in one step rather than firing a burst for work it skipped.
        while self.next <= now {
            self.next += self.period;
        }
        true
    }

    /// When the next tick is due, for the repaint request.
    #[must_use]
    pub fn due_in(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use web_time::Instant;

    use super::{FrameTimes, Ticker};

    /// A recorder that is on, for the tests that feed it frames. The
    /// crate forbids unsafe, so the tests build one directly rather
    /// than setting the environment flag.
    fn recording() -> FrameTimes {
        let times = FrameTimes::new(true);
        assert!(times.is_recording());
        times
    }

    /// Feeds `times` a run of frames: `draw_ms` of work per frame with
    /// `gap_ms` of idle between them, which is what a shell driven by
    /// a repaint timer looks like.
    fn feed(times: &mut FrameTimes, frames: u32, draw_ms: u64, gap_ms: u64) {
        let mut now = Instant::now();
        for _ in 0..frames {
            times.begin_frame(now);
            let drawn = Duration::from_millis(draw_ms);
            now += drawn;
            times.end_frame(now, drawn);
            now += Duration::from_millis(gap_ms);
        }
    }

    #[test]
    fn no_flag_means_no_recording_and_no_summary() {
        let mut times = FrameTimes::new(false);
        assert!(!times.is_recording());
        times.begin_frame(Instant::now());
        assert!(!times.end_frame(Instant::now(), Duration::ZERO));
        assert_eq!(times.summary(), None);
        // A report with nothing recorded stays silent.
        times.report();
    }

    #[test]
    fn an_empty_run_reports_no_frames() {
        let times = recording();
        assert_eq!(
            times.summary().as_deref(),
            Some("frames: 0 (no frames drawn)")
        );
    }

    #[test]
    fn a_draw_is_measured_around_the_draw_call() {
        let mut times = recording();
        feed(&mut times, 100, 5, 0);
        let line = times.summary().expect("a summary");
        assert!(line.starts_with("frames: 100 over"), "{line}");
        assert!(
            line.contains("draw p50 5.") && line.contains("max 5."),
            "the draw cost is in the line: {line}"
        );
    }

    #[test]
    fn idle_time_is_reported_apart_from_draw_cost() {
        // The case the first harness got wrong: a run that draws for a
        // moment and then idles. The draw is cheap; the run is mostly
        // waiting, and the line has to say so.
        let mut times = recording();
        feed(&mut times, 20, 6, 50);
        let line = times.summary().expect("a summary");
        assert!(
            line.contains("idle"),
            "the idle time is in the line: {line}"
        );
        let idle_ms: f64 = line
            .split("idle ")
            .nth(1)
            .and_then(|rest| rest.split(' ').next())
            .and_then(|value| value.parse().ok())
            .expect("an idle figure");
        assert!(idle_ms > 900.0, "most of the run was idle: {line}");
        assert!(
            line.contains("draw p50 6."),
            "the draw cost is what it was: {line}"
        );
    }

    #[test]
    fn a_long_gap_is_not_counted_as_a_slow_draw() {
        // The regression this module exists for: a one-second gap
        // between frames used to read as a one-second frame.
        let mut times = recording();
        let mut now = Instant::now();
        times.begin_frame(now);
        now += Duration::from_millis(4);
        times.end_frame(now, Duration::from_millis(4));
        // egui schedules nothing for a second; the next frame lands a
        // second later and draws in the same four milliseconds.
        times.begin_frame(now);
        now += Duration::from_secs(1) + Duration::from_millis(4);
        times.end_frame(now, Duration::from_millis(4));
        let line = times.summary().expect("a summary");
        assert!(line.contains("max 4."), "the gap is not a draw: {line}");
        assert!(
            line.contains("idle 1000") && line.contains("99%"),
            "the whole second is idle: {line}"
        );
    }

    #[test]
    fn the_worst_frame_carries_its_index() {
        let mut times = recording();
        let mut now = Instant::now();
        for draw_ms in [4_u64, 4, 90, 4] {
            times.begin_frame(now);
            now += Duration::from_millis(draw_ms);
            times.end_frame(now, Duration::from_millis(draw_ms));
            now += Duration::from_millis(1);
        }
        let line = times.summary().expect("a summary");
        // The 90 ms draw is the third frame; the line names where the
        // worst one landed so a reader knows whether it was startup
        // work or steady-state cost.
        assert!(
            line.contains("max 90.00 ms at frame 2"),
            "the index is in the line: {line}"
        );
    }

    #[test]
    fn the_summary_is_printed_once() {
        let mut times = recording();
        feed(&mut times, 5, 4, 1);
        times.report();
        times.report();
    }

    /// Drives a ticker at `fps` for `seconds`, one tick per frame, and
    /// reports how many ticks landed. This is the shape of the real
    /// `--stream` run: the shell asks for a tick every frame and the
    /// work is the frame itself.
    fn ticks_at(fps: u32, seconds: u32) -> (usize, usize) {
        let mut ticker = Ticker::hz30(Instant::now());
        let mut now = Instant::now();
        let frame = Duration::from_secs_f64(1.0 / f64::from(fps));
        let frames = usize::try_from(fps * seconds).expect("a test-sized run");
        for _ in 0..frames {
            now += frame;
            ticker.tick(now);
        }
        (ticker.fired(), frames)
    }

    #[test]
    fn thirty_hertz_holds_its_ticks_above_sixty_frames_per_second() {
        // The NFR-01 budget: 299 of 300 ticks in ten seconds. At a
        // frame rate above the tick rate the old clock reset cost one
        // frame of drift per tick and delivered 284.
        let (fired, frames) = ticks_at(64, 10);
        assert!(
            fired * 300 >= 299 * frames / 10,
            "{fired} of {frames} frames ticked, under 99%"
        );
        // 10 s holds 300 whole periods plus a fraction, so the count
        // is 300 or 301 depending on where the frames land. The old
        // clock reset delivered 284 at this rate.
        assert!(
            (300..=301).contains(&fired),
            "a 60+ fps run holds every 30 Hz tick, got {fired}"
        );
    }

    #[test]
    fn the_rate_is_the_rate_at_every_frame_rate_above_it() {
        for fps in [31_u32, 45, 60, 64, 120, 144] {
            let (fired, _frames) = ticks_at(fps, 2);
            let expected = usize::try_from(fps * 2 / 30).expect("a test-sized run");
            assert!(
                fired + 1 >= expected,
                "at {fps} fps only {fired} of ~{expected} ticks fired"
            );
        }
    }

    #[test]
    fn a_frame_below_the_rate_fires_once_the_tick_is_due() {
        // One instant for the ticker and the frames: two reads of the
        // clock can land a period apart on a loaded runner, which
        // fires the first tick early.
        let start = Instant::now();
        let mut ticker = Ticker::hz30(start);
        let mut now = start;
        // Two frames inside one period: no tick.
        now += Duration::from_millis(10);
        assert!(!ticker.tick(now));
        now += Duration::from_millis(10);
        assert!(!ticker.tick(now), "20 ms is still inside 33 ms");
        now += Duration::from_millis(15);
        assert!(ticker.tick(now), "35 ms is past the period");
        assert_eq!(ticker.fired(), 1);
    }

    #[test]
    fn a_slow_frame_fires_once_and_drops_the_debt() {
        let mut ticker = Ticker::hz30(Instant::now());
        let mut now = Instant::now();
        // A 200 ms stall is six periods. The tick fires once: the
        // frame did not earn six appends, and the counter says so.
        now += Duration::from_millis(200);
        assert!(ticker.tick(now));
        assert_eq!(ticker.fired(), 1, "the debt is dropped, not burst");
        now += Duration::from_millis(40);
        assert!(ticker.tick(now), "the rate resumes right after");
        assert_eq!(ticker.fired(), 2);
    }

    #[test]
    fn a_tick_does_not_fire_early() {
        let start = Instant::now();
        let mut ticker = Ticker::new(start, Duration::from_millis(100));
        let mut now = start;
        now += Duration::from_millis(99);
        assert!(!ticker.tick(now), "99 ms is early for a 100 ms period");
        now += Duration::from_millis(2);
        assert!(ticker.tick(now));
        assert_eq!(ticker.fired(), 1);
    }

    #[test]
    fn the_period_and_due_time_are_readable() {
        let ticker = Ticker::hz30(Instant::now());
        assert!((ticker.period().as_secs_f64() - 1.0 / 30.0).abs() < 1e-9);
        let now = Instant::now();
        assert!(ticker.due_in(now) <= ticker.period());
    }
}
