//! Recording clock that excises paused spans, kept OS-free and testable by
//! taking the "current instant" as a parameter (tests feed synthetic instants).
//!
//! `active` is wall time since start minus every paused span (completed spans
//! plus any in-progress one). It drives both the UI timer (`elapsed`) and the
//! per-frame presentation timestamps, so the output video has neither a frozen
//! stretch nor a jump across a pause.

use std::time::{Duration, Instant};

pub(crate) struct RecordClock {
    start: Instant,
    /// Sum of every completed paused span.
    paused_total: Duration,
    /// When currently paused, the instant the pause began.
    paused_since: Option<Instant>,
}

impl RecordClock {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            start: now,
            paused_total: Duration::ZERO,
            paused_since: None,
        }
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.paused_since.is_some()
    }

    /// Begin a pause. Idempotent: a second call while already paused keeps the
    /// original pause start (so the excised span is not shortened).
    pub(crate) fn pause(&mut self, now: Instant) {
        if self.paused_since.is_none() {
            self.paused_since = Some(now);
        }
    }

    /// End a pause, folding its span into the accumulated total. No-op when not
    /// paused.
    pub(crate) fn resume(&mut self, now: Instant) {
        if let Some(since) = self.paused_since.take() {
            self.paused_total += now.saturating_duration_since(since);
        }
    }

    /// Active (unpaused) time elapsed as of `now`. Frozen while paused.
    pub(crate) fn active(&self, now: Instant) -> Duration {
        let raw = now.saturating_duration_since(self.start);
        let mut paused = self.paused_total;
        if let Some(since) = self.paused_since {
            paused += now.saturating_duration_since(since);
        }
        raw.saturating_sub(paused)
    }

    /// Active time as a Media Foundation / WGC timestamp in 100-nanosecond
    /// ticks — the unit `VideoEncoder::send_frame_buffer` expects.
    pub(crate) fn active_hns(&self, now: Instant) -> i64 {
        (self.active(now).as_nanos() / 100) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn active_tracks_wall_time_without_pauses() {
        let base = Instant::now();
        let clock = RecordClock::new(base);
        assert_eq!(clock.active(at(base, 0)), Duration::ZERO);
        assert_eq!(clock.active(at(base, 1500)), Duration::from_millis(1500));
    }

    #[test]
    fn active_is_frozen_during_a_pause() {
        let base = Instant::now();
        let mut clock = RecordClock::new(base);
        clock.pause(at(base, 1000));
        assert!(clock.is_paused());
        // Time passes while paused, active stays put.
        assert_eq!(clock.active(at(base, 1000)), Duration::from_millis(1000));
        assert_eq!(clock.active(at(base, 3000)), Duration::from_millis(1000));
    }

    #[test]
    fn resume_excises_the_paused_span() {
        let base = Instant::now();
        let mut clock = RecordClock::new(base);
        clock.pause(at(base, 1000));
        clock.resume(at(base, 3000)); // 2s paused
        assert!(!clock.is_paused());
        // 4s wall, 2s paused -> 2s active.
        assert_eq!(clock.active(at(base, 4000)), Duration::from_millis(2000));
    }

    #[test]
    fn multiple_pause_cycles_accumulate() {
        let base = Instant::now();
        let mut clock = RecordClock::new(base);
        clock.pause(at(base, 1000));
        clock.resume(at(base, 2000)); // +1s paused
        clock.pause(at(base, 4000));
        clock.resume(at(base, 4500)); // +0.5s paused
        // 6s wall, 1.5s paused -> 4.5s active.
        assert_eq!(clock.active(at(base, 6000)), Duration::from_millis(4500));
    }

    #[test]
    fn double_pause_keeps_original_start() {
        let base = Instant::now();
        let mut clock = RecordClock::new(base);
        clock.pause(at(base, 1000));
        clock.pause(at(base, 2000)); // ignored
        clock.resume(at(base, 3000)); // span is 1000..3000 = 2s
        assert_eq!(clock.active(at(base, 5000)), Duration::from_millis(3000));
    }

    #[test]
    fn resume_without_pause_is_noop() {
        let base = Instant::now();
        let mut clock = RecordClock::new(base);
        clock.resume(at(base, 1000));
        assert_eq!(clock.active(at(base, 2000)), Duration::from_millis(2000));
    }

    #[test]
    fn active_hns_uses_100ns_ticks() {
        let base = Instant::now();
        let clock = RecordClock::new(base);
        // 1 second = 10,000,000 * 100ns.
        assert_eq!(clock.active_hns(at(base, 1000)), 10_000_000);
    }
}
