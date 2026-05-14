//! Persistence layer (PURA-10).
//!
//! D8 ratified deviation (2026-05-07): the spec's SQLite is replaced by
//! SurrealDB v3 embedded with the SurrealKV backend. The `Surreal<Any>`
//! handle here is the single connection point for the rest of the binary;
//! repos in [`crate::repos`] borrow it via `Arc<Database>`.

use std::sync::Arc;

use anyhow::{Context, Result};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::config::{Config, DEFAULT_DB_NAME, DEFAULT_DB_NAMESPACE};

pub mod error;
pub mod migrations;

pub use error::{ClassifyDbResult, DbBoundary, DbError, classify, classify_surrealdb};

#[cfg(test)]
mod tests;

/// Shared SurrealDB handle. Cloning is cheap (internal `Arc`), but this
/// alias is what the rest of the codebase passes around so the underlying
/// engine choice (`surrealkv://`, `ws://`, `memory`) stays an implementation
/// detail.
pub type Database = Surreal<Any>;

/// Open a connection to the database described by `cfg.database_url` and
/// select the `ts6 / ts6_manager` namespace + database. Caller is expected
/// to invoke [`migrations::run`] before serving traffic.
pub async fn connect(cfg: &Config) -> Result<Arc<Database>> {
    if let Some(dir) = surrealkv_dir(&cfg.database_url) {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create SurrealKV directory `{}`", dir.display()))?;
    }
    let db = surrealdb::engine::any::connect(&cfg.database_url)
        .await
        .with_context(|| format!("failed to connect to database `{}`", cfg.database_url))?;
    db.use_ns(DEFAULT_DB_NAMESPACE)
        .use_db(DEFAULT_DB_NAME)
        .await
        .context("failed to select namespace/database")?;
    tracing::info!(
        url = %cfg.database_url,
        namespace = DEFAULT_DB_NAMESPACE,
        database = DEFAULT_DB_NAME,
        "SurrealDB connected"
    );
    Ok(Arc::new(db))
}

/// Extract the on-disk directory for a `surrealkv://` URL. SurrealKV expects
/// the directory to exist before [`connect`] runs; the boot path uses this
/// to create the directory on a fresh checkout (data/ is gitignored).
fn surrealkv_dir(database_url: &str) -> Option<std::path::PathBuf> {
    database_url
        .strip_prefix("surrealkv://")
        .map(std::path::PathBuf::from)
}

/// Connect to an in-memory SurrealDB instance for tests. Each call returns
/// an isolated database — callers do not need a tempdir.
#[cfg(test)]
pub async fn connect_in_memory() -> Result<Arc<Database>> {
    let db = surrealdb::engine::any::connect("memory")
        .await
        .context("failed to connect to in-memory SurrealDB")?;
    db.use_ns(DEFAULT_DB_NAMESPACE)
        .use_db(DEFAULT_DB_NAME)
        .await
        .context("failed to select namespace/database")?;
    Ok(Arc::new(db))
}
