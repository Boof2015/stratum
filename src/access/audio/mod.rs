//! Experimental authenticated segments from a complete, private source copy.
//! This intentionally measures a staging baseline, not a progressive-source policy.
use super::*;
use crate::resources::{
    self,
    scheduler::{Priority, Scheduler},
    working::{Reservation, Working},
};
use crate::{
    media::{Quality, SegmentMetadata, SourceInfo},
    timeline::Interval,
};
use serde::Serialize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{path::PathBuf, process::Stdio};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, sync::OwnedSemaphorePermit};

mod worker;
pub use worker::worker_main;
pub(super) const MAX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const MAX_SEGMENT_SECONDS: u64 = 4;
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const WORKER_LIMIT: Duration = Duration::from_secs(8);

pub(crate) struct Store {
    root: Arc<TempDir>,
    sources: Arc<Semaphore>,
    workers: Scheduler,
    working: Option<Arc<Working>>,
    resource_settings: Mutex<resources::Settings>,
    detected_cpus: AtomicUsize,
    executable: PathBuf,
    retained: Option<Arc<crate::cache::Cache>>,
    cache_jobs: Arc<Semaphore>,
}
impl Store {
    pub(crate) async fn drain(&self) {
        self.workers.drain().await;
    }
    pub(crate) fn new() -> anyhow::Result<Self> {
        Ok(Self {
            root: Arc::new(
                tempfile::Builder::new()
                    .prefix("stratum-audio-")
                    .tempdir()?,
            ),
            sources: Arc::new(Semaphore::new(4)),
            workers: Scheduler::new(2, false),
            working: None,
            resource_settings: Mutex::new(resources::Settings::default()),
            detected_cpus: AtomicUsize::new(4),
            executable: std::env::current_exe()?,
            retained: None,
            cache_jobs: Arc::new(Semaphore::new(2)),
        })
    }
    pub(crate) fn persistent(
        directory: &std::path::Path,
        settings: &crate::cache::Settings,
        working_directory: &std::path::Path,
        resources: &resources::Settings,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new()?;
        let cache = Arc::new(crate::cache::Cache::open(directory, settings)?);
        let working = Working::open(working_directory, cache.clone(), resources)?;
        store.root = working.root.clone();
        store.retained = Some(cache);
        store.working = Some(working);
        let cpus = resources::available_cpus();
        store.workers = Scheduler::new(resources.conversions(cpus), true);
        store.detected_cpus.store(cpus, Ordering::Relaxed);
        *store.resource_settings.lock().unwrap() = resources.clone();
        Ok(store)
    }
    pub(crate) fn apply_resources(&self, settings: &resources::Settings) {
        let mut previous = self.resource_settings.lock().unwrap();
        if *previous == *settings {
            return;
        }
        let cpus = resources::available_cpus();
        self.detected_cpus.store(cpus, Ordering::Relaxed);
        self.workers.set_limit(settings.conversions(cpus));
        if let Some(working) = &self.working {
            working.set_limit(settings.working_bytes);
        }
        *previous = settings.clone();
    }
    pub(crate) fn resource_stats(&self) -> Value {
        json!({"conversions":self.workers.stats(),"detected_cpus":self.detected_cpus.load(Ordering::Relaxed),
               "working":self.working.as_ref().map(|work|work.stats())})
    }
    async fn reserve(&self, size: u64) -> Result<Option<Reservation>, Failure> {
        let Some(working) = self.working.clone() else {
            return Ok(None);
        };
        let permit = self
            .cache_jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            working.reserve(size)
        })
        .await
        .map_err(|_| Failure::Internal)?
        .map(Some)
        .map_err(|_| Failure::Storage)
    }
    pub(crate) fn retained(&self) -> Option<Arc<crate::cache::Cache>> {
        self.retained.clone()
    }
    pub(crate) fn working_bytes(&self) -> anyhow::Result<u64> {
        fn count(path: &std::path::Path, depth: usize) -> anyhow::Result<u64> {
            anyhow::ensure!(depth < 8, "Unexpected working directory depth.");
            let mut bytes = 0;
            for entry in std::fs::read_dir(path)? {
                let entry = entry?;
                let metadata = match std::fs::symlink_metadata(entry.path()) {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error.into()),
                };
                if metadata.file_type().is_symlink() {
                    continue;
                }
                if metadata.is_dir() {
                    // A worker may remove its completed staging directory during the snapshot.
                    match count(&entry.path(), depth + 1) {
                        Ok(size) => bytes += size,
                        Err(error)
                            if error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
                        Err(error) => return Err(error),
                    }
                } else if metadata.is_file() {
                    bytes += metadata.len();
                }
            }
            Ok(bytes)
        }
        count(self.root.path(), 0)
    }
    async fn cache_job<T: Send + 'static>(
        &self,
        work: impl FnOnce(Arc<crate::cache::Cache>) -> anyhow::Result<T> + Send + 'static,
    ) -> Option<T> {
        let cache = self.retained.clone()?;
        let permit = self.cache_jobs.clone().try_acquire_owned().ok()?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work(cache)
        })
        .await
        .ok()?
        .ok()
    }
}

