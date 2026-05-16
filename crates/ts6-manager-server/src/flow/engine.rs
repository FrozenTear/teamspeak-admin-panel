//! Flow engine — PURA-241.
//!
//! [`FlowEngine::start`] is called from `main.rs` before the router is
//! assembled (brief §6.1):
//!
//! 1. Apply the boot-time interrupt sweep — every `bot_flow_run` left in
//!    `in_flight` becomes `interrupted` before the trigger bus opens.
//! 2. Spawn the cron-tick task per enabled cron-trigger flow.
//! 3. Spawn the global TTL janitor (hourly).
//! 4. Return a [`FlowEngineHandle`] cloneable into `AppState`.
//!
//! The handle exposes the imperative surface the routes layer needs:
//!
//! - [`FlowEngineHandle::fire`] — synchronous insert of the in-flight
//!   row + spawn of the run task; returns the row id.
//! - [`FlowEngineHandle::enable`] / [`FlowEngineHandle::disable`] —
//!   trigger-subscription toggle. The next routes child wires
//!   `PATCH /api/flows/{id}` to call this.
//! - [`FlowEngineHandle::on_client_joined`] — fan-in for the WS hub's
//!   `ts:client:connected` republisher.
//! - [`FlowEngineHandle::interrupt_runs_for_flow`] — `?force=true`
//!   delete path: marks in-flight runs `interrupted` so the row delete
//!   does not orphan a still-running task.
//!
//! Per-flow drop-on-busy is implemented via per-flow `Semaphore::new(1)`
//! with `try_acquire_owned`. Cross-flow parallelism is bounded by the
//! engine-wide semaphore in [`EngineDeps::max_parallel_runs`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value as JsonValue;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use ts6_manager_shared::flows::{
    Action, ActionResult, ActionStatus, FlowDefinition, FlowId, FlowRunId, FlowRunStatus,
};

use crate::db::Database;
use crate::repos::{bot_flow_runs, bot_flows};

/// Flow-action command whitelist (`http-api.md` §3.1) — consulted by the
/// routes layer at create / patch time and by the production dispatcher
/// at run time. Path is `flow::engine::commands` as named in the spec.
pub mod commands;

use super::trigger::{ParsedTrigger, TriggerEvent};

/// Dependencies the engine takes at boot. Built by the routes child's
/// `main.rs` wiring; tests pass a minimal mock dispatcher.
#[derive(Clone)]
pub struct EngineDeps {
    pub db: Arc<Database>,
    /// Action dispatcher — gets called for every executed action.
    /// `BasicDispatcher` ships in this module; the routes child swaps in
    /// a richer impl that wires `ControlBackendPool`, `MusicBotService`,
    /// and `reqwest`.
    pub dispatcher: Arc<dyn ActionDispatcher>,
    /// Engine-wide concurrency cap (brief §6.3). The routes child reads
    /// this off env / cpu count; tests cap at 4.
    pub max_parallel_runs: usize,
    /// Global TTL for run rows (brief §5.3). Default is 30 days.
    pub run_ttl: Duration,
    /// How often the TTL janitor runs.
    pub ttl_sweep_interval: Duration,
}

impl EngineDeps {
    /// Production defaults — 30-day TTL, hourly sweep, `max(4, num_cpus)`
    /// concurrent runs. The routes child calls this then overrides
    /// `dispatcher` before passing to [`FlowEngine::start`].
    pub fn new(db: Arc<Database>, dispatcher: Arc<dyn ActionDispatcher>) -> Self {
        let max_parallel = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .max(4);
        Self {
            db,
            dispatcher,
            max_parallel_runs: max_parallel,
            run_ttl: Duration::from_secs(30 * 86_400),
            ttl_sweep_interval: Duration::from_secs(3_600),
        }
    }
}

