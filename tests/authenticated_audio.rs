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
    proof,
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
struct Harness {
    dir: TempDir,
    fixture: PathBuf,
    state: Arc<Upstream>,
    base: String,
    process: Child,
    task: tokio::task::JoinHandle<()>,
    http: reqwest::Client,
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
        let dir = tempfile::tempdir().unwrap();
        let fixture = dir.path().join("fixture.flac");
        proof::fixture_at_rate(&fixture, 6 * rate + 317, 2, 24, rate).unwrap();
        let state = Arc::new(Upstream {
            data: Mutex::new(fs::read(&fixture).unwrap()),
            ..Default::default()
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_url = format!("http://{}", listener.local_addr().unwrap());
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
        let process = Command::new(env!("CARGO_BIN_EXE_sidecar-access"))
            .arg("--config")
            .arg(config)
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
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let base = loop {
            if let Ok(value) =
                serde_json::from_slice::<Value>(&fs::read(dir.path().join("service.log")).unwrap())
            {
                if let Some(url) = value["listen"].as_str() {
                    break url.to_owned();
                }
            }
            assert!(Instant::now() < deadline, "service did not start");
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        Self {
            dir,
            fixture,
            state,
            base,
            process,
            task,
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
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
}

async fn audio_contract(rate: u64) {
    let test = Harness::new(rate).await;
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
    audio_contract(44100).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_48000() {
    audio_contract(48000).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn audio_96000() {
    audio_contract(96000).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cached_audio_is_private_reauthorized_and_recovers_after_outage() {
    let test = Harness::new(48000).await;
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
