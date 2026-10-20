//! A cached, credential-free reachability check, only while an owner polls.
use super::setup::{Probe, ProbeFailure};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::{sync::Semaphore, time::Instant};
use tokio_util::sync::CancellationToken;

type Key = Option<(String, String)>;
#[derive(Default)]
struct State {
    key: Key,
    checked: Option<Instant>,
    outcome: Option<Result<(), ProbeFailure>>,
}
pub(super) struct Health {
    state: Arc<Mutex<State>>,
    gate: Arc<Semaphore>,
    probe: Probe,
}
impl Health {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Self {
            state: Arc::new(Mutex::new(State::default())),
            gate: Arc::new(Semaphore::new(1)),
            probe: Probe::new()?,
        })
    }
    pub fn view(&self, key: Key, stop: CancellationToken) -> Value {
        let mut state = self.state.lock().unwrap();
        if state.key != key {
            *state = State {
                key: key.clone(),
                ..State::default()
            };
        }
        let Some((_, url)) = &key else {
            return json!({"status":"setup_required"});
        };
        if state.checked.is_none_or(|at| at.elapsed().as_secs() >= 30) {
            if let Ok(permit) = self.gate.clone().try_acquire_owned() {
                let shared = self.state.clone();
                let probe = self.probe.clone();
                let url = url.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let result = tokio::select! {
                        biased;
                        _ = stop.cancelled() => return,
                        result = probe.check(&url) => result.map(|_| ()),
                    };
                    let mut state = shared.lock().unwrap();
                    // An answer for an old configuration cannot describe the new one.
                    if state.key == key {
                        state.checked = Some(Instant::now());
                        state.outcome = Some(result);
                    }
                });
            }
        }
        let age = state.checked.map(|at| at.elapsed().as_secs());
        match state.outcome {
            Some(Ok(())) => json!({"status":"responding","checked_seconds_ago":age}),
            Some(Err(error)) => json!({"status":"problem","message":error.message(),
                "checked_seconds_ago":age,
                "http_status":if let ProbeFailure::Http(code)=error {Some(code)} else {None}}),
            None => json!({"status":"checking"}),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State as WebState, response::IntoResponse, routing::post, Json, Router};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;
    #[derive(Default)]
    struct Fixture {
        calls: AtomicUsize,
        fail: AtomicBool,
        hold: AtomicBool,
        arrived: Notify,
        release: Notify,
    }
    async fn ping(WebState(state): WebState<Arc<Fixture>>) -> axum::response::Response {
        state.calls.fetch_add(1, Ordering::SeqCst);
        if state.hold.load(Ordering::SeqCst) {
            state.arrived.notify_one();
            state.release.notified().await;
        }
        if state.fail.load(Ordering::SeqCst) {
            axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
        } else {
            Json(json!({"subsonic-response":{"type":"navidrome","status":"failed","error":{"code":10}}})).into_response()
        }
    }
    #[tokio::test]
    async fn checks_are_shared_cached_and_discard_answers_for_previous_settings() {
        let state = Arc::new(Fixture::default());
        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", socket.local_addr().unwrap());
        let app = Router::new()
            .route("/rest/ping.view", post(ping))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(socket, app).await.unwrap();
        });
        let health = Health::new().unwrap();
        let stop = CancellationToken::new();
        let key = Some(("revision-one".into(), url.clone()));
        state.hold.store(true, Ordering::SeqCst);
        assert_eq!(health.view(key.clone(), stop.clone())["status"], "checking");
        tokio::time::timeout(std::time::Duration::from_secs(2), state.arrived.notified())
            .await
            .unwrap();
        for _ in 0..10 {
            health.view(key.clone(), stop.clone());
        }
        assert_eq!(state.calls.load(Ordering::SeqCst), 1);
        let next = Some(("revision-two".into(), url));
        assert_eq!(
            health.view(next.clone(), stop.clone())["status"],
            "checking"
        );
        state.release.notify_one();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while health.gate.available_permits() == 0 {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(health.state.lock().unwrap().outcome.is_none());
        state.hold.store(false, Ordering::SeqCst);
        state.fail.store(true, Ordering::SeqCst);
        health.view(next.clone(), stop.clone());
        while health.gate.available_permits() == 0 {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let value = health.view(next.clone(), stop.clone());
        assert_eq!(value["status"], "problem");
        assert_eq!(value["http_status"], 503);
        assert_eq!(state.calls.load(Ordering::SeqCst), 2);
        for _ in 0..10 {
            health.view(next.clone(), stop.clone());
        }
        assert_eq!(state.calls.load(Ordering::SeqCst), 2);
        health.state.lock().unwrap().checked =
            Some(Instant::now() - std::time::Duration::from_secs(31));
        state.fail.store(false, Ordering::SeqCst);
        health.view(next.clone(), stop.clone());
        while health.gate.available_permits() == 0 {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(health.view(next, stop.clone())["status"], "responding");
        assert_eq!(state.calls.load(Ordering::SeqCst), 3);
        stop.cancel();
        server.abort();
    }
}
