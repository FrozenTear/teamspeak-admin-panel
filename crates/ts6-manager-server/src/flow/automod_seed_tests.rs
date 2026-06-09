//! Seed automod rule smoke tests — PURA-304 (Phase 9.1.5).
//!
//! The four starter automod flows ship as importable v2 graph blobs under
//! `docs/flows/automod-seeds/`. They are `include_str!`'d here verbatim, so
//! the files an operator pastes into `POST /api/flows` are the exact same
//! bytes these tests exercise — the docs and the tested artefacts cannot
//! drift.
//!
//! Each test stands up an in-memory SurrealDB, the real [`FlowEngine`] on
//! the production [`ProductionDispatcher`], seeds one starter flow, fires
//! its trigger end-to-end, and asserts the run lands `ok` and the
//! `Moderate` node opened a **shadow-mode** `moderation_case` (status
//! `open`, no TS6 effect). Shadow is the default for every unpromoted rule
//! (automod brief §6.1), so none of the seed rules needs the
//! `AUTOMOD_ENFORCE_RULES` allowlist to stay safe.

#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use ts6_manager_shared::flows::FlowRunStatus;
use ts6_manager_shared::flows::v2::decode_flow_data;

use super::dispatch::ProductionDispatcher;
use super::engine::graph::{validate_expressions, validate_graph};
use super::engine::{EngineDeps, FlowEngine};
use crate::app_state::AppState;
use crate::control::ControlBackendPool;
use crate::db::{Database, connect_in_memory, migrations};
use crate::repos::{
    bot_flow_runs, bot_flows, moderation_case_actions, moderation_cases, server_connections,
};
use crate::webquery::WebQueryPool;
use crate::ws::Hub;

// The four starter automod flows, as the operator-facing seed files.
const BAD_NAME_KICK: &str = include_str!("../../../../docs/flows/automod-seeds/bad-name-kick.json");
const CHAT_FILTER_WARN: &str =
    include_str!("../../../../docs/flows/automod-seeds/chat-filter-warn.json");
const CONNECT_FLOOD_BAN: &str =
    include_str!("../../../../docs/flows/automod-seeds/connect-flood-ban.json");
const CHANNEL_HOP_MUTE: &str =
    include_str!("../../../../docs/flows/automod-seeds/channel-hop-mute.json");

