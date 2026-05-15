//! Embedded SurrealQL migrations and the runner that applies them.
//!
//! Migration files live under `crates/ts6-manager-server/migrations/` and are
//! `include_str!`-ed into the binary so the `cargo run -- migrate` subcommand
//! works against a release build with no extra files on disk.
//!
//! Applied migrations are tracked in the `_migration` table (one row per
//! migration name). The runner is idempotent — running it twice in a row is
//! a no-op on the second call.

use anyhow::{Context, Result};
use serde::Deserialize;
use surrealdb::types::SurrealValue;

use super::Database;

/// Ordered list of migrations. Adding a new file means appending an entry
/// here; the file's name (without extension) is the dedup key.
const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_baseline",
        include_str!("../../migrations/0001_baseline.surql"),
    ),
    (
        "0002_query_bot_nickname",
        include_str!("../../migrations/0002_query_bot_nickname.surql"),
    ),
    (
        "0003_ssh_bot_nickname",
        include_str!("../../migrations/0003_ssh_bot_nickname.surql"),
    ),
    (
        "0004_chapter4_remaining_entities",
        include_str!("../../migrations/0004_chapter4_remaining_entities.surql"),
    ),
    (
        "0005_ssh_bridge_auth",
        include_str!("../../migrations/0005_ssh_bridge_auth.surql"),
    ),
    (
        "0006_ssh_audit_log",
        include_str!("../../migrations/0006_ssh_audit_log.surql"),
    ),
    (
        "0007_video_source",
        include_str!("../../migrations/0007_video_source.surql"),
    ),
    (
        "0008_server_last_seen_at",
        include_str!("../../migrations/0008_server_last_seen_at.surql"),
    ),
];

#[derive(Debug, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
struct AppliedRow {
    name: String,
}

/// SurrealQL run once per [`run`] call to make sure the bookkeeping table
/// exists. Idempotent thanks to `DEFINE … IF NOT EXISTS`.
const ENSURE_MIGRATION_TABLE: &str = "
    DEFINE TABLE IF NOT EXISTS _migration SCHEMAFULL;
    DEFINE FIELD IF NOT EXISTS name      ON _migration TYPE string;
    DEFINE FIELD IF NOT EXISTS appliedAt ON _migration TYPE datetime VALUE $value OR time::now() READONLY;
    DEFINE INDEX IF NOT EXISTS _migration_name_unique ON _migration FIELDS name UNIQUE;
";

/// Apply every migration that has not yet been recorded in `_migration`.
/// Existing applied migrations are skipped without re-running their SurrealQL.
pub async fn run(db: &Database) -> Result<MigrationReport> {
    db.query(ENSURE_MIGRATION_TABLE)
        .await
        .context("failed to ensure _migration table exists")?
        .check()
        .context("_migration bootstrap query reported an error")?;

    let applied: Vec<AppliedRow> = db
        .query("SELECT name FROM _migration")
        .await
        .context("failed to read _migration table")?
        .take(0)
        .context("failed to deserialise _migration rows")?;
    let already: std::collections::HashSet<String> = applied.into_iter().map(|r| r.name).collect();

    let mut applied_now = Vec::new();
    let mut skipped = Vec::new();

    for (name, sql) in MIGRATIONS {
        if already.contains(*name) {
            skipped.push((*name).to_string());
            tracing::debug!(migration = %name, "migration already applied; skipping");
            continue;
        }

        tracing::info!(migration = %name, "applying migration");
        db.query(*sql)
            .await
            .with_context(|| format!("migration `{name}` failed to execute"))?
            .check()
            .with_context(|| format!("migration `{name}` reported an error"))?;

        db.query("CREATE _migration CONTENT { name: $name }")
            .bind(("name", (*name).to_string()))
            .await
            .with_context(|| format!("failed to record migration `{name}` as applied"))?
            .check()
            .with_context(|| format!("recording migration `{name}` reported an error"))?;

        applied_now.push((*name).to_string());
    }

    Ok(MigrationReport {
        applied: applied_now,
        skipped,
    })
}

/// Summary of a single [`run`] invocation. Useful for logging at boot and
/// for asserting expected behaviour in tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
}