/// Context passed to [`ActionDispatcher::dispatch`] for each action.
/// Carries the flow / run / trigger info plus the action's 0-based
/// index. Templating against `${trigger.*}` is the dispatcher's job —
/// the engine forwards the raw trigger JSON here so each action kind
/// can pick its own substitution rules.
#[derive(Debug, Clone)]
pub struct ActionContext {
    pub flow_id: FlowId,
    pub run_id: FlowRunId,
    pub flow_name: String,
    pub server_config_id: i64,
    pub virtual_server_id: i64,
    pub action_index: u32,
    /// Resolved trigger document — the same JSON persisted on the run
    /// row. Action templating reads `${trigger.<key>}` against this map.
    pub trigger: JsonValue,
}

/// Outcome of one action. The engine maps this into `ActionResult`.
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    Ok,
    Errored(String),
}

/// Pluggable action dispatcher. The default [`BasicDispatcher`] handles
/// `LogLine` only; the F-impl-routes child plugs in the production impl
/// that wires `ControlBackendPool` / `MusicBotService` / `reqwest`.
#[async_trait]
pub trait ActionDispatcher: Send + Sync + 'static {
    async fn dispatch(&self, ctx: &ActionContext, action: &Action) -> ActionOutcome;
}

/// Stand-in dispatcher used until F-impl-routes wires the production
/// one. Handles `LogLine` (real) and errors on every other action kind
/// with a clear "wiring pending" message so a misconfigured flow fails
/// loudly instead of silently no-op'ing.
#[derive(Debug, Default)]
pub struct BasicDispatcher;

#[async_trait]
impl ActionDispatcher for BasicDispatcher {
    async fn dispatch(&self, ctx: &ActionContext, action: &Action) -> ActionOutcome {
        match action {
            Action::LogLine { message } => {
                tracing::info!(
                    flow.id = ctx.flow_id.0,
                    flow.name = %ctx.flow_name,
                    run.id = ctx.run_id.0,
                    action.index = ctx.action_index,
                    message = %message,
                    "flow logLine"
                );
                ActionOutcome::Ok
            }
            Action::Ts6Command { .. }
            | Action::MusicBotCommand { .. }
            | Action::WebhookOut { .. } => ActionOutcome::Errored(
                "action dispatcher not wired yet — pending F-impl-routes".to_string(),
            ),
        }
    }
}

/// Live engine. Owned by `main.rs`; `Drop` aborts the background tasks
/// so a clean shutdown stops scheduling new runs.
pub struct FlowEngine {
    handle: FlowEngineHandle,
    cron_tasks: Vec<JoinHandle<()>>,
    ttl_task: JoinHandle<()>,
}

impl FlowEngine {
    /// Boot the engine. Brief §6.1.
    pub async fn start(deps: EngineDeps) -> Result<FlowEngine> {
        // 1. Boot-time sweep — every still-in-flight row becomes
        //    interrupted before the trigger bus opens.
        let swept = bot_flow_runs::mark_in_flight_as_interrupted(&deps.db)
            .await
            .context("boot-time interrupt sweep failed")?;
        if swept > 0 {
            tracing::warn!(
                count = swept,
                "rewrote stale in_flight bot_flow_run rows to interrupted"
            );
        }

        let inner = Arc::new(EngineInner {
            db: deps.db.clone(),
            dispatcher: deps.dispatcher.clone(),
            cross_flow_semaphore: Arc::new(Semaphore::new(deps.max_parallel_runs)),
            per_flow_locks: StdMutex::new(HashMap::new()),
            drop_counter: StdMutex::new(0),
            ts6_subs: StdMutex::new(Vec::new()),
        });
        let handle = FlowEngineHandle {
            inner: inner.clone(),
        };

        // 2. Spawn cron loops for every enabled `bot_flow` whose trigger
        //    parses as cron. A flow with a malformed cron expression is
        //    skipped with a WARN — the routes layer is the gate for
        //    validation at create / patch time.
        let mut cron_tasks = Vec::new();
        let flows = bot_flows::list(&deps.db)
            .await
            .context("flow_engine boot: list bot_flow")?;
        for flow in flows.into_iter().filter(|f| f.enabled) {
            let Some(definition) = parse_definition(&flow.flowData) else {
                tracing::warn!(
                    flow.id = flow.id,
                    "flow_engine boot: malformed flowData JSON; skipping registration"
                );
                continue;
            };
            let Ok(parsed) = ParsedTrigger::parse(&definition.trigger) else {
                tracing::warn!(
                    flow.id = flow.id,
                    "flow_engine boot: malformed trigger; skipping cron registration"
                );
                continue;
            };
            match &parsed {
                ParsedTrigger::Cron(_) => {
                    let task = spawn_cron_loop(handle.clone(), FlowId(flow.id), parsed.clone());
                    cron_tasks.push(task);
                }
                ParsedTrigger::Ts6ClientJoined { channel_id } => {
                    // Producer-driven: the WS hub republisher calls
                    // `handle.on_client_joined`. Register the subscription
                    // so the engine knows which flows want the events.
                    lock_or_poisoned(&inner.ts6_subs).push(Ts6Subscription {
                        flow_id: FlowId(flow.id),
                        virtual_server_id: flow.virtualServerId,
                        channel_id: *channel_id,
                    });
                }
                ParsedTrigger::ManualFire => {}
            }
        }

        // 3. TTL janitor.
        let ttl_task = spawn_ttl_janitor(deps.db.clone(), deps.run_ttl, deps.ttl_sweep_interval);

        Ok(FlowEngine {
            handle,
            cron_tasks,
            ttl_task,
        })
    }

