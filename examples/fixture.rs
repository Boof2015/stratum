//! Generate a deterministic source for the disposable Navidrome experiment.
use anyhow::{ensure, Context, Result};
use std::path::Path;
use stratum_poc::{proof::fixture_at_rate, timeline::RATE};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args
        .next()
        .context("usage: cargo run --example fixture -- output.flac [44100|48000]")?;
    let rate = args
        .next()
        .map(|value| value.parse::<u64>())
        .transpose()?
        .unwrap_or(RATE);
    ensure!(args.next().is_none(), "unexpected argument");
    ensure!(
        !Path::new(&output).exists(),
        "fixture destination already exists"
    );
    fixture_at_rate(Path::new(&output), 160 * rate + 317, 2, 24, rate)
}
