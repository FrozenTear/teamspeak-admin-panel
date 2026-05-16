//! Production [`ActionDispatcher`] — PURA-249.
//!
//! PURA-241 shipped the engine on a [`BasicDispatcher`] stand-in:
//! `logLine` ran for real, the other three action kinds failed loudly.
//! This module is the real thing — it lowers each [`Action`] onto the
//! manager's existing subsystems:
//!
//! - **`ts6Command`** → a typed [`crate::control::ControlBackend`] call
//!   via the shared [`ControlBackendPool`], against the flow's
//!   `serverConfigId` / `virtualServerId`. Arguments support single-pass
//!   `${trigger.*}` substitution against the run's resolved trigger
//!   document (`docs/flows/architecture.md` §4).
//! - **`musicBotCommand`** → a `music_bot::BotCommand` dispatched at the
//!   target bot actor through the [`MusicBotService`] supervisor.
//! - **`webhookOut`** → a `reqwest` POST, **SSRF-gated** via
//!   [`ts6_ssrf::is_url_allowed`] with the resolved IP pinned onto the
//!   outbound connection so DNS rebinding cannot win between validation
//!   and connect. Redirects are disabled so a `30x` to an internal URL
//!   cannot bypass the gate.
//! - **`logLine`** → unchanged from [`BasicDispatcher`].
//!
//! The command whitelist consulted at create / patch time lives in
//! [`crate::flow::engine::commands`]; the dispatcher re-checks it at run
//! time before lowering, since a stored flow row can predate a whitelist
//! change.
//!
//! [`BasicDispatcher`]: crate::flow::engine::BasicDispatcher
//! [`ActionDispatcher`]: crate::flow::engine::ActionDispatcher

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value as JsonValue, json};
use ts6_manager_shared::flows::{Action, ModerateEffect};
use ts6_ssrf::Resolver;

use music_bot::{AudioCommand, BotCommand, BotId, NewTrack, QueueCommand};

use crate::app_state::AppState;
use crate::audit::AuditKind;
use crate::control::{ControlBackend, ControlBackendPool};
use crate::db::Database;
use crate::music_bots::MusicBotService;
use crate::repos::{admin_audit_log, moderation_case_actions, moderation_cases, server_connections};
use crate::webquery::BanAddParams;

use super::engine::commands;
use super::engine::{ActionContext, ActionDispatcher, ActionOutcome};