    pub fn handle(&self) -> FlowEngineHandle {
        self.handle.clone()
    }
}

impl Drop for FlowEngine {
    fn drop(&mut self) {
        for t in &self.cron_tasks {
            t.abort();
        }
        self.ttl_task.abort();
    }
}

/// Cloneable engine handle — what `AppState` carries. All run-control
/// API is on this type; [`FlowEngine`] only owns the background tasks.
#[derive(Clone)]
pub struct FlowEngineHandle {
    inner: Arc<EngineInner>,
}

struct EngineInner {
    db: Arc<Database>,
    dispatcher: Arc<dyn ActionDispatcher>,
    cross_flow_semaphore: Arc<Semaphore>,
    /// Per-flow `Semaphore::new(1)` slots. Existing semaphores are
    /// retained so concurrent fires hit the same lock. Created lazily.
    per_flow_locks: StdMutex<HashMap<i64, Arc<Semaphore>>>,
    /// Dropped-trigger counter. Brief §6.3 counter
    /// `ts6_flow_runs_dropped_total{reason="busy"}` — `routes::metrics`
    /// owns the Prometheus exposition; the engine just keeps the raw
    /// value.
    drop_counter: StdMutex<u64>,
    /// Per-flow subscriptions for the `ts6ClientJoined` source. Tuple of
    /// (flow_id, virtual_server_id, optional channel filter). Engine
    /// rebuilds this set when the routes child calls
    /// [`FlowEngineHandle::enable`] on create / patch.
    ts6_subs: StdMutex<Vec<Ts6Subscription>>,
}

#[derive(Debug, Clone)]
struct Ts6Subscription {
    flow_id: FlowId,
    virtual_server_id: i64,
    channel_id: Option<i64>,
}

/// Fire-time errors visible to the routes layer. The HTTP layer maps
/// these onto the wire `ErrorBody` envelope (`docs/flows/http-api.md`
/// §4).
#[derive(Debug, thiserror::Error)]
pub enum FireError {
    #[error("flow {} not found", .0.0)]
    NotFound(FlowId),
    #[error("flow {} has malformed flowData: {}", .0.0, .1)]
    MalformedFlow(FlowId, String),
    /// Per-flow slot busy — a previous run for this flow is still
    /// in-flight. Brief §6.3 (drop-on-busy). The routes layer maps this
    /// to `429 rate_limited`.
    #[error("flow {} busy: previous run still in flight", .0.0)]
    Busy(FlowId),
    /// Cross-flow engine semaphore saturated past the wait budget.
    #[error("engine saturated: cross-flow semaphore wait timed out")]
    EngineSaturated,
    #[error(transparent)]
    Persist(#[from] anyhow::Error),
}

impl FlowEngineHandle {
    /// Manual fire (`POST /api/flows/{id}/fire`). Synchronously inserts
    /// the in-flight row, spawns the run task, and returns the row id.
    /// Always allowed even when `enabled = false` (brief §3).
    pub async fn fire(
        &self,
        flow_id: FlowId,
        context: Option<serde_json::Map<String, JsonValue>>,
    ) -> std::result::Result<FlowRunId, FireError> {
        let flow = bot_flows::find_by_id(&self.inner.db, flow_id.0)
            .await
            .map_err(FireError::Persist)?
            .ok_or(FireError::NotFound(flow_id))?;
        let definition = parse_definition(&flow.flowData)
            .ok_or_else(|| FireError::MalformedFlow(flow_id, "definition JSON".into()))?;
        let event = TriggerEvent::Manual { context };
        self.fire_event(
            flow_id,
            &flow.name,
            &definition,
            event,
            /*allow_disabled*/ true,
        )
        .await
    }

