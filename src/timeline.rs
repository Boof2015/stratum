use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

/// Experimental clock: one frame contains one sample per channel.
pub const RATE: u64 = 48_000;
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
        ensure!(frames > 0, "an interval must contain at least one frame");
        ensure!(
            frames <= MAX_SEGMENT_FRAMES,
            "intervals are limited to 30 seconds"
        );
        let end = start.checked_add(frames).context("interval overflow")?;
        ensure!(end <= total, "interval extends beyond the source");
        Ok(Self { start, frames })
    }

    pub fn end(self) -> u64 {
        self.start + self.frames
    }

    pub fn with_context(self, total: u64) -> Self {
        let start = self.start.saturating_sub(CONTEXT_FRAMES);
        let end = self.end().saturating_add(CONTEXT_FRAMES).min(total);
        Self {
            start,
            frames: end - start,
        }
    }
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
}
