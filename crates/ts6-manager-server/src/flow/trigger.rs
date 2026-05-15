//! Trigger sources for the v1.1 flow engine (PURA-241).
//!
//! Per the architecture brief §3 a flow carries exactly one trigger. This
//! module is the only place that knows how to parse, register, and pump
//! events for each source:
//!
//!   - [`ParsedTrigger::Cron`] holds a `cron::Schedule`. The engine
//!     spawns a per-flow loop that sleeps until the next UTC tick, then
//!     emits a [`TriggerEvent`] onto the engine bus. Catch-up on missed
//!     ticks is skipped (brief §3 / §6.4).
//!   - [`ParsedTrigger::ManualFire`] has no scheduler — the
//!     `POST /api/flows/{id}/fire` handler calls
//!     [`crate::flow::FlowEngineHandle::fire`] directly. Stored here as a
//!     marker so the validation pass at flow-create rejects malformed
//!     trigger documents uniformly.
//!   - [`ParsedTrigger::Ts6ClientJoined`] holds the optional channel
//!     filter. Producers (the WS hub's `ts:client:connected` republisher)
//!     call [`crate::flow::FlowEngineHandle::on_client_joined`] for every
//!     observed event; the engine fans the event into matching flows.
//!
//! Validation happens at parse time — `Trigger::Cron { expression }` is
//! rejected early if `cron::Schedule::from_str` fails. The route layer
//! surfaces this as a `400 validation`.

use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use ts6_manager_shared::flows::{FlowId, Trigger};

/// Parsed, validated trigger definition. Holds the runtime-resolved
/// shape — `Schedule` for cron, filter ids for ts6ClientJoined. The
/// repo layer continues to persist the raw `Trigger` (JSON in
/// `bot_flow.flowData`); the engine re-parses on load.
#[derive(Debug, Clone)]
pub enum ParsedTrigger {
    /// Boxed — `cron::Schedule` is ~250 bytes; boxing keeps the enum
    /// small for the cheap `ManualFire` / `Ts6ClientJoined` cases.
    Cron(Box<cron::Schedule>),
    ManualFire,
    Ts6ClientJoined {
        channel_id: Option<i64>,
    },
}

impl ParsedTrigger {
    /// Parse `Trigger` into the runtime shape, rejecting invalid cron
    /// expressions with a human-readable error.
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
        }
    }

    /// Convenience for the engine's per-flow trigger loop. `None` when
    /// the source does not schedule itself (manualFire,
    /// ts6ClientJoined). For cron, returns the next UTC tick.
    pub fn next_tick(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            ParsedTrigger::Cron(schedule) => schedule.after(&now).next(),
            ParsedTrigger::ManualFire | ParsedTrigger::Ts6ClientJoined { .. } => None,
        }
    }
}

/// Marker for the source that fired a trigger. Persisted into the run
/// row's `trigger` document so operators can tell at-a-glance how a run
/// was kicked off.
#[derive(Debug, Clone)]
pub enum TriggerEvent {
    /// Cron tick at the given UTC instant.
    Cron { tick: DateTime<Utc> },
    /// Operator-driven fire. `context` is the body's optional JSON map.
    Manual {
        context: Option<serde_json::Map<String, serde_json::Value>>,
    },
    /// TS6 `notifycliententerview`. Carries the originating client info
    /// so action templating can reference `${trigger.clientNickname}`
    /// etc.
    Ts6ClientJoined {
        virtual_server_id: i64,
        channel_id: i64,
        client_unique_identifier: String,
        client_nickname: String,
        ts: DateTime<Utc>,
    },
}

impl TriggerEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            TriggerEvent::Cron { .. } => "cron",
            TriggerEvent::Manual { .. } => "manualFire",
            TriggerEvent::Ts6ClientJoined { .. } => "ts6ClientJoined",
        }
    }

    /// Serialise the event into the JSON document persisted on the run
    /// row. Round-trip with [`crate::repos::bot_flow_runs::BotFlowRun::trigger`].
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
        }
    }

    /// Idempotency key per brief §3. Engine logs the key alongside the
    /// run row so duplicate triggers (e.g. ts6 replays) can be
    /// correlated in the audit trail. Not currently used to deduplicate
    /// — the per-flow drop-on-busy semaphore is the primary defence.
    pub fn idempotency_key(&self, flow_id: FlowId) -> String {
        match self {
            TriggerEvent::Cron { tick } => format!("cron:{}:{}", flow_id.0, tick.timestamp()),
            TriggerEvent::Manual { .. } => {
                // Server-minted on each fire — opaque uuid would be over-
                // kill; the run id is already unique.
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_expression_parses_and_advances() {
        // `cron@0.12` uses 6-field expressions (sec min hour day mon dow).
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
        assert!(err.is_err(), "garbage cron must be rejected at parse time");
    }

    #[test]
    fn manual_fire_and_ts6_have_no_self_schedule() {
        for trigger in [
            Trigger::ManualFire,
            Trigger::Ts6ClientJoined {
                channel_id: Some(5),
            },
        ] {
            let parsed = ParsedTrigger::parse(&trigger).unwrap();
            assert!(parsed.next_tick(Utc::now()).is_none());
        }
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
        assert_eq!(json["clientUniqueIdentifier"], "uid-abc");
        assert_eq!(json["clientNickname"], "Alice");
    }
}
