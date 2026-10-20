//! Loopback experimental access and authenticated-audio service.
//! The original 64-byte probe remains separate from the encoded audio routes.
pub mod audio;
pub(crate) mod transport;

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::Semaphore, time::Instant};
use tokio_util::sync::CancellationToken;

const MAX_SESSIONS: usize = 32;
const MAX_TRACKS: usize = 16;
const PREFIX_BYTES: usize = 64;
const JSON_LIMIT: usize = 64 * 1024;
const INPUT_LIMIT: usize = 4096;
const REQUEST_LIMIT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub upstream: String,
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,
    #[serde(default = "default_ttl")]
    pub session_seconds: u64,
    #[serde(default = "default_lifetime")]
    pub run_seconds: u64,
    #[serde(default = "default_connection")]
    pub connection_seconds: u64,
}
fn default_listen() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}
fn default_ttl() -> u64 {
    600
}
fn default_lifetime() -> u64 {
    900
}
fn default_connection() -> u64 {
    30
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Credential {
    username: String,
    token: String,
    salt: String,
}
// Deliberately no Debug/Serialize implementation for credentials or sessions.
impl Credential {
    fn validate(&self) -> Result<(), Failure> {
        if self.username.is_empty()
            || self.username.len() > 128
            || self.username.trim() != self.username
            || self.username.chars().any(char::is_control)
            || self.token.len() != 32
            || !self.token.bytes().all(|c| c.is_ascii_hexdigit())
            || self.salt.is_empty()
            || self.salt.len() > 128
            || self.salt.chars().any(char::is_control)
        {
            return Err(Failure::Input);
        }
        Ok(())
    }
}

struct Session {
    auth: Mutex<SessionAuth>,
    cancelled: CancellationToken,
    // Private to this session. No source revision or cross-session cache claim.
    prefixes: Mutex<HashMap<String, Bytes>>,
    playbacks: Mutex<HashMap<String, Arc<audio::Playback>>>,
    playback_slots: Arc<Semaphore>,
}
struct SessionAuth {
    credential: Credential,
    expires: Instant,
}
impl Session {
    fn new(credential: Credential, expires: Instant) -> Self {
        Self {
            auth: Mutex::new(SessionAuth {
                credential,
                expires,
            }),
            cancelled: CancellationToken::new(),
            prefixes: Mutex::new(HashMap::new()),
            playbacks: Mutex::new(HashMap::new()),
            playback_slots: Arc::new(Semaphore::new(2)),
        }
    }
    fn credential(&self) -> Credential {
        self.auth.lock().unwrap().credential.clone()
    }
    fn active(&self) -> bool {
        !self.cancelled.is_cancelled() && Instant::now() < self.auth.lock().unwrap().expires
    }
    async fn expired(&self) {
        // Renewal only extends the deadline. Wake at the old deadline and read
        // it again, so requests already in flight survive a successful renewal.
        loop {
            let deadline = self.auth.lock().unwrap().expires;
            tokio::time::sleep_until(deadline).await;
            if Instant::now() >= self.auth.lock().unwrap().expires {
                return;
            }
        }
    }
    fn revoke(&self) {
        self.cancelled.cancel();
        self.prefixes.lock().unwrap().clear();
        for playback in self.playbacks.lock().unwrap().drain().map(|(_, p)| p) {
            playback.cancelled.cancel();
        }
    }
}

#[derive(Clone)]
pub struct Service(Arc<Inner>);
struct Inner {
    upstream: Url,
    client: Client,
    ttl: Duration,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    attempts: Mutex<VecDeque<Instant>>,
    requests: Arc<Semaphore>,
    stopped: CancellationToken,
    audio: Arc<audio::Store>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    Input,
    Session,
    Access,
    Upstream,
    Unavailable,
    Busy,
    UnsafeUpstream,
    Internal,
    Unsupported,
    TooLarge,
    Playback,
    Encoding,
    Storage,
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, code) = match self {
            Self::Input => (StatusCode::BAD_REQUEST, "invalid_request"),
            Self::Session => (StatusCode::UNAUTHORIZED, "sign_in_required"),
            Self::Access => (StatusCode::NOT_FOUND, "track_unavailable"),
            Self::Upstream => (StatusCode::BAD_GATEWAY, "upstream_response_invalid"),
            Self::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "upstream_unavailable"),
            Self::Busy => (StatusCode::TOO_MANY_REQUESTS, "limit_reached"),
            Self::UnsafeUpstream => (
                StatusCode::BAD_GATEWAY,
                "upstream_does_not_validate_credentials",
            ),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            Self::Unsupported => (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_source"),
            Self::Storage => (
                StatusCode::SERVICE_UNAVAILABLE,
                "working_storage_unavailable",
            ),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "source_limit_exceeded"),
            Self::Playback => (StatusCode::NOT_FOUND, "playback_unavailable"),
            Self::Encoding => (StatusCode::UNPROCESSABLE_ENTITY, "audio_preparation_failed"),
        };
        let mut response = (status, Json(json!({"error": code}))).into_response();
        if matches!(self, Self::Busy | Self::Storage) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, "1".parse().unwrap());
        }
        response
    }
}

