//! Independent file/PCM oracle for the two-track natural EOF experiment.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::{fs, path::PathBuf};
use stratum_poc::{media, proof};

#[derive(Deserialize)]
struct Delivery {
    tracks: Vec<Track>,
    frames: u64,
    segments: Vec<Segment>,
    underruns: Vec<Underrun>,
}
#[derive(Deserialize)]
struct Track {
    source: PathBuf,
    #[serde(default)]
    origin: u64,
    frames: u64,
}
#[derive(Deserialize)]
struct Segment {
    track: usize,
    source_start: u64,
    start: u64,
    frames: u64,
    quality: String,
    pcm: String,
}
#[derive(Deserialize)]
struct Underrun {
    at_media_frame: u64,
    silence_frames: u64,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(args.len() == 1, "usage: handoff_check DELIVERY.json");
    let report = PathBuf::from(&args[0]);
    let parent = report.parent().context("no report directory")?;
    let delivery: Delivery = serde_json::from_slice(&fs::read(&report)?)?;
    ensure!(
        delivery.tracks.len() == 2,
        "expected two independent tracks"
    );
    let mut references = Vec::new();
    for (index, track) in delivery.tracks.iter().enumerate() {
        let source = media::Source::open(&track.source)?;
        let end = track
            .origin
            .checked_add(track.frames)
            .context("interval overflow")?;
        ensure!(
            end <= source.info.frames && track.frames > 0 && source.info.channels == 2,
            "source manifest mismatch"
        );
        ensure!(
            index != 0 || end == source.info.frames,
            "first track must reach actual EOF"
        );
        ensure!(
            index != 1 || track.origin == 0,
            "second track must start at frame zero"
        );
        references.push(media::decode(source.path(), 2, source.info.frames)?);
    }
    let (mut position, mut track, mut source_position) = (0, 0, 0);
    let (mut exact, mut zero_lag, mut windows, mut inconclusive) = (true, true, 0, 0);
    let mut short_lossy_frames = 0;
    let mut pcm_bytes = Vec::new();
    let mut boundaries = Vec::new();
    for segment in delivery.segments {
        ensure!(
            track < delivery.tracks.len()
                && segment.track == track
                && segment.start == position
                && segment.source_start == delivery.tracks[track].origin + source_position
                && segment.frames > 0
                && segment.frames <= delivery.tracks[track].frames - source_position,
            "missing, repeated, reordered or cross-track interval"
        );
        ensure!(
            ["lossless", "opus-64", "opus-128"].contains(&segment.quality.as_str()),
            "unknown quality"
        );
        let bytes = fs::read(parent.join(segment.pcm))?;
        ensure!(bytes.len() as u64 == segment.frames * 8, "wrong PCM length");
        let samples: Vec<i32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|bytes| i32::from_le_bytes(*bytes))
            .collect();
        let absolute = delivery.tracks[track].origin + source_position;
        let begin = absolute as usize * 2;
        if segment.quality == "lossless" {
            exact &= samples == references[track][begin..begin + samples.len()];
        } else if segment.frames < 128 {
            short_lossy_frames += segment.frames;
        }
        for alignment in proof::alignment(&references[track], &samples, absolute, 2) {
            if alignment.correlation <= 0.5 {
                inconclusive += 1;
            } else {
                windows += 1;
                zero_lag &= alignment.best_lag_frames == 0;
            }
        }
        pcm_bytes.extend_from_slice(&bytes);
        position += segment.frames;
        source_position += segment.frames;
        if source_position == delivery.tracks[track].frames {
            boundaries.push(position);
            track += 1;
            source_position = 0;
        }
    }
    ensure!(track == 2, "did not finish both tracks");
    let mut rendered = Vec::new();
    let mut at = 0;
    for underrun in delivery.underruns {
        let next = underrun.at_media_frame as usize * 8;
        ensure!(
            next >= at && next <= pcm_bytes.len(),
            "invalid underrun position"
        );
        ensure!(underrun.silence_frames > 0, "empty underrun");
        rendered.extend_from_slice(&pcm_bytes[at..next]);
        rendered.resize(rendered.len() + underrun.silence_frames as usize * 8, 0);
        at = next;
    }
    rendered.extend_from_slice(&pcm_bytes[at..]);
    let captured = fs::read(parent.join("rendered.s32le"))?;
    println!(
        "{}",
        json!({"exact_frames": position == delivery.frames,
            "frames": position, "lossless_exact": exact,
            "rendered_matches": rendered == captured,
            "zero_lag": zero_lag && windows > 0, "evaluable_windows": windows,
            "inconclusive": inconclusive, "short_lossy_frames_without_lag_estimate": short_lossy_frames,
            "track_end_frames": boundaries})
    );
    Ok(())
}
