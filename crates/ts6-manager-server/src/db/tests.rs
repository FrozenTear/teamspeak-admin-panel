//! End-to-end DB smoke tests against an in-memory SurrealDB engine.
//!
//! These run on every `cargo test` and need no external services. They cover
//! the migration runner's idempotency property (re-running it does not
//! re-apply any migration) and confirm every Chapter-4 table exists.

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
            "0004_chapter4_remaining_entities".to_string(),
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
        4,
        "second run should skip every migration applied on the first run"
    );
}

#[tokio::test]
async fn chapter4_tables_exist_after_migrations() {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations run");

    // Every Chapter-4 table from spec §4.2.1–§4.2.17 must be defined and
    // SELECT-able even when empty. Slice 1 plus slice 2.
    let tables: &[&str] = &[
        // §4.2.1–§4.2.4 priority slice
        "user",
        "refresh_token",
        "server_connection",
        "server_user_grant",
        // §4.2.5–§4.2.17 slice 2
        "bot_flow",
        "bot_variable",
        "bot_execution",
        "bot_execution_log",
        "app_setting",
        "music_bot",
        "song",
        "playlist",
        "playlist_song",
        "radio_station",
        "widget",
        "music_request",
        "stream_session",
    ];
    for table in tables {
        let q = format!("SELECT count() FROM {table} GROUP ALL");
        let mut response = db.query(&q).await.expect("count query");
        let _: Vec<serde_json::Value> = response.take(0).expect("count rows deserialise");
    }
}
