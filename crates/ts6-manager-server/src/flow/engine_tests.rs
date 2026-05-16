//! Graph-engine acceptance tests — PURA-266 (`docs/flows/v2/architecture.md`
//! §5–§6).
//!
//! Each test stands up an in-memory SurrealDB, runs the chapter-4 migration
//! set, seeds a `server_connection` + one or more `bot_flow` rows, then
//! boots [`FlowEngine`] with a test dispatcher and asserts on the persisted
//! `bot_flow_run.nodeResults`.
//!
//! There is **one engine**: a legacy v1.1 linear flow is loaded through the
//! projection shim into a degenerate path graph and run by the same v2
//! topological scheduler (§5.4). The first two tests are the v1.1
//! serial-execution cases ported as path-graph assertions.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ts6_manager_shared::flows::v2::{NodeResult, NodeStatus};
use ts6_manager_shared::flows::{Action, FlowDefinition, FlowId, FlowRunStatus, Trigger};

use super::engine::{
    ActionContext, ActionDispatcher, ActionOutcome, BasicDispatcher, EngineDeps, FireError,
    FlowEngine,
};
use crate::db::{Database, connect_in_memory, migrations};
use crate::repos::{bot_flow_runs, bot_flows, server_connections};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// Fresh in-memory DB with the chapter-4 schema applied.
async fn fresh_db() -> Arc<Database> {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations");
    db
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

/// Seed one `bot_flow` from a raw `flowData` blob (legacy or v2 envelope).
async fn seed_flow(db: &Database, server_id: i64, flow_data: String, enabled: bool) -> i64 {
    bot_flows::insert(
        db,
        bot_flows::NewBotFlow {
            name: "test-flow".into(),
            description: None,
            flowData: flow_data,
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled,
        },
    )
    .await
    .expect("seed flow")
    .id
}

/// Boot the engine with the given dispatcher.
async fn start_engine(db: Arc<Database>, dispatcher: Arc<dyn ActionDispatcher>) -> FlowEngine {
    let deps = EngineDeps {
        db,
        dispatcher,
        max_parallel_runs: 4,
        run_ttl: Duration::from_secs(30 * 86_400),
        ttl_sweep_interval: Duration::from_secs(3_600),
    };
    FlowEngine::start(deps).await.expect("engine start")
}

/// Boot the schema + seed one server + one flow from a v1.1
/// [`FlowDefinition`], then boot the engine.
async fn boot(
    dispatcher: Arc<dyn ActionDispatcher>,
    definition: FlowDefinition,
    enabled: bool,
) -> (Arc<Database>, FlowEngine, FlowId) {
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let flow_data = serde_json::to_string(&definition).expect("encode definition");
    let flow_id = seed_flow(&db, server_id, flow_data, enabled).await;
    let engine = start_engine(db.clone(), dispatcher).await;
    (db, engine, FlowId(flow_id))
}

/// Wait until the run row reaches a terminal state, or fail.
async fn wait_terminal(db: &Database, run_id: i64) -> bot_flow_runs::BotFlowRun {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let run = bot_flow_runs::find_by_id(db, run_id)
            .await
            .expect("read run")
            .expect("run row exists");
        if !matches!(run.status, FlowRunStatus::InFlight) {
            return run;
        }
        if std::time::Instant::now() >= deadline {
            panic!("run {run_id} never reached terminal state");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Look up one node's run record by id.
fn node<'a>(run: &'a bot_flow_runs::BotFlowRun, id: &str) -> &'a NodeResult {
    run.nodeResults
        .iter()
        .find(|n| n.node_id.0 == id)
        .unwrap_or_else(|| {
            let ids: Vec<&String> = run.nodeResults.iter().map(|n| &n.node_id.0).collect();
            panic!("no nodeResult for `{id}` — have {ids:?}")
        })
}

/// `manualFire` context as the optional JSON map [`FlowEngineHandle::fire`]
/// accepts.
fn ctx(value: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(value.as_object().cloned().expect("context is an object"))
}

// ---------------------------------------------------------------------------
// v1.1 serial cases — ported as path-graph assertions (§5.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn legacy_linear_flow_runs_as_a_path_graph() {
    // A v1.1 linear flow is projected `trigger -> action_0` and run by the
    // v2 scheduler; the run records per-node results, not actionResults.
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![Action::LogLine {
            message: "hello world".into(),
        }],
    };
    let (db, engine, flow_id) = boot(Arc::new(BasicDispatcher), definition, true).await;

    let run_id = engine.handle().fire(flow_id, None).await.expect("fire ok");
    let run = wait_terminal(&db, run_id.0).await;

    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    // v2 runs leave `actionResults` empty and populate `nodeResults`.
    assert!(run.actionResults.is_empty());
    assert_eq!(run.nodeResults.len(), 2, "trigger + one action node");
    assert!(matches!(node(&run, "trigger").status, NodeStatus::Ok));
    let action = node(&run, "action_0");
    assert!(matches!(action.status, NodeStatus::Ok));
    assert_eq!(action.kind, "action");
    assert!(run.error.is_none());
    assert!(run.finishedAt.is_some());
}

