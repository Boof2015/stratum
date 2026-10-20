//! Persistent owner-access foundation. Listener credentials and the experimental
//! audio service remain separate; these cookies never authorize music access.
pub mod discovery;
pub mod setup;
mod store;
pub use store::{validate_password, Config, Identity, Store};

use axum::{
    extract::{rejection::JsonRejection, DefaultBodyLimit, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{HashMap, VecDeque},
    net::TcpListener,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

const COOKIE: &str = "__Host-sidecar-owner";
const SESSION_SECONDS: u64 = 12 * 60 * 60;
const SESSION_LIMIT: usize = 16;

#[derive(Clone)]
pub struct Service(Arc<Inner>);
struct Inner {
    store: Store,
    config: Config,
    sessions: Mutex<HashMap<String, Session>>,
    attempts: Mutex<VecDeque<Instant>>,
    work: Arc<Semaphore>,
    probes: Semaphore,
    probe: setup::Probe,
}
#[derive(Clone)]
struct Session {
    generation: String,
    expires: Instant,
    cancellation: CancellationToken,
}

#[derive(Clone, Copy)]
enum Failure {
    Input,
    Password,
    Authentication,
    Origin,
    Busy,
    Internal,
    Conflict,
    Setup(&'static str),
    Probe(setup::ProbeFailure),
    Discovery(discovery::Error),
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::Input => (StatusCode::BAD_REQUEST, "Check the fields and try again."),
            Self::Password => (StatusCode::BAD_REQUEST, "Use a password of at least 12 characters, up to 1024 bytes, without control characters."),
            Self::Authentication => (StatusCode::UNAUTHORIZED, "Sign-in failed or expired. Check your password or setup code."),
            Self::Origin => (StatusCode::FORBIDDEN, "Open Sidecar at a configured administration address."),
            Self::Busy => (StatusCode::TOO_MANY_REQUESTS, "Too many requests. Wait a minute and try again."),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "Sidecar couldn't read or save its owner settings. Check the service's data directory."),
            Self::Conflict => (StatusCode::CONFLICT, "Setup changed elsewhere. Reload the saved settings before trying again."),
            Self::Setup(message) => (StatusCode::BAD_REQUEST, message),
            Self::Probe(error) => (StatusCode::BAD_GATEWAY, error.message()),
            Self::Discovery(error) => (match error {
                discovery::Error::Busy => StatusCode::CONFLICT,
                discovery::Error::Conflict => StatusCode::CONFLICT,
                discovery::Error::Interrupted => StatusCode::UNAUTHORIZED,
                _ => StatusCode::BAD_REQUEST,
            }, error.message()),
        };
        let mut body = json!({"error":message});
        if let Self::Probe(setup::ProbeFailure::Http(code)) = self {
            body["httpStatus"] = json!(code);
        }
        (status, Json(body)).into_response()
    }
}

