use crate::{
    media::{decode, presentation_pcm, Engine, Quality, Segment, Source},
    process::{ffmpeg, run},
    timeline::{converted_frames, RATE, SOURCE_RATES},
};
use anyhow::{ensure, Result};
use serde::Serialize;
use std::{
    f64::consts::TAU,
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
    time::Duration,
};

/// A reproducible non-periodic musical-ish signal with stereo distinction and
/// transients. Quantized before encoding, so lossless comparisons are exact.
pub fn fixture(path: &Path, frames: u64, channels: u32, bits: u32) -> Result<()> {
    fixture_at_rate(path, frames, channels, bits, RATE)
}

pub fn fixture_at_rate(
    path: &Path,
    frames: u64,
    channels: u32,
    bits: u32,
    rate: u64,
) -> Result<()> {
    ensure!(SOURCE_RATES.contains(&rate), "unsupported fixture rate");
    ensure!(
        (1..=2).contains(&channels) && [16, 24].contains(&bits),
        "unsupported fixture format"
    );
    let temp = tempfile::tempdir()?;
    let pcm = temp.path().join("fixture.s32le");
    let mut writer = BufWriter::new(File::create(&pcm)?);
    let mut rng = 0x1234_5678_u32;
    let mut noise = 0.0;
    for i in 0..frames {
        let time = i as f64 / rate as f64;
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        noise = 0.65 * noise + 0.35 * (rng as f64 / u32::MAX as f64 * 2.0 - 1.0);
        let envelope = 0.65 + 0.25 * (TAU * 0.71 * time).sin();
        let pulse = (-((time * 3.17).fract()) * 60.0).exp();
        let base = envelope
            * (0.18 * (TAU * (173.0 * time + 17.0 * time * time)).sin()
                + 0.13 * (TAU * 431.0 * time).sin()
                + 0.09 * (TAU * 997.0 * time).sin())
            + 0.07 * noise
            + 0.04 * pulse * (TAU * 3127.0 * time).sin()
            // Native high-band content exposes an accidental 48 kHz lossless
            // round trip. Measurement fixture only; never headphone material.
            + if rate == 96_000 { 0.03 * (TAU * 30_017.0 * time).sin() } else { 0.0 };
        for channel in 0..channels {
            let value = if channel == 0 {
                base
            } else {
                base * 0.71 + 0.09 * (TAU * 719.0 * time).sin()
            };
            let scale = (1_i64 << (bits - 1)) - 1;
            let quantized = (value.clamp(-1.0, 1.0) * scale as f64).round() as i32;
            writer.write_all(&(quantized << (32 - bits)).to_le_bytes())?;
        }
    }
    writer.flush()?;
    let mut command = ffmpeg();
    command
        .args([
            "-f",
            "s32le",
            "-ar",
            &rate.to_string(),
            "-ac",
            &channels.to_string(),
            "-i",
        ])
        .arg(&pcm)
        .args([
            "-c:a",
            "flac",
            "-bits_per_raw_sample",
            &bits.to_string(),
            "-threads",
            "1",
        ])
        .arg(path);
    run(&mut command, Duration::from_secs(30), 1_048_576)?;
    Ok(())
}

pub fn write_wav(path: &Path, pcm: &[i32], channels: u32) -> Result<()> {
    write_wav_at_rate(path, pcm, channels, RATE)
}

