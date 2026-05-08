//! Wire envelope + bounded ring buffer for the WS hub — PURA-70.
//!
//! ## Wire shape (server → client)
//!
//! ```json
//! { "id": 1234, "topic": "server:1:clients", "type": "ts:client:connected",
//!   "data": { ... }, "ts": 1715000000000 }
//! ```
//!
//! `id` is a hub-global monotonic `u64`; clients echo it back as
//! `lastEventId` on `subscribe` after a reconnect (D-WS deviation, see
//! `study-documents/ts6-manager-impl-deviations.md`).
//!
//! `topic` is the topic the event belongs to. Spec §8.4 only specifies
//! `type` + `data`; the `id`, `topic`, and `ts` keys are additive Phase 2
//! fields and do not change the meaning of the spec keys.
//!
//! ## Ring buffer
//!
//! [`RingBuffer`] is a per-server bounded `VecDeque<Envelope>` capped at
//! `RING_CAPACITY`. On overflow the oldest entry is dropped — the buffer
//! intentionally has no eviction policy beyond FIFO, because the use case
//! is "fill a small reconnect gap" not "durable replay". A reconnecting
//! client whose `lastEventId` predates the oldest buffered id receives
//! exactly the buffered tail (this is detectable by the client: the first
//! replayed `id` will be greater than `lastEventId + 1`).

use std::collections::VecDeque;

use serde::Serialize;
use serde_json::Value;

use super::topic::Topic;

/// Capacity of the per-server ring buffer. Sized so a low-traffic server
/// can replay the last few seconds of events on reconnect without holding
/// meaningful memory (256 envelopes × ~1KB ≈ 256KB worst case per server).
pub const RING_CAPACITY: usize = 256;

/// Server → client envelope.
#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub id: u64,
    /// Topic on the wire as `server:{id}:{kind}`.
    pub topic: String,
    /// Spec §8.4 event name (e.g. `ts:client:connected`,
    /// `dashboard:tick`, `dropped`). Hub does not interpret this; the
    /// emitter sets it.
    #[serde(rename = "type")]
    pub kind: String,
    /// Spec §8.4 payload. Free-form JSON.
    pub data: Value,
    /// Unix epoch milliseconds at which the hub stamped the envelope.
    pub ts: i64,
}

impl Envelope {
    pub fn new(id: u64, topic: &Topic, kind: impl Into<String>, data: Value, ts: i64) -> Self {
        Self {
            id,
            topic: topic.to_string(),
            kind: kind.into(),
            data,
            ts,
        }
    }
}

/// Bounded FIFO of recent envelopes for one server. Used for the D-WS
/// `lastEventId` reconnect-replay path.
#[derive(Debug)]
pub struct RingBuffer {
    capacity: usize,
    inner: VecDeque<Envelope>,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: VecDeque::with_capacity(capacity),
        }
    }

    pub fn push(&mut self, env: Envelope) {
        if self.inner.len() == self.capacity {
            self.inner.pop_front();
        }
        self.inner.push_back(env);
    }

    /// Return all envelopes whose `id > last_event_id` matching `topic`.
    /// Caller takes ownership of clones (the buffer is small and replay
    /// is rare).
    pub fn replay_for(&self, topic_string: &str, last_event_id: u64) -> Vec<Envelope> {
        self.inner
            .iter()
            .filter(|e| e.id > last_event_id && e.topic == topic_string)
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::topic::{Topic, TopicKind};
    use serde_json::json;

    fn env(id: u64, topic: &Topic, kind: &str) -> Envelope {
        Envelope::new(id, topic, kind, json!({}), 0)
    }

    #[test]
    fn ring_evicts_fifo() {
        let mut rb = RingBuffer::new(3);
        let t = Topic::new(1, TopicKind::Clients);
        rb.push(env(1, &t, "a"));
        rb.push(env(2, &t, "b"));
        rb.push(env(3, &t, "c"));
        rb.push(env(4, &t, "d"));
        assert_eq!(rb.len(), 3);
        let all = rb.replay_for(&t.to_string(), 0);
        let ids: Vec<u64> = all.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![2, 3, 4], "oldest entry should evict");
    }

    #[test]
    fn replay_respects_last_event_id() {
        let mut rb = RingBuffer::new(8);
        let t = Topic::new(1, TopicKind::Clients);
        for i in 1..=5u64 {
            rb.push(env(i, &t, "x"));
        }
        let after_3 = rb.replay_for(&t.to_string(), 3);
        let ids: Vec<u64> = after_3.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![4, 5]);
    }

    #[test]
    fn replay_filters_by_topic() {
        let mut rb = RingBuffer::new(8);
        let clients = Topic::new(1, TopicKind::Clients);
        let channels = Topic::new(1, TopicKind::Channels);
        rb.push(env(1, &clients, "a"));
        rb.push(env(2, &channels, "b"));
        rb.push(env(3, &clients, "c"));
        let only_clients = rb.replay_for(&clients.to_string(), 0);
        let ids: Vec<u64> = only_clients.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 3], "channels event must be filtered out");
    }

    #[test]
    fn envelope_serialises_with_spec_keys() {
        let t = Topic::new(7, TopicKind::Clients);
        let e = Envelope::new(42, &t, "ts:client:connected", json!({"clid": 5}), 1_715_000_000_000);
        let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
        assert_eq!(v["id"], 42);
        assert_eq!(v["topic"], "server:7:clients");
        assert_eq!(v["type"], "ts:client:connected", "spec §8.4 'type' key");
        assert_eq!(v["data"]["clid"], 5);
        assert_eq!(v["ts"], 1_715_000_000_000_i64);
    }
}
