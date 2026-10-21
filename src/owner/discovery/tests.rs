use super::*;
use axum::{
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct FakeState {
    radios: Vec<Value>,
    serial: u32,
    mutations: u32,
    login_mode: String,
    lose_reply: bool,
    no_retention: bool,
    seen: Vec<String>,
}
#[derive(Clone, Default)]
struct Fake(
    Arc<Mutex<FakeState>>,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
);
struct Server {
    base: String,
    fake: Fake,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn fake_server() -> Server {
    let fake = Fake::default();
    let app=Router::new()
        .route("/music/rest/ping.view",post(||async{Json(json!({"subsonic-response":{"type":"navidrome","status":"failed","error":{"code":10}}}))}))
        .route("/music/auth/login",post(|State(fake):State<Fake>,Json(body):Json<Value>|async move {
            assert_eq!(body["username"],"owner");assert_eq!(body["password"],"nav-secret");
            let delayed = fake.0.lock().unwrap().login_mode == "delay";
            if delayed { fake.1.notify_one(); fake.2.notified().await; }
            let mut state=fake.0.lock().unwrap();state.seen.push("login".into());
            if state.login_mode=="limited" {return (axum::http::StatusCode::TOO_MANY_REQUESTS,"slow down").into_response()}
            if state.login_mode=="wrong" {return (axum::http::StatusCode::UNAUTHORIZED,Json(json!({"error":"do not display SECRET"}))).into_response()}
            Json(json!({"username":"owner","isAdmin":state.login_mode!="ordinary","subsonicToken":"transient-token","subsonicSalt":"transient-salt","token":"unused-native-token"})).into_response()
        }))
        .route("/music/rest/{method}",post(radio_api)).with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/music", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Server { base, fake, task }
}
async fn radio_api(
    State(fake): State<Fake>,
    axum::extract::Path(method): axum::extract::Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let form: HashMap<String, String> = reqwest::Url::parse(&format!(
        "http://fixture/?{}",
        std::str::from_utf8(&body).unwrap()
    ))
    .unwrap()
    .query_pairs()
    .into_owned()
    .collect();
    assert!(!headers.contains_key("authorization") && !headers.contains_key("cookie"));
    assert_eq!(form.get("t").unwrap(), "transient-token");
    assert_eq!(form.get("s").unwrap(), "transient-salt");
    assert!(!form.contains_key("p"));
    let mut state = fake.0.lock().unwrap();
    state.seen.push(method.clone());
    if method == "getInternetRadioStations.view" {
        return Json(json!({"subsonic-response":{"type":"navidrome","status":"ok","internetRadioStations":{"internetRadioStation":state.radios}}})).into_response();
    }
    state.mutations += 1;
    if state.no_retention {
        return Json(json!({"subsonic-response":{"type":"navidrome","status":"ok"}}))
            .into_response();
    }
    match method.as_str() {
        "createInternetRadioStation.view" => {
            state.serial += 1;
            let id = format!("station-{}", state.serial);
            state.radios.push(json!({"id":id,"name":form["name"],"streamUrl":form["streamUrl"],"homePageUrl":form["homepageUrl"]}));
        }
        "updateInternetRadioStation.view" => {
            let row = state
                .radios
                .iter_mut()
                .find(|r| r["id"] == form["id"])
                .unwrap();
            *row = json!({"id":form["id"],"name":form["name"],"streamUrl":form["streamUrl"],"homePageUrl":form["homepageUrl"]});
        }
        "deleteInternetRadioStation.view" => state.radios.retain(|r| r["id"] != form["id"]),
        _ => panic!("unexpected method"),
    }
    if state.lose_reply {
        state.lose_reply = false;
        return (axum::http::StatusCode::BAD_GATEWAY, "lost response").into_response();
    }
    Json(json!({"subsonic-response":{"type":"navidrome","status":"ok"}})).into_response()
}
fn credentials() -> Credentials {
    Credentials {
        username: "owner".into(),
        password: "nav-secret".into(),
    }
}
fn fixture(base: &str) -> (tempfile::TempDir, Store) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sidecar");
    let store = Store::initialize(&path, &super::super::Config::default()).unwrap();
    store
        .set_password("disposable owner password", false)
        .unwrap();
    store
        .save_setup(
            &Settings {
                navidrome_url: Some(base.into()),
                listeners: vec![
                    Endpoint {
                        url: "https://sidecar.lan:7843/prefix".into(),
                        trust: super::super::setup::EndpointTrust::SidecarKey,
                    },
                    Endpoint {
                        url: "https://public.example/stratum".into(),
                        trust: super::super::setup::EndpointTrust::HttpsProxy,
                    },
                ],
            },
            "initial",
            None,
        )
        .unwrap();
    (directory, store)
}
fn operation(store: &Store) -> Operation {
    store
        .begin_discovery(
            &store.config().unwrap().setup.revision,
            &store.discovery().unwrap().revision,
            None,
        )
        .unwrap()
}
async fn run(
    store: &Store,
    action: Action,
    recreate: bool,
) -> std::result::Result<Discovery, Error> {
    operation(store)
        .run(action, credentials(), recreate, CancellationToken::new())
        .await
}

#[tokio::test]
async fn publish_update_remove_verifies_the_same_station_and_keeps_other_radio() {
    let server = fake_server().await;
    let (_dir, store) = fixture(&server.base);
    server.fake.0.lock().unwrap().radios.push(json!({"id":"normal","name":"Ordinary radio","streamUrl":"https://radio.example/live","homePageUrl":"https://radio.example"}));
    let published = run(&store, Action::Publish, false).await.unwrap();
    let id = published.record.as_ref().unwrap().station_id.clone();
    assert_eq!(
        published.view(&store.config().unwrap().setup, &store.identity().unwrap())["status"],
        "published"
    );
    assert!(published.pending.is_none());
    let rows = server.fake.0.lock().unwrap().radios.clone();
    let radio: Radio = serde_json::from_value(rows[1].clone()).unwrap();
    assert!(
        radio.manifest().unwrap().endpoints == store.config().unwrap().setup.settings.listeners
    );
    assert!(radio
        .stream_url
        .ends_with("/prefix/stratum/access/v1/capabilities"));
    let mut settings = store.config().unwrap().setup;
    settings.settings.listeners[0].url = "https://new.lan:7843".into();
    store
        .save_setup(&settings.settings, &settings.revision, None)
        .unwrap();
    assert_eq!(
        store
            .discovery()
            .unwrap()
            .view(&store.config().unwrap().setup, &store.identity().unwrap())["status"],
        "update-needed"
    );
    let updated = run(&store, Action::Publish, false).await.unwrap();
    assert_eq!(updated.record.unwrap().station_id, id);
    run(&store, Action::Publish, false).await.unwrap();
    assert_eq!(
        server.fake.0.lock().unwrap().mutations,
        2,
        "no-op publish must not write"
    );
    let removed = run(&store, Action::Remove, false).await.unwrap();
    assert!(removed.record.is_none() && removed.manual);
    assert_eq!(server.fake.0.lock().unwrap().radios.len(), 1);
    assert_eq!(server.fake.0.lock().unwrap().radios[0]["id"], "normal");
}
#[tokio::test]
async fn lost_create_and_delete_replies_reconcile_after_restart_without_duplicates() {
    let server = fake_server().await;
    let (dir, store) = fixture(&server.base);
    server.fake.0.lock().unwrap().lose_reply = true;
    assert_eq!(
        run(&store, Action::Publish, false).await.err(),
        Some(Error::Unverified)
    );
    assert!(store.discovery().unwrap().pending.is_some());
    let reopened = Store::open(&dir.path().join("sidecar")).unwrap();
    run(&reopened, Action::Publish, false).await.unwrap();
    assert_eq!(server.fake.0.lock().unwrap().mutations, 1);
    assert_eq!(server.fake.0.lock().unwrap().radios.len(), 1);
    server.fake.0.lock().unwrap().lose_reply = true;
    assert_eq!(
        run(&store, Action::Remove, false).await.err(),
        Some(Error::Unverified)
    );
    let removed = run(&store, Action::Remove, false).await.unwrap();
    assert!(removed.record.is_none() && removed.pending.is_none());
    assert_eq!(server.fake.0.lock().unwrap().mutations, 2);
    for file in ["config.json", "private.json", "discovery.json"] {
        let text = std::fs::read_to_string(dir.path().join("sidecar").join(file)).unwrap();
        for secret in [
            "nav-secret",
            "transient-token",
            "transient-salt",
            "unused-native-token",
        ] {
            assert!(!text.contains(secret));
        }
    }
}
#[tokio::test]
async fn rejected_or_nonadmin_signin_never_writes_and_manual_setup_stays_available() {
    for (mode, error) in [
        ("wrong", Error::Authentication),
        ("ordinary", Error::NotAdmin),
        ("limited", Error::RateLimited),
    ] {
        let server = fake_server().await;
        let (_dir, store) = fixture(&server.base);
        server.fake.0.lock().unwrap().login_mode = mode.into();
        assert_eq!(run(&store, Action::Publish, false).await.err(), Some(error));
        assert_eq!(server.fake.0.lock().unwrap().mutations, 0);
        assert!(store.discovery().unwrap().pending.is_none());
        assert!(operation(&store).manual().unwrap().manual);
        assert_eq!(server.fake.0.lock().unwrap().seen, vec!["login"]);
    }
}
#[tokio::test]
async fn removed_station_needs_explicit_recreation_and_reused_id_is_never_overwritten() {
    let server = fake_server().await;
    let (_dir, store) = fixture(&server.base);
    let first = run(&store, Action::Publish, false).await.unwrap();
    let first_id = first.record.unwrap().station_id;
    server.fake.0.lock().unwrap().radios.clear();
    assert_eq!(
        run(&store, Action::Publish, false).await.err(),
        Some(Error::Missing)
    );
    assert_eq!(server.fake.0.lock().unwrap().mutations, 1);
    let next = run(&store, Action::Publish, true).await.unwrap();
    assert_ne!(next.record.unwrap().station_id, first_id);
    server.fake.0.lock().unwrap().radios[0]["homePageUrl"] = json!("https://unrelated.example");
    assert_eq!(
        run(&store, Action::Publish, false).await.err(),
        Some(Error::Ownership)
    );
    assert_eq!(
        run(&store, Action::Remove, false).await.err(),
        Some(Error::Ownership)
    );
    assert_eq!(server.fake.0.lock().unwrap().mutations, 2);
}
#[tokio::test]
async fn duplicates_and_changed_backend_are_not_silently_claimed() {
    let server = fake_server().await;
    let (_dir, store) = fixture(&server.base);
    run(&store, Action::Publish, false).await.unwrap();
    {
        let mut fake = server.fake.0.lock().unwrap();
        let mut second = fake.radios[0].clone();
        second["id"] = json!("duplicate");
        fake.radios.push(second);
    }
    assert_eq!(
        run(&store, Action::Publish, false).await.err(),
        Some(Error::Duplicate)
    );
    assert_eq!(
        run(&store, Action::Remove, false).await.err(),
        Some(Error::Duplicate)
    );
    let mut setup = store.config().unwrap().setup;
    setup.settings.navidrome_url = Some("http://other:4533".into());
    store
        .save_setup(&setup.settings, &setup.revision, None)
        .unwrap();
    assert_eq!(
        run(&store, Action::Publish, false).await.err(),
        Some(Error::PreviousServer)
    );
    assert_eq!(server.fake.0.lock().unwrap().mutations, 1);
}
#[test]
fn revisions_and_operation_lock_cover_cli_web_edits_and_recovery() {
    let (_dir, store) = fixture("http://navi:4533");
    let setup = store.config().unwrap().setup;
    let operation = operation(&store);
    assert!(store
        .save_setup(&setup.settings, &setup.revision, None)
        .is_err());
    assert!(store
        .begin_discovery(&setup.revision, "initial", None)
        .is_err());
    store
        .set_password("new disposable owner password", true)
        .unwrap();
    assert_eq!(
        operation.authorize(&CancellationToken::new()),
        Err(Error::Interrupted)
    );
    drop(operation);
    let state = super::tests::operation(&store).manual().unwrap();
    assert!(store
        .begin_discovery(&setup.revision, "initial", None)
        .is_err());
    assert_ne!(state.revision, "initial");
}
#[test]
fn malformed_manifest_and_station_names_cannot_prove_ownership() {
    let (_dir, store) = fixture("http://navi:4533");
    let setup = store.config().unwrap().setup;
    let manifest = Manifest::new(&setup.settings, &store.identity().unwrap()).unwrap();
    let target = Record {
        navidrome_url: "http://navi:4533".into(),
        station_id: None,
        manifest: manifest.clone(),
    };
    let row = Radio {
        id: "other".into(),
        name: manifest.name(),
        stream_url: manifest.stream_url(),
        homepage: "https://unrelated".into(),
    };
    assert!(select(&[row], &target, Action::Publish, false).is_err());
    for homepage in [
        manifest.descriptor().replace("stratum-v1=", "stratum-v2="),
        format!("{}#{MARKER}%7Bbroken", manifest.stream_url()),
        "x".repeat(16385),
    ] {
        let row = Radio {
            id: "other".into(),
            name: manifest.name(),
            stream_url: manifest.stream_url(),
            homepage,
        };
        assert!(row.manifest().is_none());
    }
}

#[tokio::test]
async fn cancellation_and_password_reset_before_remote_write_do_not_publish() {
    for reset in [false, true] {
        let server = fake_server().await;
        let (_dir, store) = fixture(&server.base);
        server.fake.0.lock().unwrap().login_mode = "delay".into();
        let operation = operation(&store);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            operation
                .run(Action::Publish, credentials(), false, token)
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), server.fake.1.notified())
            .await
            .unwrap();
        if reset {
            store.set_password("another owner password", true).unwrap();
        } else {
            cancel.cancel();
        }
        server.fake.2.notify_one();
        assert_eq!(task.await.unwrap().err(), Some(Error::Interrupted));
        assert_eq!(server.fake.0.lock().unwrap().mutations, 0);
        assert!(store.discovery().unwrap().pending.is_none());
    }
}