#[tokio::test]
async fn legacy_first_errored_action_aborts_run_and_skips_remainder() {
    // Dispatcher: the action carrying the message "b" errors; "a" and "c"
    // would succeed. Under the path-graph projection the error prunes the
    // `out` edge so the downstream node settles `skipped`, not `errored`.
    #[derive(Default)]
    struct StepDispatcher {
        seen: AtomicUsize,
    }
    #[async_trait]
    impl ActionDispatcher for StepDispatcher {
        async fn dispatch(&self, _ctx: &ActionContext, action: &Action) -> ActionOutcome {
            self.seen.fetch_add(1, Ordering::SeqCst);
            match action {
                Action::LogLine { message } if message == "b" => {
                    ActionOutcome::Errored("simulated upstream failure".into())
                }
                _ => ActionOutcome::Ok,
            }
        }
    }

    let dispatcher = Arc::new(StepDispatcher::default());
    let dispatcher_for_seen = dispatcher.clone();
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![
            Action::LogLine {
                message: "a".into(),
            },
            Action::LogLine {
                message: "b".into(),
            },
            Action::LogLine {
                message: "c".into(),
            },
        ],
    };
    let (db, engine, flow_id) = boot(dispatcher, definition, true).await;
    let run_id = engine.handle().fire(flow_id, None).await.expect("fire");
    let run = wait_terminal(&db, run_id.0).await;

    assert!(matches!(run.status, FlowRunStatus::Errored));
    assert_eq!(run.nodeResults.len(), 4, "trigger + three action nodes");
    assert!(matches!(node(&run, "action_0").status, NodeStatus::Ok));
    assert!(matches!(node(&run, "action_1").status, NodeStatus::Errored));
    // The unwired `out` edge from the errored node propagates `skipped`.
    assert!(matches!(node(&run, "action_2").status, NodeStatus::Skipped));
    assert!(run.error.as_deref().unwrap_or("").contains("simulated"));
    // The dispatcher is never called for the skipped third action.
    assert_eq!(dispatcher_for_seen.seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn manual_fire_on_disabled_flow_is_allowed_and_produces_ok() {
    // Brief §3 — manualFire always runs, even on a disabled flow.
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![Action::LogLine {
            message: "boom".into(),
        }],
    };
    let (db, engine, flow_id) = boot(Arc::new(BasicDispatcher), definition, false).await;
    let run_id = engine.handle().fire(flow_id, None).await.expect("fire");
    let run = wait_terminal(&db, run_id.0).await;
    assert!(matches!(run.status, FlowRunStatus::Ok));
}

