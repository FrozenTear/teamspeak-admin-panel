//! Wire-format types for the flow engine ([PURA-198](/PURA/issues/PURA-198)).
//!
//! Mirrors the contract in `docs/flows/http-api.md`. Stays WASM-clean — no
//! engine internals, no DB types — so both the Dioxus FE (PURA-243) and
//! the future axum routes (PURA-242) can import the same wire shapes.
//!
//! Conventions match `music_bots.rs`:
//! - `#[serde(rename_all = "camelCase")]` on every struct → wire keys are
//!   `botFlowId` / `createdAt` while source stays `bot_flow_id` /
//!   `created_at`.
//! - Discriminated enums use `tag = "kind"` with `camelCase` variants on
//!   the wire (`{"kind":"manualFire"}`).
//! - Newtype IDs are `#[serde(transparent)]` over `i64`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

/// v2 graph/node wire types — flow-engine redesign ([PURA-259](/PURA/issues/PURA-259) /
/// [PURA-260](/PURA/issues/PURA-260)). The v1.1 types in this module are kept
/// untouched: the projection shim ([`v2::project_legacy`]) reads legacy rows
/// through them.
pub mod v2;

/// Flow identifier minted by the engine on `POST /api/flows`. Stable for
/// the flow's lifetime; reused by every nested resource path
/// (`/api/flows/{id}/...`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FlowId(pub i64);

/// Per-flow run identifier. Returned by `POST /fire` and by every entry in
/// `GET /api/flows/{id}/runs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FlowRunId(pub i64);

/// Source event that feeds a [`Trigger::Ts6Flood`] windowed counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FloodSource {
    /// Each `notifycliententerview` increments the counter.
    ClientJoined,
    /// Each `notifytextmessage` increments the counter.
    ChatMessage,
    /// Each `notifyclientmoved` increments the counter (channel-hopping).
    ClientMoved,
}

/// Key domain for the per-`(spec, key)` sliding-window counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FloodScope {
    /// Counter per unique client identifier.
    Subject,
    /// Counter per source IP address (not yet wired — treated as `subject`
    /// until the SSH bridge surfaces the IP).
    Ip,
    /// Single server-wide counter (all clients share one bucket).
    Global,
}

/// What event makes the engine create a run for this flow. Wire is
/// externally-tagged on `kind`, matching the example bodies in
/// `docs/flows/http-api.md` §3.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Trigger {
    /// Cron expression in the engine's chosen dialect — validated server-side.
    Cron { expression: String },
    /// Only fires via `POST /fire`. The intentional default for new flows.
    ManualFire,
    /// Fired when a TS6 client joins a channel. `channel_id = None` matches
    /// any channel.
    Ts6ClientJoined { channel_id: Option<i64> },
    /// Fired on each `notifytextmessage`. `channel_id = None` matches any
    /// channel; `target_mode` mirrors the TS6 `targetmode` parameter
    /// (`"channel"`, `"private"`, `"server"`). Run context exposes
    /// `${trigger.message}`, `${trigger.clientUniqueIdentifier}`,
    /// `${trigger.clientNickname}`, `${trigger.channelId}`.
    Ts6ChatMessage {
        channel_id: Option<i64>,
        /// Optional target-mode filter. `None` matches all modes.
        target_mode: Option<String>,
    },
    /// Windowed flood counter. Fires when `source` events from a single
    /// `scope` bucket reach `threshold` within a rolling `window_secs`
    /// window. Channel-hopping = `Ts6Flood { source: clientMoved }`.
    Ts6Flood {
        source: FloodSource,
        threshold: u32,
        window_secs: u32,
        scope: FloodScope,
    },
}

/// TS6 effect a [`Action::Moderate`] node applies to its subject. The
/// wire discriminant doubles as the `moderation_case_action.actionKind`
/// value (Phase 9.1 automod brief §4.3 / §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModerateEffect {
    /// Private `sendtextmessage` to the subject.
    Warn,
    /// Server talker-flag revoke (`client_is_talker = 0`).
    Mute,
    /// `clientkick`.
    Kick,
    /// `banadd` — temporary when `duration_secs` is set.
    Ban,
}

impl ModerateEffect {
    /// The `moderation_case_action.actionKind` string for this effect.
    pub fn as_action_kind(self) -> &'static str {
        match self {
            ModerateEffect::Warn => "warn",
            ModerateEffect::Mute => "mute",
            ModerateEffect::Kick => "kick",
            ModerateEffect::Ban => "ban",
        }
    }
}