    /// Internal entrypoint shared by manualFire + cron + ts6ClientJoined.
    /// `allow_disabled = false` means a disabled flow writes a
    /// `skipped_disabled` row but does not run actions.
    async fn fire_event(
        &self,
        flow_id: FlowId,
        flow_name: &str,
        definition: &FlowDefinition,
        event: TriggerEvent,
        allow_disabled: bool,
    ) -> std::result::Result<FlowRunId, FireError> {
        // Pre-flight enable check. ManualFire ignores this; cron /
        // ts6ClientJoined still write an audit row but skip action
        // dispatch.
        let flow = bot_flows::find_by_id(&self.inner.db, flow_id.0)
            .await
            .map_err(FireError::Persist)?
            .ok_or(FireError::NotFound(flow_id))?;
        if !flow.enabled && !allow_disabled {
            let trigger_doc = event.to_json();
            let action_results = empty_action_results(&definition.actions);
            let run = bot_flow_runs::insert(
                &self.inner.db,
                bot_flow_runs::NewBotFlowRun {
                    flowId: flow_id.0,
                    trigger: trigger_doc,
                    status: FlowRunStatus::SkippedDisabled,
                    actionResults: action_results,
                    // v1.1 linear engine — no per-node records (PURA-259).
                    nodeResults: Vec::new(),
                },
            )
            .await
            .map_err(FireError::Persist)?;
            let _ = bot_flow_runs::enforce_per_flow_cap(&self.inner.db, flow_id.0).await;
            tracing::info!(
                flow.id = flow_id.0,
                flow.name = %flow_name,
                run.id = run.id,
                trigger.kind = event.kind(),
                "flow trigger fired on disabled flow; skipped"
            );
            return Ok(FlowRunId(run.id));
        }

        // Per-flow drop-on-busy.
        let per_flow_sem = self.per_flow_semaphore(flow_id.0);
        let Ok(per_flow_permit) = per_flow_sem.clone().try_acquire_owned() else {
            if let Ok(mut counter) = self.inner.drop_counter.lock() {
                *counter = counter.saturating_add(1);
            }
            tracing::info!(
                flow.id = flow_id.0,
                flow.name = %flow_name,
                trigger.kind = event.kind(),
                "flow trigger dropped; previous run still in flight"
            );
            return Err(FireError::Busy(flow_id));
        };

        // Persist the in-flight row first so the FE can poll while the
        // run is mid-flight.
        let trigger_doc = event.to_json();
        let action_results = empty_action_results(&definition.actions);
        let run = bot_flow_runs::insert(
            &self.inner.db,
            bot_flow_runs::NewBotFlowRun {
                flowId: flow_id.0,
                trigger: trigger_doc.clone(),
                status: FlowRunStatus::InFlight,
                actionResults: action_results,
                // v1.1 linear engine — no per-node records (PURA-259).
                nodeResults: Vec::new(),
            },
        )
        .await
        .map_err(FireError::Persist)?;
        let run_id = FlowRunId(run.id);

        // Spawn the per-run task. Hold the cross-flow semaphore inside
        // the task so saturation backpressures other flows without
        // blocking this fire call.
        let inner = self.inner.clone();
        let flow_name = flow_name.to_string();
        let definition = definition.clone();
        tokio::spawn(async move {
            // `_per_flow_permit` is dropped at task end → next fire for
            // this flow can proceed.
            let _per_flow_permit = per_flow_permit;
            let cross_permit = match inner.cross_flow_semaphore.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => {
                    tracing::error!(
                        flow.id = flow_id.0,
                        run.id = run_id.0,
                        "cross-flow semaphore closed; aborting run"
                    );
                    return;
                }
            };
            run_one(
                inner,
                flow_id,
                run_id,
                flow_name,
                flow.serverConfigId,
                flow.virtualServerId,
                definition,
                trigger_doc,
                cross_permit,
            )
            .await;
        });
        Ok(run_id)
    }