fn random_token() -> Result<String, Failure> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| Failure::Internal)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn loopback_url(raw: &str) -> Option<Url> {
    let url = Url::parse(raw).ok()?;
    let ip = url
        .host_str()?
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .ok()?;
    (url.scheme() == "http"
        && ip.is_loopback()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

impl Service {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        anyhow::ensure!(
            config.listen.ip().is_loopback(),
            "access checkpoint requires a loopback listener"
        );
        anyhow::ensure!(
            (1..=600).contains(&config.session_seconds),
            "session_seconds must be 1..600"
        );
        anyhow::ensure!(
            (1..=3600).contains(&config.run_seconds),
            "run_seconds must be 1..3600"
        );
        anyhow::ensure!(
            (1..=30).contains(&config.connection_seconds),
            "connection_seconds must be 1..30"
        );
        let upstream = loopback_url(&config.upstream).ok_or_else(|| {
            anyhow::anyhow!(
                "upstream must be loopback HTTP, without credentials, query or fragment"
            )
        })?;
        Self::build(
            upstream,
            config.session_seconds,
            Arc::new(audio::Store::new()?),
        )
    }

    /// The persistent HTTPS service supplies its validated, owner-configured URL.
    /// This does not relax the loopback-only proof constructor or listener.
    pub(crate) fn persistent(upstream: Url, audio: Arc<audio::Store>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            matches!(upstream.scheme(), "http" | "https")
                && upstream.host_str().is_some()
                && upstream.username().is_empty()
                && upstream.password().is_none()
                && upstream.query().is_none()
                && upstream.fragment().is_none(),
            "invalid configured upstream"
        );
        Self::build(upstream, default_ttl(), audio)
    }

    fn build(upstream: Url, seconds: u64, audio: Arc<audio::Store>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(3))
            .pool_max_idle_per_host(2)
            .user_agent("Astra-Sidecar/0.1")
            .build()
            .map_err(|_| anyhow::anyhow!("could not initialize upstream HTTP client"))?;
        Ok(Self(Arc::new(Inner {
            upstream,
            client,
            ttl: Duration::from_secs(seconds),
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(VecDeque::new()),
            requests: Arc::new(Semaphore::new(8)),
            stopped: CancellationToken::new(),
            audio,
        })))
    }

    pub(crate) fn capabilities() -> Value {
        json!({
            "protocol": "stratum-access-proof-1", "playback": false,
            "authentication": "subsonic-token", "probeBytes": PREFIX_BYTES,
            "sessionRenewal": "fresh-credentials-same-user",
            "audioExperiment": "stratum-authenticated-audio-1",
            "audioSource": "complete-private-copy",
            "maxSourceBytes": audio::MAX_SOURCE_BYTES,
            "maxSegmentSeconds": audio::MAX_SEGMENT_SECONDS
        })
    }

    pub fn router(&self) -> Router {
        audio::routes()
            .merge(
                Router::new()
                    .route(
                        "/access/v1/capabilities",
                        get(|| async { Json(Self::capabilities()) }),
                    )
                    .route("/access/v1/sessions", post(create_session))
                    .route(
                        "/access/v1/session",
                        delete(delete_session).post(renew_session),
                    )
                    .route("/access/v1/media/{track}", get(media)),
            )
            .fallback(|| async { Failure::Input })
            .layer(middleware::from_fn_with_state(self.clone(), guard))
            .with_state(self.clone())
    }

    pub(crate) fn reap(&self) {
        self.0.sessions.lock().unwrap().retain(|_, session| {
            if session.active() {
                true
            } else {
                session.revoke();
                false
            }
        });
    }
    pub(crate) fn clear(&self) {
        self.0.stopped.cancel();
        let mut sessions = self.0.sessions.lock().unwrap();
        for session in sessions.values() {
            session.revoke();
        }
        sessions.clear();
    }
    fn session(&self, headers: &HeaderMap) -> Result<(String, Arc<Session>), Failure> {
        self.reap();
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let token = values
            .next()
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .ok_or(Failure::Session)?;
        if values.next().is_some()
            || token.len() != 64
            || !token.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(Failure::Session);
        }
        let session = self
            .0
            .sessions
            .lock()
            .unwrap()
            .get(token)
            .cloned()
            .ok_or(Failure::Session)?;
        Ok((token.to_owned(), session))
    }
    fn revoke(&self, token: &str) {
        if let Some(session) = self.0.sessions.lock().unwrap().remove(token) {
            session.revoke();
        }
    }
    fn admit_login(&self) -> Result<(), Failure> {
        let now = Instant::now();
        let mut attempts = self.0.attempts.lock().unwrap();
        while attempts
            .front()
            .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
        {
            attempts.pop_front();
        }
        if attempts.len() >= 30 {
            return Err(Failure::Busy);
        }
        attempts.push_back(now);
        Ok(())
    }

    async fn authenticate(&self, credential: &Credential) -> Result<(), Failure> {
        // Recheck on renewal too: a proxy can become permissive after login.
        let bogus = Credential {
            username: format!("stratum-invalid-{}", random_token()?),
            token: random_token()?[..32].into(),
            salt: random_token()?,
        };
        match self.json("ping", &bogus, &[]).await {
            Err(Failure::Session) => {}
            Ok(_) => return Err(Failure::UnsafeUpstream),
            Err(error) => return Err(error),
        }
        self.json("ping", credential, &[]).await?;
        Ok(())
    }

    async fn upstream(
        &self,
        endpoint: &str,
        credential: &Credential,
        params: &[(&str, String)],
        range: Option<usize>,
    ) -> Result<reqwest::Response, Failure> {
        // Endpoints are code constants; URL/identity never come from media input.
        let mut url = self.0.upstream.clone();
        url.set_path(&format!(
            "{}/rest/{endpoint}.view",
            url.path().trim_end_matches('/')
        ));
        let mut form = vec![
            ("u", credential.username.clone()),
            ("t", credential.token.clone()),
            ("s", credential.salt.clone()),
            ("v", "1.16.1".into()),
            ("c", "AstraSidecar".into()),
            ("f", "json".into()),
        ];
        form.extend_from_slice(params);
        let mut request = self
            .0
            .client
            .post(url)
            .form(&form)
            .header(header::ACCEPT_ENCODING, "identity");
        if let Some(bytes) = range {
            request = request.header(header::RANGE, format!("bytes=0-{}", bytes - 1));
        }
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() || e.is_connect() {
                Failure::Unavailable
            } else {
                Failure::Upstream
            }
        })?;
        match response.status().as_u16() {
            200..=299 => Ok(response),
            401 => Err(Failure::Session),
            403 | 404 => Err(Failure::Access),
            502..=504 => Err(Failure::Unavailable),
            _ => Err(Failure::Upstream), // Includes every redirect. Never print upstream errors/URLs.
        }
    }
    async fn json(
        &self,
        endpoint: &str,
        credential: &Credential,
        params: &[(&str, String)],
    ) -> Result<Value, Failure> {
        envelope(
            read_bounded(
                self.upstream(endpoint, credential, params, None).await?,
                JSON_LIMIT,
            )
            .await?,
        )
    }
    async fn range(
        &self,
        credential: &Credential,
        track: &str,
        size: u64,
        length: usize,
    ) -> Result<Bytes, Failure> {
        let response = self
            .upstream(
                "stream",
                credential,
                &[
                    ("id", track.into()),
                    ("format", "raw".into()),
                    ("maxBitRate", "0".into()),
                ],
                Some(length),
            )
            .await?;
        if response.status() != StatusCode::PARTIAL_CONTENT {
            // Navidrome may send an authentication error with HTTP 200.
            let is_json = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|v| v.starts_with("application/json"));
            if is_json {
                envelope(read_bounded(response, JSON_LIMIT).await?)?;
            }
            return Err(Failure::Upstream);
        }
        let expected = format!("bytes 0-{}/{size}", length - 1);
        if response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|h| h.to_str().ok())
            != Some(&expected)
            || response
                .headers()
                .get(header::CONTENT_ENCODING)
                .is_some_and(|v| v != "identity")
            || !response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|h| h.to_str().ok())
                .is_some_and(|v| v.starts_with("audio/") || v == "application/octet-stream")
        {
            return Err(Failure::Upstream);
        }
        let bytes = read_bounded(response, length).await?;
        if bytes.len() != length {
            return Err(Failure::Upstream);
        }
        Ok(bytes)
    }
    async fn authorize(&self, session: &Session, track: &str) -> Result<u64, Failure> {
        let value = self
            .json("getSong", &session.credential(), &[("id", track.into())])
            .await?;
        if value["song"]["id"].as_str() != Some(track) {
            return Err(Failure::Upstream);
        }
        let size = value["song"]["size"]
            .as_u64()
            .filter(|n| *n >= PREFIX_BYTES as u64)
            .ok_or(Failure::Upstream)?;
        self.range(&session.credential(), track, size, 1).await?;
        Ok(size)
    }
}

