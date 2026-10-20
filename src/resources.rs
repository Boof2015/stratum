//! Owner limits. Automatic is deliberately conservative, not a CPU-usage target.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};

pub const MB: u64 = 1_000_000;
pub const MAX_CONVERSIONS: usize = 4; // current development playback bound
pub const SCRATCH_BYTES: u64 = 32 * 1024 * 1024;
pub const FREE_RESERVE: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub revision: String,
    /// None means Automatic.
    pub max_conversions: Option<usize>,
    pub working_bytes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: "initial".into(),
            max_conversions: None,
            working_bytes: 512 * MB,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.revision == "initial"
                || (self.revision.len() == 64
                    && self
                        .revision
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))),
            "Invalid resource settings revision."
        );
        ensure!(
            self.max_conversions
                .is_none_or(|n| (1..=MAX_CONVERSIONS).contains(&n)),
            "Choose Automatic or between 1 and 4 simultaneous conversions."
        );
        ensure!(
            (128 * MB..=9_007_199_254_740_991).contains(&self.working_bytes),
            "Use a working-storage limit of at least 128 MB, within the supported numeric range."
        );
        Ok(())
    }
    pub fn conversions(&self, cpus: usize) -> usize {
        self.max_conversions
            .unwrap_or_else(|| (cpus / 2).clamp(1, MAX_CONVERSIONS))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub revision: String,
    pub max_conversions: Option<usize>,
    pub working_bytes: u64,
}

/// Uses Rust's affinity/quota-aware estimate. A denied/unknown query falls back
/// to one. Read at startup/settings save, never on every segment.
pub fn available_cpus() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
}

pub(crate) mod scheduler;
pub(crate) mod working;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_and_custom_limits_are_conservative_and_validated() {
        let settings = Settings::default();
        assert_eq!(
            (1..=10)
                .map(|n| settings.conversions(n))
                .collect::<Vec<_>>(),
            [1, 1, 1, 2, 2, 3, 3, 4, 4, 4]
        );
        for n in 1..=4 {
            let custom = Settings {
                max_conversions: Some(n),
                ..settings.clone()
            };
            custom.validate().unwrap();
            assert_eq!(custom.conversions(64), n);
        }
        for n in [0, 5, usize::MAX] {
            assert!(Settings {
                max_conversions: Some(n),
                ..settings.clone()
            }
            .validate()
            .is_err());
        }
        assert!(Settings {
            working_bytes: 128 * MB - 1,
            ..settings
        }
        .validate()
        .is_err());
    }
}
