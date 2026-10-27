//! Private subprocess entry point. Receives only generated local paths, never credentials.
use super::*;
use crate::media::{Engine, Source};
use anyhow::{ensure, Context};
use std::{fs, process::Command};

pub fn worker_main() {
    // The service keeps this stdin pipe open. EOF after a hard parent crash
    // must kill FFmpeg descendants too, before a new service reclaims staging.
    #[cfg(unix)]
    std::thread::spawn(|| {
        use std::io::Read;
        let mut byte = [0u8; 1];
        loop {
            match std::io::stdin().read(&mut byte) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                _ => break,
            }
        }
        unsafe {
            if libc::getpgrp() == libc::getpid() {
                libc::kill(-libc::getpgrp(), libc::SIGKILL);
            }
        }
        std::process::exit(1);
    });
    // The worker and its FFmpeg children inherit a per-file write ceiling. Parent
    // supervision adds a deadline, bounded concurrency and process-group teardown.
    #[cfg(unix)]
    unsafe {
        let limit = libc::rlimit {
            rlim_cur: MAX_PAYLOAD_BYTES as _,
            rlim_max: MAX_PAYLOAD_BYTES as _,
        };
        if libc::setrlimit(libc::RLIMIT_FSIZE, &limit) != 0 {
            std::process::exit(1);
        }
    }
    match work() {
        Ok(bytes) => {
            use std::io::Write;
            if std::io::stdout().write_all(&bytes).is_err() {
                std::process::exit(1);
            }
        }
        Err(WorkerFailure::Unsupported) => std::process::exit(2),
        Err(WorkerFailure::Internal) => std::process::exit(1),
    }
}
enum WorkerFailure {
    Unsupported,
    Internal,
}
impl From<anyhow::Error> for WorkerFailure {
    fn from(_: anyhow::Error) -> Self {
        Self::Internal
    }
}
fn work() -> Result<Vec<u8>, WorkerFailure> {
    let directory = std::env::args_os()
        .nth(2)
        .map(PathBuf::from)
        .ok_or(WorkerFailure::Internal)?;
    let _lease = crate::resources::working::worker_lease(&directory)?;
    let job: Job =
        serde_json::from_slice(&fs::read(directory.join("job.json")).map_err(anyhow::Error::from)?)
            .map_err(anyhow::Error::from)?;
    if let Some(request) = job.segment {
        if job.source_sha256.is_some() {
            return Err(WorkerFailure::Internal);
        }
        let prepared = job.prepared.ok_or(WorkerFailure::Internal)?;
        let source = Source::resume(
            &directory.join("source"),
            prepared.source,
            prepared.source_stamp,
        )?;
        if prepared.encoder != prepared.prepared_encoder.identity {
            return Err(WorkerFailure::Internal);
        }
        let cache = directory.join("cache");
        // Retain at most the last requested segment. Preserve a true warm hit,
        // clear before preparing a different interval/quality; never an unbounded cache.
        let key = serde_json::to_vec(&request).map_err(anyhow::Error::from)?;
        if fs::read(directory.join("last-request")).ok().as_ref() != Some(&key) {
            if cache.exists() {
                fs::remove_dir_all(&cache).map_err(anyhow::Error::from)?;
            }
            let _ = fs::remove_file(directory.join("payload"));
        }
        if cache.exists() {
            // A killed worker cannot run TempDir destructors. Remove its staging
            // directories before a warm retry; completed entries are plain files.
            for entry in fs::read_dir(&cache).map_err(anyhow::Error::from)? {
                let entry = entry.map_err(anyhow::Error::from)?;
                if entry.file_type().map_err(anyhow::Error::from)?.is_dir() {
                    fs::remove_dir_all(entry.path()).map_err(anyhow::Error::from)?;
                }
            }
        }
        let engine = Engine::prepared(source, &cache, prepared.prepared_encoder)?;
        let segment = engine.segment(request.start, request.frames, request.quality)?;
        if segment.metadata.payload_bytes > MAX_PAYLOAD_BYTES as u64 {
            return Err(WorkerFailure::Internal);
        }
        fs::copy(&segment.path, directory.join("payload")).map_err(anyhow::Error::from)?;
        fs::write(directory.join("last-request"), key).map_err(anyhow::Error::from)?;
        let mut value = serde_json::to_value(&segment.metadata).map_err(anyhow::Error::from)?;
        value["cache_hit"] = json!(segment.cache_hit);
        Ok(serde_json::to_vec(&value).map_err(anyhow::Error::from)?)
    } else {
        if job.prepared.is_some() {
            return Err(WorkerFailure::Internal);
        }
        let source = Source::acquired(
            &directory.join("source"),
            job.source_sha256.ok_or(WorkerFailure::Internal)?,
        )
        .map_err(|_| WorkerFailure::Unsupported)?;
        // Bound compressed inputs that declare many hours despite a small file size.
        if source.info.frames > source.info.sample_rate * MAX_SOURCE_SECONDS {
            return Err(WorkerFailure::Unsupported);
        }
        let prepared_encoder = PreparedEncoder::capture()?;
        validate_start(&source, &prepared_encoder)?;
        prepared_encoder.check()?;
        Ok(serde_json::to_vec(&Prepared {
            encoder: prepared_encoder.identity.clone(),
            prepared_encoder,
            source_stamp: source.preparation_stamp()?,
            source: source.info,
        })
        .map_err(anyhow::Error::from)?)
    }
}
fn validate_start(source: &Source, encoder: &PreparedEncoder) -> anyhow::Result<()> {
    // Startup work is independent of track duration. Every later encoding uses
    // -xerror and verifies its exact decoded frame count; a damaged later interval
    // fails then rather than silently skipping, padding, or reporting EOF.
    let expected = source
        .info
        .frames
        .min(source.info.sample_rate * MAX_SEGMENT_SECONDS);
    let output = crate::process::run(
        Command::new(encoder.path())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-xerror",
                "-threads",
                "1",
                "-protocol_whitelist",
                "file,pipe",
                "-i",
            ])
            .arg(source.path())
            .args([
                "-map",
                "0:a:0",
                "-vn",
                "-sn",
                "-dn",
                "-af",
                &format!("atrim=end_sample={expected}"),
                "-c:a",
                "pcm_s32le",
                "-f",
                "framehash",
                "pipe:1",
            ]),
        WORKER_LIMIT,
        MAX_PAYLOAD_BYTES as u64,
    )?;
    let text = std::str::from_utf8(&output)?;
    ensure!(
        text.lines()
            .any(|line| line == format!("#tb 0: 1/{}", source.info.sample_rate)),
        "unexpected decode clock"
    );
    let mut total = 0_u64;
    for line in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        ensure!(
            fields.len() == 6 && fields[0] == "0",
            "invalid decoded frame record"
        );
        let duration: u64 = fields[3].parse()?;
        let bytes: u64 = fields[4].parse()?;
        ensure!(
            bytes == duration * u64::from(source.info.channels) * 4,
            "decoded frame mismatch"
        );
        total = total
            .checked_add(duration)
            .context("decode duration overflow")?;
    }
    ensure!(total == expected, "incomplete source decode");
    Ok(())
}