    /// Producer entry for the `ts6ClientJoined` source. Called by the WS
    /// hub republisher for every observed event. The engine fans the
    /// event into every subscription that matches `(virtualServerId,
    /// channelId?)`.
    pub async fn on_client_joined(
        &self,
        virtual_server_id: i64,
        channel_id: i64,
        client_unique_identifier: String,
        client_nickname: String,
    ) {
        let subs = match self.inner.ts6_subs.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for sub in subs {
            if sub.virtual_server_id != virtual_server_id {
                continue;
            }
            if let Some(filter) = sub.channel_id
                && filter != channel_id
            {
                continue;
            }
            // Re-fetch the flow definition fresh so updates land without
            // restarting the engine.
            let flow = match bot_flows::find_by_id(&self.inner.db, sub.flow_id.0).await {
                Ok(Some(f)) => f,
                Ok(None) => continue,
                Err(e) => {
                    tracing::warn!(error = %e, flow.id = sub.flow_id.0, "ts6ClientJoined: read flow failed");
                    continue;
                }
            };
            let Some(definition) = parse_definition(&flow.flowData) else {
                continue;
            };
            let event = TriggerEvent::Ts6ClientJoined {
                virtual_server_id,
                channel_id,
                client_unique_identifier: client_unique_identifier.clone(),
                client_nickname: client_nickname.clone(),
                ts: Utc::now(),
            };
            // ts6 ignores `enabled = false` for *action dispatch* but
            // still writes a `skipped_disabled` audit row.
            let _ = self
                .fire_event(sub.flow_id, &flow.name, &definition, event, false)
                .await;
        }
    }

    /// Enable a flow — registers the appropriate trigger subscription.
    /// Called by the routes child on `PATCH ... { enabled: true }`. Idempotent.
    pub async fn enable(&self, flow_id: FlowId) -> Result<()> {
        let flow = bot_flows::find_by_id(&self.inner.db, flow_id.0)
            .await?
            .with_context(|| format!("enable: flow {} not found", flow_id.0))?;
        let definition = parse_definition(&flow.flowData)
            .with_context(|| format!("enable: flow {} flowData malformed", flow_id.0))?;
        let parsed = ParsedTrigger::parse(&definition.trigger)?;
        match parsed {
            ParsedTrigger::Cron(_) => {
                // The routes child spawns the loop; in this scope we
                // emit a debug log and rely on the next reboot to pick
                // it up. Hot-reload of cron loops belongs with the
                // routes ticket.
                tracing::debug!(
                    flow.id = flow_id.0,
                    "cron trigger enabled (hot-reload pending)"
                );
            }
            ParsedTrigger::Ts6ClientJoined { channel_id } => {
                let mut subs = lock_or_poisoned(&self.inner.ts6_subs);
                subs.retain(|s| s.flow_id != flow_id);
                subs.push(Ts6Subscription {
                    flow_id,
                    virtual_server_id: flow.virtualServerId,
                    channel_id,
                });
            }
            ParsedTrigger::ManualFire => {}
        }
        Ok(())
    }

    /// Disable a flow — deregisters its trigger subscription. In-flight
    /// runs are NOT cancelled (brief §6.4 — runs always finish).
    pub fn disable(&self, flow_id: FlowId) {
        lock_or_poisoned(&self.inner.ts6_subs).retain(|s| s.flow_id != flow_id);
    }

