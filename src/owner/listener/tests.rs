use super::*;
use crate::owner::{setup::Settings, Config, Service};
use axum::body::{to_bytes, Body};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::net::TcpListener;

async fn call(
    router: Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    router
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
async fn value(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap()
}
fn settings(url: &str) -> Settings {
    serde_json::from_value(json!({
        "navidrome_url":url,
        "listeners":[{"url":"https://listener.test/music","trust":"https_proxy"}]
    }))
    .unwrap()
}
fn save(store: &Store, url: &str) {
    store
        .save_setup(
            &settings(url),
            &store.config().unwrap().setup.revision,
            None,
        )
        .unwrap();
}
fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::initialize(&dir.path().join("owner"), &Config::default()).unwrap();
    (dir, store)
}
struct Upstream {
    url: String,
    calls: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Upstream {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!(
            "http://localhost:{}/music",
            listener.local_addr().unwrap().port()
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = Router::new().fallback(move |request: Request| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                assert_eq!(request.uri().path(), "/music/rest/ping.view");
                assert!(request.uri().query().is_none());
                let body = to_bytes(request.into_body(), 4096).await.unwrap();
                let parsed = reqwest::Url::parse(&format!("http://unused/?{}", String::from_utf8_lossy(&body))).unwrap();
                let valid = parsed.query_pairs().any(|(key, value)| key == "u" && value == "alice");
                Json(json!({"subsonic-response":if valid {json!({"status":"ok"})} else {json!({"status":"failed","error":{"code":40}})}}))
            }
        });
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { url, calls, task }
    }
}
async fn login(listener: &Listener) -> String {
    let response = call(
        listener.router(),
        "POST",
        "/access/v1/sessions",
        None,
        json!({"username":"alice","token":"a".repeat(32),"salt":"disposable"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    value(response).await["session"].as_str().unwrap().into()
}

#[tokio::test]
async fn published_path_is_public_but_owner_and_listener_credentials_are_separate() {
    let (_dir, store) = store();
    let service = Service::new(store.clone()).unwrap();
    let response = call(
        service.router(),
        "GET",
        "/stratum/access/v1/capabilities",
        None,
        Value::Null,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let capabilities = value(response).await;
    assert_eq!(capabilities["ready"], false);
    assert_eq!(
        capabilities["installation"],
        store.identity().unwrap().installation_id
    );
    assert_eq!(
        capabilities["spkiSha256"],
        store.identity().unwrap().spki_sha256
    );
    assert!(capabilities.get("navidrome_url").is_none());
    assert_eq!(
        call(
            service.router(),
            "POST",
            "/stratum/access/v1/sessions",
            None,
            json!({})
        )
        .await
        .status(),
        503
    );
    for (name, value) in [
        (header::COOKIE, "__Host-sidecar-owner=pretend"),
        (header::ORIGIN, "https://localhost:7843"),
    ] {
        let request = Request::builder()
            .uri("/stratum/access/v1/capabilities")
            .header(name, value)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            service.router().oneshot(request).await.unwrap().status(),
            400
        );
    }
    // Public listener Host values do not expand the admin Host allowlist.
    let request = Request::builder()
        .uri("/admin/api/status")
        .header(header::HOST, "listener.test")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        service.router().oneshot(request).await.unwrap().status(),
        403
    );
    let request = Request::builder()
        .uri("/admin/api/setup")
        .header(header::HOST, "localhost:7843")
        .header(header::AUTHORIZATION, "Bearer pretend")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        service.router().oneshot(request).await.unwrap().status(),
        401
    );
}

#[tokio::test]
async fn settings_changes_revoke_sessions_without_forwarding_them_to_the_new_upstream() {
    let a = Upstream::new().await;
    let b = Upstream::new().await;
    let (_dir, store) = store();
    store
        .set_password("disposable owner password", false)
        .unwrap();
    save(&store, &a.url);
    let listener = Listener::new(store.clone()).unwrap();
    let old = login(&listener).await;
    assert_eq!(a.calls.load(Ordering::SeqCst), 2);
    save(&store, &b.url);
    assert_eq!(
        call(
            listener.router(),
            "DELETE",
            "/access/v1/session",
            Some(&old),
            Value::Null
        )
        .await
        .status(),
        401
    );
    assert_eq!(b.calls.load(Ordering::SeqCst), 0);
    login(&listener).await;
    assert_eq!(b.calls.load(Ordering::SeqCst), 2);
    // Returning to the first server must not bring its old bearer back.
    save(&store, &a.url);
    assert_eq!(
        call(
            listener.router(),
            "DELETE",
            "/access/v1/session",
            Some(&old),
            Value::Null
        )
        .await
        .status(),
        401
    );
    let fresh = login(&listener).await;
    // A -> B -> A between refreshes is still a new saved configuration.
    save(&store, &b.url);
    save(&store, &a.url);
    assert_eq!(
        call(
            listener.router(),
            "DELETE",
            "/access/v1/session",
            Some(&fresh),
            Value::Null
        )
        .await
        .status(),
        401
    );
    let current = login(&listener).await;
    // A real restart stops the old process before acquiring its working directory.
    listener.stop().await;
    drop(listener);
    let restarted = Listener::new(store).unwrap();
    assert_eq!(
        call(
            restarted.router(),
            "DELETE",
            "/access/v1/session",
            Some(&current),
            Value::Null
        )
        .await
        .status(),
        401
    );
    restarted.stop().await;
}

#[tokio::test]
async fn invalid_configuration_stops_the_old_service_and_recovery_requires_new_sign_in() {
    let upstream = Upstream::new().await;
    let (dir, store) = store();
    store
        .set_password("disposable owner password", false)
        .unwrap();
    save(&store, &upstream.url);
    let listener = Listener::new(store).unwrap();
    let token = login(&listener).await;
    let config = dir.path().join("owner/config.json");
    let valid = std::fs::read(&config).unwrap();
    std::fs::write(&config, "{}").unwrap();
    assert_eq!(
        call(
            listener.router(),
            "GET",
            "/access/v1/capabilities",
            None,
            Value::Null
        )
        .await
        .status(),
        503
    );
    std::fs::write(&config, valid).unwrap();
    assert_eq!(
        call(
            listener.router(),
            "DELETE",
            "/access/v1/session",
            Some(&token),
            Value::Null
        )
        .await
        .status(),
        401
    );
    login(&listener).await;
    listener.stop().await;
    assert_eq!(
        call(
            listener.router(),
            "GET",
            "/access/v1/capabilities",
            None,
            Value::Null
        )
        .await
        .status(),
        503
    );
}

#[tokio::test]
async fn idle_maintenance_notices_cli_changes_and_disabled_setup() {
    let upstream = Upstream::new().await;
    let (_dir, store) = store();
    store
        .set_password("disposable owner password", false)
        .unwrap();
    save(&store, &upstream.url);
    let listener = Listener::new(store.clone()).unwrap();
    login(&listener).await;
    let old = listener.refresh().await.unwrap().unwrap();
    let stop = CancellationToken::new();
    let worker = listener.clone();
    let token = stop.clone();
    let task = tokio::spawn(async move { worker.maintain(token).await });
    store
        .save_setup(
            &Settings::default(),
            &store.config().unwrap().setup.revision,
            None,
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let response = call(
                old.router(),
                "GET",
                "/access/v1/capabilities",
                None,
                Value::Null,
            )
            .await;
            if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    stop.cancel();
    task.await.unwrap();
    assert_eq!(
        value(
            call(
                listener.router(),
                "GET",
                "/access/v1/capabilities",
                None,
                Value::Null
            )
            .await
        )
        .await["ready"],
        false
    );
}

#[tokio::test]
async fn cancelling_a_configuration_read_keeps_its_blocking_work_bounded() {
    let (dir, store) = store();
    let listener = Listener::new(store).unwrap();
    let lock = std::fs::File::open(dir.path().join("owner/state.lock")).unwrap();
    fs2::FileExt::lock_exclusive(&lock).unwrap();
    let worker = listener.clone();
    let task = tokio::spawn(async move { worker.refresh().await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while listener.0.state.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert!(
        listener.0.state.try_lock().is_err(),
        "cancelled reader released its work gate too soon"
    );
    fs2::FileExt::unlock(&lock).unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while listener.0.state.try_lock().is_err() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