async fn read_bounded(mut response: reqwest::Response, limit: usize) -> Result<Bytes, Failure> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return Err(Failure::Upstream);
    }
    let mut output = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Upstream)? {
        if chunk.len() > limit - output.len() {
            return Err(Failure::Upstream);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output.into())
}
fn envelope(bytes: Bytes) -> Result<Value, Failure> {
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::Upstream)?;
    let envelope = value.get("subsonic-response").ok_or(Failure::Upstream)?;
    match envelope["status"].as_str() {
        Some("ok") => Ok(envelope.clone()),
        Some("failed") => Err(match envelope["error"]["code"].as_u64() {
            Some(40 | 41 | 42 | 44) => Failure::Session,
            Some(50 | 70) => Failure::Access,
            _ => Failure::Upstream,
        }),
        _ => Err(Failure::Upstream),
    }
}

async fn guard(State(service): State<Service>, request: Request, next: Next) -> Response {
    // No ambient/browser credentials or URL authentication. Tokens are headers only.
    let invalid = request.uri().query().is_some()
        || request.headers().contains_key(header::ORIGIN)
        || request.headers().contains_key(header::COOKIE);
    let mut response = if invalid {
        Failure::Input.into_response()
    } else if let Ok(_permit) = service.0.requests.clone().try_acquire_owned() {
        tokio::select! {
            biased;
            _ = service.0.stopped.cancelled() => Failure::Unavailable.into_response(),
            result = tokio::time::timeout(REQUEST_LIMIT, next.run(request)) => match result {
                Ok(response) => response,
                Err(_) => Failure::Unavailable.into_response(),
            }
        }
    } else {
        Failure::Busy.into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
        .headers_mut()
        .insert(header::CONNECTION, "close".parse().unwrap());
    response
}

async fn read_credential(request: Request) -> Result<Credential, Failure> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/json")
    {
        return Err(Failure::Input);
    }
    let bytes = to_bytes(request.into_body(), INPUT_LIMIT)
        .await
        .map_err(|_| Failure::Input)?;
    let credential: Credential = serde_json::from_slice(&bytes).map_err(|_| Failure::Input)?;
    credential.validate()?;
    Ok(credential)
}

