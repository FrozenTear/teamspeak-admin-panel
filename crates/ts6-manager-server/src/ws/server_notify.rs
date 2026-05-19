//! Phase 2 PURA-80 — TS server-notify event source for the WS hub.
//!
//! Per `server_connection` row whose `controlPath = 'ssh'`, spawn a
//! worker that:
//!
//! 1. Pulls the SSH [`TransportHandle`] from
//!    [`crate::control::ControlBackendPool`] via
//!    [`crate::control::ControlBackend::ssh_transport`]. WebQuery rows
//!    are skipped silently — the HTTP path has no `notify*` event
//!    surface.
//! 2. Subscribes to the transport's `notify*` broadcast.
//! 3. Issues `servernotifyregister` for the four event classes the WS
//!    topics consume (`server` / `channel` / `textserver` / `textchannel`).
//! 4. Translates each parsed [`NotifyFrame`] into a [`Topic`]
//!    (`server:{id}:clients` for client events, `server:{id}:channels`
//!    for channel events, `server:{id}:logs` for text messages) and
//!    calls [`Hub::publish`].
//! 5. Re-registers after every supervisor reconnect, gated on the
//!    [`TransportHandle::subscribe_session_up`] tick. The upstream
//!    drops the registration when the SSH session ends.
//!
//! ## Topic mapping
//!
//! | TS notify event                       | Topic kind   | Envelope `type`               |
//! | ------------------------------------- | ------------ | ----------------------------- |
//! | `notifycliententer(view)`             | `clients`    | `ts:client:connected`         |
//! | `notifyclientleftview`                | `clients`    | `ts:client:disconnected`      |
//! | `notifyclientmoved`                   | `clients`    | `ts:client:moved`             |
//! | `notifyclientupdated`                 | `clients`    | `ts:client:updated`           |
//! | `notifychannelcreated`                | `channels`   | `ts:channel:created`          |
//! | `notifychanneldeleted`                | `channels`   | `ts:channel:deleted`          |
//! | `notifychanneledited`                 | `channels`   | `ts:channel:edited`           |
//! | `notifychanneldescriptionchanged`     | `channels`   | `ts:channel:description-changed` |
//! | `notifychannelmoved`                  | `channels`   | `ts:channel:moved`            |
//! | `notifytextmessage`                   | `logs`       | `ts:textmessage`              |
//!
//! ## Out of scope (sibling/follow-up)
//!
//! - Synthetic event derivation from `notifyclientupdated`
//!   (`client_mic_muted`, `client_recording_started`, …) per impl-plan
//!   §3.5 — needs a per-client state cache, follow-up child issue.
//! - `logview` periodic tail → `server:{id}:logs`.
//! - Reconnect state-flush diff (impl-plan §3.5 risk) — the worker
//!   re-registers but does not yet replay a fresh `clientlist` /
//!   `channellist` snapshot to recover state changes that happened
//!   during the disconnect window.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::control::ControlBackendPool;
use crate::db::Database;
use crate::repos::server_connections::{self, ServerConnection};
use crate::sshbridge::SshBridgeError;
use crate::sshbridge::transport::TransportHandle;
use crate::sshbridge::wire::NotifyFrame;
use crate::ws::Hub;
use crate::ws::topic::{Topic, TopicKind};

/// How often the supervisor reconciles the row set with running
/// workers. Same cadence as the dashboard tick so a flipped `enabled`
/// flag (or a `controlPath` switch) takes effect within one cycle.
const RECONCILE_INTERVAL_SECS: u64 = 5;

/// Inputs the supervisor and per-server workers need.
#[derive(Clone)]
pub struct EventSourceDeps {
    pub db: Arc<Database>,
    pub hub: Hub,
    pub control: ControlBackendPool,
    /// Flow-engine handle — the worker bridges join/chat/move notify
    /// frames into the automod trigger surface (PURA-300).
    pub flow_engine: crate::flow::FlowEngineHandle,
}

