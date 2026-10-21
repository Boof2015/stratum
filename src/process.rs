//! Bounded, direct subprocesses. No shell, player, GUI, or recursive launcher.
use anyhow::{bail, Context, Result};
use std::{
    io::{Read, Seek, SeekFrom},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub fn run(command: &mut Command, timeout: Duration, max_stdout: u64) -> Result<Vec<u8>> {
    // Files avoid pipe deadlocks when FFmpeg writes more than a pipe can hold.
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let program = command.get_program().to_string_lossy().into_owned();
    let mut child = command.spawn().with_context(|| {
        format!("could not start {program}; install FFmpeg and ensure it is on PATH")
    })?;
    let started = Instant::now();
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(anyhow::Error::new(error)),
        }
        let too_large = stdout
            .metadata()
            .map(|m| m.len() > max_stdout)
            .unwrap_or(true)
            || stderr
                .metadata()
                .map(|m| m.len() > 1_048_576)
                .unwrap_or(true);
        if started.elapsed() > timeout || too_large {
            break Err(anyhow::anyhow!(
                "{program} exceeded its time or output limit"
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let status = outcome?;
    if !status.success() {
        stderr.seek(SeekFrom::Start(0))?;
        let mut message = String::new();
        stderr.take(8192).read_to_string(&mut message)?;
        bail!("{program} failed ({status}): {}", message.trim());
    }
    if stdout.metadata()?.len() > max_stdout {
        bail!("{program} exceeded its output limit");
    }
    stdout.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub fn ffmpeg() -> Command {
    ffmpeg_at(std::path::Path::new("ffmpeg"))
}

pub(crate) fn ffmpeg_at(path: &std::path::Path) -> Command {
    let mut command = Command::new(path);
    command.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-threads",
        "1",
        "-filter_threads",
        "1",
    ]);
    command
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn terminates_a_stuck_child() {
        let now = Instant::now();
        let error = run(
            Command::new("sleep").arg("5"),
            Duration::from_millis(30),
            10,
        )
        .unwrap_err();
        assert!(error.to_string().contains("limit"));
        assert!(now.elapsed() < Duration::from_secs(2));
    }
}
