//! Pure audio DSP for the recording engine: decode a WASAPI device buffer to
//! interleaved-stereo `f32`, clamp/convert `f32` to `i16`, and stream-resample
//! to the encoder's 48 kHz. No OS calls, so every function here is unit-tested
//! on any platform without an audio device.
//!
//! The encoder derives audio time from the cumulative *count* of sample frames
//! it has been handed, so the resampler is stateful: it carries sub-sample phase
//! and the last input frame across calls, letting consecutive device buffers
//! join with neither a click nor a gap.

/// Encoder target: 48 kHz interleaved-stereo 16-bit (the `AudioSettingsBuilder`
/// defaults). One frame is two channels of `i16` = 4 bytes.
pub(crate) const DST_SAMPLE_RATE: u32 = 48_000;

/// PCM sample encoding of a capture device's mix format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFmt {
    /// 32-bit IEEE float, nominal range [-1.0, 1.0] (typical WASAPI shared mode).
    F32,
    /// 16-bit signed PCM.
    I16,
    /// 32-bit signed PCM.
    I32,
}

impl SampleFmt {
    /// Bytes occupied by one single-channel sample in this format.
    pub(crate) fn bytes(self) -> usize {
        match self {
            SampleFmt::F32 | SampleFmt::I32 => 4,
            SampleFmt::I16 => 2,
        }
    }
}

/// Decode one device buffer (`channels`-interleaved samples encoded as `fmt`)
/// into interleaved-**stereo** `f32` frames appended to `out`.
///
/// Channel mapping with no mixing gymnastics: mono is duplicated to L and R,
/// two channels pass through, and more than two keeps the first two (front
/// L/R). A truncated trailing partial frame is ignored.
pub(crate) fn decode_to_stereo_f32(raw: &[u8], channels: u16, fmt: SampleFmt, out: &mut Vec<f32>) {
    let channels = channels.max(1) as usize;
    let stride = fmt.bytes() * channels;
    if stride == 0 {
        return;
    }
    out.reserve(raw.len() / stride * 2);
    for frame in raw.chunks_exact(stride) {
        let l = sample_at(frame, 0, fmt);
        let r = if channels >= 2 {
            sample_at(frame, 1, fmt)
        } else {
            l
        };
        out.push(l);
        out.push(r);
    }
}

/// Read channel `ch` of a single interleaved frame as `f32`.
fn sample_at(frame: &[u8], ch: usize, fmt: SampleFmt) -> f32 {
    let off = ch * fmt.bytes();
    match fmt {
        SampleFmt::F32 => f32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]),
        SampleFmt::I16 => {
            let v = i16::from_le_bytes([frame[off], frame[off + 1]]);
            f32::from(v) / 32768.0
        }
        SampleFmt::I32 => {
            let v = i32::from_le_bytes([frame[off], frame[off + 1], frame[off + 2], frame[off + 3]]);
            v as f32 / 2_147_483_648.0
        }
    }
}

/// Convert a normalized `f32` sample to `i16`, clamping out-of-range values so
/// a hot source cannot wrap around into loud noise. Symmetric scaling by 32767
/// keeps +1.0 -> 32767 and -1.0 -> -32767.
pub(crate) fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * 32767.0).round() as i16
}

/// Streaming linear-interpolation resampler: fixed stereo, `f32` in / `i16` out.
///
/// Keeps sub-sample phase and the previous input frame across `process` calls so
/// device buffers stitch together seamlessly. The output frame count tracks the
/// input frame count times `dst/src`, so cumulative output stays locked to the
/// captured audio's real duration (no phase drift accumulates: phase stays in
/// [0,1) and the integer position advances exactly one per input frame).
pub(crate) struct StereoResampler {
    /// Input frames consumed per output frame (`src_rate / dst_rate`).
    step: f64,
    /// Position in [0,1) of the next output between `prev` and the next input.
    phase: f64,
    /// Last input frame seen; the left anchor of the current interpolation span.
    prev: [f32; 2],
    /// False until the first input frame primes `prev`.
    have_prev: bool,
}

impl StereoResampler {
    pub(crate) fn new(src_rate: u32, dst_rate: u32) -> Self {
        let src = f64::from(src_rate.max(1));
        let dst = f64::from(dst_rate.max(1));
        Self {
            step: src / dst,
            phase: 0.0,
            prev: [0.0; 2],
            have_prev: false,
        }
    }

    /// Reset phase and the primed input frame. Used when a span is excised (a
    /// pause) so resumed audio does not interpolate across the discarded gap.
    pub(crate) fn reset(&mut self) {
        self.phase = 0.0;
        self.prev = [0.0; 2];
        self.have_prev = false;
    }

    /// Resample interleaved-stereo `f32` `input`, appending interleaved-stereo
    /// `i16` frames to `out`.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<i16>) {
        for frame in input.chunks_exact(2) {
            let cur = [frame[0], frame[1]];
            if !self.have_prev {
                self.prev = cur;
                self.have_prev = true;
                continue;
            }
            // Emit every output sample whose position falls in [prev, cur).
            while self.phase < 1.0 {
                let t = self.phase as f32;
                let l = self.prev[0] + (cur[0] - self.prev[0]) * t;
                let r = self.prev[1] + (cur[1] - self.prev[1]) * t;
                out.push(f32_to_i16(l));
                out.push(f32_to_i16(r));
                self.phase += self.step;
            }
            // Advance the integer input position by exactly one frame.
            self.phase -= 1.0;
            self.prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i16_clamps_and_scales() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        // Out of range is clamped, not wrapped.
        assert_eq!(f32_to_i16(2.5), 32767);
        assert_eq!(f32_to_i16(-9.0), -32767);
        assert_eq!(f32_to_i16(0.5), 16384); // 0.5*32767 = 16383.5 -> round 16384
    }

