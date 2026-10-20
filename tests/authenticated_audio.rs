//! End-to-end service + real encoder tests against a controllable HTTP upstream.
//! No sound devices, existing accounts or user music are used.
#![cfg(unix)]
use axum::{
    body::{to_bytes, Body},
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use stratum_poc::{
    media::{self, Segment, SegmentMetadata},
    owner, proof,
};
use tempfile::TempDir;
use tokio::{net::TcpListener, sync::Notify};

#[derive(Default)]
struct Upstream {
    data: Mutex<Vec<u8>>,
    denied: AtomicBool,
    unavailable: AtomicBool,
    mode: AtomicUsize,
    full_reads: AtomicUsize,
    songs: AtomicUsize,
    hold_full: AtomicBool,
    hold_song: AtomicUsize,
    reached: Notify,
    release: Notify,
}
fn subsonic(value: Value) -> Response {
    Json(json!({"subsonic-response":value})).into_response()
}
fn denied(code: u64) -> Response {
    subsonic(json!({"status":"failed","error":{"code":code,"message":"SECRET-NOT-FOR-CLIENT"}}))
}
async fn upstream(State(state): State<Arc<Upstream>>, request: Request) -> Response {
    assert!(request.uri().query().is_none());
    let endpoint = request.uri().path().to_owned();
    let range = request.headers().get(header::RANGE).cloned();
    let body = to_bytes(request.into_body(), 4096).await.unwrap();
    let url = reqwest::Url::parse(&format!(
        "http://unused/?{}",
        String::from_utf8_lossy(&body)
    ))
    .unwrap();
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    if !["alice", "bob"].contains(&params["u"].as_str()) || params["t"] != "a".repeat(32) {
        return denied(40);
    }
    if endpoint.ends_with("ping.view") {
        return subsonic(json!({"status":"ok"}));
    }
    if state.unavailable.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if endpoint.ends_with("getSong.view") {
        let count = state.songs.fetch_add(1, Ordering::SeqCst) + 1;
        if state.hold_song.load(Ordering::SeqCst) == count {
            state.reached.notify_one();
            state.release.notified().await;
        }
    }
    if state.denied.load(Ordering::SeqCst) || (params["u"] == "bob" && params["id"] != "b") {
        return denied(50);
    }
    let data = state.data.lock().unwrap().clone();
    if endpoint.ends_with("getSong.view") {
        return subsonic(
            json!({"status":"ok","song":{"id":params["id"],"size": if state.mode.load(Ordering::SeqCst) == 1 { 67108865 } else { data.len() } }}),
        );
    }
    assert!(endpoint.ends_with("stream.view"));
    assert_eq!(params["format"], "raw");
    assert_eq!(params["maxBitRate"], "0");
    if let Some(range) = range {
        assert_eq!(range, "bytes=0-0");
        return (
            StatusCode::PARTIAL_CONTENT,
            [
                (
                    header::CONTENT_RANGE,
                    format!(
                        "bytes 0-0/{}",
                        if state.mode.load(Ordering::SeqCst) == 1 {
                            67108865
                        } else {
                            data.len()
                        }
                    ),
                ),
                (header::CONTENT_TYPE, "audio/flac".into()),
            ],
            vec![data[0]],
        )
            .into_response();
    }
    state.full_reads.fetch_add(1, Ordering::SeqCst);
    if state.hold_full.load(Ordering::SeqCst) {
        state.reached.notify_one();
        state.release.notified().await;
    }
    match state.mode.load(Ordering::SeqCst) {
        2 => denied(40),
        3 => ([(header::CONTENT_TYPE, "text/html")], data).into_response(),
        4 => (
            [(header::CONTENT_TYPE, "audio/flac")],
            data[..data.len() - 1].to_vec(),
        )
            .into_response(),
        5 => (
            [(header::CONTENT_TYPE, "audio/flac")],
            vec![b'x'; data.len()],
        )
            .into_response(),
        6 => {
            let chunk_size = data.len().div_ceil(8);
            let body = Body::from_stream(futures_util::stream::unfold(
                (data, 0),
                move |(data, offset)| async move {
                    if offset == data.len() {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                    let end = (offset + chunk_size).min(data.len());
                    Some((
                        Ok::<_, std::io::Error>(data[offset..end].to_vec()),
                        (data, end),
                    ))
                },
            ));
            ([(header::CONTENT_TYPE, "audio/flac")], body).into_response()
        }
        _ => ([(header::CONTENT_TYPE, "audio/flac")], data).into_response(),
    }
}
// Ensure a startup assertion cannot leave the new persistent service running.
struct StartupChild(Option<Child>);
impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
struct Harness {
    dir: TempDir,
    fixture: PathBuf,
    state: Arc<Upstream>,
    base: String,
    process: Child,
    task: tokio::task::JoinHandle<()>,
    http: reqwest::Client,
    owner: Option<owner::Store>,
}
impl Drop for Harness {
    fn drop(&mut self) {
        unsafe {
            libc::kill(self.process.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        while self.process.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.process.kill();
        let _ = self.process.wait();
        self.task.abort();
    }
}
impl Harness {
    async fn new(rate: u64) -> Self {
        Self::configured(rate, 600).await
    }
    async fn configured(rate: u64, ttl: u64) -> Self {
        Self::launch(rate, ttl, false).await
    }
    async fn installed(rate: u64) -> Self {
        Self::launch(rate, 600, true).await
    }
    async fn launch(rate: u64, ttl: u64, installed: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture.flac");
        proof::fixture_at_rate(&fixture, 6 * rate + 317, 2, 24, rate).unwrap();
        let state = Arc::new(Upstream {
            data: Mutex::new(fs::read(&fixture).unwrap()),
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!(
            "http://{}:{}/music",
            if installed { "localhost" } else { "127.0.0.1" },
            listener.local_addr().unwrap().port()
        );
        let app = Router::new().fallback(upstream).with_state(state.clone());
        let task = tokio::spawn(async {
            axum::serve(listener, app).await.unwrap();
        });
        let config = dir.path().join("config.json");
        fs::write(
            &config,
            json!({"upstream":upstream_url,"run_seconds":120,"session_seconds":ttl}).to_string(),
        )
        .unwrap();
        let log = fs::File::create(dir.path().join("service.log")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let shim_dir = dir.path().join("bin");
        fs::create_dir(&shim_dir).unwrap();
        let real_ffmpeg = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|p| p.join("ffmpeg"))
            .find(|p| p.is_file())
            .unwrap();
        let shim = shim_dir.join("ffmpeg");
        fs::write(&shim, "#!/bin/sh\nif [ -f \"$STRATUM_TEST_HOLD\" ]; then\n  echo $$ > \"$STRATUM_TEST_PID\"\n  exec /bin/sleep 30\nfi\nexec \"$STRATUM_TEST_FFMPEG\" \"$@\"\n").unwrap();
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        let mut http = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(15));
        let mut command;
        let owner = if installed {
            let store = owner::Store::initialize(
                &dir.path().join("owner"),
                &owner::Config {
                    listen: "127.0.0.1:0".parse().unwrap(),
                    resources: stratum_poc::resources::Settings {
                        max_conversions: Some(2),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
            store
                .set_password("disposable owner password", false)
                .unwrap();
            store
                .save_setup(
                    &serde_json::from_value(json!({
                        "navidrome_url":upstream_url,
                        "listeners":[{"url":"https://localhost:7843","trust":"sidecar_key"}]
                    }))
                    .unwrap(),
                    "initial",
                    None,
                )
                .unwrap();
            let private: Value =
                serde_json::from_slice(&fs::read(dir.path().join("owner/private.json")).unwrap())
                    .unwrap();
            http = http.add_root_certificate(
                reqwest::Certificate::from_pem(
                    private["certificate_pem"].as_str().unwrap().as_bytes(),
                )
                .unwrap(),
            );
            command = Command::new(env!("CARGO_BIN_EXE_sidecar"));
            command
                .args(["serve", "--data-dir"])
                .arg(dir.path().join("owner"));
            Some(store)
        } else {
            command = Command::new(env!("CARGO_BIN_EXE_sidecar-access"));
            command.arg("--config").arg(config);
            None
        };
        let mut child = StartupChild(Some(
            command
                .env("TMPDIR", dir.path())
                .env(
                    "PATH",
                    format!("{}:{}", shim_dir.display(), std::env::var("PATH").unwrap()),
                )
                .env("STRATUM_TEST_FFMPEG", real_ffmpeg)
                .env("STRATUM_TEST_HOLD", dir.path().join("hold-worker"))
                .env("STRATUM_TEST_PID", dir.path().join("worker-pid"))
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        let base = loop {
            let log = fs::read_to_string(dir.path().join("service.log")).unwrap();
            if installed && log.contains("fingerprint is") {
                if let Some(address) = log
                    .lines()
                    .find_map(|line| line.strip_prefix("Sidecar HTTPS listener: "))
                {
                    break format!("https://{address}/stratum");
                }
            } else if let Ok(value) = serde_json::from_str::<Value>(&log) {
                if let Some(url) = value["listen"].as_str() {
                    break url.to_owned();
                }
            }
            assert!(
                child.0.as_mut().unwrap().try_wait().unwrap().is_none(),
                "service exited during startup"
            );
            assert!(Instant::now() < deadline, "service did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let http = http.build().unwrap();
        loop {
            if http
                .get(format!("{base}/access/v1/capabilities"))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < deadline, "service did not accept requests");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Self {
            dir,
            fixture,
            state,
            base,
            process: child.0.take().unwrap(),
            task,
            http,
            owner,
        }
    }
    async fn login(&self, user: &str) -> String {
        let response = self
            .http
            .post(format!("{}/access/v1/sessions", self.base))
            .json(&json!({"username":user,"token":"a".repeat(32),"salt":"test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        response.json::<Value>().await.unwrap()["session"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn open(&self, token: &str, track: &str) -> reqwest::Response {
        self.http
            .post(format!("{}/audio/v1/playbacks/{track}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
    }
    async fn handle(&self, token: &str) -> Value {
        let response = self.open(token, "a").await;
        let status = response.status();
        let bytes = response.bytes().await.unwrap();
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&bytes));
        serde_json::from_slice(&bytes).unwrap()
    }
    async fn segment(
        &self,
        token: &str,
        handle: &str,
        start: u64,
        frames: u64,
        quality: &str,
    ) -> reqwest::Response {
        self.http
            .post(format!("{}/audio/v1/playback/{handle}/segment", self.base))
            .bearer_auth(token)
            .json(&json!({"start":start,"frames":frames,"quality":quality}))
            .send()
            .await
            .unwrap()
    }
    async fn decoded(
        &self,
        token: &str,
        handle: &str,
        start: u64,
        frames: u64,
        quality: &str,
    ) -> (Value, Vec<i32>) {
        let response = self.segment(token, handle, start, frames, quality).await;
        let status = response.status();
        let bytes = response.bytes().await.unwrap();
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&bytes));
        let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        let value: Value = serde_json::from_slice(&bytes[4..4 + length]).unwrap();
        let metadata: SegmentMetadata = serde_json::from_value(value.clone()).unwrap();
        let path = self.dir.path().join(format!(
            "received.{}",
            if quality == "lossless" {
                "flac"
            } else {
                "opus"
            }
        ));
        fs::write(&path, &bytes[4 + length..]).unwrap();
        let pcm = media::presentation_pcm(&Segment {
            path,
            metadata,
            cache_hit: false,
            prepare_ms: 0.0,
        })
        .unwrap();
        (value, pcm)
    }
    fn source_dirs(&self) -> usize {
        if let Some(owner) = &self.owner {
            return fs::read_dir(owner.working_directory())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e.file_type().unwrap().is_dir())
                .map(|e| {
                    fs::read_dir(e.path())
                        .unwrap()
                        .filter_map(Result::ok)
                        .filter(|child| child.file_type().unwrap().is_dir())
                        .count()
                })
                .sum();
        }
        fs::read_dir(self.dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("stratum-audio-")
            })
            .map(|e| fs::read_dir(e.path()).unwrap().count())
            .sum()
    }
    async fn wait_clean(&self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.source_dirs() != 0 {
            assert!(Instant::now() < deadline, "staged source not cleaned");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn close_playback(&self, token: &str, handle: &str) {
        assert!(self
            .http
            .delete(format!("{}/audio/v1/playback/{handle}", self.base))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .status()
            .is_success());
    }
    async fn restart_installed(&mut self) {
        if self.process.try_wait().unwrap().is_none() {
            unsafe {
                libc::kill(self.process.id() as i32, libc::SIGTERM);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(6);
        while self.process.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let log_path = self.dir.path().join("restart.log");
        let log = fs::File::create(&log_path).unwrap();
        self.process = Command::new(env!("CARGO_BIN_EXE_sidecar"))
            .args(["serve", "--data-dir"])
            .arg(self.dir.path().join("owner"))
            .env("TMPDIR", self.dir.path())
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let log = fs::read_to_string(&log_path).unwrap();
            if log.contains("fingerprint is") {
                if let Some(addr) = log
                    .lines()
                    .find_map(|line| line.strip_prefix("Sidecar HTTPS listener: "))
                {
                    self.base = format!("https://{addr}/stratum");
                    if self
                        .http
                        .get(format!("{}/access/v1/capabilities", self.base))
                        .send()
                        .await
                        .is_ok()
                    {
                        break;
                    }
                }
            }
            assert!(self.process.try_wait().unwrap().is_none());
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn retained_entries(&self) -> usize {
        fs::read_dir(self.owner.as_ref().unwrap().cache_directory())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "segment"))
            .count()
    }
    async fn cache_change(&self, enabled: bool, clear: bool) {
        let store = self.owner.as_ref().unwrap();
        let saved = store.config().unwrap().storage;
        store
            .save_storage(
                &stratum_poc::cache::Change {
                    revision: saved.revision,
                    enabled,
                    limit_bytes: saved.limit_bytes,
                    clear,
                    confirmed: true,
                },
                None,
            )
            .unwrap();
        // Every listener request applies changes, without a timing-dependent sleep.
        assert_eq!(
            self.http
                .get(format!("{}/access/v1/capabilities", self.base))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    async fn resource_change(&self, conversions: usize, working_mb: u64) {
        let store = self.owner.as_ref().unwrap();
        let current = store.config().unwrap().resources;
        store
            .save_resources(
                &stratum_poc::resources::Change {
                    revision: current.revision,
                    max_conversions: Some(conversions),
                    working_bytes: working_mb * 1_000_000,
                },
                None,
            )
            .unwrap();
        assert_eq!(
            self.http
                .get(format!("{}/access/v1/capabilities", self.base))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_working_limit_protects_current_streams_and_releases_closed_files() {
    let test = Harness::installed(96000).await;
    test.resource_change(1, 128).await;
    let alice = test.login("alice").await;
    let bob = test.login("bob").await;
    let a = test.handle(&alice).await;
    let next = test.handle(&alice).await;
    let b = test.open(&bob, "b").await.json::<Value>().await.unwrap();
    // Three small sources plus their worker headroom fit; a fourth does not.
    let refused = test.open(&bob, "b").await;
    assert_eq!(refused.status(), 503);
    assert_eq!(refused.headers()[header::RETRY_AFTER], "1");
    assert_eq!(
        refused.json::<Value>().await.unwrap()["error"],
        "working_storage_unavailable"
    );
    test.decoded(
        &alice,
        a["playback"].as_str().unwrap(),
        0,
        4 * 96000,
        "lossless",
    )
    .await;
    test.close_playback(&alice, next["playback"].as_str().unwrap())
        .await;
    let more = test.open(&bob, "b").await;
    assert_eq!(more.status(), 201);
    test.decoded(
        &bob,
        b["playback"].as_str().unwrap(),
        0,
        4 * 96000,
        "opus-64",
    )
    .await;
    test.resource_change(2, 512).await;
    test.decoded(
        &alice,
        a["playback"].as_str().unwrap(),
        96000,
        96000,
        "opus-128",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_hard_crash_stops_encoder_descendants_and_recovers_owned_working_files() {
    let mut test = Harness::installed(48000).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    let old_root = fs::read_dir(test.owner.as_ref().unwrap().working_directory())
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.file_type().unwrap().is_dir())
        .unwrap()
        .path();
    fs::write(test.dir.path().join("hold-worker"), b"hold").unwrap();
    let pending = test
        .http
        .post(format!("{}/audio/v1/playback/{handle}/segment", test.base))
        .bearer_auth(&token)
        .json(&json!({"start":0,"frames":48000,"quality":"opus-64"}))
        .send();
    tokio::pin!(pending);
    let wait_pid = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(value) = fs::read_to_string(test.dir.path().join("worker-pid")) {
                if let Ok(pid) = value.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let pid = tokio::select! {_=&mut pending=>panic!("encoder was not held"),pid=wait_pid=>pid};
    test.process.kill().unwrap(); // SIGKILL: no service destructors or graceful shutdown
    let _ = pending.await;
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "encoder survived its parent's hard crash"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        old_root.exists(),
        "hard kill should leave working files to recover"
    );
    test.restart_installed().await;
    assert!(!old_root.exists());
    let token = test.login("alice").await;
    let fresh = test.handle(&token).await;
    test.decoded(
        &token,
        fresh["playback"].as_str().unwrap(),
        0,
        48000,
        "lossless",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_retention_survives_restart_and_rechecks_each_listeners_access() {
    let mut test = Harness::installed(48000).await;
    let alice = test.login("alice").await;
    let opened = test.handle(&alice).await;
    let handle = opened["playback"].as_str().unwrap();
    let (cold, expected) = test.decoded(&alice, handle, 37, 48001, "opus-64").await;
    assert_ne!(cold["retained_cache_hit"], true);
    assert_eq!(test.retained_entries(), 1);
    test.close_playback(&alice, handle).await;
    test.wait_clean().await;
    let bob = test.login("bob").await;
    assert_eq!(test.open(&bob, "a").await.status(), 404);
    let allowed = test.open(&bob, "b").await.json::<Value>().await.unwrap();
    let handle = allowed["playback"].as_str().unwrap();
    // A hit on another authorized playback must not invoke FFmpeg at all.
    fs::write(test.dir.path().join("hold-worker"), b"hold").unwrap();
    let (warm, actual) = test.decoded(&bob, handle, 37, 48001, "opus-64").await;
    assert_eq!(warm["retained_cache_hit"], true);
    assert_eq!(actual, expected);
    // Scheduling intent must not change the encoded interval's cache identity.
    let prefetch = test
        .http
        .post(format!("{}/audio/v1/playback/{handle}/segment", test.base))
        .bearer_auth(&bob)
        .json(&json!({"start":37,"frames":48001,"quality":"opus-64","priority":"prefetch"}))
        .send()
        .await
        .unwrap();
    assert_eq!(prefetch.status(), 200);
    let bytes = prefetch.bytes().await.unwrap();
    let length = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
    let metadata: Value = serde_json::from_slice(&bytes[4..4 + length]).unwrap();
    assert_eq!(metadata["retained_cache_hit"], true);
    assert!(!test.dir.path().join("worker-pid").exists());
    fs::remove_file(test.dir.path().join("hold-worker")).unwrap();
    assert_eq!(
        test.state.full_reads.load(Ordering::SeqCst),
        2,
        "fresh playbacks still verify their complete source"
    );
    // Permission can disappear between the initial check and delivery of a hit.
    test.state.hold_song.store(
        test.state.songs.load(Ordering::SeqCst) + 2,
        Ordering::SeqCst,
    );
    {
        let pending = test.segment(&bob, handle, 37, 48001, "opus-64");
        tokio::pin!(pending);
        tokio::select! {
            _=&mut pending=>panic!("cached delivery completed before reauthorization"),
            result=tokio::time::timeout(Duration::from_secs(3),test.state.reached.notified())=>result.unwrap(),
        }
        test.state.denied.store(true, Ordering::SeqCst);
        test.state.release.notify_one();
        assert_eq!(pending.await.status(), 404);
    }
    test.state.denied.store(false, Ordering::SeqCst);
    test.wait_clean().await;
    test.restart_installed().await;
    assert_eq!(test.open(&alice, "a").await.status(), 401);
    let alice = test.login("alice").await;
    let opened = test.handle(&alice).await;
    let (warm, actual) = test
        .decoded(
            &alice,
            opened["playback"].as_str().unwrap(),
            37,
            48001,
            "opus-64",
        )
        .await;
    assert_eq!(warm["retained_cache_hit"], true);
    assert_eq!(actual, expected);
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_clear_during_preparation_and_disable_keep_active_audio_working() {
    let test = Harness::installed(48000).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    test.decoded(&token, handle, 0, 48000, "lossless").await;
    assert_eq!(test.retained_entries(), 1);
    // Hold the newly encoded interval at its final permission check, before insert.
    test.state.hold_song.store(
        test.state.songs.load(Ordering::SeqCst) + 2,
        Ordering::SeqCst,
    );
    let pending = test.segment(&token, handle, 48000, 48000, "opus-64");
    tokio::pin!(pending);
    tokio::select! {
        _=&mut pending=>panic!("preparation completed before clear"),
        result=tokio::time::timeout(Duration::from_secs(3),test.state.reached.notified())=>result.unwrap(),
    }
    test.cache_change(true, true).await;
    assert_eq!(test.retained_entries(), 0);
    test.state.release.notify_one();
    assert_eq!(
        pending.await.status(),
        200,
        "clear must not cancel active audio"
    );
    assert_eq!(
        test.retained_entries(),
        0,
        "late preparation must not refill the cleared cache"
    );
    test.decoded(&token, handle, 96000, 48000, "opus-64").await;
    assert_eq!(test.retained_entries(), 1);
    test.cache_change(false, false).await;
    assert_eq!(test.retained_entries(), 0);
    test.decoded(&token, handle, 144000, 48000, "lossless")
        .await;
    assert_eq!(test.retained_entries(), 0);
    test.cache_change(true, false).await;
    test.decoded(&token, handle, 192000, 48000, "lossless")
        .await;
    assert_eq!(test.retained_entries(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_new_source_hash_cannot_reuse_old_retained_audio() {
    let test = Harness::installed(48000).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    test.decoded(&token, handle, 0, 48000, "lossless").await;
    test.close_playback(&token, handle).await;
    // Alter an existing FLAC padding byte. The source is still decodable, same
    // length and audio, but different content. Weak server metadata is unchanged.
    {
        let mut data = test.state.data.lock().unwrap();
        assert_eq!(&data[..4], b"fLaC");
        let mut offset = 4;
        loop {
            let kind = data[offset] & 127;
            let length = (usize::from(data[offset + 1]) << 16)
                | (usize::from(data[offset + 2]) << 8)
                | usize::from(data[offset + 3]);
            if kind == 1 && length > 0 {
                data[offset + 4] ^= 1;
                break;
            }
            assert_eq!(data[offset] & 128, 0, "fixture must contain padding");
            offset += 4 + length;
        }
    }
    let fresh = test.handle(&token).await;
    assert_ne!(fresh["source"]["sha256"], opened["source"]["sha256"]);
    let (metadata, _) = test
        .decoded(
            &token,
            fresh["playback"].as_str().unwrap(),
            0,
            48000,
            "lossless",
        )
        .await;
    assert_ne!(metadata["retained_cache_hit"], true);
    assert_eq!(metadata["source_sha256"], fresh["source"]["sha256"]);
    assert_eq!(test.retained_entries(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_cache_hit_does_not_wait_for_busy_encoder_slots() {
    let test = Harness::installed(48000).await;
    let alice = test.login("alice").await;
    let bob = test.login("bob").await;
    let hot = test.handle(&alice).await;
    let hot = hot["playback"].as_str().unwrap();
    test.decoded(&alice, hot, 0, 48000, "opus-64").await;
    let cold_a = test.handle(&alice).await;
    let cold_a = cold_a["playback"].as_str().unwrap();
    let cold_b = test.open(&bob, "b").await.json::<Value>().await.unwrap();
    let cold_b = cold_b["playback"].as_str().unwrap();
    // Both real encoder slots remain occupied until their playbacks close.
    fs::write(
        test.dir.path().join("bin/ffmpeg"),
        "#!/bin/sh\necho $$ >> \"$STRATUM_TEST_PID\"\nexec /bin/sleep 30\n",
    )
    .unwrap();
    let pending = async {
        tokio::join!(
            test.segment(&alice, cold_a, 48000, 48000, "opus-64"),
            test.segment(&bob, cold_b, 48000, 48000, "opus-64")
        )
    };
    tokio::pin!(pending);
    let occupied = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if fs::read_to_string(test.dir.path().join("worker-pid"))
                .unwrap_or_default()
                .lines()
                .count()
                == 2
            {
                break;
            }
            assert!(Instant::now() < deadline, "both encoders did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::select! {
        _=&mut pending=>panic!("encoders did not stay occupied"),
        _=occupied=>{},
    }
    let (metadata, _) = test.decoded(&alice, hot, 0, 48000, "opus-64").await;
    assert_eq!(metadata["retained_cache_hit"], true);
    assert_eq!(
        fs::read_to_string(test.dir.path().join("worker-pid"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    test.close_playback(&alice, cold_a).await;
    test.close_playback(&bob, cold_b).await;
    let (a, b) = pending.await;
    assert!(!a.status().is_success() && !b.status().is_success());
}

async fn audio_contract(rate: u64, installed: bool) {
    let test = if installed {
        Harness::installed(rate).await
    } else {
        Harness::new(rate).await
    };
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    assert_eq!(
        opened["source"]["sha256"],
        media::hash_file(&test.fixture).unwrap()
    );
    let reference = media::decode(&test.fixture, 2, 6 * rate + 317).unwrap();
    let codec_reference = (rate == 96000).then(|| {
        media::decode_at_rate(&test.fixture, 2, (6 * rate + 317).div_ceil(2), Some(48000)).unwrap()
    });
    let mut end = 0;
    for (start, frames, quality) in [
        (0, 2 * rate + 37, "lossless"),
        (2 * rate + 37, 2 * rate - 37, "opus-64"),
        (4 * rate, 2 * rate, "opus-128"),
        (6 * rate, 317, "lossless"),
    ] {
        assert_eq!(start, end);
        let (metadata, pcm) = test.decoded(&token, handle, start, frames, quality).await;
        assert_eq!(pcm.len() as u64, frames * 2);
        if quality == "lossless" {
            assert_eq!(
                pcm,
                reference[(start * 2) as usize..((start + frames) * 2) as usize]
            );
        } else {
            let windows = proof::alignment_at_rate(&reference, &pcm, start, 2, rate);
            if let Some(reference_48) = &codec_reference {
                let codec_windows =
                    proof::opus_clock_alignment(reference_48, &pcm, start, 2).unwrap();
                assert!(!codec_windows.is_empty());
                assert!(codec_windows
                    .iter()
                    .all(|w| w.correlation > 0.5 && w.best_lag_frames == 0));
            }
            assert!(windows.iter().all(|w| w.correlation > 0.5
                && w.best_lag_frames.abs() <= if rate == 96000 { 1 } else { 0 }));
        }
        assert_eq!(metadata["source_sha256"], opened["source"]["sha256"]);
        end += frames;
    }
    assert_eq!(end, 6 * rate + 317);
    let (warm, _) = test
        .decoded(&token, handle, 6 * rate, 317, "lossless")
        .await;
    assert_eq!(warm["cache_hit"], true);
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), 1);
    assert_eq!(
        test.segment(&token, handle, end, 1, "lossless")
            .await
            .status(),
        400
    );
    assert_eq!(
        test.segment(&token, handle, 0, 4 * rate + 1, "lossless")
            .await
            .status(),
        400
    );
    test.http
        .delete(format!("{}/audio/v1/playback/{handle}", test.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    test.wait_clean().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_44100() {
    audio_contract(44100, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_48000() {
    audio_contract(48000, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_96000() {
    audio_contract(96000, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_audio_is_private_reauthorized_and_recovers_after_outage() {
    permission_contract(false).await;
}
async fn permission_contract(installed: bool) {
    let test = if installed {
        Harness::installed(48000).await
    } else {
        Harness::new(48000).await
    };
    let alice = test.login("alice").await;
    let bob = test.login("bob").await;
    assert_eq!(test.open("invalid", "a").await.status(), 401);
    let opened = test.handle(&alice).await;
    let handle = opened["playback"].as_str().unwrap();
    test.decoded(&alice, handle, 37, 48001, "opus-64").await;
    assert_eq!(
        test.segment(&bob, handle, 37, 48001, "opus-64")
            .await
            .status(),
        404
    );
    assert_eq!(test.open(&bob, "a").await.status(), 404);
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), 1);
    test.state.unavailable.store(true, Ordering::SeqCst);
    assert_eq!(
        test.segment(&alice, handle, 37, 48001, "opus-64")
            .await
            .status(),
        503
    );
    test.state.unavailable.store(false, Ordering::SeqCst);
    let (warm, _) = test.decoded(&alice, handle, 37, 48001, "opus-64").await;
    assert_eq!(warm["cache_hit"], true);
    test.state.denied.store(true, Ordering::SeqCst);
    assert_eq!(
        test.segment(&alice, handle, 37, 48001, "opus-64")
            .await
            .status(),
        404
    );
    test.wait_clean().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_lost_after_encoding_blocks_final_delivery() {
    let test = Harness::new(48000).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    test.state.hold_song.store(
        test.state.songs.load(Ordering::SeqCst) + 2,
        Ordering::SeqCst,
    );
    let pending = test
        .http
        .post(format!("{}/audio/v1/playback/{handle}/segment", test.base))
        .bearer_auth(&token)
        .json(&json!({"start":37,"frames":48001,"quality":"opus-64"}))
        .send();
    tokio::pin!(pending);
    tokio::select! { _=&mut pending=>panic!("delivery completed early"), result=tokio::time::timeout(Duration::from_secs(5),test.state.reached.notified())=>{result.unwrap();} }
    test.state.denied.store(true, Ordering::SeqCst);
    test.state.release.notify_one();
    assert_eq!(pending.await.unwrap().status(), 404);
    test.wait_clean().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frozen_source_does_not_mix_new_upstream_bytes() {
    let test = Harness::new(48000).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    let (_, before) = test.decoded(&token, handle, 0, 48000, "lossless").await;
    // Same length, invalid new source contents, and no usable revision header.
    // Existing playback uses its complete verified copy; fresh acquisition fails.
    let length = test.state.data.lock().unwrap().len();
    *test.state.data.lock().unwrap() = vec![b'x'; length];
    let (_, after) = test.decoded(&token, handle, 0, 48000, "lossless").await;
    assert_eq!(before, after);
    assert_eq!(test.open(&token, "a").await.status(), 415);
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn source_limits_bad_responses_and_logout_cleanup() {
    let test = Harness::new(48000).await;
    let mut token = test.login("alice").await;
    for (mode, status) in [(1, 413), (2, 401), (3, 502), (4, 502), (5, 415)] {
        test.state.mode.store(mode, Ordering::SeqCst);
        assert_eq!(test.open(&token, "a").await.status(), status, "mode {mode}");
        test.wait_clean().await;
        if status == 401 {
            token = test.login("alice").await;
        }
    }
    test.state.mode.store(0, Ordering::SeqCst);
    test.handle(&token).await;
    test.handle(&token).await;
    let fetched = test.state.full_reads.load(Ordering::SeqCst);
    assert_eq!(test.open(&token, "a").await.status(), 429);
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), fetched);
    test.http
        .delete(format!("{}/access/v1/session", test.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    test.wait_clean().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_during_acquisition_rejects_late_bytes_and_cleans_source() {
    let test = Harness::new(48000).await;
    let token = test.login("alice").await;
    test.state.hold_full.store(true, Ordering::SeqCst);
    let pending = test
        .http
        .post(format!("{}/audio/v1/playbacks/a", test.base))
        .bearer_auth(&token)
        .send();
    tokio::pin!(pending);
    tokio::select! { _=&mut pending=>panic!("acquisition completed early"), result=tokio::time::timeout(Duration::from_secs(3),test.state.reached.notified())=>{result.unwrap();} }
    test.http
        .delete(format!("{}/access/v1/session", test.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(pending.await.unwrap().status(), 401);
    test.state.release.notify_one();
    test.wait_clean().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_playback_kills_active_encoder_and_releases_its_files() {
    let test = Harness::new(48000).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    fs::write(test.dir.path().join("hold-worker"), b"hold").unwrap();
    let pending = test
        .http
        .post(format!("{}/audio/v1/playback/{handle}/segment", test.base))
        .bearer_auth(&token)
        .json(&json!({"start":37,"frames":48001,"quality":"opus-64"}))
        .send();
    tokio::pin!(pending);
    let pid_path = test.dir.path().join("worker-pid");
    let wait_pid = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = fs::read_to_string(&pid_path) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "encoder did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let pid = tokio::select! { _=&mut pending=>panic!("encoder was not held"), pid=wait_pid=>pid };
    test.http
        .delete(format!("{}/audio/v1/playback/{handle}", test.base))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(pending.await.unwrap().status(), 404);
    test.wait_clean().await;
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "encoder descendant survived cancellation"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fs::remove_file(test.dir.path().join("hold-worker")).unwrap();
    test.handle(&token).await; // Slot is reusable after real worker cleanup.
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_clears_staged_sources_and_rejects_cached_segments() {
    let test = Harness::configured(48000, 2).await;
    let token = test.login("alice").await;
    let opened = test.handle(&token).await;
    let handle = opened["playback"].as_str().unwrap();
    test.decoded(&token, handle, 0, 48000, "lossless").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        test.segment(&token, handle, 0, 48000, "lossless")
            .await
            .status(),
        401
    );
    test.wait_clean().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_source_limit_is_held_across_sessions_and_released_on_close() {
    let test = Harness::new(48000).await;
    let a = test.login("alice").await;
    let b = test.login("alice").await;
    let c = test.login("alice").await;
    let first = test.handle(&a).await;
    test.handle(&a).await;
    test.handle(&b).await;
    test.handle(&b).await;
    let fetched = test.state.full_reads.load(Ordering::SeqCst);
    assert_eq!(test.open(&c, "a").await.status(), 429);
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), fetched);
    test.http
        .delete(format!(
            "{}/audio/v1/playback/{}",
            test.base,
            first["playback"].as_str().unwrap()
        ))
        .bearer_auth(&a)
        .send()
        .await
        .unwrap();
    test.handle(&c).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn staging_measurement_includes_slow_chunked_source_acquisition() {
    let test = Harness::new(48000).await;
    let token = test.login("alice").await;
    test.state.mode.store(6, Ordering::SeqCst);
    let opened = test.handle(&token).await;
    assert!(opened["acquisitionMs"].as_f64().unwrap() >= 200.0);
    assert!(opened["readyMs"].as_f64().unwrap() > opened["acquisitionMs"].as_f64().unwrap());
    assert_eq!(
        opened["acquiredBytes"].as_u64().unwrap(),
        fs::metadata(&test.fixture).unwrap().len()
    );
    assert_eq!(
        opened["source"]["sha256"],
        media::hash_file(&test.fixture).unwrap()
    );
    let handle = opened["playback"].as_str().unwrap();
    test.decoded(&token, handle, 0, 48000, "lossless").await;
    test.decoded(&token, handle, 48000, 48000, "opus-64").await;
    assert_eq!(test.state.full_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn renewal_retains_current_and_next_audio_handles_without_refetching_sources() {
    let h = Harness::configured(48000, 2).await;
    let token = h.login("alice").await;
    let current = h.handle(&token).await;
    let next = h.handle(&token).await;
    let first = h
        .decoded(
            &token,
            current["playback"].as_str().unwrap(),
            0,
            48000,
            "lossless",
        )
        .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let response = h
        .http
        .post(format!("{}/access/v1/session", h.base))
        .bearer_auth(&token)
        .json(&json!({"username":"alice","token":"a".repeat(32),"salt":"fresh"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["session"], token);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let repeated = h
        .decoded(
            &token,
            current["playback"].as_str().unwrap(),
            0,
            48000,
            "lossless",
        )
        .await;
    assert_eq!(first.1, repeated.1);
    let following = h
        .decoded(
            &token,
            next["playback"].as_str().unwrap(),
            48000,
            48000,
            "opus-64",
        )
        .await;
    assert_eq!(following.0["presentation"]["start"], 48000);
    assert_eq!(h.state.full_reads.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_https_audio_preserves_exact_samples_and_quality_alignment() {
    audio_contract(48000, true).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_https_audio_preserves_scoped_access_and_revocation() {
    permission_contract(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn installed_reconfiguration_cancels_inflight_source_and_shutdown_reaps_encoder() {
    let mut test = Harness::installed(48000).await;
    let token = test.login("alice").await;
    let capabilities: Value = test
        .http
        .get(format!("{}/access/v1/capabilities", test.base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let owner = test.owner.as_ref().unwrap();
    assert_eq!(
        capabilities["spkiSha256"],
        owner.identity().unwrap().spki_sha256
    );
    assert_eq!(capabilities["playback"], true);
    test.state.hold_full.store(true, Ordering::SeqCst);
    let pending = test
        .http
        .post(format!("{}/audio/v1/playbacks/a", test.base))
        .bearer_auth(&token)
        .send();
    tokio::pin!(pending);
    tokio::select! { _=&mut pending=>panic!("source was not held"), result=tokio::time::timeout(Duration::from_secs(3),test.state.reached.notified())=>{result.unwrap();} }
    // Simulate CLI configure while the web service is running. Even an address
    // edit on the same upstream is a new connection revision and revokes sessions.
    let setup = owner.config().unwrap().setup;
    owner
        .save_setup(&setup.settings, &setup.revision, None)
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(3), &mut pending)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(response.status().as_u16(), 401 | 503));
    test.state.release.notify_one();
    test.state.hold_full.store(false, Ordering::SeqCst);
    test.wait_clean().await;
    assert_eq!(test.open(&token, "a").await.status(), 401);
    let fresh = test.login("alice").await;
    let opened = test.handle(&fresh).await;
    let handle = opened["playback"].as_str().unwrap();
    fs::write(test.dir.path().join("hold-worker"), b"hold").unwrap();
    let pending = test
        .http
        .post(format!("{}/audio/v1/playback/{handle}/segment", test.base))
        .bearer_auth(&fresh)
        .json(&json!({"start":0,"frames":48000,"quality":"opus-64"}))
        .send();
    tokio::pin!(pending);
    let wait_pid = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = fs::read_to_string(test.dir.path().join("worker-pid")) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "encoder did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let pid = tokio::select! { _=&mut pending=>panic!("encoder was not held"), pid=wait_pid=>pid };
    unsafe {
        libc::kill(test.process.id() as i32, libc::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(6), &mut pending)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        if let Some(status) = test.process.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "shutdown timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "encoder outlived shutdown"
    );
    test.wait_clean().await;
}
