use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;
use tower::ServiceExt;

#[derive(Default)]
struct Mock {
    requests: AtomicUsize,
    song_error: AtomicUsize,
    stream_error: AtomicUsize,
    streams: AtomicUsize,
    fetches: AtomicUsize,
    songs: AtomicUsize,
    mode: AtomicUsize,
    hold: AtomicUsize,
    hold_login: AtomicUsize,
    login_error: AtomicUsize,
    reached: Notify,
    release: Notify,
    redirect: Mutex<String>,
}
fn ok(value: Value) -> Response {
    Json(json!({"subsonic-response": value})).into_response()
}
fn refused(code: usize) -> Response {
    ok(json!({"status":"failed", "error":{"code":code,"message":"DO-NOT-REFLECT-UPSTREAM"}}))
}
async fn fake(State(state): State<Arc<Mock>>, request: Request) -> Response {
    state.requests.fetch_add(1, Ordering::SeqCst);
    let path = request.uri().path().to_owned();
    assert!(request.uri().query().is_none()); // Secrets travel in form bodies, never URLs.
    let range = request
        .headers()
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let body = to_bytes(request.into_body(), 4096).await.unwrap();
    let parsed = Url::parse(&format!(
        "http://unused/?{}",
        String::from_utf8(body.to_vec()).unwrap()
    ))
    .unwrap();
    let params: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    let mode = state.mode.load(Ordering::SeqCst);
    if mode == 1 {
        return (
            StatusCode::FOUND,
            [(header::LOCATION, state.redirect.lock().unwrap().clone())],
        )
            .into_response();
    }
    if mode != 2
        && (params
            .get("u")
            .is_none_or(|u| !["alice", "bob"].contains(&u.as_str()))
            || params.get("t").is_none_or(|v| v != &"a".repeat(32)))
    {
        return refused(40);
    }
    if path.ends_with("/ping.view") {
        if state.hold_login.load(Ordering::SeqCst) != 0 {
            state.reached.notify_one();
            state.release.notified().await;
        }
        if state.login_error.load(Ordering::SeqCst) != 0 {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        return ok(json!({"status":"ok"}));
    }
    if path.ends_with("/getSong.view") {
        state.songs.fetch_add(1, Ordering::SeqCst);
        let error = state.song_error.load(Ordering::SeqCst);
        if error != 0 {
            return refused(error);
        }
        if mode == 6 {
            return Body::from(vec![b'x'; JSON_LIMIT + 1]).into_response();
        }
        return ok(json!({"status":"ok", "song":{"id":params["id"],"size":1000}}));
    }
    if path.ends_with("/stream.view") {
        state.streams.fetch_add(1, Ordering::SeqCst);
        if state.stream_error.load(Ordering::SeqCst) != 0 {
            return refused(state.stream_error.load(Ordering::SeqCst));
        }
        let length = if range == "bytes=0-0" {
            1
        } else {
            assert_eq!(range, "bytes=0-63");
            PREFIX_BYTES
        };
        if length == PREFIX_BYTES {
            state.fetches.fetch_add(1, Ordering::SeqCst);
            if state.hold.load(Ordering::SeqCst) != 0 {
                state.reached.notify_one();
                state.release.notified().await;
            }
        }
        let mut response =
            Body::from(vec![b'f'; if mode == 5 { length - 1 } else { length }]).into_response();
        *response.status_mut() = if mode == 3 {
            StatusCode::OK
        } else {
            StatusCode::PARTIAL_CONTENT
        };
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            if mode == 4 { "text/html" } else { "audio/flac" }
                .parse()
                .unwrap(),
        );
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            format!("bytes 0-{}/1000", length - 1).parse().unwrap(),
        );
        return response;
    }
    panic!("unexpected fake route");
}
struct TestServer {
    mock: Arc<Mock>,
    service: Service,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for TestServer {
    fn drop(&mut self) {
        self.service.clear();
        self.task.abort();
    }
}
fn config(upstream: String) -> Config {
    Config {
        upstream,
        listen: default_listen(),
        session_seconds: 600,
        run_seconds: 60,
        connection_seconds: 1,
    }
}
async fn setup() -> TestServer {
    let mock = Arc::new(Mock::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let service = Service::new(&config(format!(
        "http://{}/nested",
        listener.local_addr().unwrap()
    )))
    .unwrap();
    let app = Router::new().fallback(fake).with_state(mock.clone());
    let task = tokio::spawn(async {
        axum::serve(listener, app).await.unwrap();
    });
    TestServer {
        mock,
        service,
        task,
    }
}
async fn call(
    service: &Service,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> Response {
    let mut builder = axum::http::Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    service
        .router()
        .oneshot(builder.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
async fn login(service: &Service, username: &str) -> String {
    let response = call(
        service,
        "POST",
        "/access/v1/sessions",
        None,
        json!({"username":username,"token":"a".repeat(32),"salt":"test-salt"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let json: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    json["session"].as_str().unwrap().into()
}
async fn bytes(response: Response) -> Bytes {
    to_bytes(response.into_body(), 4096).await.unwrap()
}

#[test]
fn rejects_nonlocal_or_credentialed_configuration() {
    for url in [
        "http://example.com",
        "http://192.168.1.2",
        "http://localhost",
        "https://127.0.0.1",
        "http://u:p@127.0.0.1",
        "http://127.0.0.1?secret=x",
        "http://127.0.0.1#x",
    ] {
        assert!(Service::new(&config(url.into())).is_err());
    }
    let mut value = config("http://127.0.0.1/nested/".into());
    assert!(Service::new(&value).is_ok());
    value.listen = "0.0.0.0:0".parse().unwrap();
    assert!(Service::new(&value).is_err());
}

#[tokio::test]
async fn cache_and_session_caps_hold_and_shutdown_cannot_issue_sessions() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    for i in 0..=MAX_TRACKS {
        assert_eq!(
            call(
                &test.service,
                "GET",
                &format!("/access/v1/media/t{i}"),
                Some(&token),
                Value::Null
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert!(
            test.service.0.sessions.lock().unwrap()[&token]
                .prefixes
                .lock()
                .unwrap()
                .len()
                <= MAX_TRACKS
        );
    }
    {
        let mut sessions = test.service.0.sessions.lock().unwrap();
        let existing = sessions[&token].clone();
        // Fill the store without waiting a minute for the independent login limiter.
        for i in 1..MAX_SESSIONS {
            sessions.insert(format!("{i:064x}"), existing.clone());
        }
    }
    let response = call(
        &test.service,
        "POST",
        "/access/v1/sessions",
        None,
        json!({"username":"alice","token":"a".repeat(32),"salt":"s"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(test.service.0.sessions.lock().unwrap().len(), MAX_SESSIONS);
    test.service.clear();
    assert_eq!(
        call(
            &test.service,
            "POST",
            "/access/v1/sessions",
            None,
            json!({"username":"alice","token":"a".repeat(32),"salt":"s"})
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert!(test.service.0.sessions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn expiry_cancels_fetch_before_it_can_finish() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    {
        let mut sessions = test.service.0.sessions.lock().unwrap();
        let old = sessions.remove(&token).unwrap();
        sessions.insert(
            token.clone(),
            Arc::new(Session::new(
                old.credential(),
                Instant::now() + Duration::from_millis(200),
            )),
        );
    }
    test.mock.hold.store(1, Ordering::SeqCst);
    let service = test.service.clone();
    let pending = tokio::spawn(async move {
        call(
            &service,
            "GET",
            "/access/v1/media/song",
            Some(&token),
            Value::Null,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), test.mock.reached.notified())
        .await
        .unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(test.service.0.sessions.lock().unwrap().is_empty());
    test.mock.release.notify_one();
}

#[tokio::test]
async fn login_cache_and_session_isolation() {
    let test = setup().await;
    let a = login(&test.service, "alice").await;
    let b = login(&test.service, "bob").await;
    assert_ne!(a, b);
    for (token, hit) in [(&a, "0"), (&a, "1"), (&b, "0")] {
        let response = call(
            &test.service,
            "GET",
            "/access/v1/media/song",
            Some(token),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-stratum-cache-hit"], hit);
        assert_eq!(bytes(response).await.len(), PREFIX_BYTES);
    }
    assert_eq!(test.mock.fetches.load(Ordering::SeqCst), 2);
    assert_eq!(test.mock.songs.load(Ordering::SeqCst), 6);
    let other = Service::new(&config(test.service.0.upstream.to_string())).unwrap();
    assert_eq!(
        call(
            &other,
            "GET",
            "/access/v1/media/song",
            Some(&a),
            Value::Null
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn rejects_missing_wrong_and_ambient_credentials_without_streaming() {
    let test = setup().await;
    for token in [
        None,
        Some("bad"),
        Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
    ] {
        assert_eq!(
            call(
                &test.service,
                "GET",
                "/access/v1/media/song",
                token,
                Value::Null
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    for body in [
        json!({"username":"alice","token":"b".repeat(32),"salt":"s"}),
        json!({"username":"alice","token":"a".repeat(32),"salt":"s","upstream":"http://evil"}),
    ] {
        let response = call(&test.service, "POST", "/access/v1/sessions", None, body).await;
        assert!(response.status().is_client_error());
        assert!(!String::from_utf8_lossy(&bytes(response).await).contains("DO-NOT-REFLECT"));
    }
    let token = login(&test.service, "alice").await;
    for (name, value) in [("origin", "http://untrusted"), ("cookie", "ambient=true")] {
        let req = Request::builder()
            .uri("/access/v1/media/song")
            .header(name, value)
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            test.service.router().oneshot(req).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            &test.service,
            "GET",
            "/access/v1/media/song?username=bob",
            Some(&token),
            Value::Null
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(test.mock.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn revoked_permission_blocks_warm_cache_and_clears_it() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    assert_eq!(
        bytes(
            call(
                &test.service,
                "GET",
                "/access/v1/media/song",
                Some(&token),
                Value::Null
            )
            .await
        )
        .await
        .len(),
        64
    );
    test.mock.song_error.store(70, Ordering::SeqCst);
    assert_eq!(
        call(
            &test.service,
            "GET",
            "/access/v1/media/song",
            Some(&token),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert!(test.service.0.sessions.lock().unwrap()[&token]
        .prefixes
        .lock()
        .unwrap()
        .is_empty());
    assert_eq!(test.mock.fetches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stream_auth_error_in_http_200_revokes_session() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    test.mock.stream_error.store(40, Ordering::SeqCst);
    assert_eq!(
        call(
            &test.service,
            "GET",
            "/access/v1/media/song",
            Some(&token),
            Value::Null
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(test.service.0.sessions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn invalid_range_type_and_oversized_metadata_are_not_audio() {
    for mode in [3, 4, 5, 6] {
        let test = setup().await;
        let token = login(&test.service, "alice").await;
        test.mock.mode.store(mode, Ordering::SeqCst);
        let response = call(
            &test.service,
            "GET",
            "/access/v1/media/song",
            Some(&token),
            Value::Null,
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY, "mode {mode}");
        assert!(bytes(response).await.len() < 100);
    }
}

#[tokio::test]
async fn logout_cancels_inflight_and_returned_body() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    let response = call(
        &test.service,
        "GET",
        "/access/v1/media/song",
        Some(&token),
        Value::Null,
    )
    .await;
    test.service.revoke(&token);
    assert!(to_bytes(response.into_body(), 4096).await.is_err());
    let token = login(&test.service, "alice").await;
    test.mock.hold.store(1, Ordering::SeqCst);
    let service = test.service.clone();
    let pending_token = token.clone();
    let pending = tokio::spawn(async move {
        call(
            &service,
            "GET",
            "/access/v1/media/other",
            Some(&pending_token),
            Value::Null,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), test.mock.reached.notified())
        .await
        .unwrap();
    assert_eq!(
        call(
            &test.service,
            "DELETE",
            "/access/v1/session",
            Some(&token),
            Value::Null
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_millis(200), pending)
            .await
            .unwrap()
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    test.mock.release.notify_one();
}

#[tokio::test]
async fn final_check_catches_permission_loss_during_fetch() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    test.mock.hold.store(1, Ordering::SeqCst);
    let service = test.service.clone();
    let pending = tokio::spawn(async move {
        call(
            &service,
            "GET",
            "/access/v1/media/song",
            Some(&token),
            Value::Null,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(2), test.mock.reached.notified())
        .await
        .unwrap();
    test.mock.song_error.store(50, Ordering::SeqCst);
    test.mock.release.notify_one();
    assert_eq!(pending.await.unwrap().status(), StatusCode::NOT_FOUND);
}

#[tokio::test(start_paused = true)]
async fn expiry_cleans_private_bytes_and_rate_limit_recovers() {
    let service = Service::new(&config("http://127.0.0.1:9".into())).unwrap();
    let session = Arc::new(Session::new(
        Credential {
            username: "a".into(),
            token: "a".repeat(32),
            salt: "s".into(),
        },
        Instant::now() + Duration::from_secs(1),
    ));
    session
        .prefixes
        .lock()
        .unwrap()
        .insert("x".into(), Bytes::from_static(b"data"));
    service
        .0
        .sessions
        .lock()
        .unwrap()
        .insert("a".repeat(64), session.clone());
    for _ in 0..30 {
        service.admit_login().unwrap();
    }
    assert_eq!(service.admit_login(), Err(Failure::Busy));
    tokio::time::advance(Duration::from_secs(61)).await;
    service.reap();
    assert!(!session.active());
    assert!(session.prefixes.lock().unwrap().is_empty());
    assert!(service.0.sessions.lock().unwrap().is_empty());
    service.admit_login().unwrap();
}

#[tokio::test]
async fn rejects_proxy_auth_override_and_redirect_without_following() {
    let test = setup().await;
    test.mock.mode.store(2, Ordering::SeqCst);
    let body = json!({"username":"alice","token":"a".repeat(32),"salt":"s"});
    let response = call(
        &test.service,
        "POST",
        "/access/v1/sessions",
        None,
        body.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&bytes(response).await)
        .contains("upstream_does_not_validate_credentials"));
    let sink = setup().await;
    *test.mock.redirect.lock().unwrap() = sink.service.0.upstream.to_string();
    test.mock.mode.store(1, Ordering::SeqCst);
    assert_eq!(
        call(&test.service, "POST", "/access/v1/sessions", None, body)
            .await
            .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(sink.mock.requests.load(Ordering::SeqCst), 0);
    assert!(sink.service.0.sessions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn oversized_body_and_request_saturation_are_bounded() {
    let service = Service::new(&config("http://127.0.0.1:9".into())).unwrap();
    let response = call(
        &service,
        "POST",
        "/access/v1/sessions",
        None,
        json!({"secret":"s".repeat(INPUT_LIMIT+1)}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let permit = service
        .0
        .requests
        .clone()
        .acquire_many_owned(8)
        .await
        .unwrap();
    assert_eq!(
        call(
            &service,
            "GET",
            "/access/v1/capabilities",
            None,
            Value::Null
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(permit);
    assert_eq!(
        call(
            &service,
            "GET",
            "/access/v1/capabilities",
            None,
            Value::Null
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn bounded_listener_closes_slow_headers_and_releases_sockets() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cfg = config("http://127.0.0.1:9".into());
    let service = Service::new(&cfg).unwrap();
    let stop = CancellationToken::new();
    let end = stop.clone();
    let server = tokio::spawn(async move {
        serve(listener, service, &cfg, end).await.unwrap();
    });
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket
        .write_all(b"GET /access/v1/capabilities HTTP/1.1\r\nHost: ")
        .await
        .unwrap();
    let mut byte = [0];
    let result = tokio::time::timeout(Duration::from_secs(2), socket.read(&mut byte))
        .await
        .unwrap();
    assert!(result.is_err() || result.unwrap() == 0);
    let client = Client::builder().no_proxy().build().unwrap();
    assert_eq!(
        client
            .get(format!("http://{address}/access/v1/capabilities"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

async fn renew(service: &Service, token: &str, username: &str, valid: bool) -> Response {
    call(service, "POST", "/access/v1/session", Some(token), json!({
        "username": username, "token": if valid { "a".repeat(32) } else { "b".repeat(32) }, "salt":"fresh-salt"
    })).await
}

#[tokio::test]
async fn renewal_preserves_private_session_and_updates_credentials() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    let session = test.service.0.sessions.lock().unwrap()[&token].clone();
    session
        .prefixes
        .lock()
        .unwrap()
        .insert("retained".into(), Bytes::from_static(b"private"));
    let old_expiry = session.auth.lock().unwrap().expires;
    let response = renew(&test.service, &token, "alice", true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value = serde_json::from_slice(&bytes(response).await).unwrap();
    assert_eq!(value["session"], token);
    assert_eq!(value["expiresInSeconds"], 600);
    assert!(Arc::ptr_eq(
        &session,
        &test.service.0.sessions.lock().unwrap()[&token]
    ));
    assert!(session.auth.lock().unwrap().expires > old_expiry);
    assert_eq!(session.credential().salt, "fresh-salt");
    assert_eq!(session.prefixes.lock().unwrap()["retained"], b"private"[..]);
    assert_eq!(test.service.0.sessions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn renewal_cannot_change_account_or_use_bearer_alone_and_refusal_revokes() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    assert_eq!(
        renew(&test.service, &token, "bob", true).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &test.service,
            "POST",
            "/access/v1/session",
            Some(&token),
            Value::Null
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(test.service.0.sessions.lock().unwrap().len(), 1);
    assert_eq!(
        renew(&test.service, &token, "alice", false).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(test.service.0.sessions.lock().unwrap().is_empty());
    assert_eq!(
        renew(&test.service, &token, "alice", true).await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn renewed_deadline_keeps_an_inflight_fetch_alive_past_old_expiry() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    let session = test.service.0.sessions.lock().unwrap()[&token].clone();
    session.auth.lock().unwrap().expires = Instant::now() + Duration::from_millis(200);
    test.mock.hold.store(1, Ordering::SeqCst);
    let service = test.service.clone();
    let bearer = token.clone();
    let pending = tokio::spawn(async move {
        call(
            &service,
            "GET",
            "/access/v1/media/song",
            Some(&bearer),
            Value::Null,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(1), test.mock.reached.notified())
        .await
        .unwrap();
    assert_eq!(
        renew(&test.service, &token, "alice", true).await.status(),
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(250)).await;
    test.service.reap();
    test.mock.release.notify_one();
    assert_eq!(pending.await.unwrap().status(), StatusCode::OK);
    assert!(session.active());
}

#[tokio::test]
async fn late_renewal_cannot_resurrect_logout_shutdown_or_expiry() {
    for mode in 0..3 {
        let test = setup().await;
        let token = login(&test.service, "alice").await;
        let session = test.service.0.sessions.lock().unwrap()[&token].clone();
        if mode == 2 {
            session.auth.lock().unwrap().expires = Instant::now() + Duration::from_millis(200);
        }
        test.mock.hold_login.store(1, Ordering::SeqCst);
        let service = test.service.clone();
        let bearer = token.clone();
        let pending = tokio::spawn(async move { renew(&service, &bearer, "alice", true).await });
        tokio::time::timeout(Duration::from_secs(1), test.mock.reached.notified())
            .await
            .unwrap();
        match mode {
            0 => test.service.revoke(&token),
            1 => test.service.clear(),
            _ => tokio::time::sleep(Duration::from_millis(250)).await,
        }
        test.mock.release.notify_one();
        let response = pending.await.unwrap();
        assert!([StatusCode::UNAUTHORIZED, StatusCode::SERVICE_UNAVAILABLE]
            .contains(&response.status()));
        test.service.reap();
        assert!(!session.active());
        assert!(test.service.0.sessions.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn renewal_rechecks_permissive_upstream_and_does_not_extend_on_outage() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    let session = test.service.0.sessions.lock().unwrap()[&token].clone();
    let expires = session.auth.lock().unwrap().expires;
    test.mock.login_error.store(1, Ordering::SeqCst);
    assert_eq!(
        renew(&test.service, &token, "alice", true).await.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(session.auth.lock().unwrap().expires, expires);
    test.mock.login_error.store(0, Ordering::SeqCst);
    test.mock.mode.store(2, Ordering::SeqCst);
    assert_eq!(
        renew(&test.service, &token, "alice", true).await.status(),
        StatusCode::BAD_GATEWAY
    );
    assert!(!session.active());
}

#[tokio::test]
async fn renewal_uses_existing_login_rate_limit() {
    let test = setup().await;
    let token = login(&test.service, "alice").await;
    for _ in 1..30 {
        test.service.admit_login().unwrap();
    }
    assert_eq!(
        renew(&test.service, &token, "alice", true).await.status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}