pub fn write_wav_at_rate(path: &Path, pcm: &[i32], channels: u32, rate: u64) -> Result<()> {
    let temp = tempfile::NamedTempFile::new()?;
    let mut writer = BufWriter::new(temp.reopen()?);
    for sample in pcm {
        writer.write_all(&sample.to_le_bytes())?;
    }
    writer.flush()?;
    run(
        ffmpeg()
            .args([
                "-f",
                "s32le",
                "-ar",
                &rate.to_string(),
                "-ac",
                &channels.to_string(),
                "-i",
            ])
            .arg(temp.path())
            .args(["-c:a", "pcm_s24le", "-threads", "1"])
            .arg(path),
        Duration::from_secs(30),
        1_048_576,
    )?;
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct Alignment {
    pub at_frame: u64,
    pub channel: u32,
    pub best_lag_frames: i32,
    pub correlation: f64,
}

/// Compare against a separate full-source decode, not the segment's own metadata.
/// Three windows per channel detect startup, mid-segment and end alignment.
pub fn alignment(reference: &[i32], actual: &[i32], start: u64, channels: u32) -> Vec<Alignment> {
    alignment_at_rate(reference, actual, start, channels, RATE)
}

/// Keep at least the original ~43 ms observation window at higher rates.
/// This changes measurement duration, never the presented PCM or expected lag.
pub fn alignment_at_rate(
    reference: &[i32],
    actual: &[i32],
    start: u64,
    channels: u32,
    rate: u64,
) -> Vec<Alignment> {
    let ch = channels as usize;
    let frames = actual.len() / ch;
    let width = frames.min((2048 * rate.max(RATE) / RATE) as usize);
    if width < 128 {
        return Vec::new();
    }
    let mut offsets = vec![0, (frames - width) / 2, frames - width];
    offsets.sort_unstable();
    offsets.dedup();
    let mut results = Vec::new();
    for offset in offsets {
        for channel in 0..ch {
            let mut best = (-1.0_f64, 0_i32);
            for lag in -32_i32..=32 {
                let origin = start as i64 + offset as i64 + i64::from(lag);
                if origin < 0 || origin as usize + width > reference.len() / ch {
                    continue;
                }
                let mut dot = 0.0;
                let mut aa = 0.0;
                let mut bb = 0.0;
                for i in 0..width {
                    let a = actual[(offset + i) * ch + channel] as f64 / i32::MAX as f64;
                    let b =
                        reference[(origin as usize + i) * ch + channel] as f64 / i32::MAX as f64;
                    dot += a * b;
                    aa += a * a;
                    bb += b * b;
                }
                let correlation = if aa * bb > 1e-20 {
                    dot / (aa * bb).sqrt()
                } else {
                    0.0
                };
                if correlation > best.0 || (correlation == best.0 && lag.abs() < best.1.abs()) {
                    best = (correlation, lag);
                }
            }
            results.push(Alignment {
                at_frame: start + offset as u64,
                channel: channel as u32,
                best_lag_frames: best.1,
                correlation: best.0,
            });
        }
    }
    results
}

/// Independent waveform comparison on Opus's 48 kHz grid for 96 kHz sources.
/// The reference is a full-source decode at 48 kHz, not an encoded segment.
/// Native frame ordering/counts must still be verified separately. This cannot
/// establish sub-codec-frame accuracy of arbitrary lossy waveform estimates.
pub fn opus_clock_alignment(
    reference_48: &[i32],
    actual_96: &[i32],
    start_96: u64,
    channels: u32,
) -> Result<Vec<Alignment>> {
    let ch = channels as usize;
    ensure!(
        (1..=2).contains(&channels) && actual_96.len().is_multiple_of(ch),
        "invalid PCM layout"
    );
    // Begin on the full source's even native-frame phase. Removing this first
    // diagnostic sample does not alter playback or its admission counts.
    let skip = (start_96 % 2) as usize;
    let frames = actual_96.len() / ch;
    if frames < 256 + skip {
        return Ok(Vec::new());
    }
    let temp = tempfile::NamedTempFile::new()?;
    let mut writer = BufWriter::new(temp.reopen()?);
    for sample in &actual_96[skip * ch..] {
        writer.write_all(&sample.to_le_bytes())?;
    }
    writer.flush()?;
    let expected = converted_frames((frames - skip) as u64, 96000, RATE)?;
    let bytes = run(
        ffmpeg()
            .args([
                "-f",
                "s32le",
                "-ar",
                "96000",
                "-ac",
                &channels.to_string(),
                "-i",
            ])
            .arg(temp.path())
            .args([
                "-af",
                "aresample=48000:resampler=swr:exact_rational=1:async=0:dither_method=none",
                "-f",
                "s32le",
                "-c:a",
                "pcm_s32le",
                "pipe:1",
            ]),
        Duration::from_secs(30),
        expected * u64::from(channels) * 4 + 65536,
    )?;
    ensure!(
        bytes.len() as u64 == expected * u64::from(channels) * 4,
        "wrong diagnostic resampling count"
    );
    let pcm: Vec<i32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| i32::from_le_bytes(*b))
        .collect();
    Ok(alignment(
        reference_48,
        &pcm,
        (start_96 + skip as u64) / 2,
        channels,
    ))
}

#[derive(Serialize)]
pub struct SegmentCheck {
    pub segment: Segment,
    pub warm_prepare_ms: f64,
    pub decoded_frames: u64,
    pub lossless_exact: Option<bool>,
    pub alignment: Vec<Alignment>,
}

#[derive(Serialize)]
pub struct Boundary {
    pub at_frame: u64,
    pub at_seconds: f64,
    pub from: Quality,
    pub to: Quality,
    /// Difference between the switched waveform's step and the reference step.
    /// Useful for listening investigation; not a universal audibility threshold.
    pub additional_step_peak: f64,
}

#[derive(Serialize)]
pub struct Report {
    pub experiment: &'static str,
    pub source: crate::media::SourceInfo,
    pub output_frames: u64,
    pub exact_frame_count: bool,
    pub lossless_sections_exact: bool,
    pub alignment_windows_at_zero_lag: bool,
    pub alignment_evaluable_windows: usize,
    pub alignment_inconclusive_windows: usize,
    pub segments: Vec<SegmentCheck>,
    pub boundaries: Vec<Boundary>,
    pub scope: Vec<&'static str>,
}