    /// Forces in-flight runs for the given flow into `interrupted`. The
    /// routes child calls this from `DELETE /api/flows/{id}?force=true`.
    pub async fn interrupt_runs_for_flow(&self, flow_id: FlowId) -> Result<u64> {
        let sql = "
            SELECT count() FROM bot_flow_run
                WHERE flowId = $fid AND status = 'in_flight'
                GROUP ALL;
            UPDATE bot_flow_run SET
                status = 'interrupted',
                error = 'force-deleted',
                finishedAt = time::now()
            WHERE flowId = $fid AND status = 'in_flight';
        ";
        use surrealdb::types::SurrealValue;
        #[derive(serde::Deserialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct CountRow {
            count: i64,
        }
        let mut resp = self
            .inner
            .db
            .query(sql)
            .bind(("fid", flow_id.0))
            .await?
            .check()?;
        let counted: Option<CountRow> = resp.take(0)?;
        Ok(counted.map(|c| c.count.max(0) as u64).unwrap_or(0))
    }

    /// Observability — current number of times the engine has dropped a
    /// trigger due to per-flow contention. The routes child surfaces
    /// this on `/metrics`.
    pub fn dropped_count(&self) -> u64 {
        *lock_or_poisoned(&self.inner.drop_counter)
    }

    fn per_flow_semaphore(&self, flow_id: i64) -> Arc<Semaphore> {
        let mut map = lock_or_poisoned(&self.inner.per_flow_locks);
        map.entry(flow_id)
            .or_insert_with(|| Arc::new(Semaphore::new(1)))
            .clone()
    }
}

/// `std::sync::Mutex` lock helper — short critical sections only. We
/// never hold these across `.await`, so poisoning recovery is the only
/// case of interest.
fn lock_or_poisoned<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

/// Per-run task. Spawned by [`FlowEngineHandle::fire_event`]; finishes
/// when the action list is exhausted or the first action errors.
#[allow(clippy::too_many_arguments)]
async fn run_one(
    inner: Arc<EngineInner>,
    flow_id: FlowId,
    run_id: FlowRunId,
    flow_name: String,
    server_config_id: i64,
    virtual_server_id: i64,
    definition: FlowDefinition,
    trigger_doc: JsonValue,
    _cross_permit: OwnedSemaphorePermit,
) {
    let start = Instant::now();
    let mut results: Vec<ActionResult> = empty_action_results(&definition.actions);
    let mut overall_status = FlowRunStatus::Ok;
    let mut overall_error: Option<String> = None;

    for (idx, action) in definition.actions.iter().enumerate() {
        let action_start = Instant::now();
        let ctx = ActionContext {
            flow_id,
            run_id,
            flow_name: flow_name.clone(),
            server_config_id,
            virtual_server_id,
            action_index: idx as u32,
            trigger: trigger_doc.clone(),
        };

        // Catch panics in the dispatcher so a misbehaving action does
        // not kill the engine task. `tokio::spawn` propagates panics
        // through the JoinHandle as `Err(JoinError)` — we map that to a
        // synthetic `Errored` outcome and continue.
        let outcome = {
            let inner = inner.clone();
            let action = action.clone();
            let ctx = ctx.clone();
            match tokio::spawn(async move { inner.dispatcher.dispatch(&ctx, &action).await }).await
            {
                Ok(o) => o,
                Err(join_err) => ActionOutcome::Errored(format!("dispatcher panicked: {join_err}")),
            }
        };

        let duration = action_start.elapsed();
        let row = ActionResult {
            index: idx as u32,
            kind: action_kind(action).to_string(),
            status: match &outcome {
                ActionOutcome::Ok => ActionStatus::Ok,
                ActionOutcome::Errored(_) => ActionStatus::Errored,
            },
            duration_ms: duration.as_millis() as u64,
            error: match &outcome {
                ActionOutcome::Ok => None,
                ActionOutcome::Errored(msg) => Some(msg.clone()),
            },
        };
        results[idx] = row;

        if let ActionOutcome::Errored(msg) = outcome {
            overall_status = FlowRunStatus::Errored;
            overall_error = Some(msg);
            // Remaining actions stay as `Skipped` (their default).
            break;
        }
    }

    let total_ms = start.elapsed().as_millis() as u64;
    let finish = bot_flow_runs::FinishRun {
        status: overall_status,
        error: overall_error,
        actionResults: results,
        // v1.1 linear engine — no per-node records (PURA-259).
        nodeResults: Vec::new(),
    };
    if let Err(e) = bot_flow_runs::finish(&inner.db, run_id.0, finish).await {
        tracing::error!(
            error = %e,
            flow.id = flow_id.0,
            run.id = run_id.0,
            "flow run finish: failed to write final state"
        );
    }
    // Cap enforcement runs after every insert; we also nudge after the
    // finish for callers that fire faster than the cap would otherwise
    // notice (e.g. drop-on-busy + manual re-fire bursts).
    if let Err(e) = bot_flow_runs::enforce_per_flow_cap(&inner.db, flow_id.0).await {
        tracing::warn!(error = %e, flow.id = flow_id.0, "flow run cap enforcement failed");
    }

    tracing::info!(
        flow.id = flow_id.0,
        flow.name = %flow_name,
        run.id = run_id.0,
        status = ?overall_status,
        duration_ms = total_ms,
        "flow run finished"
    );
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Ts6Command { .. } => "ts6Command",
        Action::MusicBotCommand { .. } => "musicBotCommand",
        Action::WebhookOut { .. } => "webhookOut",
        Action::LogLine { .. } => "logLine",
    }
}

