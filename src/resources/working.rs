//! Installation-owned staging and conservative space reservations.
use super::{Settings, FREE_RESERVE, SCRATCH_BYTES};
use crate::cache::Cache;
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct State {
    limit: u64,
    reserved: u64,
}
pub(crate) struct Working {
    pub root: Arc<TempDir>,
    _lease: File,
    state: Mutex<State>,
    cache: Arc<Cache>,
    same_volume: bool,
}
pub(crate) struct Reservation {
    working: Arc<Working>,
    bytes: u64,
}
#[derive(Serialize)]
pub(crate) struct Stats {
    pub limit_bytes: u64,
    pub reserved_bytes: u64,
    pub free_reserve_bytes: u64,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.working.state.lock().unwrap();
        state.reserved -= self.bytes;
        if self.working.same_volume {
            self.working.cache.set_working_reserve(state.reserved);
        }
    }
}
impl Working {
    /// Caller holds serve.lock. A worker from a crashed parent keeps its run's
    /// shared lease until its parent-death watchdog has killed the process group.
    pub fn open(path: &Path, cache: Arc<Cache>, settings: &Settings) -> Result<Arc<Self>> {
        crate::cache::prepare_directory(path)?;
        clean_old_runs(path)?;
        let root = Arc::new(tempfile::Builder::new().prefix("run-").tempdir_in(path)?);
        let lease = lease_file(&root.path().join(".lease"), true)?;
        FileExt::lock_shared(&lease)?;
        let same_volume = same_volume(path, &cache.directory())?;
        Ok(Arc::new(Self {
            root,
            _lease: lease,
            state: Mutex::new(State {
                limit: settings.working_bytes,
                reserved: 0,
            }),
            cache,
            same_volume,
        }))
    }
    pub fn set_limit(&self, limit: u64) {
        self.state.lock().unwrap().limit = limit;
    }
    pub fn stats(&self) -> Stats {
        let state = self.state.lock().unwrap();
        Stats {
            limit_bytes: state.limit,
            reserved_bytes: state.reserved,
            free_reserve_bytes: FREE_RESERVE,
        }
    }
    pub fn reserve(self: &Arc<Self>, source_bytes: u64) -> Result<Reservation> {
        self.reserve_with_space(source_bytes, || fs2::available_space(self.root.path()))
    }
    fn reserve_with_space(
        self: &Arc<Self>,
        source_bytes: u64,
        mut free: impl FnMut() -> std::io::Result<u64>,
    ) -> Result<Reservation> {
        let amount = source_bytes
            .checked_add(SCRATCH_BYTES)
            .context("Working-storage request is too large.")?;
        let mut state = self.state.lock().unwrap();
        let reserved = state
            .reserved
            .checked_add(amount)
            .context("Working-storage limit reached.")?;
        ensure!(reserved <= state.limit, "Working-storage limit reached.");
        // Use the complete reservation even if some of it is already on disk.
        // This is conservative and covers in-flight writes and unlinked temp files.
        if self.same_volume {
            self.cache.set_working_reserve(reserved);
        }
        let result = (|| {
            let available = free()?;
            if available < FREE_RESERVE.saturating_add(reserved) && self.same_volume {
                self.cache
                    .reclaim_bytes(FREE_RESERVE.saturating_add(reserved) - available)?;
            }
            ensure!(
                free()? >= FREE_RESERVE.saturating_add(reserved),
                "Not enough free working storage."
            );
            Ok(())
        })();
        if let Err(error) = result {
            if self.same_volume {
                self.cache.set_working_reserve(state.reserved);
            }
            return Err(error);
        }
        state.reserved = reserved;
        Ok(Reservation {
            working: self.clone(),
            bytes: amount,
        })
    }
}
fn same_volume(a: &Path, b: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(fs::metadata(a)?.dev() == fs::metadata(b)?.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = (a, b);
        Ok(false)
    }
}
fn lease_file(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "Invalid working-directory lease."
    );
    Ok(file)
}
pub(crate) fn worker_lease(directory: &Path) -> Result<Option<File>> {
    let parent = directory.parent().context("Missing worker directory.")?;
    let path = parent.join(".lease");
    // The loopback proof uses an ephemeral root, without installation recovery.
    if !path.try_exists()? {
        return Ok(None);
    }
    let file = lease_file(&path, false)?;
    FileExt::lock_shared(&file)?;
    Ok(Some(file))
}
fn clean_old_runs(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name
            .strip_prefix("run-")
            .is_some_and(|s| s.len() == 6 && s.bytes().all(|c| c.is_ascii_alphanumeric()))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "Unexpected entry in working storage."
        );
        let lease = entry.path().join(".lease");
        if !lease.try_exists()? {
            // The parent may have crashed immediately after mkdir, before its
            // lease was created. Only an empty directory is safe in that case.
            fs::remove_dir(entry.path()).context("Unrecognized files in working storage.")?;
            continue;
        }
        let lease = lease_file(&lease, false)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match FileExt::try_lock_exclusive(&lease) {
                Ok(()) => break,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => {
                    return Err(error).context(
                        "A previous audio worker is still stopping. Retry starting Sidecar.",
                    )
                }
            }
        }
        // std's recursive deletion does not follow symlinks inside this directory.
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Arc<Working>) {
        let root = tempfile::tempdir().unwrap();
        let cache =
            Arc::new(Cache::open(&root.path().join("retained"), &Default::default()).unwrap());
        let working =
            Working::open(&root.path().join("working"), cache, &Settings::default()).unwrap();
        (root, working)
    }
    #[test]
    fn reservations_live_until_release_and_reductions_do_not_evict_active_work() {
        let (_root, working) = fixture();
        let large = 64 * 1024 * 1024;
        let first = working.reserve_with_space(large, || Ok(u64::MAX)).unwrap();
        assert_eq!(working.stats().reserved_bytes, large + SCRATCH_BYTES);
        working.set_limit(128 * super::super::MB);
        assert!(working.reserve_with_space(large, || Ok(u64::MAX)).is_err());
        assert_eq!(working.stats().reserved_bytes, large + SCRATCH_BYTES);
        drop(first);
        assert!(working.reserve_with_space(large, || Ok(0)).is_err());
        assert_eq!(working.stats().reserved_bytes, 0);
        assert!(working.reserve_with_space(large, || Ok(u64::MAX)).is_ok());
    }
    #[test]
    fn startup_removes_only_owned_stale_runs_and_leaves_external_files_alone() {
        let (root, working) = fixture();
        let path = root.path().join("working");
        let old = working.root.path().to_owned();
        let cache = working.cache.clone();
        // Simulate a parent killed without TempDir cleanup.
        let saved = working.root.clone();
        drop(working);
        let saved = Arc::try_unwrap(saved).unwrap().keep();
        fs::write(saved.join("source"), b"stale").unwrap();
        let outside = root.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, saved.join("link")).unwrap();
        fs::create_dir(path.join("unrelated")).unwrap();
        let _fresh = Working::open(&path, cache, &Settings::default()).unwrap();
        assert!(!old.exists());
        assert!(path.join("unrelated").exists());
        assert_eq!(fs::read(outside).unwrap(), b"keep");
    }
    #[test]
    fn low_space_reclaims_retained_entries_before_refusing_new_work() {
        let (_root, working) = fixture();
        let key = "a".repeat(64);
        let ticket = working.cache.lookup(&key).unwrap().ticket;
        assert!(working.cache.insert(&key, &[1; 2048], &ticket).unwrap());
        let required = FREE_RESERVE + SCRATCH_BYTES + 100;
        let mut readings = [required - 1000, required].into_iter();
        let reservation = working
            .reserve_with_space(100, || Ok(readings.next().unwrap()))
            .unwrap();
        assert_eq!(working.cache.stats().entries, 0);
        drop(reservation);
        assert!(working.reserve_with_space(100, || Ok(0)).is_err());
        assert_eq!(working.stats().reserved_bytes, 0);
    }
}