pub(super) struct Playback {
    directory: TempDir,
    // Keep the parent alive until cancellation cleanup has reaped its worker.
    _root: Arc<TempDir>,
    _reservation: Option<Reservation>,
    _global_slot: OwnedSemaphorePermit,
    _session_slot: OwnedSemaphorePermit,
    track: String,
    size: u64,
    pub(super) cancelled: CancellationToken,
    operation: tokio::sync::Mutex<()>,
    worker_busy: Arc<tokio::sync::Mutex<()>>,
    source: Mutex<Option<Prepared>>,
}
impl Drop for Playback {
    fn drop(&mut self) {
        self.cancelled.cancel();
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prepared {
    source: SourceInfo,
    encoder: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Job {
    // None means inspect and validate the complete staged input.
    segment: Option<SegmentRequest>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentRequest {
    start: u64,
    frames: u64,
    quality: Quality,
    #[serde(default, skip_serializing)]
    priority: Priority,
}

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/audio/v1/playbacks/{track}", post(open))
        .route("/audio/v1/playback/{handle}", delete(close))
        .route("/audio/v1/playback/{handle}/segment", post(segment))
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
fn active(session: &Session, playback: &Playback) -> Result<(), Failure> {
    if !session.active() {
        Err(Failure::Session)
    } else if playback.cancelled.is_cancelled() {
        Err(Failure::Playback)
    } else {
        Ok(())
    }
}
fn invalidate(session: &Session, handle: &str) {
    if let Some(playback) = session.playbacks.lock().unwrap().remove(handle) {
        playback.cancelled.cancel();
    }
}
fn final_body(session: Arc<Session>, cancelled: CancellationToken, bytes: Bytes) -> Body {
    // Bound each yielded chunk and recheck liveness when the socket asks for more.
    // Already-delivered/socket-buffered bytes cannot be recalled.
    Body::from_stream(futures_util::stream::unfold(
        (session, cancelled, bytes, 0),
        |(session, cancelled, bytes, offset)| async move {
            if offset == bytes.len() {
                return None;
            }
            if !session.active() || cancelled.is_cancelled() {
                return Some((
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "playback ended",
                    )),
                    (session, cancelled, bytes.clone(), bytes.len()),
                ));
            }
            let end = (offset + 16 * 1024).min(bytes.len());
            Some((
                Ok(bytes.slice(offset..end)),
                (session, cancelled, bytes, end),
            ))
        },
    ))
}
async fn original(
    service: &Service,
    session: &Session,
    playback: &Playback,
) -> Result<(), Failure> {
    let mut response = service
        .upstream(
            "stream",
            &session.credential(),
            &[
                ("id", playback.track.clone()),
                ("format", "raw".into()),
                ("maxBitRate", "0".into()),
            ],
            None,
        )
        .await?;
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if content_type.starts_with("application/json") {
        envelope(read_bounded(response, JSON_LIMIT).await?)?;
        return Err(Failure::Upstream);
    }
    if response.status() != StatusCode::OK
        || !(content_type.starts_with("audio/") || content_type == "application/octet-stream")
        || response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        || response
            .content_length()
            .is_some_and(|n| n != playback.size)
    {
        return Err(Failure::Upstream);
    }
    let path = playback.directory.path().join("source");
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|_| Failure::Internal)?;
    let mut received = 0_u64;
    let mut magic = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Upstream)? {
        active(session, playback)?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or(Failure::TooLarge)?;
        if received > playback.size || received > MAX_SOURCE_BYTES {
            return Err(Failure::Upstream);
        }
        if magic.len() < 12 {
            magic.extend_from_slice(&chunk[..chunk.len().min(12 - magic.len())]);
            if magic.len() == 12 && !supported_magic(&magic) {
                return Err(Failure::Unsupported);
            }
        }
        file.write_all(&chunk)
            .await
            .map_err(|_| Failure::Internal)?;
    }
    if received != playback.size {
        return Err(Failure::Upstream);
    }
    if !supported_magic(&magic) {
        return Err(Failure::Unsupported);
    }
    file.flush().await.map_err(|_| Failure::Internal)?;
    drop(file);
    // Only this service owns the random private directory; encoders read this file.
    let mut permissions = std::fs::metadata(&path)
        .map_err(|_| Failure::Internal)?
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(path, permissions).map_err(|_| Failure::Internal)?;
    Ok(())
}
fn supported_magic(bytes: &[u8]) -> bool {
    bytes.starts_with(b"fLaC") || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE"))
}

