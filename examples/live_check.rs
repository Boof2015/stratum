//! Independent post-run audio oracle for experiments/live.py.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{fs, path::PathBuf};
use stratum_poc::{media, proof};

#[derive(Deserialize)]
struct Delivery {
    origin: u64,
    frames: u64,
    segments: Vec<Segment>,
    underruns: Vec<Underrun>,
    rendered_media_frames: Option<u64>,
}
#[derive(Deserialize)]
struct Underrun {
    at_media_frame: u64,
    silence_frames: u64,
}
#[derive(Deserialize)]
struct Segment {
    start: u64,
    frames: u64,
    quality: String,
    pcm: String,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(args.len() == 2, "usage: live_check SOURCE DELIVERY.json");
    let source = media::Source::open(&PathBuf::from(&args[0]))?;
    let report_path = PathBuf::from(&args[1]);
    let parent = report_path.parent().context("no report directory")?;
    let delivery: Delivery = serde_json::from_slice(&fs::read(&report_path)?)?;
    let reference = media::decode(source.path(), source.info.channels, source.info.frames)?;
    let ch = source.info.channels as usize;
    let mut media_bytes = Vec::new();
    let (mut end, mut exact, mut windows, mut inconclusive, mut zero_lag) = (0, true, 0, 0, true);
    for segment in delivery.segments {
        ensure!(
            segment.start == end,
            "missing, repeated or reordered interval"
        );
        let bytes = fs::read(parent.join(segment.pcm))?;
        media_bytes.extend_from_slice(&bytes);
        ensure!(
            bytes.len() == segment.frames as usize * ch * 4,
            "wrong frame count"
        );
        let pcm: Vec<i32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|b| i32::from_le_bytes(*b))
            .collect();
        let start = delivery.origin + segment.start;
        let begin = start as usize * ch;
        if segment.quality == "lossless" {
            exact &= pcm == reference[begin..begin + pcm.len()];
        }
        for window in proof::alignment(&reference, &pcm, start, source.info.channels) {
            if window.correlation <= 0.5 {
                inconclusive += 1;
            } else {
                windows += 1;
                zero_lag &= window.best_lag_frames == 0;
            }
        }
        end += segment.frames;
    }
    if let Some(rendered_frames) = delivery.rendered_media_frames {
        ensure!(rendered_frames <= end, "rendered beyond accepted media");
        media_bytes.truncate(rendered_frames as usize * ch * 4);
    }
    let mut expected_render = Vec::new();
    let mut position = 0;
    for underrun in delivery.underruns {
        let at = underrun.at_media_frame as usize * ch * 4;
        ensure!(
            at >= position && at <= media_bytes.len(),
            "invalid underrun position"
        );
        expected_render.extend_from_slice(&media_bytes[position..at]);
        expected_render.resize(
            expected_render.len() + underrun.silence_frames as usize * ch * 4,
            0,
        );
        position = at;
    }
    expected_render.extend_from_slice(&media_bytes[position..]);
    let rendered_matches = expected_render == fs::read(parent.join("rendered.s32le"))?;
    println!(
        "{}",
        json!({"exact_frames": end == delivery.frames, "frames": end,
        "lossless_exact": exact, "zero_lag": zero_lag && windows > 0,
        "evaluable_windows": windows, "inconclusive": inconclusive,
        "rendered_matches": rendered_matches})
    );
    Ok(())
}
