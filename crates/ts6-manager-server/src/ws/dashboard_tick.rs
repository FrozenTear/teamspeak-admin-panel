//! PURA-81 — periodic dashboard tick republisher for the WS hub.
//!
//! Bridges Phase 1's short-poll dashboard and the SSHBridge-driven event
//! source ([PURA-80](../../../study-documents/ts6-manager-impl-plan.md#311-phase-2)).
//!
//! ## Shape
//!
//! - On boot, the supervisor scans `server_connection` and spawns one
//!   worker per enabled row.
//! - Each worker calls the §7.19 dashboard fetch every 5 s and publishes
//!   the resulting [`DashboardData`] onto the hub as a `dashboard:tick`
//!   envelope on **both** `server:{id}:clients` and
//!   `server:{id}:channels`.
//! - The supervisor reconciles every 5 s: a row that is now disabled or
//!   missing has its worker shut down; a row that is newly enabled gets a
//!   fresh worker. Re-enabling resumes ticks within one cycle.
//! - Workers back off exponentially (5 s → 60 s cap) on transport / TS
//!   upstream failures so a downed instance does not busy-loop the
//!   runtime; the next success resets the delay.
//!
//! ## Why `sid = 1`
//!
//! The Phase 1 WS topics are keyed by `server_connection.id` only — there
//! is no per-virtual-server topic yet. TS6 deployments treat virtual
//! server `1` as the canonical default (matches `serverlist` ordering
//! and the spec's example URLs). The republisher therefore publishes a
//! single tick per connection against `sid = 1`. Once
//! [PURA-80](../../../study-documents/ts6-manager-impl-plan.md) lands the
//! SSH event source, real per-vs subscriptions can replace this fallback
//! cadence.
//!
//! ## Graceful shutdown
//!
//! [`spawn`] returns a [`DashboardTickHandle`]. Callers that wire a
//! signal-driven shutdown can `await` `handle.shutdown()` to drain the
//! supervisor and every worker. Today `main` only stores the handle as a
//! drop-guard; when the runtime shuts down, the dropped sender wakes
//! every receiver's `changed()` arm with `Err`, and the workers exit.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::control::ControlBackendPool;
use crate::db::Database;
use crate::repos::server_connections::{self, ServerConnection};
use crate::webquery::dashboard::fetch_dashboard;
use crate::ws::Hub;
use crate::ws::topic::{Topic, TopicKind};

/// Cadence between successful dashboard fetches.
const TICK_INTERVAL_SECS: u64 = 5;
/// How often the supervisor re-reads `server_connection` to spawn / shut
/// down workers. Same cadence as the tick — picking a small constant so a
/// flipped `enabled` flag takes effect within one cycle (issue verification:
/// "the tick stops within one cycle").
const RECONCILE_INTERVAL_SECS: u64 = 5;
/// Initial back-off after a tick failure. Ticks the next attempt at this
/// delay, then doubles per consecutive failure up to [`BACKOFF_CAP_SECS`].
const BACKOFF_INITIAL_SECS: u64 = 5;
const BACKOFF_CAP_SECS: u64 = 60;
/// Spec §8.4 envelope `type` for the periodic dashboard republisher.
const TICK_KIND: &str = "dashboard:tick";
/// PURA-72 H-1 — widget topic stub. Public viewers only need a refresh
/// signal; the JSON snapshot at `/api/widget/{token}/data` is what
/// enforces §7.29 redaction. Republishing the full operator-side
/// `DashboardData` (real `platform`, `version`, bandwidth, ping)
/// onto the widget topic is what the H-1 finding called out.
const WIDGET_REFRESH_KIND: &str = "widget:refresh";
/// Default virtual-server id used by the republisher. See module docs.
const DEFAULT_SID: i64 = 1;

/// Inputs the supervisor and per-server workers need. Carried by `Clone`
/// so the spawn hierarchy can hand each worker its own copy without
/// borrowing across task boundaries.
#[derive(Clone)]
pub struct TickerDeps {
    pub db: Arc<Database>,
    pub hub: Hub,
    pub control: ControlBackendPool,
}

