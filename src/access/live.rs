//! Bounded, in-memory observations for the owner dashboard. Never credentials.
use crate::media::Quality;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::time::Instant;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
pub(super) fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}
#[derive(Clone, Copy, Default)]
struct Bucket {
    second: u64,
    bytes: u64,
}
pub(crate) struct Traffic {
    start: Instant,
    buckets: Mutex<[Bucket; 5]>,
}
impl Default for Traffic {
    fn default() -> Self {
        Self {
            start: Instant::now(),
            buckets: Mutex::new([Bucket::default(); 5]),
        }
    }
}
impl Traffic {
    fn add(&self, bytes: usize) {
        let second = self.start.elapsed().as_secs();
        let mut buckets = self.buckets.lock().unwrap();
        let bucket = &mut buckets[second as usize % 5];
        if bucket.second != second {
            *bucket = Bucket { second, bytes: 0 };
        }
        bucket.bytes = bucket.bytes.saturating_add(bytes as u64);
    }
    pub(crate) fn bytes_per_second(&self) -> u64 {
        let second = self.start.elapsed().as_secs();
        self.buckets
            .lock()
            .unwrap()
            .iter()
            .filter(|b| second.saturating_sub(b.second) < 5)
            .map(|b| b.bytes)
            .sum::<u64>()
            / 5
    }
}
pub(super) struct Stream {
    id: u64,
    last: Mutex<Option<(Quality, Instant)>>,
}
impl Default for Stream {
    fn default() -> Self {
        Self {
            id: next_id(),
            last: Mutex::new(None),
        }
    }
}
impl Stream {
    pub(super) fn view(&self) -> Value {
        let last = *self.last.lock().unwrap();
        json!({
            "id": self.id,
            "quality": last.map(|(quality, _)| quality),
            "sent_seconds_ago": last.map(|(_, at)| at.elapsed().as_secs()),
        })
    }
}
#[derive(Clone)]
pub(super) struct Delivery {
    pub traffic: Arc<Traffic>,
    pub stream: Arc<Stream>,
    pub quality: Quality,
    pub payload_offset: usize,
}
impl Delivery {
    pub(super) fn sent(&self, start: usize, end: usize) {
        let bytes = end.saturating_sub(start.max(self.payload_offset));
        if bytes > 0 {
            self.traffic.add(bytes);
            *self.stream.last.lock().unwrap() = Some((self.quality, Instant::now()));
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn delivery_counts_only_yielded_audio_and_expires_without_polling() {
        let traffic = Arc::new(Traffic::default());
        let stream = Arc::new(Stream::default());
        let delivery = Delivery {
            traffic: traffic.clone(),
            stream: stream.clone(),
            quality: Quality::Opus64,
            payload_offset: 100,
        };
        assert_eq!(stream.view()["quality"], Value::Null);
        delivery.sent(0, 80);
        assert_eq!(traffic.bytes_per_second(), 0);
        assert_eq!(stream.view()["quality"], Value::Null);
        delivery.sent(80, 600);
        assert_eq!(traffic.bytes_per_second(), 100);
        assert_eq!(stream.view()["quality"], "opus-64");
        tokio::time::advance(std::time::Duration::from_secs(5)).await;
        assert_eq!(traffic.bytes_per_second(), 0);
        assert_eq!(stream.view()["sent_seconds_ago"], 5);
        let original = Delivery {
            quality: Quality::Lossless,
            ..delivery
        };
        original.sent(100, 350);
        assert_eq!(stream.view()["quality"], "lossless");
        assert_eq!(traffic.bytes_per_second(), 50);
    }
}