#[tokio::test]
async fn successful_write_reply_without_retained_change_does_not_claim_publication() {
    let server = fake_server().await;
    let (_dir, store) = fixture(&server.base);
    server.fake.0.lock().unwrap().no_retention = true;
    assert_eq!(
        run(&store, Action::Publish, false).await.err(),
        Some(Error::Unverified)
    );
    let state = store.discovery().unwrap();
    assert!(state.record.is_none() && state.pending.is_some());
    assert_eq!(
        state.view(&store.config().unwrap().setup, &store.identity().unwrap())["status"],
        "unverified"
    );
    server.fake.0.lock().unwrap().no_retention = false;
    assert!(run(&store, Action::Publish, false)
        .await
        .unwrap()
        .pending
        .is_none());
}

fn changed_listeners(store: &Store) -> Vec<Endpoint> {
    let mut listeners = store.config().unwrap().setup.settings.listeners;
    listeners[0].url = "https://corrected.lan:7843".into();
    listeners
}

#[tokio::test]
async fn address_update_requires_admin_and_commits_only_after_readback() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let server = fake_server().await;
    let (_dir, store) = fixture(&server.base);
    let published = run(&store, Action::Publish, false).await.unwrap();
    let before = store.config().unwrap().setup;
    let listeners = changed_listeners(&store);
    let service = super::super::Service::new(store.clone()).unwrap();
    let request = |path: &str, cookie: Option<&str>, body: Value| {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("Host", "localhost:7843")
            .header("Origin", "https://localhost:7843")
            .header("Content-Type", "application/json");
        if let Some(cookie) = cookie {
            builder = builder.header("Cookie", cookie);
        }
        builder.body(Body::from(body.to_string())).unwrap()
    };
    let login = service
        .router()
        .oneshot(request(
            "/admin/api/login",
            None,
            json!({"password":"disposable owner password"}),
        ))
        .await
        .unwrap();
    assert_eq!(login.status(), 200);
    let cookie = login.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let no_admin = service
        .router()
        .oneshot(request(
            "/admin/api/setup/listeners",
            Some(&cookie),
            json!({"revision":before.revision,"listeners":listeners}),
        ))
        .await
        .unwrap();
    assert_eq!(no_admin.status(), 400);
    assert_eq!(store.config().unwrap().setup.revision, before.revision);
    assert_eq!(server.fake.0.lock().unwrap().mutations, 1);
    let update = service
        .router()
        .oneshot(request(
            "/admin/api/discovery",
            Some(&cookie),
            json!({"action":"publish", "setup_revision":before.revision,
            "discovery_revision":published.revision, "listeners":listeners,
            "username":"owner", "password":"nav-secret"}),
        ))
        .await
        .unwrap();
    assert_eq!(update.status(), 200);
    let value: Value =
        serde_json::from_slice(&to_bytes(update.into_body(), 16384).await.unwrap()).unwrap();
    assert_eq!(value["discovery"]["status"], "published");
    assert!(store.config().unwrap().setup.settings.listeners == listeners);
    let record = store.discovery().unwrap().record.unwrap();
    assert!(record.manifest.endpoints == listeners);
    assert_eq!(record.station_id, published.record.unwrap().station_id);
    assert_eq!(server.fake.0.lock().unwrap().radios.len(), 1);
    assert_eq!(server.fake.0.lock().unwrap().mutations, 2);
}