/// One step executed when a run starts. `args` are `serde_json::Value`
/// containers because the schema differs per command and the engine
/// validates against its whitelist server-side.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Action {
    /// Run a whitelisted TS6 command (e.g. `sendtextmessage`).
    Ts6Command {
        command: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
    },
    /// Dispatch a command at a specific music bot.
    MusicBotCommand {
        bot_id: u64,
        command: String,
        #[serde(default)]
        args: serde_json::Map<String, serde_json::Value>,
    },
    /// Outbound HTTP POST. URL is checked against the manager's SSRF
    /// allow-list at send time.
    WebhookOut {
        url: String,
        #[serde(default)]
        headers: Vec<(String, String)>,
    },
    /// Smoke / debug action — appends one line to the manager log.
    LogLine { message: String },
    /// Apply an audited moderation effect to the trigger's subject and
    /// bridge it into the Phase 9.0 case model (Phase 9.1 automod brief
    /// §4). The subject (UID + nickname snapshot) is resolved from the
    /// trigger context at dispatch time, so it is not a config field.
    Moderate {
        /// The TS6 effect to apply.
        effect: ModerateEffect,
        /// Mute / ban duration in seconds. `None` for `warn` / `kick`,
        /// and for a permanent ban (operator-only policy lands in 9.1.3).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_secs: Option<u32>,
        /// Templated reason — interpolated by the engine, then stored on
        /// `moderation_case.reason` and the timeline action row.
        reason_template: String,
        /// Stable rule identifier. Drives case dedup (one open automod
        /// case per subject + rule) and per-rule metrics.
        rule_key: String,
    },
}

/// Trigger + ordered action list — what the engine executes when a run starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowDefinition {
    pub trigger: Trigger,
    pub actions: Vec<Action>,
}

/// `GET /api/flows` / `GET /api/flows/{id}` row. `last_run` is `None` for
/// freshly-created flows or for flows whose history was force-deleted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Flow {
    pub id: FlowId,
    pub name: String,
    pub description: Option<String>,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub enabled: bool,
    pub definition: FlowDefinition,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_run: Option<FlowRunSummary>,
}

/// Compact run shape embedded inside [`Flow.last_run`]. The full
/// [`FlowRun`] (with action results) lives at `GET /api/flows/{id}/runs`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRunSummary {
    pub id: FlowRunId,
    pub status: FlowRunStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub duration_ms: Option<u64>,
}

/// `GET /api/flows/{id}/runs` row. `summary` is flattened so the wire
/// stays one struct per run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlowRun {
    #[serde(flatten)]
    pub summary: FlowRunSummary,
    pub flow_id: FlowId,
    /// Resolved trigger event JSON. Shape depends on the trigger kind that
    /// produced the run.
    pub trigger: serde_json::Value,
    pub error: Option<String>,
    pub action_results: Vec<ActionResult>,
}

/// Run lifecycle. `skipped_disabled` is the engine's reused tag for
/// "a run was already in flight" (see `ui-brief.md` §6) — the operator
/// surface tooltips the in-flight case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowRunStatus {
    InFlight,
    Ok,
    Errored,
    Interrupted,
    SkippedDisabled,
}

/// Per-action outcome inside [`FlowRun.action_results`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    pub index: u32,
    /// Wire discriminant for the action kind that produced this result
    /// (e.g. `"ts6Command"`, `"logLine"`). Matches the camelCase
    /// discriminant on [`Action`].
    pub kind: String,
    pub status: ActionStatus,
    pub duration_ms: u64,
    pub error: Option<String>,
}

/// Per-action lifecycle marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Ok,
    Errored,
    Skipped,
}

/// `POST /api/flows` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateFlowRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    #[serde(default)]
    pub enabled: bool,
    pub definition: FlowDefinition,
}

/// `PATCH /api/flows/{id}` body — every field optional so callers can
/// partial-update. `description` uses `double_option` so the wire can
/// distinguish "field absent" (no change) from "field present and null"
/// (clear it).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFlowRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub description: Option<Option<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub virtual_server_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition: Option<FlowDefinition>,
}

/// Body for `POST /api/flows/{id}/fire`. The optional `context` is merged
/// into the run's resolved trigger document under the `manualFire`
/// discriminant; v1.1 does not substitute it into action args.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FireFlowRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
}

/// `POST /api/flows/{id}/fire` response (202 Accepted).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FireFlowResponse {
    pub run_id: FlowRunId,
    pub flow_id: FlowId,
    pub started_at: DateTime<Utc>,
}

/// `GET /api/flows` response wrapper. v1.1 does not paginate flows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListFlowsResponse {
    pub flows: Vec<Flow>,
}