impl Service {
    pub fn new(store: Store) -> anyhow::Result<Self> {
        let config = store.config()?;
        Ok(Self(Arc::new(Inner {
            store,
            config,
            sessions: Mutex::new(HashMap::new()),
            attempts: Mutex::new(VecDeque::new()),
            work: Arc::new(Semaphore::new(2)),
            probes: Semaphore::new(2),
            probe: setup::Probe::new()?,
        })))
    }
    pub fn router(&self) -> Router {
        Router::new()
            .route("/", get(|| async { Html(include_str!("index.html")) }))
            .route(
                "/owner.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        include_str!("owner.js"),
                    )
                }),
            )
            .route(
                "/owner.css",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                        include_str!("owner.css"),
                    )
                }),
            )
            .route("/admin/api/status", get(status))
            .route("/admin/api/claim", post(claim))
            .route("/admin/api/login", post(login))
            .route("/admin/api/logout", post(logout))
            .route("/admin/api/setup", get(setup_state))
            .route("/admin/api/setup/navidrome", post(save_navidrome))
            .route("/admin/api/setup/listeners", post(save_listeners))
            .route("/admin/api/setup/check", post(check_navidrome))
            .route("/admin/api/discovery", post(change_discovery))
            .layer(DefaultBodyLimit::max(32768))
            .layer(middleware::from_fn_with_state(self.clone(), boundary))
            .with_state(self.clone())
    }

    async fn disk<T: Send + 'static>(
        &self,
        action: impl FnOnce(Store) -> anyhow::Result<T> + Send + 'static,
    ) -> Result<T, Failure> {
        let permit = self
            .0
            .work
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)?;
        let store = self.0.store.clone();
        tokio::task::spawn_blocking(move || {
            // A timed-out HTTP request must not free the hash-work slot while
            // its blocking task is still running.
            let _permit = permit;
            action(store)
        })
        .await
        .map_err(|_| Failure::Internal)?
        .map_err(
            |error| match error.downcast_ref::<store::SetupWriteError>() {
                Some(store::SetupWriteError::Conflict) => Failure::Conflict,
                Some(store::SetupWriteError::OwnerChanged) => Failure::Authentication,
                None => error
                    .downcast_ref::<discovery::Error>()
                    .copied()
                    .map(Failure::Discovery)
                    .unwrap_or(Failure::Internal),
            },
        )
    }
    fn admit_login(&self) -> Result<(), Failure> {
        let mut attempts = self.0.attempts.lock().unwrap();
        let now = Instant::now();
        while attempts
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            attempts.pop_front();
        }
        if attempts.len() >= 10 {
            return Err(Failure::Busy);
        }
        attempts.push_back(now);
        Ok(())
    }
    fn session(&self, generation: String) -> Result<Response, Failure> {
        let token = store::random_hex(32).map_err(|_| Failure::Internal)?;
        let mut sessions = self.0.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, session| session.expires > now);
        if sessions.len() >= SESSION_LIMIT {
            // Successful owner authentication retires the oldest session rather
            // than making local recovery necessary to clear a full session list.
            let oldest = sessions
                .iter()
                .min_by_key(|(_, s)| s.expires)
                .map(|(key, _)| key.clone())
                .unwrap();
            if let Some(session) = sessions.remove(&oldest) {
                session.cancellation.cancel();
            }
        }
        sessions.insert(
            store::digest(token.as_bytes()),
            Session {
                generation,
                expires: now + Duration::from_secs(SESSION_SECONDS),
                cancellation: CancellationToken::new(),
            },
        );
        Ok((
            [(
                header::SET_COOKIE,
                format!(
            "{COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={SESSION_SECONDS}"
        ),
            )],
            Json(json!({"ok":true})),
        )
            .into_response())
    }
    async fn authorize(&self, headers: &HeaderMap) -> Result<String, Failure> {
        let key = cookie_key(headers).ok_or(Failure::Authentication)?;
        let session = self
            .0
            .sessions
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or(Failure::Authentication)?;
        let generation = self.disk(|store| store.owner_generation()).await?;
        if session.expires <= Instant::now() || generation.as_deref() != Some(&session.generation) {
            if let Some(session) = self.0.sessions.lock().unwrap().remove(&key) {
                session.cancellation.cancel();
            }
            return Err(Failure::Authentication);
        }
        // Logout may happen while the disk check is pending.
        if !self.0.sessions.lock().unwrap().contains_key(&key) {
            return Err(Failure::Authentication);
        }
        Ok(session.generation)
    }

    async fn probe(&self, url: &str) -> Result<setup::Detection, Failure> {
        let _permit = self.0.probes.try_acquire().map_err(|_| Failure::Busy)?;
        self.0.probe.check(url).await.map_err(Failure::Probe)
    }

    async fn setup_at(&self, revision: &str) -> Result<setup::SetupState, Failure> {
        let current = self.disk(|store| Ok(store.config()?.setup)).await?;
        if current.revision != revision {
            return Err(Failure::Conflict);
        }
        Ok(current)
    }
}