#[tokio::test]
async fn failed_address_update_keeps_local_settings_and_retry_reconciles_remote_write() {
    for mode in ["wrong", "ordinary", "unretained", "lost"] {
        let server = fake_server().await;
        let (dir, store) = fixture(&server.base);
        run(&store, Action::Publish, false).await.unwrap();
        let before = store.config().unwrap().setup;
        let listeners = changed_listeners(&store);
        {
            let mut state = server.fake.0.lock().unwrap();
            state.login_mode = mode.into();
            state.no_retention = mode == "unretained";
            state.lose_reply = mode == "lost";
        }
        let result = operation(&store)
            .with_listeners(listeners.clone())
            .unwrap()
            .run(
                Action::Publish,
                credentials(),
                false,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            result.err(),
            Some(match mode {
                "wrong" => Error::Authentication,
                "ordinary" => Error::NotAdmin,
                _ => Error::Unverified,
            })
        );
        assert_eq!(store.config().unwrap().setup.revision, before.revision);
        assert!(store.config().unwrap().setup.settings.listeners == before.settings.listeners);
        {
            let mut state = server.fake.0.lock().unwrap();
            state.login_mode.clear();
            state.no_retention = false;
        }
        let reopened = Store::open(&dir.path().join("sidecar")).unwrap();
        operation(&reopened)
            .with_listeners(listeners.clone())
            .unwrap()
            .run(
                Action::Publish,
                credentials(),
                false,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(reopened.config().unwrap().setup.settings.listeners == listeners);
        assert!(
            reopened
                .discovery()
                .unwrap()
                .record
                .unwrap()
                .manifest
                .endpoints
                == listeners
        );
        assert!(reopened.discovery().unwrap().pending.is_none());
        let expected_mutations = if mode == "unretained" { 3 } else { 2 };
        assert_eq!(
            server.fake.0.lock().unwrap().mutations,
            expected_mutations,
            "{mode}"
        );
        assert_eq!(server.fake.0.lock().unwrap().radios.len(), 1);
        for file in ["config.json", "private.json", "discovery.json"] {
            let content = std::fs::read_to_string(dir.path().join("sidecar").join(file)).unwrap();
            for secret in ["nav-secret", "transient-token", "transient-salt"] {
                assert!(!content.contains(secret));
            }
        }
    }
}

#[tokio::test]
async fn address_update_holds_edit_lock_and_cancellation_leaves_saved_addresses() {
    let server = fake_server().await;
    let (_dir, store) = fixture(&server.base);
    run(&store, Action::Publish, false).await.unwrap();
    let before = store.config().unwrap().setup;
    server.fake.0.lock().unwrap().login_mode = "delay".into();
    let cancel = CancellationToken::new();
    let operation = operation(&store)
        .with_listeners(changed_listeners(&store))
        .unwrap();
    let task = tokio::spawn(operation.run(Action::Publish, credentials(), false, cancel.clone()));
    server.fake.1.notified().await;
    assert_eq!(store.config().unwrap().setup.revision, before.revision);
    assert_eq!(
        store
            .save_setup(&before.settings, &before.revision, None)
            .err()
            .unwrap()
            .downcast_ref::<Error>(),
        Some(&Error::Busy)
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap().err(), Some(Error::Interrupted));
    server.fake.2.notify_one();
    assert!(store.config().unwrap().setup.settings.listeners == before.settings.listeners);
    assert_eq!(server.fake.0.lock().unwrap().mutations, 1);
}