async fn open(
    State(service): State<Service>,
    Path(track): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    if !valid_id(&track) {
        return Err(Failure::Input);
    }
    let priority = match headers.get("x-stratum-priority").map(|v| v.to_str()) {
        None | Some(Ok("playback")) => Priority::Playback,
        Some(Ok("prefetch")) => Priority::Prefetch,
        _ => return Err(Failure::Input),
    };
    let (token, session) = service.session(&headers)?;
    let work = async {
        let global = service
            .0
            .audio
            .sources
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let local = session
            .playback_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let size = service.authorize(&session, &track).await?;
        if size > MAX_SOURCE_BYTES {
            return Err(Failure::TooLarge);
        }
        let reservation = service.0.audio.reserve(size).await?;
        let playback = Arc::new(Playback {
            _reservation: reservation,
            directory: tempfile::tempdir_in(service.0.audio.root.path())
                .map_err(|_| Failure::Internal)?,
            _root: service.0.audio.root.clone(),
            _global_slot: global,
            _session_slot: local,
            track: track.clone(),
            size,
            cancelled: session.cancelled.child_token(),
            operation: tokio::sync::Mutex::new(()),
            worker_busy: Arc::new(tokio::sync::Mutex::new(())),
            source: Mutex::new(None),
        });
        // If the handler is dropped (deadline, client disconnect, shutdown), workers
        // see cancellation; their owned Arc keeps files/permits until they are reaped.
        let pending = playback.cancelled.clone().drop_guard();
        let started = Instant::now();
        original(&service, &session, &playback).await?;
        let acquisition_ms = started.elapsed().as_secs_f64() * 1000.0;
        let bytes = worker(&service, playback.clone(), Job { segment: None }, priority).await?;
        let prepared: Prepared = serde_json::from_slice(&bytes).map_err(|_| Failure::Encoding)?;
        let info = &prepared.source;
        if info.bytes != size {
            return Err(Failure::Encoding);
        }
        if service.authorize(&session, &track).await? != size {
            return Err(Failure::Upstream);
        }
        active(&session, &playback)?;
        let handle = random_token()?;
        let value = json!({"playback": handle, "source": info,
            "sourcePolicy": "complete-private-copy", "acquiredBytes": size,
            "acquisitionMs": acquisition_ms, "readyMs": started.elapsed().as_secs_f64()*1000.0,
            "maxSegmentSeconds": MAX_SEGMENT_SECONDS});
        *playback.source.lock().unwrap() = Some(prepared);
        {
            let mut playbacks = session.playbacks.lock().unwrap();
            active(&session, &playback)?;
            playbacks.insert(handle, playback.clone());
        }
        pending.disarm();
        let mut response = Response::new(final_body(
            session.clone(),
            playback.cancelled.clone(),
            serde_json::to_vec(&value)
                .map_err(|_| Failure::Internal)?
                .into(),
        ));
        *response.status_mut() = StatusCode::CREATED;
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        Ok(response)
    };
    let result = tokio::select! {
        biased;
        _ = session.cancelled.cancelled() => Err(Failure::Session),
        _ = session.expired() => Err(Failure::Session),
        result = work => result,
    };
    if matches!(result, Err(Failure::Session)) {
        service.revoke(&token);
    }
    result
}
async fn close(
    State(service): State<Service>,
    Path(handle): Path<String>,
    headers: HeaderMap,
) -> Result<StatusCode, Failure> {
    let (_, session) = service.session(&headers)?;
    // Unknown handles disclose nothing about other sessions.
    invalidate(&session, &handle);
    Ok(StatusCode::NO_CONTENT)
}
async fn segment(
    State(service): State<Service>,
    Path(handle): Path<String>,
    request: Request,
) -> Result<Response, Failure> {
    let (token, session) = service.session(request.headers())?;
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        != Some("application/json")
    {
        return Err(Failure::Input);
    }
    let input = to_bytes(request.into_body(), INPUT_LIMIT)
        .await
        .map_err(|_| Failure::Input)?;
    let interval: SegmentRequest = serde_json::from_slice(&input).map_err(|_| Failure::Input)?;
    let playback = session
        .playbacks
        .lock()
        .unwrap()
        .get(&handle)
        .cloned()
        .ok_or(Failure::Playback)?;
    let work = async {
        active(&session, &playback)?;
        let _operation = playback.operation.try_lock().map_err(|_| Failure::Busy)?;
        let prepared = playback
            .source
            .lock()
            .unwrap()
            .clone()
            .ok_or(Failure::Internal)?;
        let info = &prepared.source;
        if interval.frames > info.sample_rate * MAX_SEGMENT_SECONDS
            || Interval::at_rate(
                interval.start,
                interval.frames,
                info.frames,
                info.sample_rate,
            )
            .is_err()
        {
            return Err(Failure::Input);
        }
        // Weak live metadata is an access check, not the staged copy's identity.
        // A changed upstream file does not change this playback's immutable input.
        service.authorize(&session, &playback.track).await?;
        let id = crate::media::hash_bytes(
            &serde_json::to_vec(&(
                "stratum-retained-segment-1",
                crate::media::EXPERIMENT,
                info,
                &prepared.encoder,
                interval,
            ))
            .map_err(|_| Failure::Internal)?,
        );
        let cache_id = id.clone();
        let lookup = service
            .0
            .audio
            .cache_job(move |cache| cache.lookup(&cache_id))
            .await;
        let ticket = lookup.as_ref().map(|value| value.ticket.clone());
        let invalid_cached_packet = lookup
            .as_ref()
            .and_then(|value| value.bytes.as_ref())
            .is_some_and(|bytes| !valid_packet(bytes, &prepared, interval, true));
        let mut output = lookup.and_then(|value| value.bytes).and_then(|bytes| {
            if valid_packet(&bytes, &prepared, interval, true) {
                mark_hit(&bytes).ok()
            } else {
                None
            }
        });
        let hit = output.is_some();
        if !hit {
            // Corrupt entries are discarded before attempting regeneration.
            if invalid_cached_packet {
                let cache_id = id.clone();
                service
                    .0
                    .audio
                    .cache_job(move |cache| cache.invalidate(&cache_id))
                    .await;
            }
            let metadata_bytes = worker(
                &service,
                playback.clone(),
                Job {
                    segment: Some(interval),
                },
                interval.priority,
            )
            .await?;
            let payload = tokio::fs::read(playback.directory.path().join("payload"))
                .await
                .map_err(|_| Failure::Encoding)?;
            let mut bytes = Vec::with_capacity(4 + metadata_bytes.len() + payload.len());
            bytes.extend_from_slice(&(metadata_bytes.len() as u32).to_be_bytes());
            bytes.extend_from_slice(&metadata_bytes);
            bytes.extend_from_slice(&payload);
            if !valid_packet(&bytes, &prepared, interval, false) {
                return Err(Failure::Encoding);
            }
            output = Some(bytes);
        }
        service.authorize(&session, &playback.track).await?;
        active(&session, &playback)?;
        let output = output.ok_or(Failure::Encoding)?;
        if !hit && valid_packet(&output, &prepared, interval, true) {
            if let Some(ticket) = ticket {
                let cached = output.clone();
                service
                    .0
                    .audio
                    .cache_job(move |cache| cache.insert(&id, &cached, &ticket))
                    .await;
            }
        }
        active(&session, &playback)?;
        let mut response = Response::new(final_body(
            session.clone(),
            playback.cancelled.clone(),
            output.into(),
        ));
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/vnd.stratum.segment".parse().unwrap(),
        );
        Ok(response)
    };
    let result = tokio::select! {
        biased;
        _ = session.cancelled.cancelled() => Err(Failure::Session),
        _ = session.expired() => Err(Failure::Session),
        _ = playback.cancelled.cancelled() => Err(Failure::Playback),
        result = work => result,
    };
    match result {
        Err(Failure::Session) => service.revoke(&token),
        Err(Failure::Access | Failure::Encoding) => invalidate(&session, &handle),
        _ => {} // A temporary outage permits an authorized retry of this frozen copy.
    }
    result
}

