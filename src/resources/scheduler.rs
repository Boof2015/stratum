//! Bounded waiting, cancellation-safe leases, and non-preemptive live resizing.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Priority {
    #[default]
    Playback,
    Prefetch,
}
struct Waiting {
    id: u64,
    priority: Priority,
    since: Instant,
}
struct State {
    limit: usize,
    running: usize,
    next: u64,
    waiting: VecDeque<Waiting>,
    stopped: bool,
}
struct Inner {
    state: Mutex<State>,
    changed: Notify,
    queue: bool,
}
#[derive(Clone)]
pub(crate) struct Scheduler(Arc<Inner>);
#[derive(Serialize)]
pub(crate) struct Stats {
    pub limit: usize,
    pub running: usize,
    pub waiting: usize,
}
pub(crate) struct Permit(Arc<Inner>);
struct Ticket {
    inner: Arc<Inner>,
    id: u64,
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.inner
            .state
            .lock()
            .unwrap()
            .waiting
            .retain(|entry| entry.id != self.id);
        self.inner.changed.notify_waiters();
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().running -= 1;
        self.0.changed.notify_waiters();
    }
}
impl Scheduler {
    pub fn new(limit: usize, queue: bool) -> Self {
        Self(Arc::new(Inner {
            state: Mutex::new(State {
                limit,
                running: 0,
                next: 0,
                waiting: VecDeque::new(),
                stopped: false,
            }),
            changed: Notify::new(),
            queue,
        }))
    }
    pub fn set_limit(&self, limit: usize) {
        self.0.state.lock().unwrap().limit = limit;
        self.0.changed.notify_waiters();
    }
    pub fn stats(&self) -> Stats {
        let state = self.0.state.lock().unwrap();
        Stats {
            limit: state.limit,
            running: state.running,
            waiting: state.waiting.len(),
        }
    }
    pub async fn acquire(&self, priority: Priority) -> Option<Permit> {
        let ticket = {
            let mut state = self.0.state.lock().unwrap();
            if state.stopped {
                return None;
            }
            if state.running < state.limit && state.waiting.is_empty() {
                state.running += 1;
                return Some(Permit(self.0.clone()));
            }
            if !self.0.queue || state.waiting.len() >= 8 {
                return None;
            }
            state.next += 1;
            let id = state.next;
            state.waiting.push_back(Waiting {
                id,
                priority,
                since: Instant::now(),
            });
            Ticket {
                inner: self.0.clone(),
                id,
            }
        };
        let wait = async {
            loop {
                // Register before checking the state to avoid lost wakeups.
                let notified = self.0.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                {
                    let mut state = self.0.state.lock().unwrap();
                    if state.stopped {
                        return None;
                    }
                    let oldest = state.waiting.front();
                    // Prefer playback, but let an older prefetch make progress.
                    let first = if oldest
                        .is_some_and(|w| w.since.elapsed() >= Duration::from_millis(500))
                    {
                        oldest
                    } else {
                        state
                            .waiting
                            .iter()
                            .find(|w| w.priority == Priority::Playback)
                            .or(oldest)
                    };
                    if state.running < state.limit && first.is_some_and(|w| w.id == ticket.id) {
                        state.waiting.retain(|w| w.id != ticket.id);
                        state.running += 1;
                        self.0.changed.notify_waiters();
                        return Some(Permit(self.0.clone()));
                    }
                }
                notified.await;
            }
        };
        // Waiting must not consume the whole HTTP deadline. Clients can retry.
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .ok()
            .flatten()
    }
    pub async fn drain(&self) {
        self.0.state.lock().unwrap().stopped = true;
        self.0.changed.notify_waiters();
        loop {
            let notified = self.0.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.0.state.lock().unwrap().running == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn priority_cancellation_timeout_and_resizing_keep_real_leases() {
        let scheduler = Scheduler::new(2, true);
        let a = scheduler.acquire(Priority::Playback).await.unwrap();
        let b = scheduler.acquire(Priority::Playback).await.unwrap();
        scheduler.set_limit(1);
        let other = scheduler.clone();
        let low = tokio::spawn(async move { other.acquire(Priority::Prefetch).await });
        tokio::task::yield_now().await;
        let other = scheduler.clone();
        let high = tokio::spawn(async move { other.acquire(Priority::Playback).await });
        tokio::task::yield_now().await;
        assert_eq!(scheduler.stats().waiting, 2);
        drop(a);
        tokio::task::yield_now().await;
        assert!(!high.is_finished());
        drop(b);
        tokio::task::yield_now().await;
        let next = high.await.unwrap().unwrap();
        assert!(!low.is_finished());
        low.abort();
        let _ = low.await;
        assert_eq!(scheduler.stats().waiting, 0);
        let other = scheduler.clone();
        let timed = tokio::spawn(async move { other.acquire(Priority::Playback).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(timed.await.unwrap().is_none());
        assert_eq!(scheduler.stats().running, 1);
        assert_eq!(scheduler.stats().waiting, 0);
        drop(next);
        scheduler.drain().await;
        assert!(scheduler.acquire(Priority::Playback).await.is_none());
    }
    #[tokio::test(start_paused = true)]
    async fn an_older_prefetch_is_not_starved_and_increasing_limit_wakes_waiters() {
        let scheduler = Scheduler::new(1, true);
        let first = scheduler.acquire(Priority::Playback).await.unwrap();
        let other = scheduler.clone();
        let low = tokio::spawn(async move { other.acquire(Priority::Prefetch).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(600)).await;
        let other = scheduler.clone();
        let high = tokio::spawn(async move { other.acquire(Priority::Playback).await });
        tokio::task::yield_now().await;
        drop(first);
        tokio::task::yield_now().await;
        let low = low.await.unwrap().unwrap();
        assert!(!high.is_finished());
        scheduler.set_limit(2);
        let high = high.await.unwrap().unwrap();
        assert_eq!(scheduler.stats().running, 2);
        drop(low);
        drop(high);
    }
}