/// `webhookOut` outbound-request budget. One shot, no retry
/// (`architecture.md` §4 — "We send once; failure → run errors").
const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Production [`ActionDispatcher`]. Cheap to clone (every field is
/// `Arc`-shared); `main.rs` builds one from [`AppState`] and hands it to
/// the engine via `EngineDeps`.
#[derive(Clone)]
pub struct ProductionDispatcher {
    db: Arc<Database>,
    control: ControlBackendPool,
    music: MusicBotService,
    ssrf_resolver: Arc<dyn Resolver>,
}

impl ProductionDispatcher {
    /// Build the dispatcher from the shared [`AppState`]. Called once in
    /// `run_serve` before the flow engine boots.
    pub fn new(state: &AppState) -> Self {
        Self {
            db: state.db.clone(),
            control: state.control.clone(),
            music: state.music_bots.clone(),
            ssrf_resolver: state.ssrf_resolver.clone(),
        }
    }
}

#[async_trait]
impl ActionDispatcher for ProductionDispatcher {
    async fn dispatch(&self, ctx: &ActionContext, action: &Action) -> ActionOutcome {
        let result = match action {
            Action::LogLine { message } => {
                tracing::info!(
                    flow.id = ctx.flow_id.0,
                    flow.name = %ctx.flow_name,
                    run.id = ctx.run_id.0,
                    action.index = ctx.action_index,
                    message = %message,
                    "flow logLine"
                );
                Ok(())
            }
            Action::Ts6Command { command, args } => self.dispatch_ts6(ctx, command, args).await,
            Action::MusicBotCommand {
                bot_id,
                command,
                args,
            } => self.dispatch_music_bot(*bot_id, command, args).await,
            Action::WebhookOut { url, headers } => {
                dispatch_webhook(self.ssrf_resolver.as_ref(), ctx, url, headers).await
            }
            Action::Moderate {
                effect,
                duration_secs,
                reason_template,
                rule_key,
            } => {
                self.dispatch_moderate(ctx, *effect, *duration_secs, reason_template, rule_key)
                    .await
            }
        };
        match result {
            Ok(()) => ActionOutcome::Ok,
            Err(message) => ActionOutcome::Errored(message),
        }
    }
}

impl ProductionDispatcher {
    /// Lower a `ts6Command` onto a typed [`crate::control::ControlBackend`]
    /// call. `${trigger.*}` placeholders in string arguments are resolved
    /// first; an unresolved placeholder errors the action.
    async fn dispatch_ts6(
        &self,
        ctx: &ActionContext,
        command: &str,
        args: &Map<String, JsonValue>,
    ) -> Result<(), String> {
        // Run-time re-check — the stored flow row can predate a whitelist
        // change (the routes layer also gates this at create / patch time).
        commands::validate_ts6_command(command, args)?;

        let rendered = render_args(args, &ctx.trigger)?;

        // The flow's `serverConfigId` is the `server_connection` row id —
        // the same key the `/api/control/{configId}/...` routes use.
        let connection = server_connections::find_by_id(&self.db, ctx.server_config_id)
            .await
            .map_err(|e| format!("ts6Command: reading server connection failed: {e}"))?
            .ok_or_else(|| {
                format!(
                    "ts6Command: no server connection for serverConfigId {}",
                    ctx.server_config_id
                )
            })?;
        let backend = self
            .control
            .get_or_build(connection.id, Some(&connection))
            .await
            .map_err(|e| format!("ts6Command: control backend unavailable: {e}"))?;

        let sid = ctx.virtual_server_id;
        match command {
            "clientmove" => {
                let clid = arg_i64(&rendered, "clid")?;
                let cid = arg_i64(&rendered, "cid")?;
                let cpw = opt_arg_str(&rendered, "cpw")?;
                backend
                    .clientmove(sid, clid, cid, cpw.as_deref())
                    .await
                    .map_err(|e| format!("ts6Command clientmove failed: {e}"))?;
            }
            "clientkick" => {
                let clid = arg_i64(&rendered, "clid")?;
                // §7.8 — reasonid defaults to 5 (server kick).
                let reasonid = opt_arg_i64(&rendered, "reasonid")?.unwrap_or(5);
                let reasonmsg = opt_arg_str(&rendered, "reasonmsg")?;
                backend
                    .clientkick(sid, clid, reasonid, reasonmsg.as_deref())
                    .await
                    .map_err(|e| format!("ts6Command clientkick failed: {e}"))?;
            }
            "clientmute" => {
                let clid = arg_i64(&rendered, "clid")?;
                // PURA-295 / PURA-292: server-side mute is revoking the
                // `client_is_talker` flag. The client-self `client_*_muted`
                // properties the old `client_set_muted` wrote are rejected
                // `1538` for a third party on a live TS6 host.
                backend
                    .client_set_talker(sid, clid, false)
                    .await
                    .map_err(|e| format!("ts6Command clientmute failed: {e}"))?;
            }
            "clientunmute" => {
                let clid = arg_i64(&rendered, "clid")?;
                // PURA-295 / PURA-292: restore the `client_is_talker` flag.
                // TS6 answers `1538` when the target is not in a moderated
                // channel — there the client can already speak, so the
                // unmute intent is satisfied (mirrors
                // `routes/moderation/actions.rs`). Surface any other error.
                match backend.client_set_talker(sid, clid, true).await {
                    Ok(()) => {}
                    Err(e) if e.upstream_code() == 1538 => {}
                    Err(e) => return Err(format!("ts6Command clientunmute failed: {e}")),
                }
            }
            "sendtextmessage" => {
                let targetmode = arg_i64(&rendered, "targetmode")?;
                let target = arg_i64(&rendered, "target")?;
                let msg = arg_str(&rendered, "msg")?;
                backend
                    .sendtextmessage(sid, targetmode, target, &msg)
                    .await
                    .map_err(|e| format!("ts6Command sendtextmessage failed: {e}"))?;
            }
            "servergroupaddclient" => {
                let sgid = arg_i64(&rendered, "sgid")?;
                let cldbid = arg_i64(&rendered, "cldbid")?;
                backend
                    .servergroupaddclient(sid, sgid, cldbid)
                    .await
                    .map_err(|e| format!("ts6Command servergroupaddclient failed: {e}"))?;
            }
            // Unreachable past `validate_ts6_command`, but kept exhaustive.
            other => return Err(format!("ts6Command `{other}` is not whitelisted")),
        }
        Ok(())
    }

    /// Lower a `musicBotCommand` onto a `music_bot::BotCommand` and
    /// dispatch it at the target bot actor via the supervisor.
    async fn dispatch_music_bot(
        &self,
        bot_id: u64,
        command: &str,
        args: &Map<String, JsonValue>,
    ) -> Result<(), String> {
        commands::validate_music_bot_command(command, args)?;

        let cmd = match command {
            "connect" => BotCommand::Connect,
            "disconnect" => BotCommand::Disconnect,
            "leaveChannel" => BotCommand::LeaveChannel,
            "joinChannel" => BotCommand::JoinChannel(arg_u64(args, "channelId")?),
            "pause" => BotCommand::Audio(AudioCommand::Pause),
            "resume" => BotCommand::Audio(AudioCommand::Resume),
            "stop" => BotCommand::Audio(AudioCommand::Stop),
            "skipNext" => BotCommand::Audio(AudioCommand::SkipNext),
            "skipPrev" => BotCommand::Audio(AudioCommand::SkipPrev),
            "clearQueue" => BotCommand::Queue(QueueCommand::Clear),
            "enqueue" => {
                let title = arg_str(args, "title")?;
                let url = arg_str(args, "url")?;
                BotCommand::Queue(QueueCommand::Enqueue(NewTrack::url(title, url)))
            }
            other => return Err(format!("musicBotCommand `{other}` is not whitelisted")),
        };

        self.music
            .supervisor
            .send(BotId(bot_id), cmd)
            .await
            .map_err(|e| format!("musicBotCommand: bot {bot_id}: {e}"))
    }

    /// Lower a `Moderate` node onto the Phase 9.0 case model — the single
    /// audited bridge between an automod flow run and `moderation_case`
    /// (Phase 9.1 brief §4.3 / §5). Resolves the subject from the trigger
    /// event, applies the TS6 effect in `enforce` mode, then opens or
    /// reuses the case and appends the timeline action + audit row.
    async fn dispatch_moderate(
        &self,
        ctx: &ActionContext,
        effect: ModerateEffect,
        duration_secs: Option<u32>,
        reason: &str,
        rule_key: &str,
    ) -> Result<(), String> {
        let subject = resolve_subject(&ctx.trigger)?;

        // §6 safeguards (shadow/enforce, circuit breaker, exemptions,
        // cooldown, kill switch) land in Phase 9.1.3; the hook is stubbed.
        let mode = evaluate_safeguards(ctx, rule_key);

        let trigger_kind = ctx
            .trigger
            .get("kind")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let observed_count = ctx.trigger.get("count").and_then(JsonValue::as_i64);

        // Step 3 — the TS6 effect. `shadow` records the case but applies
        // nothing (brief §6.1). A failed `enforce` effect (subject already
        // gone, upstream error) must not drop the audit trail: it is
        // logged and the case is still recorded — brief §4.2 atomicity is
        // "no effect without a case", not "no case without an effect".
        let mut ts_ref: Option<String> = None;
        if mode == ModerationMode::Enforce {
            match self
                .apply_effect(ctx, &subject, effect, duration_secs, reason)
                .await
            {
                Ok(r) => ts_ref = r,
                Err(e) => tracing::warn!(
                    flow.id = ctx.flow_id.0,
                    run.id = ctx.run_id.0,
                    rule_key,
                    error = %e,
                    "Moderate: TS6 effect failed; recording the case anyway"
                ),
            }
        }

        // Steps 4–5 — open/reuse the case, append the timeline action and
        // the audit row.
        bridge_to_case(
            &self.db,
            &CaseBridge {
                server_config_id: ctx.server_config_id,
                virtual_server_id: ctx.virtual_server_id,
                subject: &subject,
                effect,
                reason,
                rule_key,
                flow_id: ctx.flow_id.0,
                run_id: ctx.run_id.0,
                trigger_kind,
                observed_count,
                mode,
                ts_ref,
            },
        )
        .await
    }

    /// Resolve the control backend and apply the [`ModerateEffect`].
    /// Returns the TS6 ban id when `effect == Ban`, `None` otherwise.
    async fn apply_effect(
        &self,
        ctx: &ActionContext,
        subject: &ResolvedSubject,
        effect: ModerateEffect,
        duration_secs: Option<u32>,
        reason: &str,
    ) -> Result<Option<String>, String> {
        let connection = server_connections::find_by_id(&self.db, ctx.server_config_id)
            .await
            .map_err(|e| format!("Moderate: reading server connection failed: {e}"))?
            .ok_or_else(|| {
                format!(
                    "Moderate: no server connection for serverConfigId {}",
                    ctx.server_config_id
                )
            })?;
        let backend = self
            .control
            .get_or_build(connection.id, Some(&connection))
            .await
            .map_err(|e| format!("Moderate: control backend unavailable: {e}"))?;
        let sid = ctx.virtual_server_id;

        match effect {
            ModerateEffect::Ban => {
                // `banadd` keys on the durable UID, so no `clid` lookup is
                // needed. `time = None` is a permanent ban (operator-only
                // policy is a 9.1.3 safeguard, not enforced here).
                let params = BanAddParams {
                    ip: None,
                    uid: Some(&subject.uid),
                    mytsid: None,
                    name: None,
                    banreason: Some(reason),
                    time: duration_secs.map(i64::from),
                };
                let ban_id = backend
                    .banadd(sid, &params)
                    .await
                    .map_err(|e| format!("Moderate ban (banadd) failed: {e}"))?;
                Ok(Some(ban_id.to_string()))
            }
            // warn / mute / kick act on the live session `clid`; the
            // trigger context carries only the durable UID.
            ModerateEffect::Warn => {
                let clid = connected_clid(backend.as_ref(), sid, &subject.uid).await?;
                backend
                    .sendtextmessage(sid, 1, clid, reason)
                    .await
                    .map_err(|e| format!("Moderate warn (sendtextmessage) failed: {e}"))?;
                Ok(None)
            }
            ModerateEffect::Mute => {
                let clid = connected_clid(backend.as_ref(), sid, &subject.uid).await?;
                backend
                    .client_set_talker(sid, clid, false)
                    .await
                    .map_err(|e| format!("Moderate mute (talker flag) failed: {e}"))?;
                Ok(None)
            }
            ModerateEffect::Kick => {
                let clid = connected_clid(backend.as_ref(), sid, &subject.uid).await?;
                backend
                    .clientkick(sid, clid, 5, Some(reason))
                    .await
                    .map_err(|e| format!("Moderate kick (clientkick) failed: {e}"))?;
                Ok(None)
            }
        }
    }
}

// ---- `Moderate` → Phase 9.0 case bridge ---------------------------------

/// Per-action automod mode (brief §6.1). Phase 9.1.3 resolves this from
/// the rule's stored shadow/enforce flag and circuit-breaker state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModerationMode {
    /// Record the case + audit row, apply no TS6 effect.
    Shadow,
    /// Apply the TS6 effect and mark the case `actioned`.
    Enforce,
}