#[tokio::test]
async fn ts6_event_on_disabled_flow_produces_skipped_disabled_row() {
    // A producer-driven trigger on a disabled flow writes an audit row but
    // never executes — so it carries no per-node results.
    let definition = FlowDefinition {
        trigger: Trigger::Ts6ClientJoined { channel_id: None },
        actions: vec![Action::LogLine {
            message: "welcome".into(),
        }],
    };
    let (db, engine, flow_id) = boot(Arc::new(BasicDispatcher), definition, false).await;
    engine.handle().enable(flow_id).await.expect("enable");

    engine
        .handle()
        .on_client_joined(1, 5, "uid-abc".into(), "Alice".into())
        .await;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let runs = loop {
        let runs = bot_flow_runs::list_for_flow(&db, flow_id.0, 25, None)
            .await
            .expect("list runs");
        if !runs.is_empty() {
            break runs;
        }
        if std::time::Instant::now() >= deadline {
            panic!("no run row materialised for disabled-flow ts6 event");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(runs.len(), 1);
    assert!(matches!(runs[0].status, FlowRunStatus::SkippedDisabled));
    assert!(runs[0].actionResults.is_empty());
    assert!(runs[0].nodeResults.is_empty());
}

#[tokio::test]
async fn per_flow_drop_on_busy_returns_busy_error() {
    // A dispatcher that blocks on a barrier so the per-flow slot is held
    // while a second fire is attempted.
    struct StallDispatcher {
        barrier: tokio::sync::Notify,
        seen: AtomicUsize,
    }
    #[async_trait]
    impl ActionDispatcher for StallDispatcher {
        async fn dispatch(&self, _ctx: &ActionContext, _action: &Action) -> ActionOutcome {
            self.seen.fetch_add(1, Ordering::SeqCst);
            self.barrier.notified().await;
            ActionOutcome::Ok
        }
    }

    let dispatcher = Arc::new(StallDispatcher {
        barrier: tokio::sync::Notify::new(),
        seen: AtomicUsize::new(0),
    });
    let dispatcher_clone = dispatcher.clone();

    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![Action::LogLine {
            message: "stalled".into(),
        }],
    };
    let (db, engine, flow_id) = boot(dispatcher, definition, true).await;

    let first = engine
        .handle()
        .fire(flow_id, None)
        .await
        .expect("first fire");
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while dispatcher_clone.seen.load(Ordering::SeqCst) == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("dispatcher never observed the first fire");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let second = engine.handle().fire(flow_id, None).await;
    assert!(
        matches!(second, Err(FireError::Busy(fid)) if fid == flow_id),
        "expected Busy, got {second:?}"
    );
    assert_eq!(engine.handle().dropped_count(), 1);

    dispatcher_clone.barrier.notify_waiters();
    let run = wait_terminal(&db, first.0).await;
    assert!(matches!(run.status, FlowRunStatus::Ok));

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let third = loop {
        match engine.handle().fire(flow_id, None).await {
            Ok(id) => break id,
            Err(FireError::Busy(_)) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(e) => panic!("third fire failed: {e:?}"),
        }
    };
    assert_ne!(third.0, first.0);
}

#[tokio::test]
async fn fire_on_unknown_flow_returns_not_found() {
    let (_db, engine, _flow_id) = boot(
        Arc::new(BasicDispatcher),
        FlowDefinition {
            trigger: Trigger::ManualFire,
            actions: vec![Action::LogLine {
                message: "noop".into(),
            }],
        },
        true,
    )
    .await;

    let res = engine.handle().fire(FlowId(99_999), None).await;
    assert!(matches!(res, Err(FireError::NotFound(_))), "got {res:?}");
}

#[tokio::test]
async fn boot_marks_pre_existing_in_flight_rows_interrupted() {
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![Action::LogLine {
            message: "noop".into(),
        }],
    };
    let flow_data = serde_json::to_string(&definition).unwrap();
    let flow_id = seed_flow(&db, server_id, flow_data, true).await;
    let stale = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_id,
            trigger: serde_json::json!({"kind":"manualFire"}),
            status: FlowRunStatus::InFlight,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();
    assert!(matches!(stale.status, FlowRunStatus::InFlight));

    let _engine = start_engine(db.clone(), Arc::new(BasicDispatcher)).await;

    let after = bot_flow_runs::find_by_id(&db, stale.id)
        .await
        .unwrap()
        .expect("stale row still exists");
    assert!(
        matches!(after.status, FlowRunStatus::Interrupted),
        "expected interrupted, got {:?}",
        after.status
    );
    assert!(after.finishedAt.is_some());
    assert_eq!(after.error.as_deref(), Some("manager restart"));
}