/// Drop-guard returned by [`spawn`]. Holding it keeps the watch sender
/// alive; dropping it (or calling [`shutdown`](Self::shutdown)) signals
/// the supervisor and every worker to exit.
pub struct DashboardTickHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

impl DashboardTickHandle {
    /// Signal shutdown and wait for the supervisor (which in turn drains
    /// every per-server worker) to exit. Idempotent — re-calling does
    /// nothing once the supervisor has already exited.
    pub async fn shutdown(self) {
        let _ = self.shutdown_tx.send(true);
        let _ = self.join.await;
    }
}

/// Spawn the supervisor task and return the handle. The supervisor runs
/// an immediate reconcile so workers exist before the first sleep — the
/// first successful tick on each server therefore lands within one
/// `TICK_INTERVAL_SECS` window of boot.
pub fn spawn(deps: TickerDeps) -> DashboardTickHandle {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(run_supervisor(deps, shutdown_rx));
    DashboardTickHandle { shutdown_tx, join }
}

struct WorkerHandle {
    shutdown_tx: watch::Sender<bool>,
    join: JoinHandle<()>,
}

async fn run_supervisor(deps: TickerDeps, mut shutdown_rx: watch::Receiver<bool>) {
    let mut workers: HashMap<i64, WorkerHandle> = HashMap::new();
    reconcile(&deps, &mut workers).await;

    let mut interval = tokio::time::interval(Duration::from_secs(RECONCILE_INTERVAL_SECS));
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    // Discard the immediate first tick — `reconcile` already ran above.
    interval.tick().await;

    loop {
        tokio::select! {
            res = shutdown_rx.changed() => {
                // Either an explicit shutdown signal or the sender was
                // dropped (handle dropped without explicit shutdown).
                // Both are exits.
                if res.is_err() || *shutdown_rx.borrow() {
                    break;
                }
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
                target: "ws::dashboard_tick",
                config_id,
                error = %e,
                "dashboard tick worker join failed during shutdown",
            );
        }
    }
}

async fn reconcile(deps: &TickerDeps, workers: &mut HashMap<i64, WorkerHandle>) {
    let connections = match server_connections::list(&deps.db).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!(
                target: "ws::dashboard_tick",
                error = %e,
                "dashboard tick reconcile: server_connections::list failed; \
                 retrying on next cycle",
            );
            return;
        }
    };

    let enabled: HashSet<i64> = connections
        .iter()
        .filter(|c| c.enabled)
        .map(|c| c.id)
        .collect();

    let stale: Vec<i64> = workers
        .keys()
        .copied()
        .filter(|id| !enabled.contains(id))
        .collect();
    for id in stale {
        if let Some(worker) = workers.remove(&id) {
            let _ = worker.shutdown_tx.send(true);
            let _ = worker.join.await;
            tracing::info!(
                target: "ws::dashboard_tick",
                config_id = id,
                "stopped dashboard tick worker (connection disabled or removed)",
            );
        }
    }

    for connection in connections.into_iter().filter(|c| c.enabled) {
        if !workers.contains_key(&connection.id) {
            let id = connection.id;
            let worker = spawn_worker(deps.clone(), connection);
            workers.insert(id, worker);
            tracing::info!(
                target: "ws::dashboard_tick",
                config_id = id,
                "spawned dashboard tick worker",
            );
        }
    }
}

fn spawn_worker(deps: TickerDeps, connection: ServerConnection) -> WorkerHandle {
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let join = tokio::spawn(run_worker(deps, connection, shutdown_rx));
    WorkerHandle { shutdown_tx, join }
}