impl ModerationMode {
    fn as_str(self) -> &'static str {
        match self {
            ModerationMode::Shadow => "shadow",
            ModerationMode::Enforce => "enforce",
        }
    }
}

/// Safeguard evaluation hook — Phase 9.1.3 replaces this stub with the
/// brief §6 safeguards (shadow-by-default, per-rule circuit breaker,
/// exemptions, subject cooldown, global kill switch). Until then every
/// automod action resolves to `enforce` with no suppression, so the
/// 9.1.2 case bridge is exercised end to end.
fn evaluate_safeguards(_ctx: &ActionContext, _rule_key: &str) -> ModerationMode {
    ModerationMode::Enforce
}

/// The automod subject — resolved from the trigger event document, never
/// from the `Moderate` node config (brief §4.1).
struct ResolvedSubject {
    uid: String,
    nickname: String,
}

/// Pull the subject UID + nickname snapshot off a trigger event.
/// `ts6ChatMessage` / `ts6ClientJoined` carry `clientUniqueIdentifier`;
/// a subject-scoped `ts6Flood` carries the UID in `bucketKey`. A
/// global-scope flood (`bucketKey == "*"`) has no subject and cannot
/// drive a `Moderate` node.
fn resolve_subject(trigger: &JsonValue) -> Result<ResolvedSubject, String> {
    let uid = trigger
        .get("clientUniqueIdentifier")
        .and_then(JsonValue::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            trigger
                .get("bucketKey")
                .and_then(JsonValue::as_str)
                .filter(|s| !s.is_empty() && *s != "*")
        })
        .ok_or("Moderate: the trigger event carries no resolvable subject UID")?;
    let nickname = trigger
        .get("clientNickname")
        .and_then(JsonValue::as_str)
        .unwrap_or("")
        .to_string();
    Ok(ResolvedSubject {
        uid: uid.to_string(),
        nickname,
    })
}