#[tokio::test]
async fn per_flow_cap_enforced_after_inserts() {
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let flow_id = seed_flow(
        &db,
        server_id,
        r#"{"trigger":{"kind":"manualFire"},"actions":[]}"#.to_string(),
        true,
    )
    .await;

    for _ in 0..(bot_flow_runs::PER_FLOW_RUN_CAP + 5) {
        bot_flow_runs::insert(
            &db,
            bot_flow_runs::NewBotFlowRun {
                flowId: flow_id,
                trigger: serde_json::json!({"kind":"manualFire"}),
                status: FlowRunStatus::Ok,
                actionResults: vec![],
                nodeResults: vec![],
            },
        )
        .await
        .unwrap();
    }

    let removed = bot_flow_runs::enforce_per_flow_cap(&db, flow_id)
        .await
        .unwrap();
    assert_eq!(removed, 5);
    let listed = bot_flow_runs::list_for_flow(&db, flow_id, 200, None)
        .await
        .unwrap();
    assert_eq!(listed.len(), bot_flow_runs::PER_FLOW_RUN_CAP);
}

#[tokio::test]
async fn ttl_prune_removes_finished_rows_older_than_cutoff() {
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let flow_id = seed_flow(
        &db,
        server_id,
        r#"{"trigger":{"kind":"manualFire"},"actions":[]}"#.to_string(),
        true,
    )
    .await;

    let old = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_id,
            trigger: serde_json::json!({"kind":"manualFire"}),
            status: FlowRunStatus::Ok,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();
    db.query("UPDATE type::record('bot_flow_run', $id) SET finishedAt = $t;")
        .bind(("id", old.id))
        .bind(("t", chrono::Utc::now() - chrono::Duration::days(60)))
        .await
        .unwrap()
        .check()
        .unwrap();
    let _recent = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow_id,
            trigger: serde_json::json!({"kind":"manualFire"}),
            status: FlowRunStatus::Ok,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let pruned = bot_flow_runs::prune_older_than(&db, cutoff).await.unwrap();
    assert_eq!(pruned, 1);
    let remaining = bot_flow_runs::list_for_flow(&db, flow_id, 200, None)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
}

// ---------------------------------------------------------------------------
// v2 graph cases — branch, parallel (§4, §5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v2_branch_graph_routes_to_the_matched_port_and_prunes_the_rest() {
    // trigger -> branch{lobby} -> lobby_msg
    //                  \default -> default_msg
    // The branch matches `lobby`; the `default` subgraph is pruned and its
    // node settles `skipped` (§5.2 / §5.3), not `errored`.
    let graph = r#"{"version":2,"graph":{
      "nodes":[
        {"id":"t","kind":"trigger","config":{"kind":"manualFire"},"position":{"x":0,"y":0}},
        {"id":"route","kind":"branch","cases":[
          {"label":"lobby","when":"trigger.context.channel == 1"}],"position":{"x":0,"y":0}},
        {"id":"lobby_msg","kind":"action","config":{"kind":"logLine","message":"lobby"},"position":{"x":0,"y":0}},
        {"id":"default_msg","kind":"action","config":{"kind":"logLine","message":"default"},"position":{"x":0,"y":0}}
      ],
      "edges":[
        {"id":"e0","from":{"node":"t","port":"out"},"to":{"node":"route","port":"in"}},
        {"id":"e1","from":{"node":"route","port":"lobby"},"to":{"node":"lobby_msg","port":"in"}},
        {"id":"e2","from":{"node":"route","port":"default"},"to":{"node":"default_msg","port":"in"}}
      ]}}"#;

    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let flow_id = seed_flow(&db, server_id, graph.to_string(), true).await;
    let engine = start_engine(db.clone(), Arc::new(BasicDispatcher)).await;

    let run_id = engine
        .handle()
        .fire(FlowId(flow_id), ctx(serde_json::json!({ "channel": 1 })))
        .await
        .expect("fire");
    let run = wait_terminal(&db, run_id.0).await;

    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_eq!(run.nodeResults.len(), 4);
    assert!(matches!(node(&run, "route").status, NodeStatus::Ok));
    assert!(matches!(node(&run, "lobby_msg").status, NodeStatus::Ok));
    // The not-taken side is pruned to `skipped`.
    assert!(matches!(
        node(&run, "default_msg").status,
        NodeStatus::Skipped
    ));
}