/// Drop-guard returned by [`spawn`]. Holding it keeps the watch sender
/// alive; dropping it (or calling [`shutdown`](Self::shutdown)) signals
/// the supervisor and every worker to exit.
pub struct EventSourceHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl EventSourceHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join.await;
    }
}

/// Spawn the supervisor task. Mirrors the dashboard-tick spawn shape so
/// `main.rs` wires both with the same `_handle`-as-drop-guard pattern.
pub fn spawn(deps: EventSourceDeps) -> EventSourceHandle {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(run_supervisor(deps, shutdown_rx));
    EventSourceHandle { shutdown_tx, join }
}

struct WorkerHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

async fn run_supervisor(deps: EventSourceDeps, mut shutdown_rx: watch::Receiver<bool>) {
    let mut workers: HashMap<i64, WorkerHandle> = HashMap::new();
    reconcile(&deps, &mut workers).await;

    let mut interval = tokio::time::interval(Duration::from_secs(RECONCILE_INTERVAL_SECS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    interval.tick().await; // discard the immediate first tick

    loop {
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() { break; }
            }
            _ = interval.tick() => {
                reconcile(&deps, &mut workers).await;
            }
        }
    }

    for (config_id, worker) in workers.drain() {
        let _ = worker.shutdown_tx.send(true);
        if let Err(e) = worker.join.await {
            tracing::warn!(
                target: "ws::server_notify",
                config_id,
                error = %e,
                "server-notify worker join failed during shutdown",
            );
        }
    }
}

async fn reconcile(deps: &EventSourceDeps, workers: &mut HashMap<i64, WorkerHandle>) {
    let connections = match server_connections::list(&deps.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "ws::server_notify",
                error = %e,
                "server-notify reconcile: server_connections::list failed; \
                 retrying on next cycle",
            );
            return;
        }
    };

    let live: HashSet<i64> = connections
        .iter()
        .filter(|c| c.enabled && c.controlPath == "ssh")
        .map(|c| c.id)
        .collect();

    let stale: Vec<i64> = workers
        .keys()
        .copied()
        .filter(|id| !live.contains(id))
        .collect();
    for id in stale {
        if let Some(worker) = workers.remove(&id) {
            let _ = worker.shutdown_tx.send(true);
            let _ = worker.join.await;
            tracing::info!(
                target: "ws::server_notify",
                config_id = id,
                "stopped server-notify worker (disabled, removed, or controlPath changed)",
            );
        }
    }

    for connection in connections
        .into_iter()
        .filter(|c| c.enabled && c.controlPath == "ssh")
    {
        if !workers.contains_key(&connection.id) {
            let id = connection.id;
            let worker = spawn_worker(deps.clone(), connection);
            workers.insert(id, worker);
            tracing::info!(
                target: "ws::server_notify",
                config_id = id,
                "spawned server-notify worker",
            );
        }
    }
}

fn spawn_worker(deps: EventSourceDeps, connection: ServerConnection) -> WorkerHandle {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(run_worker(deps, connection, shutdown_rx));
    WorkerHandle { shutdown_tx, join }
}