/// Resolve a connected client's session `clid` from its durable UID via
/// `clientlist`. Errors when the subject is not currently connected.
async fn connected_clid(
    backend: &dyn ControlBackend,
    sid: i64,
    uid: &str,
) -> Result<i64, String> {
    let clients = backend
        .clientlist(sid)
        .await
        .map_err(|e| format!("Moderate: clientlist failed: {e}"))?;
    clients
        .into_iter()
        .find(|c| c.client_unique_identifier == uid)
        .map(|c| c.clid)
        .ok_or_else(|| format!("Moderate: subject `{uid}` is not connected"))
}

/// Inputs to [`bridge_to_case`].
struct CaseBridge<'a> {
    server_config_id: i64,
    virtual_server_id: i64,
    subject: &'a ResolvedSubject,
    effect: ModerateEffect,
    reason: &'a str,
    rule_key: &'a str,
    flow_id: i64,
    run_id: i64,
    trigger_kind: &'a str,
    observed_count: Option<i64>,
    mode: ModerationMode,
    ts_ref: Option<String>,
}

/// Open or reuse the subject's automod `moderation_case` for this rule,
/// then append the timeline `moderation_case_action` and the
/// `admin_audit_log` row (brief §5). Dedup: an open automod case for the
/// same subject + `originRef` folds the action in rather than spawning a
/// new case, so a 20-message flood produces one case, not twenty.
async fn bridge_to_case(db: &Database, b: &CaseBridge<'_>) -> Result<(), String> {
    let origin_ref = format!("{}:{}", b.rule_key, b.flow_id);

    // Dedup — reuse the subject's open (non-resolved) automod case for
    // this rule.
    let existing = moderation_cases::list_for_subject(db, &b.subject.uid)
        .await
        .map_err(|e| format!("Moderate: case dedup query failed: {e}"))?
        .into_iter()
        .find(|c| {
            c.origin == "automod"
                && c.status != "resolved"
                && c.originRef.as_deref() == Some(origin_ref.as_str())
        });

    let case_id = match existing {
        Some(c) => c.id,
        None => {
            let case = moderation_cases::insert(
                db,
                moderation_cases::NewModerationCase {
                    serverConfigId: b.server_config_id,
                    virtualServerId: b.virtual_server_id,
                    subjectUid: b.subject.uid.clone(),
                    subjectNicknameSnapshot: b.subject.nickname.clone(),
                    origin: "automod".to_string(),
                    originRef: Some(origin_ref.clone()),
                    reason: b.reason.to_string(),
                    openedByUserId: None,
                },
            )
            .await
            .map_err(|e| format!("Moderate: opening the case failed: {e}"))?;
            // `insert` always lands `open`; an `enforce`-mode action moves
            // it straight to `actioned` (brief §5). `shadow` stays `open`.
            if b.mode == ModerationMode::Enforce {
                moderation_cases::set_status(db, case.id, "actioned", None)
                    .await
                    .map_err(|e| format!("Moderate: marking the case actioned failed: {e}"))?;
            }
            case.id
        }
    };

    let payload = json!({
        "flowId": b.flow_id,
        "runId": b.run_id,
        "ruleKey": b.rule_key,
        "triggerKind": b.trigger_kind,
        "observedCount": b.observed_count,
        "mode": b.mode.as_str(),
    });

    moderation_case_actions::insert(
        db,
        moderation_case_actions::NewModerationCaseAction {
            caseId: case_id,
            actorUserId: None,
            actorUsernameSnapshot: "automod".to_string(),
            actionKind: b.effect.as_action_kind().to_string(),
            reason: b.reason.to_string(),
            tsRef: b.ts_ref.clone(),
            payload: Some(payload.clone()),
        },
    )
    .await
    .map_err(|e| format!("Moderate: appending the timeline action failed: {e}"))?;

    admin_audit_log::insert(
        db,
        admin_audit_log::NewAdminAuditLog {
            actorUserId: None,
            actorUsername: "automod".to_string(),
            kind: AuditKind::ModerationAutomodAction.as_str().to_string(),
            targetKind: Some("moderationCase".to_string()),
            targetId: Some(case_id),
            targetLabel: Some(b.subject.uid.clone()),
            payload: Some(payload),
            outcome: "success".to_string(),
            errorMsg: None,
            requestIp: None,
            requestUserAgent: None,
        },
    )
    .await
    .map_err(|e| format!("Moderate: writing the audit row failed: {e}"))?;

    Ok(())
}

