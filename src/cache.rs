//! Private retained encoded packets. Callers supply content-derived keys and
//! authorize listeners independently; cache possession grants no media access.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

pub const GB: u64 = 1_000_000_000;
pub const MAX_PACKET: usize = 4 * 1024 * 1024 + 65540;
const MAGIC: &[u8; 8] = b"STRMC001";
const HEADER: usize = 40;
// Retention yields space before consuming the last working-space reserve.
const FREE_RESERVE: u64 = 512 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub revision: String,
    pub enabled: bool,
    pub limit_bytes: u64,
    pub clear_epoch: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            revision: "initial".into(),
            enabled: true,
            limit_bytes: 5 * GB,
            clear_epoch: "initial".into(),
        }
    }
}
fn revision(value: &str) -> bool {
    value == "initial" || key(value)
}
fn key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            revision(&self.revision) && revision(&self.clear_epoch),
            "Invalid cache revision."
        );
        // JSON/JavaScript exact integer ceiling, not an arbitrary storage tier.
        ensure!(
            (GB..=9_007_199_254_740_991).contains(&self.limit_bytes),
            "Use a cache limit of at least 1 GB, within the supported numeric range."
        );
        Ok(())
    }
}

/// One owner-authorized change; clear/disable require explicit confirmation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub revision: String,
    pub enabled: bool,
    pub limit_bytes: u64,
    #[serde(default)]
    pub clear: bool,
    #[serde(default)]
    pub confirmed: bool,
}

#[derive(Serialize)]
pub struct Stats {
    pub settings: Settings,
    pub retained_bytes: u64,
    pub entries: usize,
    pub problem: bool,
}
#[derive(Clone)]
pub struct Ticket(String);
pub struct Lookup {
    pub ticket: Ticket,
    pub bytes: Option<Vec<u8>>,
}
struct Entry {
    bytes: u64,
    used: u64,
}
struct State {
    settings: Settings,
    entries: HashMap<String, Entry>,
    bytes: u64,
    clock: u64,
    problem: bool,
}
pub struct Cache {
    directory: PathBuf,
    state: Mutex<State>,
}