async fn run_worker(
    deps: EventSourceDeps,
    connection: ServerConnection,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let config_id = connection.id;

    let backend = match deps
        .control
        .get_or_build(config_id, Some(&connection))
        .await
    {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(
                target: "ws::server_notify",
                config_id,
                error = %e,
                "failed to build SSH control backend; worker exiting",
            );
            return;
        }
    };
    let Some(transport) = backend.ssh_transport() else {
        // The reconcile loop already filtered for `controlPath == 'ssh'`,
        // so this branch only fires if the pool returned a non-SSH
        // backend (e.g. a future fallback). Log and exit.
        tracing::info!(
            target: "ws::server_notify",
            config_id,
            "control backend is not SSH-driven; worker exiting",
        );
        return;
    };

    // Subscribe BEFORE the first registration so the bootstrap
    // session-up tick (fired by the supervisor task on first connect)
    // does not race the first call to `register_all`.
    let mut session_up_rx = transport.subscribe_session_up();
    let mut notify_rx = transport.subscribe_notify();

    if let Err(e) = register_all(&transport).await {
        tracing::warn!(
            target: "ws::server_notify",
            config_id,
            error = %e,
            "initial servernotifyregister failed; will retry on next session_up",
        );
    }

    // Per-worker clid→UID cache for the flow-engine bridge. `notifyclientmoved`
    // frames carry only the numeric client id, so the UID a subject-scoped
    // flood window needs is recovered from the `enterview` the client sent on
    // join. Reset on worker restart — acceptable: flood windows are
    // seconds-to-minutes and a restart simply forgives an in-flight burst
    // (automod brief §3.3).
    let mut clid_uid: HashMap<String, String> = HashMap::new();

    loop {
        tokio::select! {
            biased;

            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() { return; }
            }

            up = session_up_rx.recv() => {
                match up {
                    Ok(()) => {
                        match register_all(&transport).await {
                            Ok(()) => tracing::info!(
                                target: "ws::server_notify",
                                config_id,
                                "re-registered notify subscriptions after session_up",
                            ),
                            Err(e) => tracing::warn!(
                                target: "ws::server_notify",
                                config_id,
                                error = %e,
                                "servernotifyregister after session_up failed; \
                                 will retry on next reconnect tick",
                            ),
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Missed a tick. `servernotifyregister` is
                        // idempotent on the upstream — re-issue anyway
                        // so we recover without waiting for the next
                        // reconnect.
                        let _ = register_all(&transport).await;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // Sender side gone. The notify_rx arm will
                        // observe the same closure on its next recv
                        // and we'll exit there.
                    }
                }
            }

            recv = notify_rx.recv() => {
                match recv {
                    Ok(frame) => {
                        publish_notify(&deps.hub, config_id, &frame).await;
                        bridge_to_flow_engine(&deps.flow_engine, &mut clid_uid, &frame).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // Slow consumer. Each lagged item is one missed
                        // notify line; bump the dropped counter the
                        // same way the session loop does.
                        for _ in 0..n {
                            deps.hub.record_dropped();
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::info!(
                            target: "ws::server_notify",
                            config_id,
                            "notify channel closed (transport supervisor exited); \
                             worker exiting",
                        );
                        return;
                    }
                }
            }
        }
    }
}

/// The four event classes the WS topics consume. `id=0` on the channel
/// and textchannel registers means "every channel" — the upstream
/// requires an explicit cid/0 sentinel for those two events.
const REGISTER_LINES: [&str; 4] = [
    "servernotifyregister event=server",
    "servernotifyregister event=channel id=0",
    "servernotifyregister event=textserver",
    "servernotifyregister event=textchannel id=0",
];

async fn register_all(transport: &TransportHandle) -> Result<(), SshBridgeError> {
    for line in REGISTER_LINES {
        transport.execute(line.to_string(), None, None).await?;
    }
    Ok(())
}

/// Envelope kind broadcast on `server:{id}:widget` whenever an upstream
/// notify frame would refresh the public snapshot. Spec §7.29 — public
/// widget surfaces MUST NOT carry client UIDs, IPs, or permissions
/// metadata, so the widget topic never re-publishes the operator-side
/// `NotifyFrame` payload. Public viewers refetch
/// `/api/widget/{token}/data` whenever they receive a frame on this
/// topic (see `ui/pages/public_widget.rs::drive_socket`), and the JSON
/// route is what enforces redaction.
const WIDGET_REFRESH_KIND: &str = "widget:refresh";

/// Static body for [`WIDGET_REFRESH_KIND`] envelopes. Carries no
/// upstream fields — just a refresh signal.
fn widget_refresh_payload() -> Value {
    json!({ "refresh": true })
}