    #[test]
    fn decode_mono_duplicates_to_both_channels() {
        // Two mono f32 frames: 0.25 and -0.5.
        let mut raw = Vec::new();
        raw.extend_from_slice(&0.25f32.to_le_bytes());
        raw.extend_from_slice(&(-0.5f32).to_le_bytes());
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw, 1, SampleFmt::F32, &mut out);
        assert_eq!(out, vec![0.25, 0.25, -0.5, -0.5]);
    }

    #[test]
    fn decode_stereo_passthrough() {
        let mut raw = Vec::new();
        for v in [0.1f32, 0.2, -0.3, -0.4] {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw, 2, SampleFmt::F32, &mut out);
        assert_eq!(out, vec![0.1, 0.2, -0.3, -0.4]);
    }

    #[test]
    fn decode_multichannel_keeps_front_left_right() {
        // One 5.1-ish frame with 6 channels; only the first two survive.
        let mut raw = Vec::new();
        for v in [0.9f32, -0.8, 0.1, 0.2, 0.3, 0.4] {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw, 6, SampleFmt::F32, &mut out);
        assert_eq!(out, vec![0.9, -0.8]);
    }

    #[test]
    fn decode_i16_scales_to_unit_range() {
        let mut raw = Vec::new();
        raw.extend_from_slice(&i16::MAX.to_le_bytes()); // L
        raw.extend_from_slice(&i16::MIN.to_le_bytes()); // R
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw, 2, SampleFmt::I16, &mut out);
        assert!((out[0] - 0.999_97).abs() < 1e-3);
        assert!((out[1] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn decode_ignores_partial_trailing_frame() {
        // 2ch f32 stride is 16 bytes; give 16 + 3 stray bytes.
        let mut raw = Vec::new();
        for v in [0.1f32, 0.2] {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        raw.extend_from_slice(&[1, 2, 3]);
        let mut out = Vec::new();
        decode_to_stereo_f32(&raw, 2, SampleFmt::F32, &mut out);
        assert_eq!(out, vec![0.1, 0.2]);
    }

    /// Feed interleaved-stereo `f32` in one shot.
    fn interleave(l: &[f32], r: &[f32]) -> Vec<f32> {
        l.iter().zip(r).flat_map(|(&a, &b)| [a, b]).collect()
    }

    #[test]
    fn resampler_same_rate_preserves_values_with_one_frame_latency() {
        let mut rs = StereoResampler::new(48_000, 48_000);
        let l = [0.1f32, 0.2, 0.3, 0.4];
        let r = [-0.1f32, -0.2, -0.3, -0.4];
        let input = interleave(&l, &r);
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        // 4 input frames -> 3 output frames (one-frame priming latency), equal to
        // the first three input frames exactly.
        assert_eq!(out.len(), 3 * 2);
        assert_eq!(out[0], f32_to_i16(0.1));
        assert_eq!(out[1], f32_to_i16(-0.1));
        assert_eq!(out[2], f32_to_i16(0.2));
        assert_eq!(out[4], f32_to_i16(0.3));
    }

    #[test]
    fn resampler_upsample_produces_more_frames() {
        let mut rs = StereoResampler::new(44_100, 48_000);
        // One second of a constant signal.
        let n = 44_100usize;
        let input: Vec<f32> = std::iter::repeat_n(0.5f32, n * 2).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        let out_frames = out.len() / 2;
        // ~48000 output frames (within a few of the ideal ratio; minus the one
        // priming frame of latency).
        let ideal = (n as f64 * 48_000.0 / 44_100.0) as usize;
        assert!(
            out_frames.abs_diff(ideal) <= 2,
            "expected ~{ideal} frames, got {out_frames}"
        );
        // A constant input stays constant through linear interpolation.
        assert!(out.iter().all(|&s| s == f32_to_i16(0.5)));
    }

    #[test]
    fn resampler_downsample_produces_fewer_frames() {
        let mut rs = StereoResampler::new(48_000, 48_000 / 2);
        let n = 48_000usize;
        let input: Vec<f32> = std::iter::repeat_n(0.25f32, n * 2).collect();
        let mut out = Vec::new();
        rs.process(&input, &mut out);
        let out_frames = out.len() / 2;
        assert!(
            out_frames.abs_diff(n / 2) <= 2,
            "expected ~{} frames, got {out_frames}",
            n / 2
        );
    }

    #[test]
    fn resampler_streaming_matches_single_shot() {
        // The streaming property: splitting the input across two calls must
        // produce the exact same output as one call (continuity, no clicks).
        let l: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin()).collect();
        let r: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.017).cos()).collect();
        let input = interleave(&l, &r);

        let mut whole = StereoResampler::new(44_100, 48_000);
        let mut out_whole = Vec::new();
        whole.process(&input, &mut out_whole);

        let mut split = StereoResampler::new(44_100, 48_000);
        let mut out_split = Vec::new();
        let mid = (input.len() / 2 / 2) * 2 * 2; // split on a frame boundary
        split.process(&input[..mid], &mut out_split);
        split.process(&input[mid..], &mut out_split);

        assert_eq!(out_whole, out_split);
    }

    #[test]
    fn resampler_reset_reprimes() {
        let mut rs = StereoResampler::new(48_000, 48_000);
        let mut out = Vec::new();
        rs.process(&[0.5, 0.5, 0.6, 0.6], &mut out);
        let before = out.len();
        rs.reset();
        out.clear();
        // After reset the first frame only primes; a single frame yields nothing.
        rs.process(&[0.9, 0.9], &mut out);
        assert!(out.is_empty());
        assert!(before > 0);
    }
}
