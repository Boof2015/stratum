use super::*;
use axum::body::{to_bytes, Body};
use serde_json::Value;
use tower::ServiceExt;

const PASSWORD: &str = "test-only owner password";
fn fixture() -> (tempfile::TempDir, Store, Service) {
    let directory = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let store = Store::initialize(directory.path(), &Config::default()).unwrap();
    let service = Service::new(store.clone()).unwrap();
    (directory, store, service)
}
async fn call(
    service: &Service,
    method: &str,
    path: &str,
    cookie: Option<&str>,
    body: Value,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, "localhost:7843")
        .header(header::ORIGIN, "https://localhost:7843")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    service
        .router()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
async fn json(response: Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap()
}
fn session_cookie(response: &Response) -> String {
    response.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .into()
}
async fn enrolled(store: &Store, service: &Service) -> String {
    let response = call(
        service,
        "POST",
        "/admin/api/claim",
        None,
        json!({"code":store.setup_code().unwrap(),"password":PASSWORD}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    session_cookie(&response)
}

#[tokio::test]
async fn enrollment_needs_the_local_code_and_sets_a_protected_cookie() {
    let (_directory, store, service) = fixture();
    let code = store.setup_code().unwrap();
    assert_eq!(
        call(&service, "GET", "/admin/api/setup", None, json!({}))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &service,
            "POST",
            "/admin/api/claim",
            None,
            json!({"code":"0".repeat(64),"password":PASSWORD})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(store.owner_generation().unwrap().is_none());
    let response = call(
        &service,
        "POST",
        "/admin/api/claim",
        None,
        json!({"code":code,"password":PASSWORD}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let header = response.headers()[header::SET_COOKIE].to_str().unwrap();
    for flag in ["__Host-", "Secure", "HttpOnly", "SameSite=Strict", "Path=/"] {
        assert!(header.contains(flag));
    }
    let cookie = session_cookie(&response);
    let response = call(
        &service,
        "GET",
        "/admin/api/setup",
        Some(&cookie),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json(response).await;
    assert_eq!(body["stage"], "owner-ready");
    assert!(body["identity"]["spkiSha256"].as_str().unwrap().len() == 64);
    for secret in ["password", "keyPem", "bootstrap", &code] {
        assert!(!body.to_string().contains(secret));
    }
    assert_eq!(
        call(
            &service,
            "POST",
            "/admin/api/claim",
            None,
            json!({"code":code,"password":"attempted replacement"})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(store.login(PASSWORD).unwrap().is_some());
}

#[tokio::test]
async fn host_origin_and_json_are_required_before_enrollment_can_change() {
    let (_directory, store, service) = fixture();
    let code = store.setup_code().unwrap();
    for (host, origin, content_type) in [
        (
            "evil.example",
            Some("https://localhost:7843"),
            "application/json",
        ),
        (
            "localhost:7843",
            Some("https://evil.example"),
            "application/json",
        ),
        ("localhost:7843", None, "application/json"),
        ("localhost:7843", Some("null"), "application/json"),
        (
            "localhost:7843",
            Some("http://localhost:7843"),
            "application/json",
        ),
        (
            "localhost:7843",
            Some("https://localhost:7843"),
            "text/plain",
        ),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/admin/api/claim")
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        let response = service
            .router()
            .oneshot(
                request
                    .body(Body::from(
                        json!({"code":code,"password":PASSWORD}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_client_error());
        assert!(store.owner_generation().unwrap().is_none());
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
    }
}

#[tokio::test]
async fn local_recovery_invalidates_existing_sessions_without_restarting_service() {
    let (directory, store, service) = fixture();
    let cookie = enrolled(&store, &service).await;
    let other_process = Store::open(directory.path()).unwrap();
    other_process
        .set_password("replacement owner password", true)
        .unwrap();
    assert_eq!(
        call(
            &service,
            "GET",
            "/admin/api/setup",
            Some(&cookie),
            json!({})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &service,
            "POST",
            "/admin/api/login",
            None,
            json!({"password":PASSWORD})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(
        &service,
        "POST",
        "/admin/api/login",
        None,
        json!({"password":"replacement owner password"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        call(
            &service,
            "GET",
            "/admin/api/setup",
            Some(&session_cookie(&response)),
            json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn old_login_finishing_after_password_reset_cannot_authorize() {
    let (_dir, store, service) = fixture();
    store.set_password(PASSWORD, false).unwrap();
    let old_generation = store.login(PASSWORD).unwrap().unwrap();
    store.set_password("new test-only password", true).unwrap();
    let response = service
        .session(old_generation)
        .unwrap_or_else(IntoResponse::into_response);
    let cookie = session_cookie(&response);
    assert_eq!(
        call(
            &service,
            "GET",
            "/admin/api/setup",
            Some(&cookie),
            json!({})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn logout_restart_expiry_and_duplicate_cookies_reject_sessions() {
    let (_directory, store, service) = fixture();
    let cookie = enrolled(&store, &service).await;
    let duplicated = format!("{cookie}; {cookie}");
    assert_eq!(
        call(
            &service,
            "GET",
            "/admin/api/setup",
            Some(&duplicated),
            json!({})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let restarted = Service::new(store.clone()).unwrap();
    assert_eq!(
        call(
            &restarted,
            "GET",
            "/admin/api/setup",
            Some(&cookie),
            json!({})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(
            &service,
            "POST",
            "/admin/api/logout",
            Some(&cookie),
            json!({})
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(
            &service,
            "GET",
            "/admin/api/setup",
            Some(&cookie),
            json!({})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = call(
        &service,
        "POST",
        "/admin/api/login",
        None,
        json!({"password":PASSWORD}),
    )
    .await;
    let cookie = session_cookie(&response);
    for session in service.0.sessions.lock().unwrap().values_mut() {
        session.expires = Instant::now();
    }
    assert_eq!(
        call(
            &service,
            "GET",
            "/admin/api/setup",
            Some(&cookie),
            json!({})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn login_attempts_body_size_and_hash_concurrency_are_bounded() {
    let (_directory, _store, service) = fixture();
    let response = call(
        &service,
        "POST",
        "/admin/api/login",
        None,
        json!({"password":"x".repeat(8192)}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let permits = service.0.work.clone().acquire_many_owned(2).await.unwrap();
    assert_eq!(
        call(&service, "GET", "/admin/api/status", None, json!({}))
            .await
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    drop(permits);
    for _ in 0..10 {
        assert_eq!(
            call(
                &service,
                "POST",
                "/admin/api/login",
                None,
                json!({"password":PASSWORD})
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call(
            &service,
            "POST",
            "/admin/api/login",
            None,
            json!({"password":PASSWORD})
        )
        .await
        .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let past = Instant::now() - Duration::from_secs(61);
    service
        .0
        .attempts
        .lock()
        .unwrap()
        .iter_mut()
        .for_each(|at| *at = past);
    assert_eq!(
        call(
            &service,
            "POST",
            "/admin/api/login",
            None,
            json!({"password":PASSWORD})
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn invalid_private_state_fails_closed_without_exposing_it() {
    let (directory, store, service) = fixture();
    let cookie = enrolled(&store, &service).await;
    std::fs::write(
        directory.path().join("private.json"),
        b"SECRET corrupt data",
    )
    .unwrap();
    let response = call(
        &service,
        "GET",
        "/admin/api/setup",
        Some(&cookie),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!json(response).await.to_string().contains("SECRET"));
    assert_eq!(
        call(&service, "GET", "/admin/api/status", None, json!({}))
            .await
            .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn public_shell_has_no_secrets_and_enforces_resource_policy() {
    let (_directory, store, service) = fixture();
    let code = store.setup_code().unwrap();
    let response = call(&service, "GET", "/", None, json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("frame-ancestors 'none'"));
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    assert!(!String::from_utf8(body.to_vec()).unwrap().contains(&code));
    let response = call(&service, "GET", "/admin/api/status", None, json!({})).await;
    assert_eq!(json(response).await, json!({"ownerConfigured":false}));
}
