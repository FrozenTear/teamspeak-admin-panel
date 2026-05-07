//! End-to-end DB smoke tests against an in-memory SurrealDB engine.
//!
//! These run on every `cargo test` and need no external services. They cover
//! the migration runner's idempotency property (re-running it does not
//! re-apply any migration) and confirm the priority-slice tables exist.

use super::{connect_in_memory, migrations};

#[tokio::test]
async fn migrations_apply_priority_slice_on_fresh_db() {
    let db = connect_in_memory().await.expect("in-memory connect");
    let report = migrations::run(&db).await.expect("migrations run");

    assert_eq!(
        report.applied,
        vec![
            "0001_baseline".to_string(),
            "0002_query_bot_nickname".to_string(),
            "0003_ssh_bot_nickname".to_string(),
        ],
        "first run should apply every migration"
    );
    assert!(report.skipped.is_empty(), "first run should skip nothing");
}

#[tokio::test]
async fn migrations_runner_is_idempotent() {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("first run");
    let second = migrations::run(&db).await.expect("second run");

    assert!(
        second.applied.is_empty(),
        "second run should apply nothing, got {:?}",
        second.applied
    );
    assert_eq!(
        second.skipped.len(),
        3,
        "second run should skip the three priority-slice migrations"
    );
}

#[tokio::test]
async fn priority_slice_tables_exist_after_migrations() {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations run");

    // Each table should be defined and SELECT-able even when empty.
    for table in ["user", "refresh_token", "server_connection", "server_user_grant"] {
        let q = format!("SELECT count() FROM {table} GROUP ALL");
        let mut response = db.query(&q).await.expect("count query");
        let _: Vec<serde_json::Value> = response.take(0).expect("count rows deserialise");
    }
}