/// Translate a parsed [`NotifyFrame`] into a `(Topic, kind, data)`
/// publish. Unknown events are logged at `debug` and dropped — TS6
/// servers may add new event names in future builds, and silently
/// ignoring them is preferable to publishing on a wrong topic.
///
/// Client- and channel-class events also fan out a parallel publish onto
/// `server:{id}:widget` so public widget viewers (PURA-72 Slice E) refresh
/// within ~1 s of a real upstream change. The widget envelope is a
/// **stub** — kind `widget:refresh`, body `{"refresh": true}` — so no
/// operator-side fields (UIDs, db ids, server-groups, country, IP,
/// platform/version, bandwidth, …) can leak onto a public surface that
/// only requires a widget token. PURA-72 H-1 fix.
async fn publish_notify(hub: &Hub, server_id: i64, frame: &NotifyFrame) {
    let Some((topic_kind, envelope_kind)) = classify_event(&frame.event) else {
        tracing::debug!(
            target: "ws::server_notify",
            server_id,
            event = %frame.event,
            "unrecognised notify event; dropping",
        );
        return;
    };
    let topic = Topic::new(server_id, topic_kind);
    let data = records_to_json(&frame.records);
    hub.publish(topic, envelope_kind, data).await;
    if matches!(topic_kind, TopicKind::Clients | TopicKind::Channels) {
        hub.publish(
            Topic::new(server_id, TopicKind::Widget),
            WIDGET_REFRESH_KIND,
            widget_refresh_payload(),
        )
        .await;
    }
}

fn classify_event(event: &str) -> Option<(TopicKind, &'static str)> {
    match event {
        "notifycliententer" | "notifycliententerview" => {
            Some((TopicKind::Clients, "ts:client:connected"))
        }
        "notifyclientleftview" => Some((TopicKind::Clients, "ts:client:disconnected")),
        "notifyclientmoved" => Some((TopicKind::Clients, "ts:client:moved")),
        "notifyclientupdated" => Some((TopicKind::Clients, "ts:client:updated")),
        "notifychannelcreated" => Some((TopicKind::Channels, "ts:channel:created")),
        "notifychanneldeleted" => Some((TopicKind::Channels, "ts:channel:deleted")),
        "notifychanneledited" => Some((TopicKind::Channels, "ts:channel:edited")),
        "notifychanneldescriptionchanged" => {
            Some((TopicKind::Channels, "ts:channel:description-changed"))
        }
        "notifychannelmoved" => Some((TopicKind::Channels, "ts:channel:moved")),
        "notifytextmessage" => Some((TopicKind::Logs, "ts:textmessage")),
        _ => None,
    }
}

/// Lift the parsed records into a JSON value. Single-record events
/// project to a flat object (the common case); multi-record events
/// (`notifyclientupdated` with batched updates) project to
/// `{"records": [...]}` so the wire shape stays unambiguous.
fn records_to_json(records: &[HashMap<String, String>]) -> Value {
    match records.len() {
        0 => Value::Object(Map::new()),
        1 => record_to_value(&records[0]),
        _ => {
            let arr: Vec<Value> = records.iter().map(record_to_value).collect();
            let mut obj = Map::new();
            obj.insert("records".into(), Value::Array(arr));
            Value::Object(obj)
        }
    }
}