async fn create_session(
    State(service): State<Service>,
    request: Request,
) -> Result<Response, Failure> {
    service.admit_login()?;
    let credential = read_credential(request).await?;
    service.authenticate(&credential).await?;
    service.reap();
    let token = random_token()?;
    let session = Arc::new(Session::new(credential, Instant::now() + service.0.ttl));
    {
        let mut sessions = service.0.sessions.lock().unwrap();
        if service.0.stopped.is_cancelled() {
            return Err(Failure::Unavailable);
        }
        if sessions.len() >= MAX_SESSIONS {
            return Err(Failure::Busy);
        }
        sessions.insert(token.clone(), session);
    }
    Ok((
        StatusCode::CREATED,
        Json(json!({"session": token, "expiresInSeconds": service.0.ttl.as_secs()})),
    )
        .into_response())
}

/// Keep the same bearer and private playback handles. A bearer alone cannot
/// extend its lifetime: fresh same-user Navidrome credentials must still work.
async fn renew_session(
    State(service): State<Service>,
    request: Request,
) -> Result<Response, Failure> {
    let (token, session) = service.session(request.headers())?;
    service.admit_login()?;
    let credential = read_credential(request).await?;
    if credential.username != session.credential().username {
        return Err(Failure::Input);
    }
    let result = tokio::select! {
        biased;
        _ = session.cancelled.cancelled() => Err(Failure::Session),
        _ = session.expired() => Err(Failure::Session),
        result = service.authenticate(&credential) => result,
    };
    if let Err(error) = result {
        if matches!(
            error,
            Failure::Session | Failure::Access | Failure::UnsafeUpstream
        ) {
            service.revoke(&token);
        }
        return Err(error);
    }
    {
        // Match revoke/reap's lock order. Logout, shutdown and expiry win over
        // an authentication response arriving late; never insert a new entry.
        let sessions = service.0.sessions.lock().unwrap();
        if service.0.stopped.is_cancelled()
            || !sessions
                .get(&token)
                .is_some_and(|s| Arc::ptr_eq(s, &session))
        {
            return Err(Failure::Session);
        }
        let mut auth = session.auth.lock().unwrap();
        if session.cancelled.is_cancelled() || Instant::now() >= auth.expires {
            return Err(Failure::Session);
        }
        auth.credential = credential;
        auth.expires = Instant::now() + service.0.ttl;
    }
    Ok(
        Json(json!({"session": token, "expiresInSeconds": service.0.ttl.as_secs()}))
            .into_response(),
    )
}