/// `webhookOut` — SSRF-gated outbound POST. The body carries flow / run
/// identity plus the resolved trigger document so the receiver can
/// correlate the call (`architecture.md` §4).
///
/// Free-standing rather than a method: it depends only on the SSRF
/// resolver, which keeps it directly testable without a full `AppState`.
async fn dispatch_webhook(
    resolver: &dyn Resolver,
    ctx: &ActionContext,
    url: &str,
    headers: &[(String, String)],
) -> Result<(), String> {
    // SSRF gate (ts6-ssrf, R6). Returns the IP the validator accepted; we
    // pin the outbound connection to it so DNS rebinding between
    // validation and connect cannot reach a private range.
    let target = ts6_ssrf::is_url_allowed(url, resolver)
        .await
        .map_err(|e| format!("webhookOut: URL rejected by SSRF gate: {e}"))?;

    let mut builder = reqwest::Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        // A 30x to an internal URL would bypass the SSRF gate — the
        // redirect target is never re-validated. Disable redirects.
        .redirect(reqwest::redirect::Policy::none());
    if let Some(ip) = target.resolved_ip {
        builder = builder.resolve_to_addrs(&target.host, &[SocketAddr::new(ip, target.port)]);
    }
    let client = builder
        .build()
        .map_err(|e| format!("webhookOut: HTTP client build failed: {e}"))?;

    let body = serde_json::json!({
        "flowId": ctx.flow_id.0,
        "runId": ctx.run_id.0,
        "flowName": ctx.flow_name,
        "trigger": ctx.trigger,
    });

    let mut request = client.post(target.url.clone()).json(&body);
    for (name, value) in headers {
        request = request.header(name.as_str(), value.as_str());
    }
    let response = request
        .send()
        .await
        .map_err(|e| format!("webhookOut: POST failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("webhookOut: receiver responded {status}"));
    }
    Ok(())
}

// ---- `${trigger.*}` templating -----------------------------------------

/// Render every string-valued argument through [`substitute`]. Non-string
/// values pass through untouched — only strings can carry placeholders.
fn render_args(
    args: &Map<String, JsonValue>,
    trigger: &JsonValue,
) -> Result<Map<String, JsonValue>, String> {
    let mut out = Map::with_capacity(args.len());
    for (key, value) in args {
        let rendered = match value {
            JsonValue::String(s) => JsonValue::String(substitute(s, trigger)?),
            other => other.clone(),
        };
        out.insert(key.clone(), rendered);
    }
    Ok(out)
}

/// Single-pass `${trigger.<key>}` substitution against the run's resolved
/// trigger document. No expressions, no conditionals (`architecture.md`
/// §4); each placeholder is resolved exactly once and never re-scanned.
fn substitute(input: &str, trigger: &JsonValue) -> Result<String, String> {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err("unterminated `${...}` placeholder in argument".to_string());
        };
        out.push_str(&resolve_placeholder(after[..end].trim(), trigger)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Resolve one placeholder expression (the text between `${` and `}`).
/// Only `trigger.<key>` against a top-level key of the trigger document
/// is supported; a missing or null key errors the action.
fn resolve_placeholder(expr: &str, trigger: &JsonValue) -> Result<String, String> {
    let Some(key) = expr.strip_prefix("trigger.") else {
        return Err(format!(
            "unsupported placeholder `${{{expr}}}` — only `${{trigger.<key>}}` is supported"
        ));
    };
    let value = trigger.get(key).ok_or_else(|| {
        format!(
            "placeholder `${{trigger.{key}}}` references a key absent from the trigger document"
        )
    })?;
    match value {
        JsonValue::String(s) => Ok(s.clone()),
        JsonValue::Null => Err(format!("placeholder `${{trigger.{key}}}` resolved to null")),
        other => Ok(other.to_string()),
    }
}

// ---- Argument extraction -----------------------------------------------
//
// Run after templating, so a numeric argument may arrive either as a JSON
// number (literal in the flow definition) or as a string (the rendered
// result of a `${trigger.*}` placeholder). Both forms are accepted.

fn arg_i64(args: &Map<String, JsonValue>, key: &str) -> Result<i64, String> {
    match args.get(key) {
        Some(JsonValue::Number(n)) => n
            .as_i64()
            .ok_or_else(|| format!("argument `{key}` must be an integer")),
        Some(JsonValue::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("argument `{key}` must be an integer, got `{s}`")),
        Some(_) => Err(format!("argument `{key}` must be an integer")),
        None => Err(format!("missing required argument `{key}`")),
    }
}

fn arg_u64(args: &Map<String, JsonValue>, key: &str) -> Result<u64, String> {
    let value = arg_i64(args, key)?;
    u64::try_from(value).map_err(|_| format!("argument `{key}` must not be negative"))
}

fn arg_str(args: &Map<String, JsonValue>, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(JsonValue::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("argument `{key}` must be a string")),
        None => Err(format!("missing required argument `{key}`")),
    }
}

fn opt_arg_str(args: &Map<String, JsonValue>, key: &str) -> Result<Option<String>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(JsonValue::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("optional argument `{key}` must be a string")),
    }
}