pub fn demo(source: Option<&Path>, output: &Path) -> Result<Report> {
    fs::create_dir_all(output)?;
    let generated = output.join("source.flac");
    let source_path = match source {
        Some(path) => path,
        None => {
            fixture(&generated, 12 * RATE + 337, 2, 24)?;
            &generated
        }
    };
    let source = Source::open(source_path)?;
    let rate = source.info.sample_rate;
    for name in ["reference.wav", "switched.wav", "report.json"] {
        let destination = output.join(name);
        ensure!(destination.canonicalize().ok().as_deref() != Some(source.path()),
            "demo output would overwrite the source file: {name}; choose a different --out directory");
    }
    ensure!(
        source.info.frames >= 6 * rate,
        "demo needs at least six seconds of audio"
    );
    ensure!(
        source.info.frames <= 180 * rate,
        "demo is bounded to three minutes; use segment for longer sources"
    );
    let reference = decode(source.path(), source.info.channels, source.info.frames)?;
    let engine = Engine::new(source, &output.join("cache"))?;
    let channels = engine.source.info.channels;
    let total = engine.source.info.frames;
    let mut rendered = Vec::with_capacity(reference.len());
    let mut checks = Vec::new();
    let mut start = 0;
    while start < total {
        // Deliberately not an Opus packet boundary: tests frame trimming.
        let frames = (2 * rate + 1).min(total - start);
        let quality = if start < total / 3 || start >= 2 * total / 3 {
            Quality::Lossless
        } else if checks.len() % 2 == 0 {
            Quality::Opus128
        } else {
            Quality::Opus64
        };
        let segment = engine.segment(start, frames, quality)?;
        let warm = engine.segment(start, frames, quality)?;
        ensure!(warm.cache_hit, "second request did not use the cache");
        let pcm = presentation_pcm(&segment)?;
        let a = (start * u64::from(channels)) as usize;
        let exact = (quality == Quality::Lossless).then(|| pcm == reference[a..a + pcm.len()]);
        let checks_alignment = alignment_at_rate(&reference, &pcm, start, channels, rate);
        rendered.extend_from_slice(&pcm);
        checks.push(SegmentCheck {
            segment,
            warm_prepare_ms: warm.prepare_ms,
            decoded_frames: pcm.len() as u64 / u64::from(channels),
            lossless_exact: exact,
            alignment: checks_alignment,
        });
        start += frames;
    }
    let mut boundaries = Vec::new();
    for pair in checks.windows(2) {
        let at = pair[1].segment.metadata.presentation.start;
        let mut peak = 0.0_f64;
        for channel in 0..channels as usize {
            let i = at as usize * channels as usize + channel;
            let prev = i - channels as usize;
            let difference = (rendered[i] as f64 - rendered[prev] as f64)
                - (reference[i] as f64 - reference[prev] as f64);
            peak = peak.max(difference.abs() / i32::MAX as f64);
        }
        boundaries.push(Boundary {
            at_frame: at,
            at_seconds: at as f64 / rate as f64,
            from: pair[0].segment.metadata.quality,
            to: pair[1].segment.metadata.quality,
            additional_step_peak: peak,
        });
    }
    let evaluable = checks
        .iter()
        .flat_map(|c| &c.alignment)
        .filter(|a| a.correlation > 0.5)
        .count();
    let inconclusive = checks.iter().flat_map(|c| &c.alignment).count() - evaluable;
    let report = Report {
        experiment: crate::media::EXPERIMENT, source: engine.source.info.clone(),
        output_frames: rendered.len() as u64 / u64::from(channels), exact_frame_count: rendered.len() == reference.len(),
        lossless_sections_exact: checks.iter().all(|c| c.lossless_exact != Some(false)),
        alignment_windows_at_zero_lag: evaluable > 0 && checks.iter().flat_map(|c| &c.alignment)
            .filter(|a| a.correlation > 0.5).all(|a| a.best_lag_frames == 0),
        alignment_evaluable_windows: evaluable,
        alignment_inconclusive_windows: inconclusive,
        segments: checks, boundaries,
        scope: vec!["Offline assembly, not real-time playback or a network benchmark.",
            "44.1/48/96 kHz mono/stereo 16/24-bit lossless sources; lossless is never resampled.",
            "Opus uses 48 kHz and explicit conversion back to the native source clock before presentation trimming.",
            "Lossless segments preserve source PCM, not original encoded bytes.",
            "Opus pre-skip is handled by the decoder; context trims are applied afterward.",
            "No crossfade, silence padding, or offset correction is used to hide joins.",
            "Alignment estimates can be ambiguous for silence/periodic music; listening is still needed.",
            "A full source hash and full reference decode are local proof work, not efficient upstream streaming.",
            "No Navidrome, discovery, authentication, HTTP service, or production cache limit yet."],
    };
    write_wav_at_rate(&output.join("reference.wav"), &reference, channels, rate)?;
    write_wav_at_rate(&output.join("switched.wav"), &rendered, channels, rate)?;
    fs::write(
        output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    ensure!(
        report.exact_frame_count && report.lossless_sections_exact,
        "proof failed; inspect report.json"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alignment_detects_an_introduced_one_frame_shift() {
        let mut state = 12345_u32;
        let reference: Vec<i32> = (0..8000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as i32
            })
            .collect();
        let exact = alignment(&reference, &reference[100..4100], 100, 1);
        assert!(exact.iter().all(|a| a.best_lag_frames == 0));
        let shifted = alignment(&reference, &reference[101..4101], 100, 1);
        assert!(shifted.iter().all(|a| a.best_lag_frames == 1));
    }
}