fn empty_action_results(actions: &[Action]) -> Vec<ActionResult> {
    actions
        .iter()
        .enumerate()
        .map(|(idx, action)| ActionResult {
            index: idx as u32,
            kind: action_kind(action).to_string(),
            status: ActionStatus::Skipped,
            duration_ms: 0,
            error: None,
        })
        .collect()
}

/// Decode the JSON-encoded `bot_flow.flowData` column into the typed
/// [`FlowDefinition`]. Returns `None` on parse failure — engines treat
/// that as "skip the flow with a WARN".
pub(crate) fn parse_definition(raw: &str) -> Option<FlowDefinition> {
    serde_json::from_str(raw).ok()
}

fn spawn_cron_loop(
    handle: FlowEngineHandle,
    flow_id: FlowId,
    parsed: ParsedTrigger,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let ParsedTrigger::Cron(schedule) = parsed else {
            return;
        };
        loop {
            let now = Utc::now();
            let Some(next) = schedule.after(&now).next() else {
                // Cron expression yielded no future tick — exhausted
                // schedules just exit the loop.
                tracing::warn!(flow.id = flow_id.0, "cron schedule produced no future tick");
                return;
            };
            let delta = (next - now).to_std().unwrap_or(Duration::from_secs(1));
            tokio::time::sleep(delta).await;
            // Reload the flow on every fire so PATCHes to `flowData`
            // land without a hot-reload step.
            let flow = match bot_flows::find_by_id(&handle.inner.db, flow_id.0).await {
                Ok(Some(f)) => f,
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!(error = %e, flow.id = flow_id.0, "cron loop: flow read failed");
                    continue;
                }
            };
            let Some(definition) = parse_definition(&flow.flowData) else {
                continue;
            };
            let event = TriggerEvent::Cron { tick: next };
            let _ = handle
                .fire_event(flow_id, &flow.name, &definition, event, false)
                .await;
        }
    })
}

fn spawn_ttl_janitor(db: Arc<Database>, ttl: Duration, interval: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        // Skip the immediate tick — boot already runs the interrupt
        // sweep, and a fresh DB has no rows to prune anyway.
        tick.tick().await;
        loop {
            tick.tick().await;
            let cutoff = Utc::now() - chrono::Duration::from_std(ttl).unwrap_or_default();
            match bot_flow_runs::prune_older_than(&db, cutoff).await {
                Ok(0) => {}
                Ok(n) => tracing::info!(pruned = n, "flow run TTL sweep deleted rows"),
                Err(e) => tracing::warn!(error = %e, "flow run TTL sweep failed"),
            }
        }
    })
}