fn opt_arg_i64(args: &Map<String, JsonValue>, key: &str) -> Result<Option<i64>, String> {
    match args.get(key) {
        None | Some(JsonValue::Null) => Ok(None),
        Some(_) => arg_i64(args, key).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ts6_manager_shared::flows::{FlowId, FlowRunId};

    fn obj(v: JsonValue) -> Map<String, JsonValue> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn test_ctx() -> ActionContext {
        ActionContext {
            flow_id: FlowId(1),
            run_id: FlowRunId(1),
            flow_name: "test-flow".into(),
            server_config_id: 1,
            virtual_server_id: 1,
            action_index: 0,
            trigger: json!({ "kind": "manualFire" }),
        }
    }

    #[tokio::test]
    async fn webhook_rejected_for_loopback_literal() {
        // 127.0.0.1 is an IP literal in a blocked range — the gate
        // rejects it synchronously, before the resolver is consulted.
        let resolver = ts6_ssrf::MockResolver::new();
        let err = dispatch_webhook(&resolver, &test_ctx(), "http://127.0.0.1/hook", &[])
            .await
            .unwrap_err();
        assert!(err.contains("SSRF gate"), "got: {err}");
    }

    #[tokio::test]
    async fn webhook_rejected_for_non_http_scheme() {
        let resolver = ts6_ssrf::MockResolver::new();
        let err = dispatch_webhook(&resolver, &test_ctx(), "file:///etc/passwd", &[])
            .await
            .unwrap_err();
        assert!(err.contains("SSRF gate"), "got: {err}");
    }

    #[test]
    fn substitute_resolves_trigger_keys() {
        let trigger = json!({
            "kind": "ts6ClientJoined",
            "channelId": 5,
            "clientNickname": "Alice",
        });
        assert_eq!(
            substitute("Welcome ${trigger.clientNickname}.", &trigger).unwrap(),
            "Welcome Alice."
        );
        // Numeric trigger values stringify without JSON quoting.
        assert_eq!(substitute("${trigger.channelId}", &trigger).unwrap(), "5");
        // A literal with no placeholder is returned unchanged.
        assert_eq!(
            substitute("no placeholder", &trigger).unwrap(),
            "no placeholder"
        );
    }

    #[test]
    fn substitute_errors_on_missing_key() {
        let trigger = json!({ "kind": "manualFire" });
        let err = substitute("${trigger.clientNickname}", &trigger).unwrap_err();
        assert!(
            err.contains("absent from the trigger document"),
            "got: {err}"
        );
    }

    #[test]
    fn substitute_errors_on_unterminated_and_unknown_placeholder() {
        let trigger = json!({ "kind": "manualFire" });
        assert!(substitute("${trigger.foo", &trigger).is_err());
        let err = substitute("${env.HOME}", &trigger).unwrap_err();
        assert!(err.contains("only `${trigger.<key>}`"), "got: {err}");
    }

    #[test]
    fn substitute_is_single_pass() {
        // A trigger value that itself looks like a placeholder is NOT
        // re-expanded — substitution is single-pass by construction.
        let trigger = json!({ "a": "${trigger.b}", "b": "deep" });
        assert_eq!(
            substitute("${trigger.a}", &trigger).unwrap(),
            "${trigger.b}"
        );
    }

    #[test]
    fn render_args_only_touches_strings() {
        let trigger = json!({ "channelId": 9 });
        let args = obj(json!({
            "cid": "${trigger.channelId}",
            "clid": 42,
            "flag": true,
        }));
        let rendered = render_args(&args, &trigger).unwrap();
        assert_eq!(rendered["cid"], json!("9"));
        assert_eq!(rendered["clid"], json!(42));
        assert_eq!(rendered["flag"], json!(true));
    }

    #[test]
    fn arg_i64_accepts_number_and_numeric_string() {
        assert_eq!(arg_i64(&obj(json!({ "n": 7 })), "n").unwrap(), 7);
        assert_eq!(arg_i64(&obj(json!({ "n": "7" })), "n").unwrap(), 7);
        assert!(arg_i64(&obj(json!({ "n": "abc" })), "n").is_err());
        assert!(arg_i64(&obj(json!({})), "n").is_err());
    }

    #[test]
    fn arg_u64_rejects_negative() {
        assert_eq!(arg_u64(&obj(json!({ "n": 3 })), "n").unwrap(), 3);
        assert!(arg_u64(&obj(json!({ "n": -1 })), "n").is_err());
    }

    #[test]
    fn opt_args_treat_absent_and_null_as_none() {
        assert_eq!(opt_arg_str(&obj(json!({})), "x").unwrap(), None);
        assert_eq!(opt_arg_str(&obj(json!({ "x": null })), "x").unwrap(), None);
        assert_eq!(
            opt_arg_str(&obj(json!({ "x": "v" })), "x").unwrap(),
            Some("v".to_string())
        );
        assert_eq!(opt_arg_i64(&obj(json!({})), "n").unwrap(), None);
        assert_eq!(opt_arg_i64(&obj(json!({ "n": 5 })), "n").unwrap(), Some(5));
    }

    // ---- `Moderate` subject resolution + Phase 9.0 case bridge ----------

    #[test]
    fn resolve_subject_reads_uid_and_nickname() {
        let trigger = json!({
            "kind": "ts6ChatMessage",
            "clientUniqueIdentifier": "uid-x=",
            "clientNickname": "Bob",
        });
        let s = resolve_subject(&trigger).unwrap();
        assert_eq!(s.uid, "uid-x=");
        assert_eq!(s.nickname, "Bob");
    }

    #[test]
    fn resolve_subject_falls_back_to_flood_bucket_key() {
        let trigger = json!({ "kind": "ts6Flood", "bucketKey": "uid-flood=", "count": 5 });
        let s = resolve_subject(&trigger).unwrap();
        assert_eq!(s.uid, "uid-flood=");
        assert_eq!(s.nickname, "", "a flood event carries no nickname");
    }

    #[test]
    fn resolve_subject_rejects_global_flood_with_no_subject() {
        let trigger = json!({ "kind": "ts6Flood", "bucketKey": "*", "count": 9 });
        assert!(resolve_subject(&trigger).is_err());
        assert!(resolve_subject(&json!({ "kind": "cron" })).is_err());
    }

    async fn fresh_db() -> Arc<Database> {
        let db = crate::db::connect_in_memory().await.unwrap();
        crate::db::migrations::run(&db).await.unwrap();
        db
    }

    fn subject(uid: &str) -> ResolvedSubject {
        ResolvedSubject {
            uid: uid.into(),
            nickname: "Nick".into(),
        }
    }

    fn bridge<'a>(
        subject: &'a ResolvedSubject,
        effect: ModerateEffect,
        rule_key: &'a str,
        mode: ModerationMode,
    ) -> CaseBridge<'a> {
        CaseBridge {
            server_config_id: 1,
            virtual_server_id: 1,
            subject,
            effect,
            reason: "spam",
            rule_key,
            flow_id: 7,
            run_id: 1,
            trigger_kind: "ts6ChatMessage",
            observed_count: None,
            mode,
            ts_ref: None,
        }
    }

    #[tokio::test]
    async fn moderate_bridge_opens_one_automod_case() {
        let db = fresh_db().await;
        let s = subject("uid-a");
        bridge_to_case(
            &db,
            &bridge(&s, ModerateEffect::Kick, "bad-name", ModerationMode::Enforce),
        )
        .await
        .unwrap();

        let cases = moderation_cases::list_for_subject(&db, "uid-a").await.unwrap();
        assert_eq!(cases.len(), 1, "exactly one automod case");
        let case = &cases[0];
        assert_eq!(case.origin, "automod");
        assert_eq!(case.status, "actioned", "enforce mode marks the case actioned");
        assert_eq!(case.originRef.as_deref(), Some("bad-name:7"));
        assert!(case.openedByUserId.is_none(), "automod case has no operator");

        let actions = moderation_case_actions::list_for_case(&db, case.id)
            .await
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].actionKind, "kick");
        assert_eq!(actions[0].actorUsernameSnapshot, "automod");
        assert!(actions[0].actorUserId.is_none());
        let payload = actions[0].payload.as_ref().expect("action payload");
        assert_eq!(payload["flowId"], 7);
        assert_eq!(payload["ruleKey"], "bad-name");
        assert_eq!(payload["mode"], "enforce");
        assert_eq!(payload["triggerKind"], "ts6ChatMessage");

        let (audit, _) = admin_audit_log::list(&db, &admin_audit_log::ListFilter::default(), 50, 0)
            .await
            .unwrap();
        assert_eq!(audit.len(), 1, "one audit row per automod action");
        assert_eq!(audit[0].kind, "moderationAutomodAction");
        assert_eq!(audit[0].targetId, Some(case.id));
    }

    #[tokio::test]
    async fn moderate_bridge_dedups_into_open_case() {
        let db = fresh_db().await;
        let s = subject("uid-b");
        let b = bridge(&s, ModerateEffect::Warn, "chat-filter", ModerationMode::Enforce);
        bridge_to_case(&db, &b).await.unwrap();
        bridge_to_case(&db, &b).await.unwrap();

        let cases = moderation_cases::list_for_subject(&db, "uid-b").await.unwrap();
        assert_eq!(cases.len(), 1, "a second trigger folds into the same case");
        let actions = moderation_case_actions::list_for_case(&db, cases[0].id)
            .await
            .unwrap();
        assert_eq!(actions.len(), 2, "each trigger appends a timeline action");
    }

    #[tokio::test]
    async fn moderate_bridge_opens_a_distinct_case_per_rule() {
        let db = fresh_db().await;
        let s = subject("uid-c");
        bridge_to_case(
            &db,
            &bridge(&s, ModerateEffect::Warn, "rule-x", ModerationMode::Enforce),
        )
        .await
        .unwrap();
        bridge_to_case(
            &db,
            &bridge(&s, ModerateEffect::Warn, "rule-y", ModerationMode::Enforce),
        )
        .await
        .unwrap();
        let cases = moderation_cases::list_for_subject(&db, "uid-c").await.unwrap();
        assert_eq!(cases.len(), 2, "each rule_key keys its own case");
    }

    #[tokio::test]
    async fn moderate_bridge_shadow_mode_leaves_case_open() {
        // Also pins migration 0012: a `warn` actionKind must satisfy the
        // `moderation_case_action` ASSERT.
        let db = fresh_db().await;
        let s = subject("uid-d");
        bridge_to_case(
            &db,
            &bridge(&s, ModerateEffect::Warn, "shadow-rule", ModerationMode::Shadow),
        )
        .await
        .unwrap();
        let cases = moderation_cases::list_for_subject(&db, "uid-d").await.unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].status, "open", "shadow mode leaves the case open");
        let actions = moderation_case_actions::list_for_case(&db, cases[0].id)
            .await
            .unwrap();
        assert_eq!(actions[0].actionKind, "warn");
    }
}