async fn boundary(State(service): State<Service>, request: Request, next: Next) -> Response {
    let host_ok = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|host| service.0.config.accepts_host(host));
    let mutation = request.method() != Method::GET && request.method() != Method::HEAD;
    let origin_ok = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|origin| {
            service
                .0
                .config
                .admin_origins
                .iter()
                .any(|value| value == origin)
        });
    let json_ok = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| value.split(';').next().unwrap_or("").trim() == "application/json");
    let mut response = if !host_ok || (mutation && !origin_ok) {
        Failure::Origin.into_response()
    } else if mutation && !json_ok {
        Failure::Input.into_response()
    } else {
        let timeout = if request.uri().path() == "/admin/api/discovery" {
            40
        } else {
            10
        };
        match tokio::time::timeout(Duration::from_secs(timeout), next.run(request)).await {
            Ok(response) => response,
            Err(_) => StatusCode::REQUEST_TIMEOUT.into_response(),
        }
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy", "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'".parse().unwrap());
    response
}

async fn status(State(service): State<Service>) -> Result<Json<serde_json::Value>, Failure> {
    let configured = service
        .disk(|store| Ok(store.owner_generation()?.is_some()))
        .await?;
    Ok(Json(json!({"ownerConfigured":configured})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    code: String,
    password: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Login {
    password: String,
}

async fn claim(
    State(service): State<Service>,
    body: Result<Json<Claim>, JsonRejection>,
) -> Result<Response, Failure> {
    let Json(body) = body.map_err(|_| Failure::Input)?;
    validate_password(&body.password).map_err(|_| Failure::Password)?;
    service.admit_login()?;
    let generation = service
        .disk(move |store| store.claim(&body.code, &body.password))
        .await?
        .ok_or(Failure::Authentication)?;
    service.session(generation)
}
async fn login(
    State(service): State<Service>,
    body: Result<Json<Login>, JsonRejection>,
) -> Result<Response, Failure> {
    let Json(body) = body.map_err(|_| Failure::Input)?;
    if body.password.len() > 1024 {
        return Err(Failure::Input);
    }
    service.admit_login()?;
    let generation = service
        .disk(move |store| store.login(&body.password))
        .await?
        .ok_or(Failure::Authentication)?;
    service.session(generation)
}
async fn logout(State(service): State<Service>, headers: HeaderMap) -> Response {
    if let Some(key) = cookie_key(&headers) {
        if let Some(session) = service.0.sessions.lock().unwrap().remove(&key) {
            session.cancellation.cancel();
        }
    }
    (
        [(
            header::SET_COOKIE,
            format!("{COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0"),
        )],
        Json(json!({"ok":true})),
    )
        .into_response()
}
async fn setup_state(
    State(service): State<Service>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, Failure> {
    service.authorize(&headers).await?;
    let (identity, setup, discovery) = service
        .disk(|store| Ok((store.identity()?, store.config()?.setup, store.discovery()?)))
        .await?;
    Ok(Json(
        json!({"stage":setup.stage(), "identity":identity, "discovery":discovery.view(&setup,&identity), "setup":setup}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiscoveryChange {
    setup_revision: String,
    discovery_revision: String,
    action: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    recreate: bool,
}
async fn change_discovery(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Json<DiscoveryChange>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Failure> {
    let generation = service.authorize(&headers).await?;
    let Json(body) = body.map_err(|_| Failure::Input)?;
    if !matches!(body.action.as_str(), "manual" | "publish" | "remove") {
        return Err(Failure::Input);
    }
    if body.action != "manual" {
        service.admit_login()?;
        if !(discovery::Credentials {
            username: body.username.clone(),
            password: body.password.clone(),
        })
        .valid()
        {
            return Err(Failure::Input);
        }
    } else if !body.username.is_empty() || !body.password.is_empty() {
        return Err(Failure::Input);
    }
    let session = service
        .0
        .sessions
        .lock()
        .unwrap()
        .get(&cookie_key(&headers).ok_or(Failure::Authentication)?)
        .cloned()
        .ok_or(Failure::Authentication)?;
    let setup_revision = body.setup_revision.clone();
    let discovery_revision = body.discovery_revision.clone();
    let operation = service
        .disk(move |store| {
            store.begin_discovery(&setup_revision, &discovery_revision, Some(&generation))
        })
        .await?;
    if body.action == "manual" {
        operation.manual().map_err(|error| {
            Failure::Discovery(
                error
                    .downcast_ref::<discovery::Error>()
                    .copied()
                    .unwrap_or(discovery::Error::Storage),
            )
        })?;
    } else {
        let action = if body.action == "publish" {
            discovery::Action::Publish
        } else {
            discovery::Action::Remove
        };
        let expiry = session.expires.saturating_duration_since(Instant::now());
        let result = tokio::select! {
            _=tokio::time::sleep(expiry)=>Err(discovery::Error::Interrupted),
            result=operation.run(action,discovery::Credentials {username:body.username,password:body.password},body.recreate,session.cancellation)=>result,
        };
        result.map_err(Failure::Discovery)?;
    }
    setup_state(State(service), headers).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NavidromeSave {
    revision: String,
    url: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenersSave {
    revision: String,
    listeners: Vec<setup::Endpoint>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NavidromeCheck {
    url: String,
}

async fn save_navidrome(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Json<NavidromeSave>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Failure> {
    service.authorize(&headers).await?;
    let Json(body) = body.map_err(|_| Failure::Input)?;
    let mut current = service.setup_at(&body.revision).await?;
    let detection = service.probe(&body.url).await?;
    let generation = service.authorize(&headers).await?;
    current.settings.navidrome_url = Some(detection.url.clone());
    let saved = service
        .disk(move |store| store.save_setup(&current.settings, &body.revision, Some(&generation)))
        .await?;
    Ok(Json(
        json!({"stage":saved.stage(), "setup":saved, "detection":detection}),
    ))
}

async fn save_listeners(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Json<ListenersSave>, JsonRejection>,
) -> Result<Json<serde_json::Value>, Failure> {
    let generation = service.authorize(&headers).await?;
    let Json(body) = body.map_err(|_| Failure::Input)?;
    let mut current = service.setup_at(&body.revision).await?;
    if current.settings.navidrome_url.is_none() {
        return Err(Failure::Setup("Save the Navidrome address first."));
    }
    if body.listeners.is_empty() {
        return Err(Failure::Setup(
            "Add at least one address listeners will use.",
        ));
    }
    current.settings.listeners = body.listeners;
    current.settings = current.settings.normalized().map_err(|_| Failure::Setup(
        "Use up to 16 different HTTPS listener addresses, without credentials, query parameters or fragments."))?;
    let saved = service
        .disk(move |store| store.save_setup(&current.settings, &body.revision, Some(&generation)))
        .await?;
    Ok(Json(json!({"stage":saved.stage(), "setup":saved})))
}

async fn check_navidrome(
    State(service): State<Service>,
    headers: HeaderMap,
    body: Result<Json<NavidromeCheck>, JsonRejection>,
) -> Result<Json<setup::Detection>, Failure> {
    service.authorize(&headers).await?;
    let Json(body) = body.map_err(|_| Failure::Input)?;
    let detection = service.probe(&body.url).await?;
    service.authorize(&headers).await?;
    Ok(Json(detection))
}
fn cookie_key(headers: &HeaderMap) -> Option<String> {
    let mut token = None;
    for header in headers.get_all(header::COOKIE) {
        for cookie in header.to_str().ok()?.split(';') {
            if let Some(value) = cookie.trim().strip_prefix(&format!("{COOKIE}=")) {
                if token.is_some()
                    || value.len() != 64
                    || !value.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    return None;
                }
                token = Some(store::digest(value.as_bytes()));
            }
        }
    }
    token
}

/// HTTPS only. The caller owns the process/data-directory lock and shutdown.
pub async fn serve(
    listener: TcpListener,
    service: Service,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    listener.set_nonblocking(true)?;
    let tls = axum_server::tls_rustls::RustlsConfig::from_config(service.0.store.tls_config()?);
    let handle = axum_server::Handle::new();
    let stopping = handle.clone();
    let stop = tokio::spawn(async move {
        shutdown.cancelled().await;
        stopping.graceful_shutdown(Some(Duration::from_secs(5)));
    });
    let mut server = axum_server::from_tcp_rustls(listener, tls)?.http1_only();
    server
        .http_builder()
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .max_headers(32)
        .max_buf_size(16 * 1024)
        .header_read_timeout(Duration::from_secs(10));
    let result = server
        .handle(handle)
        .serve(service.router().into_make_service())
        .await;
    stop.abort();
    result?;
    Ok(())
}

#[cfg(test)]
mod tests;
