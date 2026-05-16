//! Trigger sources for the v1.1 flow engine (PURA-241) and automod triggers
//! (PURA-300: Ts6ChatMessage, Ts6Flood).
//!
//! Per the architecture brief §3 a flow carries exactly one trigger. This
//! module is the only place that knows how to parse, register, and pump
//! events for each source:
//!
//!   - [`ParsedTrigger::Cron`] holds a `cron::Schedule`.
//!   - [`ParsedTrigger::ManualFire`] is a marker.
//!   - [`ParsedTrigger::Ts6ClientJoined`] holds the optional channel filter.
//!   - [`ParsedTrigger::Ts6ChatMessage`] holds channel + target-mode filters.
//!   - [`ParsedTrigger::Ts6Flood`] holds the windowed-counter spec; the
//!     shared [`FloodRegistry`] owns the per-`(spec, key)` sliding-window
//!     counters and is checked on every relevant event by the engine.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ts6_manager_shared::flows::{FloodScope, FloodSource, FlowId, Trigger};

/// Parsed, validated trigger definition. Holds the runtime-resolved shape.
#[derive(Debug, Clone)]
pub enum ParsedTrigger {
    Cron(Box<cron::Schedule>),
    ManualFire,
    Ts6ClientJoined {
        channel_id: Option<i64>,
    },
    Ts6ChatMessage {
        channel_id: Option<i64>,
        target_mode: Option<String>,
    },
    Ts6Flood {
        source: FloodSource,
        threshold: u32,
        window_secs: u32,
        scope: FloodScope,
    },
}

impl ParsedTrigger {
    pub fn parse(trigger: &Trigger) -> Result<Self> {
        match trigger {
            Trigger::Cron { expression } => {
                let schedule = cron::Schedule::from_str(expression)
                    .with_context(|| format!("cron expression `{expression}` failed to parse"))?;
                Ok(ParsedTrigger::Cron(Box::new(schedule)))
            }
            Trigger::ManualFire => Ok(ParsedTrigger::ManualFire),
            Trigger::Ts6ClientJoined { channel_id } => Ok(ParsedTrigger::Ts6ClientJoined {
                channel_id: *channel_id,
            }),
            Trigger::Ts6ChatMessage {
                channel_id,
                target_mode,
            } => Ok(ParsedTrigger::Ts6ChatMessage {
                channel_id: *channel_id,
                target_mode: target_mode.clone(),
            }),
            Trigger::Ts6Flood {
                source,
                threshold,
                window_secs,
                scope,
            } => {
                if *threshold == 0 {
                    anyhow::bail!("ts6Flood threshold must be > 0");
                }
                if *window_secs == 0 {
                    anyhow::bail!("ts6Flood windowSecs must be > 0");
                }
                Ok(ParsedTrigger::Ts6Flood {
                    source: *source,
                    threshold: *threshold,
                    window_secs: *window_secs,
                    scope: *scope,
                })
            }
        }
    }

    pub fn next_tick(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            ParsedTrigger::Cron(schedule) => schedule.after(&now).next(),
            ParsedTrigger::ManualFire
            | ParsedTrigger::Ts6ClientJoined { .. }
            | ParsedTrigger::Ts6ChatMessage { .. }
            | ParsedTrigger::Ts6Flood { .. } => None,
        }
    }
}

/// Marker for the source that fired a trigger. Persisted into the run
/// row's `trigger` document.
#[derive(Debug, Clone)]
pub enum TriggerEvent {
    Cron { tick: DateTime<Utc> },
    Manual {
        context: Option<serde_json::Map<String, serde_json::Value>>,
    },
    Ts6ClientJoined {
        virtual_server_id: i64,
        channel_id: i64,
        client_unique_identifier: String,
        client_nickname: String,
        ts: DateTime<Utc>,
    },
    /// Fired from `notifytextmessage`.
    Ts6ChatMessage {
        virtual_server_id: i64,
        channel_id: Option<i64>,
        target_mode: String,
        client_unique_identifier: String,
        client_nickname: String,
        message: String,
        ts: DateTime<Utc>,
    },
    /// Fired when a flood window counter crosses its threshold.
    Ts6Flood {
        source: FloodSource,
        scope: FloodScope,
        /// The bucket key: uid for subject/ip, `"*"` for global.
        bucket_key: String,
        /// Count that tripped the threshold.
        count: u32,
        threshold: u32,
        window_secs: u32,
        ts: DateTime<Utc>,
    },
}

