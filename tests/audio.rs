//! Real FFmpeg integration tests. Missing FFmpeg is a failure, not a silent skip.
use anyhow::{Context, Result};
use std::fs;
use std::time::Duration;
use stratum_poc::{
    media::{decode, hash_file, presentation_pcm, Engine, Quality, Source},
    process::{ffmpeg, run},
    proof::{alignment, demo, fixture, fixture_at_rate, write_wav, write_wav_at_rate},
    timeline::RATE,
};

#[test]
fn adaptive_assembly_preserves_duration_and_measured_alignment() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let report = demo(None, dir.path())?;
    assert!(report.exact_frame_count);
    assert!(report.lossless_sections_exact);
    assert!(report.alignment_windows_at_zero_lag);
    assert_eq!(report.alignment_inconclusive_windows, 0);
    assert!(report
        .segments
        .iter()
        .any(|c| c.segment.metadata.quality == Quality::Opus64));
    assert!(report
        .segments
        .iter()
        .any(|c| c.segment.metadata.quality == Quality::Opus128));
    for check in &report.segments {
        for a in &check.alignment {
            assert!(a.correlation > 0.8, "{a:?}");
        }
    }
    Ok(())
}

#[test]
fn native_44100_switches_keep_pcm_and_duration() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("native.flac");
    fixture_at_rate(&path, 18 * 44100 + 317, 2, 24, 44100)?;
    let report = demo(Some(&path), &dir.path().join("demo"))?;
    assert_eq!(report.source.sample_rate, 44100);
    assert!(report.exact_frame_count && report.lossless_sections_exact);
    assert!(report.alignment_windows_at_zero_lag);
    assert_eq!(report.alignment_inconclusive_windows, 0);
    assert_eq!(
        Source::open(&dir.path().join("demo/switched.wav"))?
            .info
            .sample_rate,
        44100
    );
    for check in report.segments {
        let m = check.segment.metadata;
        assert_eq!(m.sample_rate, 44100);
        assert_eq!(
            m.decoded_frames,
            m.encoded_interval.frames + m.resample_tail_frames
        );
        assert_eq!(
            m.discard_start_frames + m.presentation.frames + m.discard_end_frames,
            m.decoded_frames
        );
        if m.quality == Quality::Lossless {
            assert_eq!(m.codec_sample_rate, 44100);
            assert_eq!(m.resample_tail_frames, 0);
            assert!(m.source_pcm_preserved);
        } else {
            assert_eq!(m.codec_sample_rate, 48000);
            assert!(m.resample_tail_frames <= 1);
            assert_eq!(m.encoded_interval.start % 147, 0);
            assert!(!m.source_pcm_preserved);
        }
    }
    Ok(())
}

#[test]
fn native_44100_seeks_and_odd_tails_preserve_the_source_clock() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("native-long.flac");
    let rate = 44100;
    fixture_at_rate(&path, 154 * rate + 317, 1, 16, rate)?;
    let source = Source::open(&path)?;
    let total = source.info.frames;
    let reference = decode(&path, 1, total)?;
    let engine = Engine::new(source, &dir.path().join("cache"))?;
    for (start, frames) in [
        (0, 1),
        (0, 88201),
        (1, 88201),
        (146, 88201),
        (147, 88201),
        (148, 88201),
        (150 * rate + 37, 88201),
        (total - 317, 317),
        (total - 1, 1),
    ] {
        for quality in [Quality::Lossless, Quality::Opus64, Quality::Opus128] {
            let segment = engine.segment(start, frames, quality).with_context(|| {
                format!("request start={start} frames={frames} quality={quality:?}")
            })?;
            let pcm = presentation_pcm(&segment)?;
            assert_eq!(pcm.len() as u64, frames);
            if quality == Quality::Lossless {
                assert_eq!(pcm, reference[start as usize..(start + frames) as usize]);
            }
            if frames > 2048 {
                for a in alignment(&reference, &pcm, start, 1) {
                    assert_eq!(a.best_lag_frames, 0, "start={start} {quality:?} {a:?}");
                    assert!(a.correlation > 0.8, "{a:?}");
                }
            }
            let warm = engine.segment(start, frames, quality)?;
            assert!(warm.cache_hit);
            assert_eq!(warm.metadata, segment.metadata);
        }
    }
    // Corrupt new rate metadata; a warm cache may not silently change clocks.
    let segment = engine.segment(148, 88201, Quality::Opus64)?;
    let manifest = segment.path.with_extension("json");
    let mut metadata: serde_json::Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    metadata["codec_sample_rate"] = 44100.into();
    fs::write(manifest, serde_json::to_vec(&metadata)?)?;
    assert!(!engine.segment(148, 88201, Quality::Opus64)?.cache_hit);
    Ok(())
}

