use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

/// Experimental clock: one frame contains one sample per channel.
pub const RATE: u64 = 48_000;
pub const SOURCE_RATES: [u64; 3] = [44_100, RATE, 96_000];
pub const MAX_SEGMENT_FRAMES: u64 = 30 * RATE;
/// Encoder context, separate from Ogg Opus pre-skip. An experimental value.
pub const CONTEXT_FRAMES: u64 = 120 * RATE / 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interval {
    pub start: u64,
    pub frames: u64,
}

impl Interval {
    pub fn new(start: u64, frames: u64, total: u64) -> Result<Self> {
        Self::at_rate(start, frames, total, RATE)
    }

    pub fn at_rate(start: u64, frames: u64, total: u64, rate: u64) -> Result<Self> {
        ensure!(SOURCE_RATES.contains(&rate), "unsupported source clock");
        ensure!(frames > 0, "an interval must contain at least one frame");
        ensure!(frames <= 30 * rate, "intervals are limited to 30 seconds");
        let end = start.checked_add(frames).context("interval overflow")?;
        ensure!(end <= total, "interval extends beyond the source");
        Ok(Self { start, frames })
    }

    pub fn end(self) -> u64 {
        self.start + self.frames
    }

    pub fn with_context(self, total: u64) -> Self {
        self.context_at_rate(total, RATE)
    }

    pub fn context_at_rate(self, total: u64, rate: u64) -> Self {
        let context = 120 * rate / 1000;
        let mut start = self.start.saturating_sub(context);
        // Align with the phase of a conversion anchored at track frame zero:
        // 147 native frames = 160 Opus frames at 44.1 kHz; 2 = 1 at 96 kHz.
        let stride = match rate {
            44_100 => 147,
            96_000 => 2,
            _ => 1,
        };
        start -= start % stride;
        let end = self.end().saturating_add(context).min(total);
        Self {
            start,
            frames: end - start,
        }
    }
}

pub fn converted_frames(frames: u64, from: u64, to: u64) -> Result<u64> {
    ensure!(from > 0 && to > 0, "invalid conversion clock");
    u64::try_from((u128::from(frames) * u128::from(to)).div_ceil(u128::from(from)))
        .context("converted frame count overflow")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_overflow_and_out_of_bounds() {
        assert!(Interval::new(0, 0, RATE).is_err());
        assert!(Interval::new(u64::MAX, 1, u64::MAX).is_err());
        assert!(Interval::new(RATE, 1, RATE).is_err());
        assert!(Interval::new(0, MAX_SEGMENT_FRAMES + 1, u64::MAX).is_err());
        assert_eq!(Interval::new(RATE - 1, 1, RATE).unwrap().end(), RATE);
    }

    #[test]
    fn context_clamps_at_both_track_edges() {
        let first = Interval::new(0, 1, RATE).unwrap().with_context(RATE);
        assert_eq!(
            first,
            Interval {
                start: 0,
                frames: CONTEXT_FRAMES + 1
            }
        );
        let last = Interval::new(RATE - 1, 1, RATE).unwrap().with_context(RATE);
        assert_eq!(last.end(), RATE);
        assert_eq!(last.frames, CONTEXT_FRAMES + 1);
    }

    #[test]
    fn native_clock_and_fractional_conversion_do_not_accumulate_rounding() {
        for frames in 1..10_000 {
            let opus = converted_frames(frames, 44100, 48000).unwrap();
            let native = converted_frames(opus, 48000, 44100).unwrap();
            assert_eq!(native - frames, u64::from(frames % 147 != 0));
        }
        let segment = Interval::at_rate(150 * 44100 + 37, 88201, 154 * 44100, 44100).unwrap();
        let context = segment.context_at_rate(154 * 44100, 44100);
        assert_eq!(context.start % 147, 0);
        assert!(segment.start - context.start >= 5292);
        assert!(segment.start - context.start < 5292 + 147);
        assert!(Interval::at_rate(0, 30 * 44100 + 1, 32 * 44100, 44100).is_err());
        assert!(converted_frames(u64::MAX, 1, u64::MAX).is_err());
    }

    #[test]
    fn high_rate_context_keeps_even_phase_and_exact_native_requests() {
        for start in [0, 1, 2, 3, 150 * 96000 + 37, 150 * 96000 + 38] {
            let interval = Interval::at_rate(start, 192001, 154 * 96000, 96000).unwrap();
            let context = interval.context_at_rate(154 * 96000, 96000);
            assert_eq!(context.start % 2, 0);
            assert_eq!(context.end(), interval.end() + 11520);
            let codec = converted_frames(context.frames, 96000, 48000).unwrap();
            assert_eq!(codec * 2 - context.frames, context.frames % 2);
        }
        assert!(Interval::at_rate(0, 30 * 96000, 31 * 96000, 96000).is_ok());
        assert!(Interval::at_rate(0, 30 * 96000 + 1, 31 * 96000, 96000).is_err());
    }
}