const SEEDS: &[(&str, &str)] = &[
    ("bad-name-kick", BAD_NAME_KICK),
    ("chat-filter-warn", CHAT_FILTER_WARN),
    ("connect-flood-ban", CONNECT_FLOOD_BAN),
    ("channel-hop-mute", CHANNEL_HOP_MUTE),
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Fresh in-memory DB with the chapter-4 schema applied.
async fn fresh_db() -> Arc<Database> {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations");
    db
}

/// Build a minimal [`AppState`] — enough to construct the production
/// dispatcher. The shadow-mode `Moderate` path is pure-DB (no TS6
/// round-trip), so the control/webquery pools are never exercised.
fn test_app(db: Arc<Database>) -> AppState {
    AppState {
        db: db.clone(),
        jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
        jwt_access_expiry: Duration::from_secs(900),
        jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
        setup_lock: Arc::new(Mutex::new(())),
        webquery: WebQueryPool::new(false),
        control: ControlBackendPool::new(false, db),
        ws_hub: Hub::new(),
        widget_cache: crate::widgets::WidgetCache::new(),
        music_bots: crate::music_bots::MusicBotService::default_for_tests(),
        sidecar: None,
        ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
        moq_public_url: None,
        yt_cookie: Arc::new(RwLock::new(None)),
        yt_api_key: Arc::new(RwLock::new(None)),
        data_dir: PathBuf::from("./data"),
        trusted_proxy_hops: 0,
    }
}

/// Seed one `server_connection` and return its id.
async fn seed_server(db: &Database) -> i64 {
    server_connections::insert(
        db,
        server_connections::NewServerConnection {
            name: "primary".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .expect("seed server")
    .id
}

/// Boot the schema, build the engine on the production dispatcher, and seed
/// one enabled starter flow from its raw v2 `flowData` blob. The flow's
/// trigger subscription is registered via [`enable`], so a producer event
/// fans into it.
async fn boot_seed(flow_data: &str) -> (Arc<Database>, FlowEngine, i64) {
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let flow_id = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "automod-seed".into(),
            description: None,
            flowData: flow_data.to_string(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .expect("seed flow")
    .id;

    let app = test_app(db.clone());
    let dispatcher = Arc::new(ProductionDispatcher::new(&app));
    let engine = FlowEngine::start(EngineDeps::new(db.clone(), dispatcher))
        .await
        .expect("engine start");
    engine
        .handle()
        .enable(ts6_manager_shared::flows::FlowId(flow_id))
        .await
        .expect("register trigger subscription");
    (db, engine, flow_id)
}

/// Poll until a run row for `flow_id` reaches a terminal state.
async fn wait_for_run(db: &Database, flow_id: i64) -> bot_flow_runs::BotFlowRun {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let runs = bot_flow_runs::list_for_flow(db, flow_id, 25, None)
            .await
            .expect("list runs");
        if let Some(run) = runs
            .into_iter()
            .find(|r| !matches!(r.status, FlowRunStatus::InFlight))
        {
            return run;
        }
        if Instant::now() >= deadline {
            panic!("flow {flow_id} produced no terminal run");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Assert the subject carries exactly one shadow-mode automod case for
/// `rule_key` with one timeline action of `action_kind`.
async fn assert_shadow_case(
    db: &Database,
    subject_uid: &str,
    rule_key: &str,
    flow_id: i64,
    action_kind: &str,
) {
    let cases = moderation_cases::list_for_subject(db, subject_uid)
        .await
        .expect("list cases");
    assert_eq!(cases.len(), 1, "exactly one automod case for {subject_uid}");
    let case = &cases[0];
    assert_eq!(case.origin, "automod");
    assert_eq!(
        case.status, "open",
        "a shadow-mode action records the case but applies no effect"
    );
    assert_eq!(
        case.originRef.as_deref(),
        Some(format!("{rule_key}:{flow_id}").as_str())
    );

    let actions = moderation_case_actions::list_for_case(db, case.id)
        .await
        .expect("list actions");
    assert_eq!(actions.len(), 1, "one timeline action per trigger");
    assert_eq!(actions[0].actionKind, action_kind);
    let payload = actions[0].payload.as_ref().expect("action payload");
    assert_eq!(payload["ruleKey"], rule_key);
    assert_eq!(payload["mode"], "shadow", "seed rules run in shadow mode");
    assert_eq!(
        payload["shadowReason"], "ruleShadowMode",
        "shadow is the unpromoted-rule default, not a kill-switch/breaker trip"
    );
}

// ---------------------------------------------------------------------------
// Validation — every seed graph loads and validates clean
// ---------------------------------------------------------------------------

#[test]
fn all_seed_graphs_load_and_validate_clean() {
    for (name, blob) in SEEDS {
        let graph = decode_flow_data(blob)
            .unwrap_or_else(|e| panic!("seed `{name}` failed to decode: {e}"));
        let report = validate_graph(&graph);
        assert!(
            report.errors.is_empty(),
            "seed `{name}` has structural errors: {:?}",
            report.errors
        );
        let expr_errors = validate_expressions(&graph);
        assert!(
            expr_errors.is_empty(),
            "seed `{name}` has expression errors: {expr_errors:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// bad-name kick — ts6ClientJoined -> branch -> Moderate(kick)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_name_kick_opens_shadow_case_for_a_flagged_name() {
    let (db, engine, flow_id) = boot_seed(BAD_NAME_KICK).await;

    engine
        .handle()
        .on_client_joined(1, 10, "uid-badname=".into(), "FREE NITRO giveaway".into())
        .await;

    let run = wait_for_run(&db, flow_id).await;
    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_shadow_case(
        &db,
        "uid-badname=",
        "automod-bad-name-kick",
        flow_id,
        "kick",
    )
    .await;
}

#[tokio::test]
async fn bad_name_kick_ignores_a_clean_name() {
    // The branch routes a non-matching name to the unwired `default` port,
    // so the `Moderate` node is pruned and no case is opened.
    let (db, engine, flow_id) = boot_seed(BAD_NAME_KICK).await;

    engine
        .handle()
        .on_client_joined(1, 10, "uid-friendly=".into(), "Friendly Newcomer".into())
        .await;

    let run = wait_for_run(&db, flow_id).await;
    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    let cases = moderation_cases::list_for_subject(&db, "uid-friendly=")
        .await
        .expect("list cases");
    assert!(cases.is_empty(), "a clean display name opens no case");
}

// ---------------------------------------------------------------------------
// chat-filter warn — ts6ChatMessage -> branch -> Moderate(warn)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_filter_warn_opens_shadow_case_for_a_blocked_phrase() {
    let (db, engine, flow_id) = boot_seed(CHAT_FILTER_WARN).await;

    engine
        .handle()
        .on_chat_message(
            1,
            Some(10),
            "2".into(),
            "uid-spammer=".into(),
            "Spammer".into(),
            "best place to buy followers cheap".into(),
        )
        .await;

    let run = wait_for_run(&db, flow_id).await;
    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_shadow_case(
        &db,
        "uid-spammer=",
        "automod-chat-filter-warn",
        flow_id,
        "warn",
    )
    .await;
}

// ---------------------------------------------------------------------------
// connect-flood temp-ban — ts6Flood{clientJoined} -> Moderate(ban)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connect_flood_ban_opens_shadow_case_after_threshold() {
    let (db, engine, flow_id) = boot_seed(CONNECT_FLOOD_BAN).await;

    // The seed threshold is 5 reconnects in 30s; the 5th crosses it.
    for _ in 0..5 {
        engine
            .handle()
            .on_client_joined(1, 10, "uid-flooder=".into(), "Flooder".into())
            .await;
    }

    let run = wait_for_run(&db, flow_id).await;
    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_shadow_case(
        &db,
        "uid-flooder=",
        "automod-connect-flood-ban",
        flow_id,
        "ban",
    )
    .await;
}

// ---------------------------------------------------------------------------
// channel-hop mute — ts6Flood{clientMoved} -> Moderate(mute)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn channel_hop_mute_opens_shadow_case_after_threshold() {
    let (db, engine, flow_id) = boot_seed(CHANNEL_HOP_MUTE).await;

    // The seed threshold is 6 channel switches in 15s; the 6th crosses it.
    for _ in 0..6 {
        engine
            .handle()
            .on_client_moved(1, "uid-hopper=".into())
            .await;
    }

    let run = wait_for_run(&db, flow_id).await;
    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_shadow_case(
        &db,
        "uid-hopper=",
        "automod-channel-hop-mute",
        flow_id,
        "mute",
    )
    .await;
}
