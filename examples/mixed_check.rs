//! Independent native-frame and capture accounting for the mixed-output proof.
use anyhow::{ensure, Result};
use serde::Deserialize;
use serde_json::json;
use std::{env, fs, path::PathBuf};
use stratum_poc::{
    media::{self, Source},
    proof,
    timeline::SOURCE_RATES,
};

#[derive(Deserialize)]
struct Track {
    source: PathBuf,
    sample_rate: u64,
    origin: u64,
    frames: u64,
    input_origin: u64,
    input_end: u64,
    output_start: u64,
    output_frames: u64,
    discard_output_start: u64,
    output_pcm: PathBuf,
}
#[derive(Deserialize)]
struct Segment {
    track: usize,
    source_start: u64,
    frames: u64,
    quality: String,
    pcm: PathBuf,
}
#[derive(Deserialize)]
struct Underrun {
    at_media_frame: u64,
    silence_frames: u64,
}
#[derive(Deserialize)]
struct Delivery {
    output_rate: u64,
    frames: u64,
    tracks: Vec<Track>,
    segments: Vec<Segment>,
    underruns: Vec<Underrun>,
}

fn main() -> Result<()> {
    let path = PathBuf::from(env::args_os().nth(1).expect("delivery.json required"));
    let parent = path.parent().unwrap();
    let delivery: Delivery = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(
        SOURCE_RATES.contains(&delivery.output_rate) && delivery.tracks.len() == 2,
        "invalid output policy"
    );
    ensure!(
        delivery
            .segments
            .windows(2)
            .all(|pair| pair[0].track <= pair[1].track),
        "cross-track segment order changed"
    );
    let (mut elapsed_ticks, mut output_end, mut segments_seen) = (0u128, 0u64, 0usize);
    let mut output = Vec::new();
    let mut checks = Vec::new();
    for (index, track) in delivery.tracks.iter().enumerate() {
        let source = Source::open(&track.source)?;
        let rate = source.info.sample_rate;
        ensure!(
            rate == track.sample_rate && source.info.channels == 2,
            "source format mismatch"
        );
        let end = track.origin.checked_add(track.frames).unwrap();
        ensure!(
            track.frames > 0
                && track.input_origin <= track.origin
                && end <= track.input_end
                && track.input_end <= source.info.frames,
            "invalid source range"
        );
        ensure!(
            (index != 0 || end == source.info.frames) && (index != 1 || track.origin == 0),
            "not real EOF into frame zero"
        );
        ensure!(
            (track.input_origin * delivery.output_rate).is_multiple_of(rate),
            "unanchored output conversion phase"
        );
        ensure!(
            track.discard_output_start
                == (track.origin - track.input_origin) * delivery.output_rate / rate,
            "wrong output seek trim"
        );
        // LCM of all supported native clocks. Independent cumulative integer
        // accounting catches accidental sums of rounded segment/track lengths.
        elapsed_ticks += u128::from(track.frames) * (14_112_000 / u128::from(rate));
        let boundary =
            u64::try_from(elapsed_ticks * u128::from(delivery.output_rate) / 14_112_000)?;
        ensure!(
            track.output_start == output_end && track.output_frames == boundary - output_end,
            "wrong output timeline"
        );
        output_end = boundary;
        let reference = media::decode(source.path(), 2, source.info.frames)?;
        let track_segments: Vec<_> = delivery
            .segments
            .iter()
            .filter(|s| s.track == index)
            .collect();
        let codec_reference =
            if rate == 96000 && track_segments.iter().any(|s| s.quality != "lossless") {
                Some(media::decode_at_rate(
                    source.path(),
                    2,
                    source.info.frames.div_ceil(2),
                    Some(48000),
                )?)
            } else {
                None
            };
        let (mut position, mut exact, mut zero, mut windows, mut inconclusive) =
            (track.input_origin, true, true, 0, 0);
        let (mut codec_zero, mut codec_windows, mut codec_inconclusive) = (true, 0, 0);
        let (mut short, mut within_codec) = (0, true);
        let mut nonzero = Vec::new();
        for segment in track_segments {
            ensure!(
                segment.source_start == position
                    && segment.frames > 0
                    && segment.frames <= track.input_end - position,
                "missing, repeated or unordered native frames"
            );
            ensure!(
                ["lossless", "opus-64", "opus-128"].contains(&segment.quality.as_str()),
                "unknown quality"
            );
            let bytes = fs::read(parent.join(&segment.pcm))?;
            ensure!(
                bytes.len() as u64 == segment.frames * 8,
                "wrong native PCM size"
            );
            let samples: Vec<i32> = bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|b| i32::from_le_bytes(*b))
                .collect();
            if segment.quality == "lossless" {
                let begin = position as usize * 2;
                exact &= samples == reference[begin..begin + samples.len()];
            } else if segment.frames < 128 {
                short += segment.frames;
            }
            for a in proof::alignment_at_rate(&reference, &samples, position, 2, rate) {
                if a.correlation <= 0.5 {
                    inconclusive += 1;
                } else {
                    windows += 1;
                    zero &= a.best_lag_frames == 0;
                    within_codec &= a.best_lag_frames.abs() < 2;
                    if a.best_lag_frames != 0 {
                        nonzero.push(json!({"quality":segment.quality,"at_frame":a.at_frame,
                            "channel":a.channel,"lag":a.best_lag_frames,"correlation":a.correlation}));
                    }
                }
            }
            if segment.quality != "lossless" {
                if let Some(reference) = &codec_reference {
                    for a in proof::opus_clock_alignment(reference, &samples, position, 2)? {
                        if a.correlation <= 0.5 {
                            codec_inconclusive += 1;
                        } else {
                            codec_windows += 1;
                            codec_zero &= a.best_lag_frames == 0;
                        }
                    }
                }
            }
            position += segment.frames;
            segments_seen += 1;
        }
        ensure!(position == track.input_end, "native track incomplete");
        let converted = fs::read(parent.join(&track.output_pcm))?;
        ensure!(
            converted.len() as u64 == track.output_frames * 8,
            "output track incomplete"
        );
        output.extend_from_slice(&converted);
        checks.push(json!({"sample_rate":rate,"native_frames":position-track.input_origin,"lossless_exact":exact,
            "zero_lag":zero && windows>0,"evaluable_windows":windows,"inconclusive":inconclusive,
            "native_lag_within_codec_frame":within_codec,"nonzero_alignment_windows":nonzero,
            "short_lossy_frames_without_lag_estimate":short,
            "lossy_codec_alignment":{"sample_rate":48000,"zero_lag":codec_zero && codec_windows>0,
                "evaluable_windows":codec_windows,"inconclusive":codec_inconclusive}}));
    }
    ensure!(
        segments_seen == delivery.segments.len() && output_end == delivery.frames,
        "unexpected native/output frames"
    );
    let (mut rendered, mut at) = (Vec::new(), 0);
    for underrun in delivery.underruns {
        let next = underrun.at_media_frame as usize * 8;
        ensure!(
            next >= at && next <= output.len() && underrun.silence_frames > 0,
            "invalid underrun"
        );
        rendered.extend_from_slice(&output[at..next]);
        rendered.resize(rendered.len() + underrun.silence_frames as usize * 8, 0);
        at = next;
    }
    rendered.extend_from_slice(&output[at..]);
    println!(
        "{}",
        json!({"tracks":checks,"frames":output_end,"exact_frames":true,
        "rendered_matches":rendered==fs::read(parent.join("rendered.s32le"))?})
    );
    Ok(())
}