// A retained packet is the same atomic metadata+audio envelope sent to clients.
fn valid_packet(
    bytes: &[u8],
    prepared: &Prepared,
    interval: SegmentRequest,
    encoder: bool,
) -> bool {
    if bytes.len() < 4 || bytes.len() > crate::cache::MAX_PACKET {
        return false;
    }
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    if length > JSON_LIMIT || 4 + length >= bytes.len() {
        return false;
    }
    let Ok(metadata) = serde_json::from_slice::<SegmentMetadata>(&bytes[4..4 + length]) else {
        return false;
    };
    let info = &prepared.source;
    metadata.experiment == crate::media::EXPERIMENT
        && metadata.source_sha256 == info.sha256
        && (!encoder || metadata.encoder == prepared.encoder)
        && metadata.quality == interval.quality
        && metadata.sample_rate == info.sample_rate
        && metadata.channels == info.channels
        && metadata.presentation.start == interval.start
        && metadata.presentation.frames == interval.frames
        && metadata.encoded_interval.start <= interval.start
        && metadata.encoded_interval.frames > 0
        && metadata
            .encoded_interval
            .start
            .checked_add(metadata.encoded_interval.frames)
            .is_some_and(|end| end <= info.frames && end >= interval.start + interval.frames)
        && metadata.discard_start_frames == interval.start - metadata.encoded_interval.start
        && metadata
            .decoded_frames
            .checked_sub(metadata.discard_start_frames)
            .and_then(|n| n.checked_sub(metadata.discard_end_frames))
            == Some(interval.frames)
        && metadata.payload_bytes <= MAX_PAYLOAD_BYTES as u64
        && metadata.payload_bytes == (bytes.len() - 4 - length) as u64
        && crate::media::hash_bytes(&bytes[4 + length..]) == metadata.payload_sha256
}
fn mark_hit(bytes: &[u8]) -> Result<Vec<u8>, Failure> {
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    let mut metadata: Value =
        serde_json::from_slice(&bytes[4..4 + length]).map_err(|_| Failure::Encoding)?;
    metadata["cache_hit"] = json!(true);
    metadata["retained_cache_hit"] = json!(true);
    let metadata = serde_json::to_vec(&metadata).map_err(|_| Failure::Encoding)?;
    let mut output = Vec::with_capacity(bytes.len() + 32);
    output.extend_from_slice(&(metadata.len() as u32).to_be_bytes());
    output.extend_from_slice(&metadata);
    output.extend_from_slice(&bytes[4 + length..]);
    Ok(output)
}

