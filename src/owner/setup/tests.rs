use super::*;
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::StatusCode,
    response::Response,
    routing::any,
    Router,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

type Captured = Arc<Mutex<Vec<(String, String, axum::http::HeaderMap, String)>>>;
struct Server {
    url: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn fake(status: u16, mime: &str, body: String, delay: Duration) -> (Server, Captured) {
    let captured: Captured = Default::default();
    let seen = captured.clone();
    let mime = mime.to_owned();
    let app = Router::new().fallback(any(move |request: Request| {
        let seen = seen.clone();
        let body = body.clone();
        let mime = mime.clone();
        async move {
            let (head, bytes) = request.into_parts();
            let body_text =
                String::from_utf8(to_bytes(bytes, 4096).await.unwrap().to_vec()).unwrap();
            seen.lock().unwrap().push((
                head.method.to_string(),
                head.uri.to_string(),
                head.headers,
                body_text,
            ));
            tokio::time::sleep(delay).await;
            Response::builder()
                .status(status)
                .header("content-type", mime)
                .header("location", "/redirect-target")
                .body(Body::from(body))
                .unwrap()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (Server { url, task }, captured)
}
fn envelope(code: u16) -> String {
    json!({"subsonic-response":{"status":"failed","type":"navidrome","serverVersion":"0.64.2","error":{"code":code,"message":"missing u"}}}).to_string()
}
#[test]
fn settings_preserve_prefixes_ipv6_and_explicit_trust() {
    for (input, expected) in [
        (
            " HTTP://Music.HOME:80/navidrome/// ",
            "http://music.home/navidrome",
        ),
        ("https://[::1]:4533/music/", "https://[::1]:4533/music"),
        ("https://music.test/a%20b/", "https://music.test/a%20b"),
    ] {
        assert_eq!(normalize_navidrome(input).unwrap(), expected);
    }
    let settings: Settings =
        serde_json::from_value(json!({"navidrome_url":"http://navi:4533/","listeners":[
            {"url":"https://sidecar.lan:7843/","trust":"sidecar_key"},
            {"url":"https://public.example/stratum/","trust":"https_proxy"}
        ]}))
        .unwrap();
    let normalized = settings.normalized().unwrap();
    assert_eq!(normalized.navidrome_url.unwrap(), "http://navi:4533");
    assert_eq!(
        normalized.listeners[1].url,
        "https://public.example/stratum"
    );
    assert!(normalized.listeners[0].trust == EndpointTrust::SidecarKey);
    assert!(normalized.listeners[1].trust == EndpointTrust::HttpsProxy);
}
#[test]
fn invalid_or_ambiguous_addresses_fail_without_guessing() {
    for input in [
        "",
        "music.lan",
        "file:///tmp/test",
        "http://u:p@host",
        "https://host?token=secret",
        "https://host#key",
        "http://0.0.0.0:4533",
        "http://[::]:4533",
        "http://host:0",
        "http://ho\\st",
        "http://ho\nst",
    ] {
        assert!(normalize_navidrome(input).is_err(), "{input}");
    }
    let mut settings = Settings {
        navidrome_url: None,
        listeners: vec![Endpoint {
            url: "http://host".into(),
            trust: EndpointTrust::SidecarKey,
        }],
    };
    assert!(settings.normalized().is_err());
    settings.listeners[0].url = "https://host/".into();
    settings.listeners.push(Endpoint {
        url: "https://HOST:443".into(),
        trust: EndpointTrust::HttpsProxy,
    });
    assert!(settings.normalized().is_err());
    assert!(serde_json::from_value::<Settings>(json!({"password":"secret"})).is_err());
    assert!(
        serde_json::from_value::<Endpoint>(json!({"url":"https://host","trust":"automatic"}))
            .is_err()
    );
}
#[tokio::test]
async fn detection_posts_only_public_parameters_and_preserves_proxy_prefix() {
    let (server, captured) = fake(
        200,
        "application/json; charset=utf-8",
        envelope(10),
        Duration::ZERO,
    )
    .await;
    let detection = Probe::new()
        .unwrap()
        .check(&format!("{}/music/", server.url))
        .await
        .unwrap();
    assert!(!detection.authenticated);
    assert_eq!(detection.server_version.as_deref(), Some("0.64.2"));
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let (method, path, headers, body) = &requests[0];
    assert_eq!(method, "POST");
    assert_eq!(path, "/music/rest/ping.view");
    assert_eq!(body, "f=json&v=1.16.1&c=AstraSidecar");
    for name in ["authorization", "cookie", "referer"] {
        assert!(!headers.contains_key(name));
    }
}
#[tokio::test]
async fn success_and_expected_auth_rejections_identify_only_the_api() {
    for code in [10, 40, 41, 50] {
        let (server, _) = fake(200, "application/json", envelope(code), Duration::ZERO).await;
        assert!(
            !Probe::new()
                .unwrap()
                .check(&server.url)
                .await
                .unwrap()
                .authenticated
        );
    }
    let (server, _) = fake(
        200,
        "application/json",
        json!({"subsonic-response":{"status":"ok","type":"navidrome"}}).to_string(),
        Duration::ZERO,
    )
    .await;
    assert!(
        !Probe::new()
            .unwrap()
            .check(&server.url)
            .await
            .unwrap()
            .authenticated
    );
}
#[tokio::test]
async fn errors_are_distinct_bounded_and_redirects_are_not_followed() {
    for (status, mime, body, expected) in [
        (
            302,
            "application/json",
            envelope(10),
            ProbeFailure::Redirect,
        ),
        (401, "application/json", envelope(10), ProbeFailure::Access),
        (403, "application/json", envelope(10), ProbeFailure::Access),
        (
            500,
            "application/json",
            envelope(10),
            ProbeFailure::Http(500),
        ),
        (
            200,
            "text/html",
            "login portal".into(),
            ProbeFailure::NotNavidrome,
        ),
        (
            200,
            "application/json",
            "not json".into(),
            ProbeFailure::Response,
        ),
        (
            200,
            "application/json",
            "x".repeat(65537),
            ProbeFailure::Response,
        ),
        (200, "application/json", envelope(20), ProbeFailure::Version),
        (
            200,
            "application/json",
            envelope(70),
            ProbeFailure::Response,
        ),
        (
            200,
            "application/json",
            envelope(10).replace("navidrome", "jellyfin"),
            ProbeFailure::NotNavidrome,
        ),
    ] {
        let (server, seen) = fake(status, mime, body, Duration::ZERO).await;
        assert_eq!(
            Probe::new().unwrap().check(&server.url).await.err(),
            Some(expected)
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }
    let (server, _) = fake(
        200,
        "application/json",
        envelope(10),
        Duration::from_secs(1),
    )
    .await;
    let probe = Probe {
        client: Probe::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap(),
    };
    assert_eq!(
        probe.check(&server.url).await.err(),
        Some(ProbeFailure::Timeout)
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(probe.check(&url).await.err(), Some(ProbeFailure::Connect));
}
#[tokio::test]
async fn https_verifies_certificates_and_does_not_downgrade() {
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".into()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        cert.pem().into_bytes(),
        key.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    let app = Router::new().fallback(any(|| async {
        (
            StatusCode::OK,
            [("content-type", "application/json")],
            envelope(10),
        )
    }));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("https://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    let server = Server { url, task };
    let untrusted = Probe::new().unwrap().check(&server.url).await;
    assert_eq!(untrusted.err(), Some(ProbeFailure::SecureConnection));
    let probe = Probe {
        client: Probe::builder()
            .add_root_certificate(reqwest::Certificate::from_der(cert.der()).unwrap())
            .build()
            .unwrap(),
    };
    assert!(!probe.check(&server.url).await.unwrap().authenticated);
}
