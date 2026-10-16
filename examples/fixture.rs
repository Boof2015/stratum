//! Generate a deterministic source for the disposable Navidrome experiment.
use anyhow::{ensure, Context, Result};
use std::path::Path;
use stratum_poc::{proof::fixture, timeline::RATE};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let output = args
        .next()
        .context("usage: cargo run --example fixture -- output.flac")?;
    ensure!(args.next().is_none(), "unexpected argument");
    ensure!(
        !Path::new(&output).exists(),
        "fixture destination already exists"
    );
    fixture(Path::new(&output), 160 * RATE + 317, 2, 24)
}
