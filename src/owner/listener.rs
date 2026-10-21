//! The installed HTTPS entry point for the existing authenticated audio engine.
//! Owner authorization and listener authorization are deliberately separate.
use super::{Identity, Store};
use crate::access;
use axum::{
    extract::{Request, State},
    http::{header, Method, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

#[derive(Clone)]
pub(super) struct Listener(Arc<Inner>);
struct Inner {
    store: Store,
    identity: Identity,
    state: Arc<Mutex<Current>>,
    // Reconfiguration shares these limits with work still being cancelled.
    audio: Arc<access::audio::Store>,
    requests: Semaphore,
    storage_jobs: Arc<Semaphore>,
    stopped: CancellationToken,
    health: super::health::Health,
}
#[derive(Default)]
struct Current {
    key: Option<(String, String)>,
    engine: Option<access::Service>,
}
impl Current {
    fn clear(&mut self) {
        if let Some(engine) = self.engine.take() {
            engine.clear();
        }
        self.key = None;
    }
}
impl Drop for Current {
    fn drop(&mut self) {
        self.clear();
    }
}
impl Listener {
    pub(super) fn new(store: Store) -> anyhow::Result<Self> {
        Ok(Self(Arc::new(Inner {
            identity: store.identity()?,
            store: store.clone(),
            state: Arc::new(Mutex::new(Current::default())),
            audio: Arc::new(access::audio::Store::persistent(
                &store.cache_directory(),
                &store.config()?.storage,
                &store.working_directory(),
                &store.config()?.resources,
            )?),
            requests: Semaphore::new(8),
            storage_jobs: Arc::new(Semaphore::new(2)),
            stopped: CancellationToken::new(),
            health: super::health::Health::new()?,
        })))
    }
    pub(super) fn router(&self) -> Router {
        Router::new().fallback(dispatch).with_state(self.clone())
    }

    /// Serialize reads as well as swaps: an older disk read must never replace a
    /// newer configuration. No filesystem access runs on the audio executor.
    async fn refresh(&self) -> Result<Option<access::Service>, ()> {
        let current = self.0.state.clone().lock_owned().await;
        if self.0.stopped.is_cancelled() {
            return Err(());
        }
        let store = self.0.store.clone();
        // The blocking read owns the gate, even if its HTTP caller times out.
        // That prevents cancelled requests piling up background filesystem work.
        let audio = self.0.audio.clone();
        let (mut current, snapshot) = tokio::task::spawn_blocking(move || {
            let snapshot = store.listener_configuration().map(|configuration| {
                // Cache problems must not cancel otherwise healthy audio. It
                // bypasses retention until the persisted settings can be applied.
                let _ = audio.retained().unwrap().apply(&configuration.storage);
                audio.apply_resources(&configuration.resources);
                configuration.key
            });
            (current, snapshot)
        })
        .await
        .map_err(|_| ())?;
        let key = match snapshot {
            Ok(value) if !self.0.stopped.is_cancelled() => value,
            _ => {
                current.clear();
                return Err(());
            }
        };
        if current.key != key {
            current.clear();
            if let Some((_, url)) = &key {
                let upstream = reqwest::Url::parse(url).map_err(|_| ())?;
                current.engine = Some(
                    access::Service::persistent(upstream, self.0.audio.clone()).map_err(|_| ())?,
                );
            }
            current.key = key;
        }
        if let Some(engine) = &current.engine {
            engine.reap();
        }
        Ok(current.engine.clone())
    }

    pub(super) async fn storage(&self) -> anyhow::Result<serde_json::Value> {
        let permit = self.0.storage_jobs.clone().try_acquire_owned()?;
        self.refresh()
            .await
            .map_err(|_| anyhow::anyhow!("Could not apply cache settings."))?;
        let audio = self.0.audio.clone();
        let store = self.0.store.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let _permit = permit;
            Ok(json!({"settings":store.config()?.storage, "cache": audio.retained().unwrap().stats(), "working_bytes":audio.working_bytes()?,"resources":store.config()?.resources,"runtime":audio.resource_stats()}))
        }).await?
    }
    pub(super) async fn dashboard(&self) -> anyhow::Result<serde_json::Value> {
        let storage = self.storage().await?;
        let current = self.0.state.lock().await;
        let activity = current
            .engine
            .as_ref()
            .map(|engine| engine.dashboard())
            .unwrap_or_else(|| json!({"clients":[]}));
        Ok(json!({
            "ready": current.engine.is_some(),
            "activity": activity,
            "bytes_per_second": self.0.audio.traffic.bytes_per_second(),
            "rate_window_seconds": 5,
            "storage": storage,
            "navidrome": self.0.health.view(current.key.clone(), self.0.stopped.clone()),
        }))
    }

    pub(super) async fn maintain(&self, stop: CancellationToken) {
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                _ = self.0.stopped.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(1)) => {
                    // Also expires idle sessions and notices CLI changes without
                    // waiting for another listener to send a request.
                    let _ = self.refresh().await;
                }
            }
        }
    }
    pub(super) async fn stop(&self) {
        self.0.stopped.cancel();
        self.0.state.lock().await.clear();
    }
    pub(super) async fn drain(&self) {
        self.0.audio.drain().await;
    }
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error":code}))).into_response()
}
async fn dispatch(State(listener): State<Listener>, request: Request) -> Response {
    let invalid = request.uri().query().is_some()
        || request.headers().contains_key(header::ORIGIN)
        || request.headers().contains_key(header::COOKIE);
    let mut response = if invalid {
        error(StatusCode::BAD_REQUEST, "invalid_request")
    } else if let Ok(_permit) = listener.0.requests.try_acquire() {
        let work = async {
            let engine = match listener.refresh().await {
                Ok(engine) => engine,
                Err(()) => {
                    return error(StatusCode::SERVICE_UNAVAILABLE, "configuration_unavailable")
                }
            };
            if request.uri().path() == "/access/v1/capabilities"
                && matches!(*request.method(), Method::GET | Method::HEAD)
            {
                let mut value = access::Service::capabilities();
                value["service"] = json!("astra-sidecar");
                value["development"] = json!(true);
                value["ready"] = json!(engine.is_some());
                value["playback"] = json!(engine.is_some());
                value["installation"] = json!(listener.0.identity.installation_id);
                value["spkiSha256"] = json!(listener.0.identity.spki_sha256);
                let mut response = Json(value).into_response();
                if request.method() == Method::HEAD {
                    *response.body_mut() = axum::body::Body::empty();
                }
                return response;
            }
            let Some(engine) = engine else {
                return error(StatusCode::SERVICE_UNAVAILABLE, "setup_required");
            };
            match engine.router().oneshot(request).await {
                Ok(response) => response,
                Err(infallible) => match infallible {},
            }
        };
        tokio::select! {
            biased;
            _ = listener.0.stopped.cancelled() => error(StatusCode::SERVICE_UNAVAILABLE, "service_stopping"),
            result = tokio::time::timeout(Duration::from_secs(10), work) =>
                result.unwrap_or_else(|_| error(StatusCode::SERVICE_UNAVAILABLE, "request_timed_out")),
        }
    } else {
        error(StatusCode::TOO_MANY_REQUESTS, "limit_reached")
    };
    let close = !response.status().is_success();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    if close {
        headers.insert(header::CONNECTION, "close".parse().unwrap());
    }
    response
}

#[cfg(test)]
mod tests;