/// A detached supervisor owns cleanup. Dropping its caller signals cancellation;
/// it does not abandon a blocking encoder or release resource permits too early.
async fn worker(
    service: &Service,
    playback: Arc<Playback>,
    job: Job,
    priority: Priority,
) -> Result<Vec<u8>, Failure> {
    let busy = playback
        .worker_busy
        .clone()
        .try_lock_owned()
        .map_err(|_| Failure::Busy)?;
    let permit = service
        .0
        .audio
        .workers
        .acquire(priority)
        .await
        .ok_or(Failure::Busy)?;
    let stop = CancellationToken::new();
    let dropped = stop.clone().drop_guard();
    let executable = service.0.audio.executable.clone();
    let task = tokio::spawn(async move {
        let _permit = permit;
        let _busy = busy;
        supervise(executable, playback, job, stop).await
    });
    let result = task.await.map_err(|_| Failure::Encoding)?;
    drop(dropped);
    result
}
async fn supervise(
    executable: PathBuf,
    playback: Arc<Playback>,
    job: Job,
    stop: CancellationToken,
) -> Result<Vec<u8>, Failure> {
    // The operation lock is held by the request. If it disappears, the playback
    // is not reused until this supervisor finishes (worker_busy below).
    if stop.is_cancelled() || playback.cancelled.is_cancelled() {
        return Err(Failure::Playback);
    }
    let directory = playback.directory.path();
    let job_path = directory.join("job.json");
    std::fs::write(
        &job_path,
        serde_json::to_vec(&job).map_err(|_| Failure::Internal)?,
    )
    .map_err(|_| Failure::Internal)?;
    let stdout = tempfile::tempfile_in(directory).map_err(|_| Failure::Internal)?;
    let stderr = tempfile::tempfile_in(directory).map_err(|_| Failure::Internal)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--audio-worker")
        .arg(directory)
        .stdin(Stdio::piped())
        .env("TMPDIR", directory)
        .env("TMP", directory)
        .env("TEMP", directory)
        .stdout(stdout.try_clone().map_err(|_| Failure::Internal)?)
        .stderr(stderr.try_clone().map_err(|_| Failure::Internal)?)
        .kill_on_drop(true)
        .env_remove("FFREPORT");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
    }
    #[cfg(not(unix))]
    {
        return Err(Failure::Unsupported);
    }
    let mut child = command.spawn().map_err(|_| Failure::Encoding)?;
    // Child::wait closes its own stdin field; keep the liveness pipe separately.
    let _parent_alive = child.stdin.take();
    let id = child.id().ok_or(Failure::Encoding)?;
    let mut group = ProcessGroup(id);
    let deadline = tokio::time::sleep(WORKER_LIMIT);
    tokio::pin!(deadline);
    let result = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break Err(Failure::Encoding),
            _ = playback.cancelled.cancelled() => break Err(Failure::Playback),
            _ = &mut deadline => break Err(Failure::Encoding),
            status = child.wait() => break match status {
                Ok(status) if status.success() => Ok(()),
                Ok(status) if status.code() == Some(2) => Err(Failure::Unsupported),
                _ => Err(Failure::Encoding),
            },
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                if stdout.metadata().map(|m| m.len() > JSON_LIMIT as u64).unwrap_or(true)
                    || stderr.metadata().map(|m| m.len() > JSON_LIMIT as u64).unwrap_or(true)
                { break Err(Failure::Encoding); }
            }
        }
    };
    // Kill descendants as well as the worker, even if its leader already exited.
    group.kill();
    let _ = child.wait().await;
    result?;
    use std::io::{Read, Seek, SeekFrom};
    let mut stdout = stdout;
    stdout
        .seek(SeekFrom::Start(0))
        .map_err(|_| Failure::Internal)?;
    let mut bytes = Vec::new();
    stdout
        .take(JSON_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::Internal)?;
    if bytes.len() > JSON_LIMIT {
        return Err(Failure::Encoding);
    }
    Ok(bytes)
}