#[tokio::test]
async fn v2_parallel_graph_fans_out_over_a_subflow() {
    // trigger -> parallel(collection = trigger.context.items, subFlow)
    // The sub-flow is a legacy single-action flow; the parallel node runs
    // it once per element and emits the array of per-element results.
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let sub_id = seed_flow(
        &db,
        server_id,
        r#"{"trigger":{"kind":"manualFire"},"actions":[{"kind":"logLine","message":"element"}]}"#
            .to_string(),
        true,
    )
    .await;

    let graph = format!(
        r#"{{"version":2,"graph":{{
          "nodes":[
            {{"id":"t","kind":"trigger","config":{{"kind":"manualFire"}},"position":{{"x":0,"y":0}}}},
            {{"id":"fan","kind":"parallel","collection":"trigger.context.items","subFlowId":{sub_id},"maxConcurrency":4,"position":{{"x":0,"y":0}}}}
          ],
          "edges":[
            {{"id":"e0","from":{{"node":"t","port":"out"}},"to":{{"node":"fan","port":"in"}}}}
          ]}}}}"#
    );
    let flow_id = seed_flow(&db, server_id, graph, true).await;
    let engine = start_engine(db.clone(), Arc::new(BasicDispatcher)).await;

    let run_id = engine
        .handle()
        .fire(
            FlowId(flow_id),
            ctx(serde_json::json!({ "items": [1, 2, 3] })),
        )
        .await
        .expect("fire");
    let run = wait_terminal(&db, run_id.0).await;

    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    let fan = node(&run, "fan");
    assert!(matches!(fan.status, NodeStatus::Ok));
    let output = fan.output.as_ref().expect("parallel node has output");
    let elements = output.as_array().expect("parallel output is an array");
    assert_eq!(elements.len(), 3, "one result per collection element");
}

#[tokio::test]
async fn v2_branch_plus_parallel_graph_produces_correct_node_results() {
    // Acceptance (PURA-266): a branch+parallel graph fires and produces
    // correct per-node results.
    //   trigger -> branch{vip} -> fan (parallel over a sub-flow)
    //                  \default -> default_msg  (pruned, skipped)
    let db = fresh_db().await;
    let server_id = seed_server(&db).await;
    let sub_id = seed_flow(
        &db,
        server_id,
        r#"{"trigger":{"kind":"manualFire"},"actions":[{"kind":"logLine","message":"greet"}]}"#
            .to_string(),
        true,
    )
    .await;

    let graph = format!(
        r#"{{"version":2,"graph":{{
          "nodes":[
            {{"id":"t","kind":"trigger","config":{{"kind":"manualFire"}},"position":{{"x":0,"y":0}}}},
            {{"id":"route","kind":"branch","cases":[
              {{"label":"vip","when":"trigger.context.tier == \"vip\""}}],"position":{{"x":0,"y":0}}}},
            {{"id":"fan","kind":"parallel","collection":"trigger.context.items","subFlowId":{sub_id},"maxConcurrency":2,"position":{{"x":0,"y":0}}}},
            {{"id":"default_msg","kind":"action","config":{{"kind":"logLine","message":"d"}},"position":{{"x":0,"y":0}}}}
          ],
          "edges":[
            {{"id":"e0","from":{{"node":"t","port":"out"}},"to":{{"node":"route","port":"in"}}}},
            {{"id":"e1","from":{{"node":"route","port":"vip"}},"to":{{"node":"fan","port":"in"}}}},
            {{"id":"e2","from":{{"node":"route","port":"default"}},"to":{{"node":"default_msg","port":"in"}}}}
          ]}}}}"#
    );
    let flow_id = seed_flow(&db, server_id, graph, true).await;
    let engine = start_engine(db.clone(), Arc::new(BasicDispatcher)).await;

    let run_id = engine
        .handle()
        .fire(
            FlowId(flow_id),
            ctx(serde_json::json!({ "tier": "vip", "items": [10, 20] })),
        )
        .await
        .expect("fire");
    let run = wait_terminal(&db, run_id.0).await;

    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_eq!(run.nodeResults.len(), 4);
    assert!(matches!(node(&run, "t").status, NodeStatus::Ok));
    assert!(matches!(node(&run, "route").status, NodeStatus::Ok));
    // The `vip` path ran the fan-out node.
    let fan = node(&run, "fan");
    assert!(matches!(fan.status, NodeStatus::Ok));
    assert_eq!(
        fan.output
            .as_ref()
            .and_then(|o| o.as_array())
            .map(|a| a.len()),
        Some(2),
        "the parallel node fanned out over both collection elements"
    );
    // The `default` path was pruned.
    assert!(matches!(
        node(&run, "default_msg").status,
        NodeStatus::Skipped
    ));
}
