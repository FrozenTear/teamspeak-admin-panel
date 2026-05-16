//! Engine unit tests — PURA-241 acceptance §F-impl-engine.
//!
//! Each test stands up an in-memory SurrealDB, runs the chapter-4
//! migration set, seeds a `server_connection` + `bot_flow`, then boots
//! [`FlowEngine`] with a test dispatcher and asserts on the persisted
//! `bot_flow_run` rows.

#![allow(non_snake_case)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use ts6_manager_shared::flows::{
    Action, ActionStatus, FlowDefinition, FlowId, FlowRunStatus, Trigger,
};

use super::engine::{
    ActionContext, ActionDispatcher, ActionOutcome, BasicDispatcher, EngineDeps, FireError,
    FlowEngine,
};
use crate::db::{Database, connect_in_memory, migrations};
use crate::repos::{bot_flow_runs, bot_flows, server_connections};

/// Helper: fully boot the schema + seed one server + one flow, then
/// build an `EngineDeps` with the supplied dispatcher.
async fn boot(
    dispatcher: Arc<dyn ActionDispatcher>,
    definition: FlowDefinition,
    enabled: bool,
) -> (Arc<Database>, FlowEngine, FlowId) {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations");

    let server_id = server_connections::insert(
        &db,
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
    .id;

    let flow_data = serde_json::to_string(&definition).expect("encode definition");
    let flow = bot_flows::insert(
        &db,
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
    .expect("seed flow");

    let deps = EngineDeps {
        db: db.clone(),
        dispatcher,
        max_parallel_runs: 4,
        run_ttl: Duration::from_secs(30 * 86_400),
        ttl_sweep_interval: Duration::from_secs(3_600),
    };
    let engine = FlowEngine::start(deps).await.expect("engine start");
    (db, engine, FlowId(flow.id))
}

/// Wait until the run row is in a terminal state (anything other than
/// in-flight), or fail.
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
            panic!("run {} never reached terminal state", run_id);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn manual_fire_of_log_line_flow_produces_ok_run() {
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![Action::LogLine {
            message: "hello world".into(),
        }],
    };
    let (db, engine, flow_id) = boot(
        Arc::new(BasicDispatcher),
        definition,
        /*enabled=*/ true,
    )
    .await;

    let run_id = engine.handle().fire(flow_id, None).await.expect("fire ok");
    let run = wait_terminal(&db, run_id.0).await;

    assert!(
        matches!(run.status, FlowRunStatus::Ok),
        "got {:?}",
        run.status
    );
    assert_eq!(run.actionResults.len(), 1);
    assert!(matches!(run.actionResults[0].status, ActionStatus::Ok));
    assert_eq!(run.actionResults[0].kind, "logLine");
    assert!(run.error.is_none());
    assert!(run.finishedAt.is_some());
}

#[tokio::test]
async fn first_errored_action_aborts_run_and_skips_remainder() {
    /// Dispatcher: action index 0 → ok, action index 1 → errored,
    /// action index 2 → would be ok but should be skipped.
    #[derive(Default)]
    struct StepDispatcher {
        seen: AtomicUsize,
    }
    #[async_trait]
    impl ActionDispatcher for StepDispatcher {
        async fn dispatch(&self, ctx: &ActionContext, _action: &Action) -> ActionOutcome {
            self.seen.fetch_add(1, Ordering::SeqCst);
            if ctx.action_index == 1 {
                ActionOutcome::Errored("simulated upstream failure".into())
            } else {
                ActionOutcome::Ok
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
    assert_eq!(run.actionResults.len(), 3);
    assert!(matches!(run.actionResults[0].status, ActionStatus::Ok));
    assert!(matches!(run.actionResults[1].status, ActionStatus::Errored));
    assert!(matches!(run.actionResults[2].status, ActionStatus::Skipped));
    assert!(run.error.as_deref().unwrap_or("").contains("simulated"));
    // Dispatcher must NOT be called for the third action.
    assert_eq!(dispatcher_for_seen.seen.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn manual_fire_on_disabled_flow_is_allowed_and_produces_ok() {
    // Brief §3 — manualFire is the test path; it always runs even when
    // the flow is disabled.
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
    // Producer-driven trigger: brief §3 / §7 says the audit row is
    // preserved even when the flow is disabled, so operators can see
    // that the engine saw the event but chose not to act.
    let definition = FlowDefinition {
        trigger: Trigger::Ts6ClientJoined { channel_id: None },
        actions: vec![Action::LogLine {
            message: "welcome".into(),
        }],
    };
    let (db, engine, flow_id) = boot(Arc::new(BasicDispatcher), definition, false).await;
    // Disabled flows still want a subscription record so we can audit
    // skipped runs. The routes child would enable() on patch; for this
    // test we mimic the boot pass by calling enable() manually.
    engine.handle().enable(flow_id).await.expect("enable");

    engine
        .handle()
        .on_client_joined(1, 5, "uid-abc".into(), "Alice".into())
        .await;

    // The skipped_disabled row is inserted synchronously inside
    // `on_client_joined`'s `fire_event`; poll briefly for it.
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
    assert_eq!(runs[0].actionResults.len(), 1);
    assert!(matches!(
        runs[0].actionResults[0].status,
        ActionStatus::Skipped
    ));
}

#[tokio::test]
async fn per_flow_drop_on_busy_returns_busy_error() {
    // Dispatcher that blocks on a barrier so we can observe the slot
    // being occupied while we attempt a second fire.
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

    // First fire — captures the per-flow slot.
    let first = engine
        .handle()
        .fire(flow_id, None)
        .await
        .expect("first fire");
    // Wait for the dispatcher to actually enter `dispatch` (so the
    // per-flow slot is genuinely held).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while dispatcher_clone.seen.load(Ordering::SeqCst) == 0 {
        if std::time::Instant::now() >= deadline {
            panic!("dispatcher never observed the first fire");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Second fire — must be rejected with `Busy(flow_id)` AND counted.
    let second = engine.handle().fire(flow_id, None).await;
    assert!(
        matches!(second, Err(FireError::Busy(fid)) if fid == flow_id),
        "expected Busy, got {second:?}"
    );
    assert_eq!(engine.handle().dropped_count(), 1);

    // Release the dispatcher and confirm the first run finishes ok.
    dispatcher_clone.barrier.notify_waiters();
    let run = wait_terminal(&db, first.0).await;
    assert!(matches!(run.status, FlowRunStatus::Ok));

    // After the first run completes the per-flow slot is freed when the
    // run task drops its permit. The terminal-status DB write happens a
    // step before the permit drop, so retry briefly to dodge that race.
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
    // Set up a flow without booting the engine, write a fake in-flight
    // row, then boot the engine and confirm the row gets rewritten.
    let db = connect_in_memory().await.expect("connect");
    migrations::run(&db).await.expect("migrations");
    let server_id = server_connections::insert(
        &db,
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
    .id;
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![Action::LogLine {
            message: "noop".into(),
        }],
    };
    let flow_data = serde_json::to_string(&definition).unwrap();
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "f".into(),
            description: None,
            flowData: flow_data,
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();
    let stale = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow.id,
            trigger: serde_json::json!({"kind":"manualFire"}),
            status: FlowRunStatus::InFlight,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();
    assert!(matches!(stale.status, FlowRunStatus::InFlight));

    // Now boot the engine — the boot sweep should rewrite the row.
    let deps = EngineDeps {
        db: db.clone(),
        dispatcher: Arc::new(BasicDispatcher),
        max_parallel_runs: 4,
        run_ttl: Duration::from_secs(30 * 86_400),
        ttl_sweep_interval: Duration::from_secs(3_600),
    };
    let _engine = FlowEngine::start(deps).await.expect("boot");

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
    // Cap is 200; insert 205 rows directly via the repo helper and the
    // engine's `enforce_per_flow_cap` should bring it back to 200.
    let db = connect_in_memory().await.expect("connect");
    migrations::run(&db).await.expect("migrations");
    let server_id = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "p".into(),
            host: "h".into(),
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
    .unwrap()
    .id;
    let definition = FlowDefinition {
        trigger: Trigger::ManualFire,
        actions: vec![],
    };
    let flow_data = serde_json::to_string(&definition).unwrap();
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "n".into(),
            description: None,
            flowData: flow_data,
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    for _ in 0..(bot_flow_runs::PER_FLOW_RUN_CAP + 5) {
        bot_flow_runs::insert(
            &db,
            bot_flow_runs::NewBotFlowRun {
                flowId: flow.id,
                trigger: serde_json::json!({"kind":"manualFire"}),
                status: FlowRunStatus::Ok,
                actionResults: vec![],
                nodeResults: vec![],
            },
        )
        .await
        .unwrap();
    }

    let removed = bot_flow_runs::enforce_per_flow_cap(&db, flow.id)
        .await
        .unwrap();
    assert_eq!(removed, 5);
    let listed = bot_flow_runs::list_for_flow(&db, flow.id, 200, None)
        .await
        .unwrap();
    assert_eq!(listed.len(), bot_flow_runs::PER_FLOW_RUN_CAP);
}

#[tokio::test]
async fn ttl_prune_removes_finished_rows_older_than_cutoff() {
    let db = connect_in_memory().await.expect("connect");
    migrations::run(&db).await.expect("migrations");
    let server_id = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "p".into(),
            host: "h".into(),
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
    .unwrap()
    .id;
    let flow = bot_flows::insert(
        &db,
        bot_flows::NewBotFlow {
            name: "n".into(),
            description: None,
            flowData: r#"{"trigger":{"kind":"manualFire"},"actions":[]}"#.into(),
            serverConfigId: server_id,
            virtualServerId: 1,
            enabled: true,
        },
    )
    .await
    .unwrap();

    let old = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow.id,
            trigger: serde_json::json!({"kind":"manualFire"}),
            status: FlowRunStatus::Ok,
            actionResults: vec![],
            nodeResults: vec![],
        },
    )
    .await
    .unwrap();
    // Backdate via raw query — the test rewrites finishedAt to 60 days ago.
    db.query("UPDATE type::record('bot_flow_run', $id) SET finishedAt = $t;")
        .bind(("id", old.id))
        .bind(("t", chrono::Utc::now() - chrono::Duration::days(60)))
        .await
        .unwrap()
        .check()
        .unwrap();
    // Plus a "recent" row that survives.
    let _recent = bot_flow_runs::insert(
        &db,
        bot_flow_runs::NewBotFlowRun {
            flowId: flow.id,
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
    let remaining = bot_flow_runs::list_for_flow(&db, flow.id, 200, None)
        .await
        .unwrap();
    assert_eq!(remaining.len(), 1);
}