// Also runs if the supervisor is unexpectedly dropped during runtime teardown.
struct ProcessGroup(u32);
impl ProcessGroup {
    fn kill(&mut self) {
        #[cfg(unix)]
        if self.0 != 0 {
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
            self.0 = 0;
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn body_delivery_checks_logout_and_playback_close_between_chunks() {
        use futures_util::StreamExt;
        for logout in [false, true] {
            let session = Arc::new(Session::new(
                Credential {
                    username: "test".into(),
                    token: "a".repeat(32),
                    salt: "test".into(),
                },
                Instant::now() + Duration::from_secs(10),
            ));
            let cancelled = session.cancelled.child_token();
            let mut body = final_body(session.clone(), cancelled.clone(), vec![1; 32768].into())
                .into_data_stream();
            assert_eq!(body.next().await.unwrap().unwrap().len(), 16384);
            if logout {
                session.revoke();
            } else {
                cancelled.cancel();
            }
            assert!(body.next().await.unwrap().is_err());
            assert!(body.next().await.is_none());
        }
    }
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_worker_request_reaps_descendants_before_releasing_lease() {
        use std::os::unix::fs::PermissionsExt;
        let config: Config =
            serde_json::from_value(json!({"upstream":"http://127.0.0.1:9"})).unwrap();
        let mut service = Service::new(&config).unwrap();
        let script = service.0.audio.root.path().join("test-worker");
        std::fs::write(
            &script,
            "#!/bin/sh\n/bin/sleep 30 &\necho $! > \"$2/child-pid\"\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        Arc::get_mut(&mut Arc::get_mut(&mut service.0).unwrap().audio)
            .unwrap()
            .executable = script;
        let playback = Arc::new(Playback {
            directory: tempfile::tempdir_in(service.0.audio.root.path()).unwrap(),
            _root: service.0.audio.root.clone(),
            _reservation: None,
            _global_slot: service.0.audio.sources.clone().try_acquire_owned().unwrap(),
            _session_slot: Arc::new(Semaphore::new(2)).try_acquire_owned().unwrap(),
            track: "test".into(),
            size: 100,
            cancelled: CancellationToken::new(),
            operation: tokio::sync::Mutex::new(()),
            worker_busy: Arc::new(tokio::sync::Mutex::new(())),
            source: Mutex::new(None),
        });
        let run_service = service.clone();
        let run_playback = playback.clone();
        let request = tokio::spawn(async move {
            worker(
                &run_service,
                run_playback,
                Job { segment: None },
                Priority::Playback,
            )
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(playback.directory.path().join("child-pid")) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(playback.worker_busy.try_lock().is_err());
        request.abort();
        let _ = request.await;
        while playback.worker_busy.try_lock().is_err() || unsafe { libc::kill(pid, 0) } == 0 {
            assert!(
                Instant::now() < deadline,
                "detached worker outlived cancellation"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(service.0.audio.workers.stats().running, 0);
        drop(playback);
        assert_eq!(service.0.audio.sources.available_permits(), 4);
    }
}
