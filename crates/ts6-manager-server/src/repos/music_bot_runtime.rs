//! `music_bot_runtime` repo (PURA-357).
//!
//! Persists the `music_bot::BotConfig` runtime shape so configured music
//! bots survive a process restart / image upgrade. WS-5 (PURA-123)
//! shipped the [`music_bot::BotSupervisor`] as in-memory only — every
//! bot vanished on the `kube down` / `kube play` cycle a deploy
//! performs (board report, PURA-356).
//!
//! - The REST layer ([`crate::routes::music_bots`]) writes a row on bot
//!   create and removes it on delete.
//! - `main.rs` reads the whole table at boot and re-spawns each bot into
//!   the supervisor under its original id.
//!
//! The record id IS the supervisor's `BotId` (a `u64`): the supervisor's
//! atomic counter stays the id authority, and boot seeds it past the
//! highest persisted id.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// One persisted music-bot config row. Mirrors the subset of
/// `music_bot::BotConfig` the supervisor needs to re-spawn a bot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MusicBotRuntime {
    /// The supervisor-minted `BotId.0`.
    pub id: i64,
    pub name: String,
    pub serverAddr: String,
    pub identityPath: String,
    pub autoConnect: bool,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    serverAddr,
    identityPath,
    autoConnect
";

/// Insert-or-replace one bot's config under its supervisor-minted
/// `BotId`. Idempotent — a second call with the same `id` overwrites the
/// row, which keeps a future bot-edit route a one-liner.
pub async fn upsert(
    db: &Database,
    id: i64,
    name: &str,
    server_addr: &str,
    identity_path: &str,
    auto_connect: bool,
) -> Result<()> {
    let sql = "
        UPSERT type::record('music_bot_runtime', $id) CONTENT {
            name: $name,
            serverAddr: $serverAddr,
            identityPath: $identityPath,
            autoConnect: $autoConnect
        };";
    db.query(sql)
        .bind(("id", id))
        .bind(("name", name.to_string()))
        .bind(("serverAddr", server_addr.to_string()))
        .bind(("identityPath", identity_path.to_string()))
        .bind(("autoConnect", auto_connect))
        .await
        .context("music_bot_runtime upsert query failed")?
        .check()
        .context("music_bot_runtime upsert reported an error")?;
    Ok(())
}

/// Every persisted bot, ordered by id ascending — the order `main.rs`
/// rehydrates them in.
pub async fn list(db: &Database) -> Result<Vec<MusicBotRuntime>> {
    let sql = format!("SELECT {PROJECTION} FROM music_bot_runtime ORDER BY id ASC;");
    let mut resp = db
        .query(sql)
        .await
        .context("music_bot_runtime list query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Remove a bot's persisted config so a deleted bot does not come back
/// on the next boot. A no-op if the row is already gone.
pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('music_bot_runtime', $id);";
    db.query(sql)
        .bind(("id", id))
        .await
        .context("music_bot_runtime delete query failed")?
        .check()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_in_memory;

    #[tokio::test]
    async fn upsert_then_list_round_trips() {
        let db = connect_in_memory().await.expect("connect");
        crate::db::migrations::run(&db).await.expect("migrations");

        upsert(&db, 1, "DJ", "ts.example.net:9987", "/data/id/bot-1", true)
            .await
            .expect("upsert 1");
        upsert(&db, 2, "Lounge", "10.0.0.5:9987", "/data/id/bot-2", false)
            .await
            .expect("upsert 2");

        let rows = list(&db).await.expect("list");
        assert_eq!(
            rows,
            vec![
                MusicBotRuntime {
                    id: 1,
                    name: "DJ".into(),
                    serverAddr: "ts.example.net:9987".into(),
                    identityPath: "/data/id/bot-1".into(),
                    autoConnect: true,
                },
                MusicBotRuntime {
                    id: 2,
                    name: "Lounge".into(),
                    serverAddr: "10.0.0.5:9987".into(),
                    identityPath: "/data/id/bot-2".into(),
                    autoConnect: false,
                },
            ],
        );
    }

    #[tokio::test]
    async fn upsert_overwrites_existing_row() {
        let db = connect_in_memory().await.expect("connect");
        crate::db::migrations::run(&db).await.expect("migrations");

        upsert(&db, 7, "Old", "old:9987", "/data/id/bot-7", true)
            .await
            .expect("first upsert");
        upsert(&db, 7, "Renamed", "new:9987", "/data/id/bot-7", false)
            .await
            .expect("second upsert");

        let rows = list(&db).await.expect("list");
        assert_eq!(rows.len(), 1, "same id must not create a second row");
        assert_eq!(rows[0].name, "Renamed");
        assert!(!rows[0].autoConnect);
    }

    #[tokio::test]
    async fn delete_removes_the_row() {
        let db = connect_in_memory().await.expect("connect");
        crate::db::migrations::run(&db).await.expect("migrations");

        upsert(&db, 3, "Temp", "h:9987", "/data/id/bot-3", true)
            .await
            .expect("upsert");
        delete(&db, 3).await.expect("delete");
        assert!(list(&db).await.expect("list").is_empty());

        // Deleting an absent row is a no-op, not an error.
        delete(&db, 999).await.expect("delete missing");
    }
}
