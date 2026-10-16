use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use stratum_poc::{
    media::{Engine, Quality, Source},
    proof,
};

const HELP: &str = "Stratum audio proof of concept

  stratum-poc demo --out artifacts/demo [--source /path/to/music.flac]
  stratum-poc inspect --source /path/to/music.flac
  stratum-poc segment --source /path/to/music.flac --start-frame 7200037 \\
      --frames 96001 --quality opus-128 --cache artifacts/cache

Requires ffmpeg (with libopus) and ffprobe on PATH.
This experiment supports 44.1/48 kHz mono/stereo, 16/24-bit FLAC or PCM WAV.
Quality: lossless, opus-64, opus-128. All frame counts are per channel.
No server is started and nothing plays automatically.
";

fn main() {
    if let Err(error) = cli() {
        eprintln!("Stratum: {error:#}");
        std::process::exit(1);
    }
}

fn cli() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "help".into());
    if ["help", "--help", "-h"].contains(&command.as_str()) {
        print!("{HELP}");
        return Ok(());
    }
    let mut options = HashMap::new();
    while let Some(key) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("missing value for {key}"))?;
        if options.insert(key.clone(), value).is_some() {
            bail!("duplicate option {key}");
        }
    }
    let allowed: &[&str] = match command.as_str() {
        "demo" => &["--source", "--out"],
        "inspect" => &["--source"],
        "segment" => &[
            "--source",
            "--start-frame",
            "--frames",
            "--quality",
            "--cache",
        ],
        _ => bail!("unknown command {command}; run --help"),
    };
    for key in options.keys() {
        if !allowed.contains(&key.as_str()) {
            bail!("unknown option {key}");
        }
    }
    let required = |key: &str| -> Result<&str> {
        options
            .get(key)
            .map(String::as_str)
            .with_context(|| format!("missing {key}"))
    };
    match command.as_str() {
        "demo" => {
            let output = PathBuf::from(required("--out")?);
            let report = proof::demo(options.get("--source").map(Path::new), &output)?;
            println!("{} frames; exact frame count: {}; lossless sections exact: {}; estimated alignment at zero: {}",
                report.output_frames, report.exact_frame_count, report.lossless_sections_exact, report.alignment_windows_at_zero_lag);
            println!(
                "Listen: {}\nReference: {}\nMeasurements: {}",
                output.join("switched.wav").display(),
                output.join("reference.wav").display(),
                output.join("report.json").display()
            );
        }
        "inspect" => println!(
            "{}",
            serde_json::to_string_pretty(&Source::open(Path::new(required("--source")?))?.info)?
        ),
        "segment" => {
            let source = Source::open(Path::new(required("--source")?))?;
            let engine = Engine::new(source, Path::new(required("--cache")?))?;
            let segment = engine.segment(
                required("--start-frame")?.parse()?,
                required("--frames")?.parse()?,
                Quality::parse(required("--quality")?)?,
            )?;
            println!("{}", serde_json::to_string_pretty(&segment)?);
        }
        _ => unreachable!(),
    }
    Ok(())
}