async fn delete_session(
    State(service): State<Service>,
    headers: HeaderMap,
) -> Result<StatusCode, Failure> {
    let (token, _) = service.session(&headers)?;
    service.revoke(&token);
    Ok(StatusCode::NO_CONTENT)
}

async fn media(
    State(service): State<Service>,
    Path(track): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Failure> {
    if track.is_empty()
        || track.len() > 128
        || !track
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
    {
        return Err(Failure::Input);
    }
    let (token, session) = service.session(&headers)?;
    let work = async {
        let size = service.authorize(&session, &track).await?;
        let cached = session.prefixes.lock().unwrap().get(&track).cloned();
        let hit = cached.is_some();
        let bytes = match cached {
            Some(bytes) => bytes,
            None => {
                service
                    .range(&session.credential(), &track, size, PREFIX_BYTES)
                    .await?
            }
        };
        // Final check covers access lost while fetching/preparing a response.
        if service.authorize(&session, &track).await? != size {
            return Err(Failure::Upstream);
        }
        if !session.active() {
            return Err(Failure::Session);
        }
        {
            let mut prefixes = session.prefixes.lock().unwrap();
            if !session.active() {
                return Err(Failure::Session);
            }
            if prefixes.len() >= MAX_TRACKS {
                prefixes.clear();
            }
            prefixes.insert(track.clone(), bytes.clone());
        }
        let body_session = session.clone();
        let body = Body::from_stream(futures_util::stream::once(async move {
            if body_session.active() {
                Ok(bytes)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "session ended",
                ))
            }
        }));
        let mut response = Response::new(body);
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            "application/octet-stream".parse().unwrap(),
        );
        response
            .headers_mut()
            .insert("x-stratum-proof-only", "1".parse().unwrap());
        response.headers_mut().insert(
            "x-stratum-cache-hit",
            if hit { "1" } else { "0" }.parse().unwrap(),
        );
        Ok(response)
    };
    let result = tokio::select! {
        biased;
        _ = session.cancelled.cancelled() => Err(Failure::Session),
        _ = session.expired() => Err(Failure::Session),
        result = work => result,
    };
    if let Err(error) = result {
        session.prefixes.lock().unwrap().remove(&track);
        if error == Failure::Session {
            service.revoke(&token);
        }
    }
    result
}

/// Runs until shutdown/deadline, then revokes all sessions and waits for bounded connections.
pub async fn serve(
    listener: TcpListener,
    service: Service,
    config: &Config,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let sweep_service = service.clone();
    let sweep_stop = shutdown.clone();
    let reaper = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = sweep_stop.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(100)) => sweep_service.reap(),
            }
        }
    });
    let stop = shutdown.clone();
    let clean = service.clone();
    let lifetime = Duration::from_secs(config.run_seconds);
    let bounded =
        transport::BoundedListener::new(listener, Duration::from_secs(config.connection_seconds));
    let result = axum::serve(bounded, service.router())
        .with_graceful_shutdown(async move {
            tokio::select! { _ = stop.cancelled() => {}, _ = tokio::time::sleep(lifetime) => {} }
            clean.clear();
            stop.cancel();
        })
        .await;
    service.clear();
    shutdown.cancel();
    let _ = reaper.await;
    service.0.audio.drain().await;
    result
}

#[cfg(test)]
mod tests;