fn record_to_value(record: &HashMap<String, String>) -> Value {
    let mut obj = Map::new();
    for (k, v) in record {
        obj.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Flow-engine bridge (PURA-300 — automod triggers)
// ---------------------------------------------------------------------------

/// The manager connects one TS6 virtual server per SSH `server_connection`
/// (the widget principal and dashboard tick both assume vserver id 1), so
/// the automod trigger surface keys flows on this id.
const FLOW_VIRTUAL_SERVER_ID: i64 = 1;

/// A notify frame translated into a flow-engine producer call. Kept as a
/// pure value so [`parse_bridge_event`] is unit-testable without a DB or a
/// live engine.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BridgeEvent {
    ClientJoined {
        channel_id: i64,
        uid: String,
        nickname: String,
        clid: Option<String>,
    },
    ClientLeft {
        clid: String,
    },
    ChatMessage {
        target_mode: String,
        uid: String,
        nickname: String,
        message: String,
    },
    ClientMoved {
        clid: String,
    },
}

/// Extract a [`BridgeEvent`] from a parsed notify frame. Returns `None` for
/// events the automod trigger surface does not consume, or when a required
/// field is missing.
fn parse_bridge_event(frame: &NotifyFrame) -> Option<BridgeEvent> {
    let rec = frame.records.first()?;
    match frame.event.as_str() {
        "notifycliententer" | "notifycliententerview" => {
            let uid = rec.get("client_unique_identifier")?.clone();
            let channel_id = rec.get("ctid")?.parse::<i64>().ok()?;
            Some(BridgeEvent::ClientJoined {
                channel_id,
                uid,
                nickname: rec.get("client_nickname").cloned().unwrap_or_default(),
                clid: rec.get("clid").cloned(),
            })
        }
        "notifyclientleftview" => Some(BridgeEvent::ClientLeft {
            clid: rec.get("clid")?.clone(),
        }),
        "notifytextmessage" => {
            // TS6 `targetmode`: 1 = private, 2 = channel, 3 = server. The
            // notify frame carries no channel id, so a channel-scoped chat
            // trigger filter cannot be matched — chat triggers run with
            // `channel_id = None` (the seed filters are mode/content based).
            let target_mode = match rec.get("targetmode").map(String::as_str) {
                Some("1") => "private",
                Some("2") => "channel",
                Some("3") => "server",
                other => other.unwrap_or("unknown"),
            }
            .to_string();
            Some(BridgeEvent::ChatMessage {
                target_mode,
                uid: rec.get("invokeruid")?.clone(),
                nickname: rec.get("invokername").cloned().unwrap_or_default(),
                message: rec.get("msg")?.clone(),
            })
        }
        "notifyclientmoved" => Some(BridgeEvent::ClientMoved {
            clid: rec.get("clid")?.clone(),
        }),
        _ => None,
    }
}

/// Bridge a parsed notify frame into the flow engine's automod trigger
/// surface. Runs alongside [`publish_notify`] — WS-topic republishing is
/// unaffected; this is a parallel fan-out so `Ts6ClientJoined` /
/// `Ts6ChatMessage` / `Ts6Flood` flows fire from real TS6 events.
async fn bridge_to_flow_engine(
    engine: &crate::flow::FlowEngineHandle,
    clid_uid: &mut HashMap<String, String>,
    frame: &NotifyFrame,
) {
    let Some(event) = parse_bridge_event(frame) else {
        return;
    };
    match event {
        BridgeEvent::ClientJoined {
            channel_id,
            uid,
            nickname,
            clid,
        } => {
            if let Some(clid) = clid {
                clid_uid.insert(clid, uid.clone());
            }
            engine
                .on_client_joined(FLOW_VIRTUAL_SERVER_ID, channel_id, uid, nickname)
                .await;
        }
        BridgeEvent::ClientLeft { clid } => {
            clid_uid.remove(&clid);
        }
        BridgeEvent::ChatMessage {
            target_mode,
            uid,
            nickname,
            message,
        } => {
            engine
                .on_chat_message(
                    FLOW_VIRTUAL_SERVER_ID,
                    None,
                    target_mode,
                    uid,
                    nickname,
                    message,
                )
                .await;
        }
        BridgeEvent::ClientMoved { clid } => {
            // `notifyclientmoved` carries no UID — recover it from the
            // join cache. A move for a client that joined before this
            // worker started is skipped; channel-hop flood tolerates the
            // gap (windows are seconds-to-minutes).
            let Some(uid) = clid_uid.get(&clid).cloned() else {
                return;
            };
            engine.on_client_moved(FLOW_VIRTUAL_SERVER_ID, uid).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws::auth::{Principal, UserPrincipal};

    fn record(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn admin() -> Principal {
        Principal::User(UserPrincipal {
            user_id: 1,
            username: "alice".into(),
            role: "admin".into(),
            is_admin: true,
            is_at_least_moderator: true,
        })
    }

    #[test]
    fn classify_known_client_events_route_to_clients_topic() {
        for (ev, kind) in [
            ("notifycliententer", "ts:client:connected"),
            ("notifycliententerview", "ts:client:connected"),
            ("notifyclientleftview", "ts:client:disconnected"),
            ("notifyclientmoved", "ts:client:moved"),
            ("notifyclientupdated", "ts:client:updated"),
        ] {
            let (topic_kind, envelope_kind) =
                classify_event(ev).unwrap_or_else(|| panic!("expected {ev} to classify"));
            assert_eq!(topic_kind, TopicKind::Clients, "wrong topic for {ev}");
            assert_eq!(envelope_kind, kind, "wrong envelope for {ev}");
        }
    }

    #[test]
    fn classify_known_channel_events_route_to_channels_topic() {
        for (ev, kind) in [
            ("notifychannelcreated", "ts:channel:created"),
            ("notifychanneldeleted", "ts:channel:deleted"),
            ("notifychanneledited", "ts:channel:edited"),
            (
                "notifychanneldescriptionchanged",
                "ts:channel:description-changed",
            ),
            ("notifychannelmoved", "ts:channel:moved"),
        ] {
            let (topic_kind, envelope_kind) =
                classify_event(ev).unwrap_or_else(|| panic!("expected {ev} to classify"));
            assert_eq!(topic_kind, TopicKind::Channels, "wrong topic for {ev}");
            assert_eq!(envelope_kind, kind, "wrong envelope for {ev}");
        }
    }

    #[test]
    fn classify_text_message_routes_to_logs_topic() {
        let (topic_kind, envelope_kind) = classify_event("notifytextmessage").expect("known");
        assert_eq!(topic_kind, TopicKind::Logs);
        assert_eq!(envelope_kind, "ts:textmessage");
    }

    #[test]
    fn classify_unknown_event_returns_none() {
        assert!(classify_event("notifysomethingnew").is_none());
        assert!(classify_event("error id=0").is_none());
    }

    #[test]
    fn records_to_json_zero_records_yields_empty_object() {
        let v = records_to_json(&[]);
        assert!(v.is_object());
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn records_to_json_single_record_projects_to_flat_object() {
        let r = vec![record(&[("clid", "5"), ("client_nickname", "alice")])];
        let v = records_to_json(&r);
        assert_eq!(v["clid"], "5");
        assert_eq!(v["client_nickname"], "alice");
        assert!(v.get("records").is_none(), "single record stays flat");
    }

    #[test]
    fn records_to_json_multi_record_wraps_in_records_array() {
        let r = vec![
            record(&[("clid", "5")]),
            record(&[("clid", "6")]),
            record(&[("clid", "7")]),
        ];
        let v = records_to_json(&r);
        let arr = v["records"].as_array().expect("records array");
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["clid"], "5");
        assert_eq!(arr[2]["clid"], "7");
    }

    fn notify(event: &str, pairs: &[(&str, &str)]) -> NotifyFrame {
        NotifyFrame {
            event: event.to_string(),
            records: vec![record(pairs)],
        }
    }

    #[test]
    fn parse_bridge_event_extracts_client_joined() {
        let frame = notify(
            "notifycliententerview",
            &[
                ("ctid", "7"),
                ("clid", "42"),
                ("client_unique_identifier", "uid-abc="),
                ("client_nickname", "Alice"),
            ],
        );
        assert_eq!(
            parse_bridge_event(&frame),
            Some(BridgeEvent::ClientJoined {
                channel_id: 7,
                uid: "uid-abc=".into(),
                nickname: "Alice".into(),
                clid: Some("42".into()),
            })
        );
    }

    #[test]
    fn parse_bridge_event_extracts_chat_message_with_target_mode() {
        let frame = notify(
            "notifytextmessage",
            &[
                ("targetmode", "2"),
                ("msg", "buy followers now"),
                ("invokeruid", "uid-spammer="),
                ("invokername", "Spammer"),
            ],
        );
        assert_eq!(
            parse_bridge_event(&frame),
            Some(BridgeEvent::ChatMessage {
                target_mode: "channel".into(),
                uid: "uid-spammer=".into(),
                nickname: "Spammer".into(),
                message: "buy followers now".into(),
            })
        );
    }

    #[test]
    fn parse_bridge_event_extracts_client_moved_and_left() {
        assert_eq!(
            parse_bridge_event(&notify(
                "notifyclientmoved",
                &[("ctid", "5"), ("clid", "12")]
            )),
            Some(BridgeEvent::ClientMoved { clid: "12".into() })
        );
        assert_eq!(
            parse_bridge_event(&notify("notifyclientleftview", &[("clid", "12")])),
            Some(BridgeEvent::ClientLeft { clid: "12".into() })
        );
    }

    #[test]
    fn parse_bridge_event_ignores_unrelated_and_incomplete_frames() {
        // Channel events are not part of the automod trigger surface.
        assert_eq!(
            parse_bridge_event(&notify("notifychannelcreated", &[("cid", "9")])),
            None
        );
        // Enterview without a UID cannot key a trigger.
        assert_eq!(
            parse_bridge_event(&notify("notifycliententerview", &[("ctid", "7")])),
            None
        );
        // Text message without an invoker UID is dropped.
        assert_eq!(
            parse_bridge_event(&notify("notifytextmessage", &[("msg", "hi")])),
            None
        );
    }

    #[tokio::test]
    async fn publish_notify_routes_client_event_to_clients_topic() {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();

        let hub = Hub::new();
        let topic = Topic::new(7, TopicKind::Clients);
        let mut sub = hub.subscribe(&db, &admin(), topic, None).await.unwrap();

        let frame = NotifyFrame {
            event: "notifycliententerview".into(),
            records: vec![record(&[("clid", "12"), ("client_nickname", "bob")])],
        };
        publish_notify(&hub, 7, &frame).await;

        let env = sub.receiver.recv().await.expect("envelope");
        assert_eq!(env.kind, "ts:client:connected");
        assert_eq!(env.topic, "server:7:clients");
        assert_eq!(env.data["clid"], "12");
        assert_eq!(env.data["client_nickname"], "bob");
    }

    #[tokio::test]
    async fn publish_notify_routes_channel_event_to_channels_topic() {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();

        let hub = Hub::new();
        let topic = Topic::new(7, TopicKind::Channels);
        let mut sub = hub.subscribe(&db, &admin(), topic, None).await.unwrap();

        let frame = NotifyFrame {
            event: "notifychanneledited".into(),
            records: vec![record(&[("cid", "5"), ("channel_name", "renamed")])],
        };
        publish_notify(&hub, 7, &frame).await;

        let env = sub.receiver.recv().await.expect("envelope");
        assert_eq!(env.kind, "ts:channel:edited");
        assert_eq!(env.topic, "server:7:channels");
        assert_eq!(env.data["cid"], "5");
        assert_eq!(env.data["channel_name"], "renamed");
    }

    fn widget_principal(server_config_id: i64) -> Principal {
        use crate::ws::auth::WidgetPrincipal;
        Principal::Widget(WidgetPrincipal {
            widget_id: 1,
            server_config_id,
            virtual_server_id: 1,
        })
    }

    /// PURA-72 H-1 regression. The widget topic must NOT republish the
    /// operator-side `NotifyFrame` envelope — anyone with a widget token
    /// could otherwise harvest UIDs / db ids / server-groups / IPs /
    /// country off `/api/ws`. Spec §7.29: the public widget surface
    /// carries no client UIDs, IPs, or permissions metadata.
    ///
    /// The hub broadcast is per-server (the session loop is what
    /// filters by topic), so subscribing to the widget topic returns
    /// the channel receiver that sees both publishes from
    /// `publish_notify`. We drain both envelopes and assert that the
    /// one bound to `server:N:widget` is the refresh stub.
    #[tokio::test]
    async fn widget_topic_publishes_redacted_refresh_stub_only() {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();

        let hub = Hub::new();
        let topic = Topic::new(7, TopicKind::Widget);
        let mut sub = hub
            .subscribe(&db, &widget_principal(7), topic, None)
            .await
            .unwrap();

        // The exact field set a leaker would sniff for. Every key here
        // must be absent from the widget envelope.
        let frame = NotifyFrame {
            event: "notifycliententerview".into(),
            records: vec![record(&[
                ("clid", "12"),
                ("client_nickname", "bob"),
                ("client_unique_identifier", "uniqueId="),
                ("client_database_id", "42"),
                ("client_servergroups", "1,7,12"),
                ("client_channel_group_id", "8"),
                ("client_country", "DE"),
                ("client_meta_data", "secret"),
                ("connection_client_ip", "203.0.113.4"),
                ("client_platform", "Linux"),
                ("client_version", "3.13.7"),
            ])],
        };
        publish_notify(&hub, 7, &frame).await;

        // Drain up to two envelopes (operator + widget fan-outs share
        // the per-server broadcast). Find the widget one.
        let mut widget_env = None;
        for _ in 0..2 {
            match tokio::time::timeout(std::time::Duration::from_millis(100), sub.receiver.recv())
                .await
            {
                Ok(Ok(env)) => {
                    if env.topic == "server:7:widget" {
                        widget_env = Some(env);
                        break;
                    }
                }
                _ => break,
            }
        }
        let env = widget_env.expect("widget envelope must be published on server:7:widget");
        assert_eq!(
            env.kind, WIDGET_REFRESH_KIND,
            "widget envelope kind must be the redaction stub, not the operator name"
        );
        assert_eq!(
            env.data,
            json!({ "refresh": true }),
            "widget envelope body must be a static refresh stub — no upstream fields"
        );
        // Belt-and-braces — flatten and assert no PII leaked even if
        // `widget_refresh_payload` is changed in future.
        let payload = env.data.to_string();
        for forbidden in [
            "uniqueId=",
            "client_unique_identifier",
            "client_database_id",
            "client_servergroups",
            "client_channel_group_id",
            "client_country",
            "client_meta_data",
            "connection_client_ip",
            "203.0.113.4",
            "Linux",
            "3.13.7",
            "bob",
        ] {
            assert!(
                !payload.contains(forbidden),
                "widget envelope leaked `{forbidden}` from operator frame: {payload}"
            );
        }
    }

    /// Operator topics must keep the full envelope — only the widget
    /// fan-out is stubbed. Belt-and-braces companion to the H-1 test:
    /// regressing the publish path back to "fan everything everywhere"
    /// would also fail this assertion if the operator envelope is
    /// quietly stripped.
    #[tokio::test]
    async fn operator_clients_topic_keeps_full_envelope() {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();

        let hub = Hub::new();
        let topic = Topic::new(7, TopicKind::Clients);
        let mut sub = hub.subscribe(&db, &admin(), topic, None).await.unwrap();

        let frame = NotifyFrame {
            event: "notifycliententerview".into(),
            records: vec![record(&[
                ("clid", "12"),
                ("client_unique_identifier", "uniqueId="),
                ("client_database_id", "42"),
            ])],
        };
        publish_notify(&hub, 7, &frame).await;

        let env = sub.receiver.recv().await.expect("operator envelope");
        assert_eq!(env.kind, "ts:client:connected");
        assert_eq!(env.data["client_unique_identifier"], "uniqueId=");
        assert_eq!(env.data["client_database_id"], "42");
    }

    #[tokio::test]
    async fn publish_notify_drops_unknown_event() {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();

        let hub = Hub::new();
        let topic = Topic::new(7, TopicKind::Clients);
        let mut sub = hub.subscribe(&db, &admin(), topic, None).await.unwrap();

        let frame = NotifyFrame {
            event: "notifysomethingnew".into(),
            records: vec![record(&[("foo", "bar")])],
        };
        publish_notify(&hub, 7, &frame).await;

        let observed =
            tokio::time::timeout(std::time::Duration::from_millis(50), sub.receiver.recv()).await;
        assert!(observed.is_err(), "unknown event must not publish");
    }
}