impl TriggerEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            TriggerEvent::Cron { .. } => "cron",
            TriggerEvent::Manual { .. } => "manualFire",
            TriggerEvent::Ts6ClientJoined { .. } => "ts6ClientJoined",
            TriggerEvent::Ts6ChatMessage { .. } => "ts6ChatMessage",
            TriggerEvent::Ts6Flood { .. } => "ts6Flood",
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            TriggerEvent::Cron { tick } => serde_json::json!({
                "kind": "cron",
                "tick": tick,
            }),
            TriggerEvent::Manual { context } => serde_json::json!({
                "kind": "manualFire",
                "context": context.clone().unwrap_or_default(),
            }),
            TriggerEvent::Ts6ClientJoined {
                virtual_server_id,
                channel_id,
                client_unique_identifier,
                client_nickname,
                ts,
            } => serde_json::json!({
                "kind": "ts6ClientJoined",
                "virtualServerId": virtual_server_id,
                "channelId": channel_id,
                "clientUniqueIdentifier": client_unique_identifier,
                "clientNickname": client_nickname,
                "ts": ts,
            }),
            TriggerEvent::Ts6ChatMessage {
                virtual_server_id,
                channel_id,
                target_mode,
                client_unique_identifier,
                client_nickname,
                message,
                ts,
            } => serde_json::json!({
                "kind": "ts6ChatMessage",
                "virtualServerId": virtual_server_id,
                "channelId": channel_id,
                "targetMode": target_mode,
                "clientUniqueIdentifier": client_unique_identifier,
                "clientNickname": client_nickname,
                "message": message,
                "ts": ts,
            }),
            TriggerEvent::Ts6Flood {
                source,
                scope,
                bucket_key,
                count,
                threshold,
                window_secs,
                ts,
            } => {
                let source_str = match source {
                    FloodSource::ClientJoined => "clientJoined",
                    FloodSource::ChatMessage => "chatMessage",
                    FloodSource::ClientMoved => "clientMoved",
                };
                let scope_str = match scope {
                    FloodScope::Subject => "subject",
                    FloodScope::Ip => "ip",
                    FloodScope::Global => "global",
                };
                serde_json::json!({
                    "kind": "ts6Flood",
                    "source": source_str,
                    "scope": scope_str,
                    "bucketKey": bucket_key,
                    "count": count,
                    "threshold": threshold,
                    "windowSecs": window_secs,
                    "ts": ts,
                })
            }
        }
    }

    pub fn idempotency_key(&self, flow_id: FlowId) -> String {
        match self {
            TriggerEvent::Cron { tick } => format!("cron:{}:{}", flow_id.0, tick.timestamp()),
            TriggerEvent::Manual { .. } => {
                format!("manual:{}:{}", flow_id.0, Utc::now().timestamp_micros())
            }
            TriggerEvent::Ts6ClientJoined {
                virtual_server_id,
                client_unique_identifier,
                ts,
                ..
            } => format!(
                "ts6:{}:{}:{}:{}",
                flow_id.0,
                virtual_server_id,
                client_unique_identifier,
                ts.timestamp_millis()
            ),
            TriggerEvent::Ts6ChatMessage {
                virtual_server_id,
                client_unique_identifier,
                ts,
                ..
            } => format!(
                "ts6chat:{}:{}:{}:{}",
                flow_id.0,
                virtual_server_id,
                client_unique_identifier,
                ts.timestamp_millis()
            ),
            TriggerEvent::Ts6Flood {
                source,
                bucket_key,
                ts,
                ..
            } => {
                let src = match source {
                    FloodSource::ClientJoined => "joined",
                    FloodSource::ChatMessage => "chat",
                    FloodSource::ClientMoved => "moved",
                };
                format!(
                    "flood:{}:{}:{}:{}",
                    flow_id.0,
                    src,
                    bucket_key,
                    ts.timestamp_millis()
                )
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Flood window registry
// ---------------------------------------------------------------------------

/// Spec key for a flood subscription — uniquely identifies the window params.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FloodSpec {
    pub flow_id: FlowId,
    pub virtual_server_id: i64,
    pub source: FloodSourceKey,
    pub threshold: u32,
    pub window_secs: u32,
    pub scope: FloodScope,
}

/// Stable, hashable form of [`FloodSource`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FloodSourceKey {
    ClientJoined,
    ChatMessage,
    ClientMoved,
}

impl From<FloodSource> for FloodSourceKey {
    fn from(s: FloodSource) -> Self {
        match s {
            FloodSource::ClientJoined => FloodSourceKey::ClientJoined,
            FloodSource::ChatMessage => FloodSourceKey::ChatMessage,
            FloodSource::ClientMoved => FloodSourceKey::ClientMoved,
        }
    }
}

/// Per-`(spec, bucket_key)` sliding-window state.
struct Window {
    /// Ring buffer of event timestamps (Instant).
    timestamps: std::collections::VecDeque<Instant>,
}

impl Window {
    fn new() -> Self {
        Self {
            timestamps: std::collections::VecDeque::new(),
        }
    }

    /// Record one event and return the count within the window after eviction.
    fn record(&mut self, window: Duration) -> u32 {
        let now = Instant::now();
        self.timestamps.push_back(now);
        // Evict expired entries from the front.
        while let Some(&front) = self.timestamps.front() {
            if now.saturating_duration_since(front) > window {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
        self.timestamps.len() as u32
    }
}

/// Shared sliding-window registry used by all flood trigger subscriptions.
/// Cloneable handle backed by an `Arc<Mutex<…>>`.
#[derive(Clone, Default)]
pub struct FloodRegistry {
    inner: Arc<Mutex<FloodRegistryInner>>,
}

#[derive(Default)]
struct FloodRegistryInner {
    /// `(spec_key, bucket_key)` → window state.
    windows: HashMap<(FloodSpec, String), Window>,
}

impl FloodRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one event for `(spec, bucket_key)`. Returns `Some(count)` if
    /// the count crossed `spec.threshold` after this event, `None` otherwise.
    pub fn record(&self, spec: &FloodSpec, bucket_key: &str) -> Option<u32> {
        let window = Duration::from_secs(spec.window_secs as u64);
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let entry = inner
            .windows
            .entry((spec.clone(), bucket_key.to_string()))
            .or_insert_with(Window::new);
        let count = entry.record(window);
        // Fire only when we cross the threshold exactly (not on every event
        // after). This means a sustained flood fires once per window slide
        // rather than on every event.
        if count == spec.threshold {
            Some(count)
        } else {
            None
        }
    }

    /// Remove all windows for a given flow (called on disable/delete).
    pub fn remove_flow(&self, flow_id: FlowId) {
        let mut inner = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        inner.windows.retain(|(spec, _), _| spec.flow_id != flow_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ts6_manager_shared::flows::Trigger;

    #[test]
    fn cron_expression_parses_and_advances() {
        let parsed = ParsedTrigger::parse(&Trigger::Cron {
            expression: "0 0 * * * *".into(),
        })
        .expect("valid cron parses");
        let now = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 1, 1, 12, 30, 0).unwrap();
        let next = parsed.next_tick(now).expect("cron advances");
        assert!(next > now);
    }

    #[test]
    fn malformed_cron_expression_is_rejected() {
        let err = ParsedTrigger::parse(&Trigger::Cron {
            expression: "this is not a cron expression".into(),
        });
        assert!(err.is_err());
    }

    #[test]
    fn manual_fire_and_ts6_have_no_self_schedule() {
        for trigger in [
            Trigger::ManualFire,
            Trigger::Ts6ClientJoined { channel_id: Some(5) },
            Trigger::Ts6ChatMessage {
                channel_id: None,
                target_mode: None,
            },
            Trigger::Ts6Flood {
                source: FloodSource::ChatMessage,
                threshold: 3,
                window_secs: 10,
                scope: FloodScope::Subject,
            },
        ] {
            let parsed = ParsedTrigger::parse(&trigger).unwrap();
            assert!(parsed.next_tick(Utc::now()).is_none());
        }
    }

    #[test]
    fn ts6_flood_zero_threshold_rejected() {
        let err = ParsedTrigger::parse(&Trigger::Ts6Flood {
            source: FloodSource::ChatMessage,
            threshold: 0,
            window_secs: 10,
            scope: FloodScope::Subject,
        });
        assert!(err.is_err());
    }

    #[test]
    fn trigger_event_to_json_preserves_camel_case_keys() {
        let ev = TriggerEvent::Ts6ClientJoined {
            virtual_server_id: 1,
            channel_id: 5,
            client_unique_identifier: "uid-abc".into(),
            client_nickname: "Alice".into(),
            ts: Utc::now(),
        };
        let json = ev.to_json();
        assert_eq!(json["kind"], "ts6ClientJoined");
        assert_eq!(json["virtualServerId"], 1);
        assert_eq!(json["channelId"], 5);

        let chat = TriggerEvent::Ts6ChatMessage {
            virtual_server_id: 1,
            channel_id: Some(3),
            target_mode: "channel".into(),
            client_unique_identifier: "uid-xyz".into(),
            client_nickname: "Bob".into(),
            message: "hello".into(),
            ts: Utc::now(),
        };
        let json = chat.to_json();
        assert_eq!(json["kind"], "ts6ChatMessage");
        assert_eq!(json["message"], "hello");
        assert_eq!(json["targetMode"], "channel");
    }

    #[test]
    fn flood_registry_fires_at_threshold() {
        let reg = FloodRegistry::new();
        let spec = FloodSpec {
            flow_id: FlowId(1),
            virtual_server_id: 1,
            source: FloodSourceKey::ChatMessage,
            threshold: 3,
            window_secs: 60,
            scope: FloodScope::Subject,
        };
        assert!(reg.record(&spec, "uid-a").is_none(), "1st event: no fire");
        assert!(reg.record(&spec, "uid-a").is_none(), "2nd event: no fire");
        let result = reg.record(&spec, "uid-a");
        assert_eq!(result, Some(3), "3rd event should cross threshold");
    }

    #[test]
    fn flood_registry_buckets_are_isolated() {
        let reg = FloodRegistry::new();
        let spec = FloodSpec {
            flow_id: FlowId(2),
            virtual_server_id: 1,
            source: FloodSourceKey::ChatMessage,
            threshold: 2,
            window_secs: 60,
            scope: FloodScope::Subject,
        };
        // Two events for uid-a: fires.
        reg.record(&spec, "uid-a");
        assert_eq!(reg.record(&spec, "uid-a"), Some(2));
        // uid-b is a separate bucket — starts fresh.
        assert!(reg.record(&spec, "uid-b").is_none());
    }

    #[test]
    fn flood_registry_window_expiry() {
        let reg = FloodRegistry::new();
        let spec = FloodSpec {
            flow_id: FlowId(3),
            virtual_server_id: 1,
            source: FloodSourceKey::ChatMessage,
            // 1-second window so we can expire without sleeping in a test.
            threshold: 3,
            window_secs: 0, // zero window: every event is immediately stale
            scope: FloodScope::Subject,
        };
        // With window_secs=0 the Duration is zero — every prior timestamp
        // is always older than "now", so the window stays at most 1.
        for _ in 0..5 {
            let count = reg.record(&spec, "uid-exp");
            assert!(count.is_none(), "each event seen alone: no threshold cross");
        }
    }

    #[test]
    fn flood_remove_flow_clears_windows() {
        let reg = FloodRegistry::new();
        let spec = FloodSpec {
            flow_id: FlowId(10),
            virtual_server_id: 1,
            source: FloodSourceKey::ClientJoined,
            threshold: 5,
            window_secs: 60,
            scope: FloodScope::Global,
        };
        reg.record(&spec, "*");
        reg.record(&spec, "*");
        reg.remove_flow(FlowId(10));
        // After removal the bucket is gone; count resets to 1 (no prior entries).
        assert!(reg.record(&spec, "*").is_none());
    }
}