impl Cache {
    /// The caller owns the installation's serve.lock (including offline CLI
    /// maintenance). One process owns this cache/index at a time.
    pub fn open(directory: &Path, settings: &Settings) -> Result<Self> {
        settings.validate()?;
        prepare_directory(directory)?;
        let mut files = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(".write-") && !entry.file_type()?.is_dir() {
                fs::remove_file(entry.path())?;
            } else if let Some(id) = name.strip_suffix(".segment").filter(|id| key(id)) {
                let metadata = fs::symlink_metadata(entry.path())?;
                if !metadata.is_file()
                    || metadata.len() <= HEADER as u64
                    || metadata.len() > (MAX_PACKET + HEADER) as u64
                {
                    // Unlink a malformed managed file, never follow a symlink.
                    if !metadata.is_dir() {
                        fs::remove_file(entry.path())?;
                    }
                    continue;
                }
                files.push((
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    id.to_owned(),
                    metadata.len(),
                ));
            }
        }
        files.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut state = State {
            settings: settings.clone(),
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            problem: false,
        };
        for (_, id, bytes) in files {
            state.clock += 1;
            state.bytes += bytes;
            state.entries.insert(
                id,
                Entry {
                    bytes,
                    used: state.clock,
                },
            );
        }
        let cache = Self {
            directory: directory.to_owned(),
            state: Mutex::new(state),
        };
        // A journaled clear/disable survives a crash before deletion finishes.
        let recorded = read_regular(&directory.join(".epoch"), 128).ok();
        let mut state = cache.state.lock().unwrap();
        if recorded.as_deref() != Some(settings.clear_epoch.as_bytes()) || !settings.enabled {
            cache.clear(&mut state)?;
            cache.write_epoch(&settings.clear_epoch)?;
        }
        cache.evict(&mut state, 0)?;
        drop(state);
        Ok(cache)
    }

    pub fn apply(&self, settings: &Settings) -> Result<()> {
        settings.validate()?;
        let mut state = self.state.lock().unwrap();
        if &state.settings == settings && !state.problem {
            return Ok(());
        }
        let result = (|| {
            if state.settings.clear_epoch != settings.clear_epoch || !settings.enabled {
                self.clear(&mut state)?;
                self.write_epoch(&settings.clear_epoch)?;
            }
            // If eviction fails, keep the old revision so the next refresh retries.
            let previous = state.settings.clone();
            state.settings = settings.clone();
            if let Err(error) = self.evict(&mut state, 0) {
                state.settings = previous;
                return Err(error);
            }
            Ok(())
        })();
        state.problem = result.is_err();
        result
    }
    pub fn stats(&self) -> Stats {
        let state = self.state.lock().unwrap();
        Stats {
            settings: state.settings.clone(),
            retained_bytes: state.bytes,
            entries: state.entries.len(),
            problem: state.problem,
        }
    }
    pub fn lookup(&self, id: &str) -> Result<Lookup> {
        ensure!(key(id), "Invalid cache key.");
        let mut state = self.state.lock().unwrap();
        let ticket = Ticket(state.settings.revision.clone());
        if state.problem || !state.settings.enabled || !state.entries.contains_key(id) {
            return Ok(Lookup {
                ticket,
                bytes: None,
            });
        }
        let path = self.path(id);
        let bytes = read_regular(&path, MAX_PACKET + HEADER)
            .ok()
            .filter(|bytes| {
                bytes.len() > HEADER
                    && bytes.starts_with(MAGIC)
                    && bytes[8..HEADER] == checksum(id, &bytes[HEADER..])
            });
        let Some(bytes) = bytes else {
            self.remove(&mut state, id)?;
            return Ok(Lookup {
                ticket,
                bytes: None,
            });
        };
        state.clock += 1;
        let used = state.clock;
        state.entries.get_mut(id).unwrap().used = used;
        // Persist approximate LRU order across restarts. Failure only loses this
        // recency hint; the verified packet remains usable.
        if let Ok(file) = open_regular(&path) {
            let _ = file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()));
        }
        Ok(Lookup {
            ticket,
            bytes: Some(bytes[HEADER..].to_vec()),
        })
    }
    pub fn invalidate(&self, id: &str) -> Result<()> {
        ensure!(key(id), "Invalid cache key.");
        self.remove(&mut self.state.lock().unwrap(), id)
    }
    /// A late encoder may finish after Clear/Disable. Its old ticket must not
    /// refill the cache, even if retention was enabled again in the meantime.
    pub fn insert(&self, id: &str, packet: &[u8], ticket: &Ticket) -> Result<bool> {
        self.insert_with_space(id, packet, ticket, || fs2::available_space(&self.directory))
    }
    fn insert_with_space(
        &self,
        id: &str,
        packet: &[u8],
        ticket: &Ticket,
        mut available_space: impl FnMut() -> std::io::Result<u64>,
    ) -> Result<bool> {
        ensure!(
            key(id) && !packet.is_empty() && packet.len() <= MAX_PACKET,
            "Invalid cache entry."
        );
        let mut state = self.state.lock().unwrap();
        if state.problem || !state.settings.enabled || state.settings.revision != ticket.0 {
            return Ok(false);
        }
        if state.entries.contains_key(id) {
            return Ok(true);
        }
        let size = (packet.len() + HEADER) as u64;
        if size > state.settings.limit_bytes {
            return Ok(false);
        }
        self.evict(&mut state, size)?;
        // Full disks must not turn successful audio preparation into a failure.
        // Reclaim retained audio first; the active playback owns separate files.
        let available = available_space()?;
        if available < FREE_RESERVE + size {
            let target = available
                .saturating_add(state.bytes)
                .saturating_sub(FREE_RESERVE + size);
            self.evict_to(&mut state, target)?;
            if available_space()? < FREE_RESERVE + size {
                return Ok(false);
            }
        }
        let mut temp = tempfile::Builder::new()
            .prefix(".write-")
            .tempfile_in(&self.directory)?;
        temp.write_all(MAGIC)?;
        temp.write_all(&checksum(id, packet))?;
        temp.write_all(packet)?;
        temp.as_file().sync_all()?;
        temp.persist(self.path(id))
            .map_err(|_| anyhow::anyhow!("Could not retain encoded audio."))?;
        state.clock += 1;
        let used = state.clock;
        state
            .entries
            .insert(id.to_owned(), Entry { bytes: size, used });
        state.bytes += size;
        Ok(true)
    }
    fn path(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.segment"))
    }
    fn write_epoch(&self, epoch: &str) -> Result<()> {
        let mut temp = tempfile::Builder::new()
            .prefix(".write-")
            .tempfile_in(&self.directory)?;
        temp.write_all(epoch.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(self.directory.join(".epoch"))
            .map_err(|_| anyhow::anyhow!("Could not save cache clear state."))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    fn clear(&self, state: &mut State) -> Result<()> {
        self.evict_to(state, 0)
    }
    fn evict(&self, state: &mut State, reserve: u64) -> Result<()> {
        self.evict_to(state, state.settings.limit_bytes.saturating_sub(reserve))
    }
    fn evict_to(&self, state: &mut State, target: u64) -> Result<()> {
        if state.bytes <= target {
            return Ok(());
        }
        let mut oldest: Vec<_> = state
            .entries
            .iter()
            .map(|(id, entry)| (entry.used, id.clone()))
            .collect();
        oldest.sort();
        for (_, id) in oldest {
            self.remove(state, &id)?;
            if state.bytes <= target {
                break;
            }
        }
        Ok(())
    }
    fn remove(&self, state: &mut State, id: &str) -> Result<()> {
        match fs::remove_file(self.path(id)) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        if let Some(entry) = state.entries.remove(id) {
            state.bytes -= entry.bytes;
        }
        Ok(())
    }
}
fn checksum(id: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(id.as_bytes());
    hash.update(bytes);
    hash.finalize().into()
}
fn prepare_directory(path: &Path) -> Result<()> {
    if !path.try_exists()? {
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Cache must be a private directory."
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o077 == 0,
            "Cache directory must be owned by this account with mode 700."
        );
    }
    Ok(())
}
fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    ensure!(
        file.metadata()?.is_file(),
        "Cache entry is not a regular file."
    );
    Ok(file)
}
fn read_regular(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let file = open_regular(path)?;
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "Cache entry is too large."
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .context("Could not read cache entry.")?;
    ensure!(bytes.len() <= limit, "Cache entry grew while reading.");
    Ok(bytes)
}

#[cfg(test)]
mod tests;
