//! Pure frame-write decision logic: fps pacing, keepalive duplication, and the
//! final frame at stop. No OS calls, unit-tested on any platform.
//!
//! WGC only delivers frames when the screen changes, so a static screen would
//! otherwise leave the mp4 ending at the last change (mvhd duration far shorter
//! than `elapsed()`). The keepalive path re-sends the last written frame with a
//! fresh active-clock timestamp whenever no frame has been written for
//! [`FramePacer::keepalive_after_hns`], and `stop()` emits one last frame at
//! the final active time so the video always ends at `elapsed()`.
//!
//! All timestamps are active-clock 100ns ticks (see `RecordClock`), which
//! freeze while paused; every decision here also requires strictly increasing
//! timestamps, so paused spans can never emit frames even without checking the
//! pause flag.

/// Keepalive threshold in frame periods: duplicate the last frame once no real
/// frame has been written for two periods. On a fully static screen this emits
/// ~fps/2 duplicate frames (which H.264 compresses to almost nothing) and keeps
/// the mp4 duration within two frame periods of the active clock even before
/// the final stop() frame.
const KEEPALIVE_PERIODS: i64 = 2;

/// Accept a real frame slightly early (90% of the nominal interval) so capture
/// jitter does not systematically halve the effective frame rate.
const REAL_FRAME_SLACK_NUM: i64 = 9;
const REAL_FRAME_SLACK_DEN: i64 = 10;

pub(crate) struct FramePacer {
    /// Nominal inter-frame gap in 100ns ticks (10_000_000 / fps).
    interval_hns: i64,
    /// Timestamp of the last written frame; `None` before the first.
    last_written_hns: Option<i64>,
}

impl FramePacer {
    pub(crate) fn new(fps: u32) -> Self {
        Self {
            interval_hns: 10_000_000 / i64::from(fps.max(1)),
            last_written_hns: None,
        }
    }

    /// Should a freshly captured frame stamped `hns` be written? Enforces the
    /// target fps (with slack) and strict monotonicity.
    pub(crate) fn accept_real(&self, hns: i64) -> bool {
        match self.last_written_hns {
            None => true,
            Some(last) => {
                hns > last
                    && (hns - last) * REAL_FRAME_SLACK_DEN
                        >= self.interval_hns * REAL_FRAME_SLACK_NUM
            }
        }
    }

    /// Should the keepalive duplicate the last frame at `hns`? Never before the
    /// first real frame (there is nothing to duplicate) and never with a
    /// non-increasing timestamp (covers the paused case, where the active clock
    /// is frozen).
    pub(crate) fn keepalive_due(&self, hns: i64) -> bool {
        match self.last_written_hns {
            None => false,
            Some(last) => hns > last && hns - last >= self.keepalive_after_hns(),
        }
    }

    /// Gap after which the keepalive fires.
    pub(crate) fn keepalive_after_hns(&self) -> i64 {
        self.interval_hns * KEEPALIVE_PERIODS
    }

    /// Should a final frame stamped `hns` be written at stop? Only when it
    /// extends the timeline (strictly after the last write) and a frame has
    /// been written at all.
    pub(crate) fn accept_final(&self, hns: i64) -> bool {
        matches!(self.last_written_hns, Some(last) if hns > last)
    }

    /// Record that a frame stamped `hns` was actually written.
    pub(crate) fn record_write(&mut self, hns: i64) {
        self.last_written_hns = Some(hns);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 30 fps -> 333_333 hns interval.
    fn pacer30() -> FramePacer {
        FramePacer::new(30)
    }

    #[test]
    fn first_real_frame_is_always_accepted() {
        let p = pacer30();
        assert!(p.accept_real(0));
        assert!(p.accept_real(123));
    }

    #[test]
    fn real_frame_within_interval_is_dropped() {
        let mut p = pacer30();
        p.record_write(1_000_000);
        // Only 0.2 of an interval later.
        assert!(!p.accept_real(1_000_000 + 333_333 / 5));
    }

    #[test]
    fn real_frame_at_ninety_percent_of_interval_is_accepted() {
        let mut p = pacer30();
        p.record_write(1_000_000);
        // Exact boundary: gap*10 >= interval*9 -> gap >= ceil(333_333*9/10).
        let boundary = (333_333i64 * 9 + 9) / 10; // 300_000
        assert!(p.accept_real(1_000_000 + boundary));
        assert!(!p.accept_real(1_000_000 + boundary - 1));
        assert!(!p.accept_real(1_000_000 + 333_333 * 8 / 10));
    }

    #[test]
    fn non_monotonic_real_frame_is_dropped() {
        let mut p = pacer30();
        p.record_write(2_000_000);
        assert!(!p.accept_real(2_000_000)); // equal
        assert!(!p.accept_real(1_500_000)); // earlier
    }

    #[test]
    fn keepalive_never_fires_before_the_first_real_frame() {
        let p = pacer30();
        assert!(!p.keepalive_due(50_000_000));
    }

    #[test]
    fn keepalive_fires_after_two_frame_periods_of_silence() {
        let mut p = pacer30();
        p.record_write(1_000_000);
        let gap = p.keepalive_after_hns();
        assert_eq!(gap, 666_666); // 2 periods at 30 fps
        assert!(!p.keepalive_due(1_000_000 + gap - 1));
        assert!(p.keepalive_due(1_000_000 + gap));
        assert!(p.keepalive_due(1_000_000 + 10 * gap));
    }

    #[test]
    fn keepalive_is_suppressed_while_the_clock_is_frozen() {
        // Paused: the active clock stops, so `hns` never exceeds the last
        // written timestamp and the keepalive must stay silent.
        let mut p = pacer30();
        p.record_write(5_000_000);
        assert!(!p.keepalive_due(5_000_000));
        assert!(!p.keepalive_due(4_000_000));
    }

    #[test]
    fn keepalive_resumes_after_unfreeze() {
        // After resume the active clock advances again; once the gap exceeds
        // the threshold the keepalive fires with a monotonic timestamp.
        let mut p = pacer30();
        p.record_write(5_000_000);
        assert!(p.keepalive_due(5_000_000 + p.keepalive_after_hns()));
    }

    #[test]
    fn keepalive_and_real_frames_share_monotonic_state() {
        let mut p = pacer30();
        p.record_write(1_000_000);
        // Keepalive wrote at t=1_666_666.
        p.record_write(1_666_666);
        // A real frame captured with an older/equal timestamp must be dropped.
        assert!(!p.accept_real(1_666_666));
        assert!(!p.accept_real(1_400_000));
        assert!(p.accept_real(1_666_666 + 333_333));
    }

    #[test]
    fn final_frame_only_when_it_extends_the_timeline() {
        let mut p = pacer30();
        // Nothing written yet: no final frame.
        assert!(!p.accept_final(10_000_000));
        p.record_write(10_000_000);
        assert!(!p.accept_final(10_000_000)); // equal: would break monotonicity
        assert!(!p.accept_final(9_000_000));
        assert!(p.accept_final(10_000_001));
    }

    #[test]
    fn one_fps_floor_prevents_division_blowup() {
        let p = FramePacer::new(0); // clamped to 1 fps internally
        assert_eq!(p.keepalive_after_hns(), 20_000_000);
    }
}