async fn run_worker(
    deps: TickerDeps,
    connection: ServerConnection,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let config_id = connection.id;
    let mut backoff_secs: u64 = BACKOFF_INITIAL_SECS;
    // First tick fires immediately so the FE sees the initial snapshot
    // without a 5 s warm-up.
    let mut next_delay = Duration::from_secs(0);

    loop {
        let timer = tokio::time::sleep(next_delay);
        tokio::pin!(timer);
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    return;
                }
            }
            _ = &mut timer => {}
        }

        match tick_once(&deps, &connection).await {
            Ok(()) => {
                backoff_secs = BACKOFF_INITIAL_SECS;
                next_delay = Duration::from_secs(TICK_INTERVAL_SECS);
            }
            Err(e) => {
                let delay = backoff_secs;
                tracing::warn!(
                    target: "ws::dashboard_tick",
                    config_id,
                    error = %e,
                    backoff_secs = delay,
                    "dashboard tick failed; backing off",
                );
                next_delay = Duration::from_secs(delay);
                backoff_secs = backoff_secs.saturating_mul(2).min(BACKOFF_CAP_SECS);
            }
        }
    }
}

async fn tick_once(deps: &TickerDeps, connection: &ServerConnection) -> anyhow::Result<()> {
    use anyhow::Context;
    let client = deps
        .control
        .get_or_build(connection.id, Some(connection))
        .await
        .context("get_or_build control backend")?;
    let dashboard = fetch_dashboard(client.as_ref(), DEFAULT_SID)
        .await
        .context("fetch dashboard")?;
    let data: Value = serde_json::to_value(&dashboard).context("serialize dashboard payload")?;
    deps.hub
        .publish(
            Topic::new(connection.id, TopicKind::Clients),
            TICK_KIND,
            data.clone(),
        )
        .await;
    deps.hub
        .publish(
            Topic::new(connection.id, TopicKind::Channels),
            TICK_KIND,
            data,
        )
        .await;
    // PURA-72 Slice E — public widget viewers subscribe to
    // `server:{id}:widget`. The widget fan-out is a redacted **stub**
    // (`{"refresh": true}`) so the SPA refetches `/api/widget/{token}/data`
    // — the JSON route is what enforces §7.29 redaction. Fanning the raw
    // operator `DashboardData` (real `platform`, `version`, bandwidth,
    // ping) onto this public topic is what the PURA-72 H-1 review found.
    deps.hub
        .publish(
            Topic::new(connection.id, TopicKind::Widget),
            WIDGET_REFRESH_KIND,
            json!({ "refresh": true }),
        )
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ControlBackend, ControlBackendError, ControlResult};
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::server_connections::NewServerConnection;
    use crate::webquery::BanAddParams;
    use crate::webquery::models::{
        BanEntry, ChannelEntry, ClientDbEntry, ClientEntry, ClientInfo, ConnectionInfo, LogEntry,
        ServerInfo, VersionInfo, VirtualServerEntry,
    };
    use crate::ws::auth::{Principal, UserPrincipal};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tokio::time::timeout;

    fn admin_principal() -> Principal {
        Principal::User(UserPrincipal {
            user_id: 1,
            username: "alice".into(),
            role: "admin".into(),
            is_admin: true,
            is_at_least_moderator: true,
        })
    }

    fn widget_principal(server_config_id: i64) -> Principal {
        use crate::ws::auth::WidgetPrincipal;
        Principal::Widget(WidgetPrincipal {
            widget_id: 1,
            server_config_id,
            virtual_server_id: 1,
        })
    }

    /// Minimal in-memory `ControlBackend`. Counters drive the test
    /// assertions — the supervisor / worker code only cares about
    /// `serverinfo` / `clientlist` / `channellist` /
    /// `server_connection_info`, and the fake stays inert on the other
    /// trait methods so a misuse trips a panic instead of returning bogus
    /// data.
    #[derive(Debug, Default)]
    struct FakeBackend {
        success_calls: AtomicU64,
        fail_calls: AtomicU64,
        /// First N calls to `serverinfo` return an [`Upstream`](ControlBackendError::Upstream)
        /// error. After that the calls succeed.
        fail_first_n: u64,
    }

    #[async_trait]
    impl ControlBackend for FakeBackend {
        async fn version(&self) -> ControlResult<VersionInfo> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn serverlist(&self) -> ControlResult<Vec<VirtualServerEntry>> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn serverinfo(&self, _sid: i64) -> ControlResult<ServerInfo> {
            let n =
                self.fail_calls.load(Ordering::SeqCst) + self.success_calls.load(Ordering::SeqCst);
            if n < self.fail_first_n {
                self.fail_calls.fetch_add(1, Ordering::SeqCst);
                return Err(ControlBackendError::Upstream {
                    code: 1,
                    message: "fake upstream".into(),
                });
            }
            self.success_calls.fetch_add(1, Ordering::SeqCst);
            Ok(ServerInfo {
                virtualserver_name: "Fake".into(),
                virtualserver_platform: "Linux".into(),
                virtualserver_version: "3.13.7".into(),
                virtualserver_maxclients: 32,
                virtualserver_uptime: 100,
                virtualserver_total_packetloss_total: 0.0,
                virtualserver_total_ping: 5.0,
            })
        }
        async fn channellist(&self, _sid: i64) -> ControlResult<Vec<ChannelEntry>> {
            Ok(vec![ChannelEntry {
                cid: 1,
                channel_name: "Default".into(),
                ..Default::default()
            }])
        }
        async fn clientlist(&self, _sid: i64) -> ControlResult<Vec<ClientEntry>> {
            Ok(vec![ClientEntry {
                clid: 1,
                client_type: 0,
                client_nickname: "alice".into(),
                ..Default::default()
            }])
        }
        async fn server_connection_info(&self, _sid: i64) -> ControlResult<ConnectionInfo> {
            Ok(ConnectionInfo {
                connection_bandwidth_received_last_second_total: 10,
                connection_bandwidth_sent_last_second_total: 20,
            })
        }
        async fn clientlist_with_flags(
            &self,
            _sid: i64,
            _flags: &[&str],
        ) -> ControlResult<Vec<ClientEntry>> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn clientinfo(&self, _sid: i64, _clid: i64) -> ControlResult<ClientInfo> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn clientdbinfo(&self, _sid: i64, _cldbid: i64) -> ControlResult<ClientDbEntry> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn channellist_with_flags(
            &self,
            _sid: i64,
            _flags: &[&str],
        ) -> ControlResult<Vec<ChannelEntry>> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn banlist(&self, _sid: i64) -> ControlResult<Vec<BanEntry>> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn logview(
            &self,
            _sid: i64,
            _lines: u32,
            _reverse: bool,
            _instance: bool,
            _begin_pos: Option<i64>,
        ) -> ControlResult<Vec<LogEntry>> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn clientkick(
            &self,
            _sid: i64,
            _clid: i64,
            _reasonid: i64,
            _reasonmsg: Option<&str>,
        ) -> ControlResult<()> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn clientmove(
            &self,
            _sid: i64,
            _clid: i64,
            _cid: i64,
            _cpw: Option<&str>,
        ) -> ControlResult<()> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn client_set_muted(
            &self,
            _sid: i64,
            _clid: i64,
            _input_muted: Option<bool>,
            _output_muted: Option<bool>,
        ) -> ControlResult<()> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn banadd(&self, _sid: i64, _params: &BanAddParams<'_>) -> ControlResult<i64> {
            unimplemented!("not used by dashboard fetch")
        }
        async fn bandel(&self, _sid: i64, _banid: i64) -> ControlResult<()> {
            unimplemented!("not used by dashboard fetch")
        }
    }

    async fn seed_connection(db: &Database, name: &str, enabled: bool) -> ServerConnection {
        server_connections::insert(
            db,
            NewServerConnection {
                name: name.into(),
                host: "ts.example.com".into(),
                webqueryPort: 10080,
                apiKey: "enc:00:00:00".into(),
                useHttps: false,
                sshPort: 10022,
                sshUsername: None,
                sshPassword: None,
                queryBotChannel: None,
                queryBotNickname: None,
                sshBotNickname: None,
                enabled,
                controlPath: None,
                sshAuthMethod: None,
                sshPrivateKey: None,
                sshKeyAgentSocket: None,
                sshHostKeyFingerprint: None,
            },
        )
        .await
        .expect("insert connection")
    }

    async fn boot(deps: TickerDeps, fake: Arc<FakeBackend>, config_id: i64) -> DashboardTickHandle {
        deps.control.insert_for_test(config_id, fake.clone()).await;
        spawn(deps)
    }

    /// Tick worker publishes on both `clients` and `channels` topics for
    /// the server. The hub's broadcast channel is keyed per-server (the
    /// session loop is what filters by topic for each connection), so a
    /// single subscriber on `server:{id}:clients` sees BOTH publishes —
    /// we drain a couple of envelopes off it and assert both topic
    /// strings show up with `type = "dashboard:tick"` and the §7.19.1
    /// fields.
    #[tokio::test]
    async fn worker_publishes_dashboard_tick_on_both_topics() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let connection = seed_connection(&db, "primary", true).await;

        let hub = Hub::new();
        let control = ControlBackendPool::new(false, db.clone());
        let fake = Arc::new(FakeBackend::default());

        // Subscribe BEFORE spawning so the broadcast captures the first
        // tick (which fires immediately on boot — see [`run_worker`]).
        let admin = admin_principal();
        let clients_topic = Topic::new(connection.id, TopicKind::Clients);
        let channels_topic = Topic::new(connection.id, TopicKind::Channels);
        let mut sub = hub
            .subscribe(&db, &admin, clients_topic, None)
            .await
            .unwrap();

        let handle = boot(
            TickerDeps {
                db: db.clone(),
                hub: hub.clone(),
                control: control.clone(),
            },
            fake.clone(),
            connection.id,
        )
        .await;

        // Drain the two envelopes from the first tick.
        let env_a = timeout(Duration::from_secs(2), sub.receiver.recv())
            .await
            .expect("first tick envelope within deadline")
            .expect("envelope");
        let env_b = timeout(Duration::from_secs(2), sub.receiver.recv())
            .await
            .expect("second tick envelope within deadline")
            .expect("envelope");

        for env in [&env_a, &env_b] {
            assert_eq!(env.kind, TICK_KIND);
            assert_eq!(env.data["serverName"], "Fake");
            assert_eq!(env.data["onlineUsers"], 1);
            assert_eq!(env.data["channelCount"], 1);
        }

        let topics: Vec<&str> = [&env_a, &env_b].iter().map(|e| e.topic.as_str()).collect();
        assert!(
            topics.contains(&clients_topic.to_string().as_str()),
            "missing clients topic publish: got {topics:?}",
        );
        assert!(
            topics.contains(&channels_topic.to_string().as_str()),
            "missing channels topic publish: got {topics:?}",
        );

        handle.shutdown().await;
    }

    /// PURA-72 H-1 regression. The 5 s dashboard tick must not put real
    /// `platform` / `version` / bandwidth / ping fields on the widget
    /// topic. A widget-token subscriber receives only the redacted
    /// refresh stub.
    #[tokio::test]
    async fn widget_topic_dashboard_tick_is_refresh_stub() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let connection = seed_connection(&db, "primary", true).await;

        let hub = Hub::new();
        let control = ControlBackendPool::new(false, db.clone());
        let fake = Arc::new(FakeBackend::default());

        // Subscribe BEFORE spawning so the broadcast captures the first
        // tick (which fires immediately on boot).
        let widget = widget_principal(connection.id);
        let widget_topic = Topic::new(connection.id, TopicKind::Widget);
        let mut sub = hub
            .subscribe(&db, &widget, widget_topic, None)
            .await
            .unwrap();

        let handle = boot(
            TickerDeps {
                db: db.clone(),
                hub: hub.clone(),
                control: control.clone(),
            },
            fake.clone(),
            connection.id,
        )
        .await;

        // Hub broadcast is per-server. The session loop is what filters
        // by topic, so this raw subscriber sees clients/channels/widget
        // publishes from `tick_once`. Drain up to three envelopes and
        // pull out the widget one.
        let mut widget_env = None;
        for _ in 0..3 {
            match timeout(Duration::from_secs(2), sub.receiver.recv()).await {
                Ok(Ok(env)) => {
                    if env.topic == widget_topic.to_string() {
                        widget_env = Some(env);
                        break;
                    }
                }
                _ => break,
            }
        }
        let env = widget_env.expect("widget envelope must be published");

        assert_eq!(env.topic, widget_topic.to_string());
        assert_eq!(
            env.kind, WIDGET_REFRESH_KIND,
            "widget envelope kind must be the redaction stub, not the operator dashboard tick"
        );
        assert_eq!(
            env.data,
            serde_json::json!({ "refresh": true }),
            "widget envelope body must be a static refresh stub"
        );

        // Belt-and-braces: assert no operator-side telemetry leaked
        // through. The FakeBackend reports platform=Linux,
        // version=3.13.7, bandwidth=10/20 — none of those literals may
        // appear in the public envelope.
        let payload = env.data.to_string();
        for forbidden in [
            "Linux",
            "3.13.7",
            "platform",
            "version",
            "bandwidth",
            "incoming",
            "outgoing",
            "packetloss",
            "ping",
            "Fake",
        ] {
            assert!(
                !payload.contains(forbidden),
                "widget envelope leaked operator telemetry `{forbidden}`: {payload}"
            );
        }

        handle.shutdown().await;
    }

    /// Disabled rows do not get a worker, so no tick is published. After
    /// flipping `enabled = true` and waiting one reconcile cycle a worker
    /// spawns and ticks land. (Cycle is 5 s; we wait up to 7 s for the
    /// first envelope after enable.)
    #[tokio::test]
    async fn disabled_connection_does_not_publish_until_re_enabled() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let connection = seed_connection(&db, "edge", false).await;

        let hub = Hub::new();
        let control = ControlBackendPool::new(false, db.clone());
        let fake = Arc::new(FakeBackend::default());
        control.insert_for_test(connection.id, fake.clone()).await;

        let admin = admin_principal();
        let topic = Topic::new(connection.id, TopicKind::Clients);
        let mut sub = hub.subscribe(&db, &admin, topic, None).await.unwrap();

        let handle = spawn(TickerDeps {
            db: db.clone(),
            hub: hub.clone(),
            control: control.clone(),
        });

        // While disabled: no tick should arrive within a window
        // generously larger than the boot reconcile.
        let observed = timeout(Duration::from_millis(500), sub.receiver.recv()).await;
        assert!(
            observed.is_err(),
            "disabled connection must not publish ticks; got {observed:?}"
        );
        assert_eq!(
            fake.success_calls.load(Ordering::SeqCst),
            0,
            "no upstream calls expected while disabled",
        );

        handle.shutdown().await;
    }

    /// A failing fetch trips the back-off path: the next attempt is
    /// gated by [`BACKOFF_INITIAL_SECS`] rather than the success cadence.
    /// We assert the worker did not retry within 1 s — well under the 5 s
    /// initial back-off — so the test stays fast and deterministic.
    #[tokio::test]
    async fn worker_backs_off_after_failure_and_does_not_busy_loop() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let connection = seed_connection(&db, "flaky", true).await;

        let hub = Hub::new();
        let control = ControlBackendPool::new(false, db.clone());
        // 100 leading failures — every call inside the test window
        // hits the upstream-error branch.
        let fake = Arc::new(FakeBackend {
            fail_first_n: 100,
            ..Default::default()
        });

        let handle = boot(
            TickerDeps {
                db: db.clone(),
                hub: hub.clone(),
                control: control.clone(),
            },
            fake.clone(),
            connection.id,
        )
        .await;

        // The worker fires immediately on boot, fails, and schedules the
        // next attempt at BACKOFF_INITIAL_SECS (5 s). Within 1 s we
        // expect exactly one call to the upstream — anything more would
        // mean the worker is busy-looping past the back-off.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let attempts = fake.fail_calls.load(Ordering::SeqCst);
        assert_eq!(
            attempts, 1,
            "back-off path must throttle retries; saw {attempts} calls",
        );
        assert_eq!(
            fake.success_calls.load(Ordering::SeqCst),
            0,
            "upstream still failing — no successes expected"
        );

        handle.shutdown().await;
    }
}