/// `GET /api/flows/{id}/runs` response. `nextCursor` is `None` when no
/// more pages are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListRunsResponse {
    pub runs: Vec<FlowRun>,
    pub next_cursor: Option<FlowRunId>,
}

/// JSON envelope returned by every non-2xx flow response. `error` is the
/// stable discriminant; clients branch on it. `message` is human-readable
/// and may shift between versions — never branch on it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub error: String,
    pub message: String,
}

/// Distinguish "field absent" from "field present and null" on deserialize,
/// so `PATCH /api/flows/{id}` can clear `description` by sending
/// `{"description": null}` while omitting the field leaves it untouched.
fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_round_trips_with_kind_discriminator() {
        let cron = Trigger::Cron {
            expression: "0 */5 * * * *".into(),
        };
        let json = serde_json::to_string(&cron).unwrap();
        assert!(json.contains(r#""kind":"cron""#), "got: {json}");
        assert_eq!(serde_json::from_str::<Trigger>(&json).unwrap(), cron);

        let manual = Trigger::ManualFire;
        let json = serde_json::to_string(&manual).unwrap();
        assert!(json.contains(r#""kind":"manualFire""#), "got: {json}");

        let joined = Trigger::Ts6ClientJoined {
            channel_id: Some(5),
        };
        let json = serde_json::to_string(&joined).unwrap();
        assert!(json.contains(r#""kind":"ts6ClientJoined""#), "got: {json}");
        assert!(json.contains(r#""channelId":5"#), "got: {json}");

        let any_channel = Trigger::Ts6ClientJoined { channel_id: None };
        let json = serde_json::to_string(&any_channel).unwrap();
        assert!(json.contains(r#""channelId":null"#), "got: {json}");

        let chat = Trigger::Ts6ChatMessage {
            channel_id: Some(3),
            target_mode: Some("channel".into()),
        };
        let json = serde_json::to_string(&chat).unwrap();
        assert!(json.contains(r#""kind":"ts6ChatMessage""#), "got: {json}");
        assert!(json.contains(r#""channelId":3"#), "got: {json}");
        assert!(json.contains(r#""targetMode":"channel""#), "got: {json}");
        assert_eq!(serde_json::from_str::<Trigger>(&json).unwrap(), chat);

        let flood = Trigger::Ts6Flood {
            source: FloodSource::ChatMessage,
            threshold: 5,
            window_secs: 10,
            scope: FloodScope::Subject,
        };
        let json = serde_json::to_string(&flood).unwrap();
        assert!(json.contains(r#""kind":"ts6Flood""#), "got: {json}");
        assert!(json.contains(r#""source":"chatMessage""#), "got: {json}");
        assert!(json.contains(r#""threshold":5"#), "got: {json}");
        assert!(json.contains(r#""windowSecs":10"#), "got: {json}");
        assert!(json.contains(r#""scope":"subject""#), "got: {json}");
        assert_eq!(serde_json::from_str::<Trigger>(&json).unwrap(), flood);
    }

    #[test]
    fn action_log_line_serialises_with_kind_and_message() {
        let a = Action::LogLine {
            message: "hello".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""kind":"logLine""#), "got: {json}");
        assert!(json.contains(r#""message":"hello""#), "got: {json}");
    }

    #[test]
    fn action_moderate_round_trips_with_camel_case_fields() {
        let a = Action::Moderate {
            effect: ModerateEffect::Ban,
            duration_secs: Some(3600),
            reason_template: "flood: ${trigger.count} msgs".into(),
            rule_key: "chat-flood".into(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains(r#""kind":"moderate""#), "got: {json}");
        assert!(json.contains(r#""effect":"ban""#), "got: {json}");
        assert!(json.contains(r#""durationSecs":3600"#), "got: {json}");
        assert!(json.contains(r#""reasonTemplate""#), "got: {json}");
        assert!(json.contains(r#""ruleKey":"chat-flood""#), "got: {json}");
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), a);

        // `durationSecs` is omitted for a `warn` and absent round-trips.
        let warn = Action::Moderate {
            effect: ModerateEffect::Warn,
            duration_secs: None,
            reason_template: "be nice".into(),
            rule_key: "chat-filter".into(),
        };
        let json = serde_json::to_string(&warn).unwrap();
        assert!(!json.contains("durationSecs"), "got: {json}");
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), warn);
    }

    #[test]
    fn moderate_effect_action_kind_strings() {
        assert_eq!(ModerateEffect::Warn.as_action_kind(), "warn");
        assert_eq!(ModerateEffect::Mute.as_action_kind(), "mute");
        assert_eq!(ModerateEffect::Kick.as_action_kind(), "kick");
        assert_eq!(ModerateEffect::Ban.as_action_kind(), "ban");
    }

    #[test]
    fn flow_run_status_uses_snake_case_on_the_wire() {
        let cases = [
            (FlowRunStatus::InFlight, "\"in_flight\""),
            (FlowRunStatus::Ok, "\"ok\""),
            (FlowRunStatus::Errored, "\"errored\""),
            (FlowRunStatus::Interrupted, "\"interrupted\""),
            (FlowRunStatus::SkippedDisabled, "\"skipped_disabled\""),
        ];
        for (status, expected) in cases {
            assert_eq!(serde_json::to_string(&status).unwrap(), expected);
        }
    }

    #[test]
    fn newtype_ids_are_transparent() {
        let id = FlowId(7);
        assert_eq!(serde_json::to_string(&id).unwrap(), "7");
        let back: FlowId = serde_json::from_str("42").unwrap();
        assert_eq!(back, FlowId(42));
    }

    #[test]
    fn create_flow_request_round_trips_with_camel_case_wire() {
        let body = CreateFlowRequest {
            name: "welcome".into(),
            description: Some("send a greeting".into()),
            server_config_id: 1,
            virtual_server_id: 2,
            enabled: true,
            definition: FlowDefinition {
                trigger: Trigger::ManualFire,
                actions: vec![Action::LogLine {
                    message: "hi".into(),
                }],
            },
        };
        let json = serde_json::to_string(&body).unwrap();
        for required in ["serverConfigId", "virtualServerId"] {
            assert!(json.contains(required), "missing `{required}` in {json}");
        }
        for forbidden in ["server_config_id", "virtual_server_id"] {
            assert!(
                !json.contains(forbidden),
                "snake_case `{forbidden}` leaked: {json}"
            );
        }
        // Round-trip back.
        let back: CreateFlowRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, body.name);
        assert_eq!(back.server_config_id, body.server_config_id);
        assert_eq!(back.virtual_server_id, body.virtual_server_id);
        assert_eq!(back.enabled, body.enabled);
    }

    #[test]
    fn update_flow_request_distinguishes_absent_from_null_description() {
        // Absent field — `description` stays `None` (no change).
        let body: UpdateFlowRequest = serde_json::from_str("{}").unwrap();
        assert!(body.description.is_none());

        // Null field — `description` becomes `Some(None)` (clear).
        let body: UpdateFlowRequest = serde_json::from_str(r#"{"description":null}"#).unwrap();
        assert_eq!(body.description, Some(None));

        // Concrete value — `Some(Some("…"))`.
        let body: UpdateFlowRequest = serde_json::from_str(r#"{"description":"new"}"#).unwrap();
        assert_eq!(body.description, Some(Some("new".into())));
    }

    #[test]
    fn flow_run_summary_flattens_into_flow_run() {
        let now = Utc::now();
        let run = FlowRun {
            summary: FlowRunSummary {
                id: FlowRunId(11),
                status: FlowRunStatus::Ok,
                started_at: now,
                finished_at: Some(now),
                duration_ms: Some(120),
            },
            flow_id: FlowId(3),
            trigger: serde_json::json!({"kind": "manualFire"}),
            error: None,
            action_results: vec![ActionResult {
                index: 0,
                kind: "logLine".into(),
                status: ActionStatus::Ok,
                duration_ms: 1,
                error: None,
            }],
        };
        let json = serde_json::to_string(&run).unwrap();
        // `summary` is flattened — its keys appear at the top level.
        assert!(json.contains(r#""id":11"#), "got: {json}");
        assert!(json.contains(r#""status":"ok""#), "got: {json}");
        assert!(json.contains(r#""flowId":3"#), "got: {json}");
        // Action results round-trip with camelCase keys.
        assert!(json.contains(r#""actionResults""#), "got: {json}");
        assert!(json.contains(r#""durationMs":1"#), "got: {json}");
        let back: FlowRun = serde_json::from_str(&json).unwrap();
        assert_eq!(back.summary.id, FlowRunId(11));
        assert_eq!(back.flow_id, FlowId(3));
    }

    #[test]
    fn error_body_round_trips_with_stable_discriminant() {
        let env = ErrorBody {
            error: "name_taken".into(),
            message: "another flow uses this name".into(),
        };
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""error":"name_taken""#), "got: {json}");
        let back: ErrorBody = serde_json::from_str(&json).unwrap();
        assert_eq!(back.error, env.error);
        assert_eq!(back.message, env.message);
    }
}