#[test]
fn native_44100_short_wav_preserves_real_frames_without_padding() -> Result<()> {
    let dir = tempfile::tempdir()?;
    for frames in [16, 31, 32, 33, 146, 147, 148] {
        let path = dir.path().join(format!("short-{frames}.wav"));
        let reference: Vec<i32> = (0..frames).map(|i| (i * 123456) << 8).collect();
        write_wav_at_rate(&path, &reference, 1, 44100)?;
        let engine = Engine::new(Source::open(&path)?, &dir.path().join("cache"))?;
        let tail = engine.segment(frames as u64 - 1, 1, Quality::Lossless)?;
        assert_eq!(presentation_pcm(&tail)?, reference[frames as usize - 1..]);
        for quality in [Quality::Opus64, Quality::Opus128] {
            let segment = engine.segment(0, frames as u64, quality);
            if frames < 32 {
                assert!(segment
                    .unwrap_err()
                    .to_string()
                    .contains("32 real source frames"));
            } else {
                assert_eq!(presentation_pcm(&segment?)?.len(), frames as usize);
            }
        }
    }
    Ok(())
}

#[test]
fn wav_input_is_lossless_and_demo_cannot_overwrite_its_source() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let flac = dir.path().join("source.flac");
    fixture(&flac, 6 * RATE, 1, 24)?;
    let reference = decode(&flac, 1, 6 * RATE)?;
    let wav = dir.path().join("reference.wav");
    write_wav(&wav, &reference, 1)?;
    let source = Source::open(&wav)?;
    let engine = Engine::new(source, &dir.path().join("cache"))?;
    let segment = engine.segment(137, 96001, Quality::Lossless)?;
    assert_eq!(presentation_pcm(&segment)?, reference[137..96138]);
    let before = hash_file(&wav)?;
    assert!(demo(Some(&wav), dir.path())
        .err()
        .expect("source collision must fail")
        .to_string()
        .contains("overwrite"));
    assert_eq!(hash_file(&wav)?, before);
    Ok(())
}

#[test]
fn unsupported_sample_rate_is_rejected_instead_of_silently_resampled() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("96000.flac");
    run(
        ffmpeg()
            .args([
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=997:sample_rate=96000:duration=0.1",
                "-c:a",
                "flac",
            ])
            .arg(&path),
        Duration::from_secs(10),
        1_048_576,
    )?;
    let error = Source::open(&path).err().expect("96 kHz must not pass yet");
    assert!(error.to_string().contains("no silent resampling"));
    Ok(())
}

#[test]
fn late_seek_edges_and_short_final_segments_match_the_reference() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("long.flac");
    fixture(&path, 154 * RATE + 317, 1, 16)?;
    let source = Source::open(&path)?;
    let total = source.info.frames;
    let reference = decode(&path, 1, total)?;
    let engine = Engine::new(source, &dir.path().join("cache"))?;
    for (start, frames) in [
        (0, 1),
        (0, 96001),
        (150 * RATE + 37, 96001),
        (total - 317, 317),
        (total - 1, 1),
    ] {
        for quality in [Quality::Lossless, Quality::Opus128] {
            let segment = engine.segment(start, frames, quality)?;
            let pcm = presentation_pcm(&segment)?;
            assert_eq!(pcm.len() as u64, frames);
            if quality == Quality::Lossless {
                assert_eq!(pcm, reference[start as usize..(start + frames) as usize]);
            }
            if frames > 2048 {
                for a in alignment(&reference, &pcm, start, 1) {
                    assert_eq!(a.best_lag_frames, 0, "{a:?}");
                    assert!(a.correlation > 0.8, "{a:?}");
                }
            }
        }
    }
    Ok(())
}

#[test]
fn cache_keys_include_quality_interval_and_source_and_repair_corruption() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("source.flac");
    fixture(&path, 3 * RATE, 2, 24)?;
    let engine = Engine::new(Source::open(&path)?, &dir.path().join("cache"))?;
    let first = engine.segment(13, RATE, Quality::Opus128)?;
    assert!(!first.cache_hit);
    let second = engine.segment(13, RATE, Quality::Opus128)?;
    assert!(second.cache_hit);
    assert_eq!(first.metadata, second.metadata);
    let shifted = engine.segment(14, RATE, Quality::Opus128)?;
    let reduced = engine.segment(13, RATE, Quality::Opus64)?;
    assert_ne!(first.path, shifted.path);
    assert_ne!(first.path, reduced.path);
    fs::write(&first.path, b"truncated cache entry")?;
    assert!(presentation_pcm(&first).is_err());
    let repaired = engine.segment(13, RATE, Quality::Opus128)?;
    assert!(!repaired.cache_hit);
    assert_eq!(presentation_pcm(&repaired)?.len(), RATE as usize * 2);
    fixture(&path, 3 * RATE + 1, 2, 24)?;
    assert!(engine.segment(13, RATE, Quality::Opus128).is_err());
    let changed = Engine::new(Source::open(&path)?, &dir.path().join("cache"))?;
    assert_ne!(
        first.path,
        changed.segment(13, RATE, Quality::Opus128)?.path
    );
    Ok(())
}
