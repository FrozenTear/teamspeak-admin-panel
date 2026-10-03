//! One summon cap per TeamSpeak server address.
//!
//! The key is the bot `serverAddr` string (`host:port`). It is not a bot
//! id and not a slot. A missing row means nothing is stored — callers
//! must not invent a number.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// One stored cap. `cap` is the number the music process has accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MusicSummonCap {
    pub serverAddr: String,
    pub cap: i64,
}

const PROJECTION: &str = "serverAddr, cap";

/// The stored number for `server_addr`, or `None` when that server has
/// no row. Addresses that dial the same socket share one row.
pub async fn get(db: &Database, server_addr: &str) -> Result<Option<u32>> {
    let want = music_bot::canon_server_addr(server_addr);
    if want.is_empty() {
        return Ok(None);
    }
    let rows = list(db).await?;
    match rows
        .into_iter()
        .find(|row| music_bot::canon_server_addr(&row.serverAddr) == want)
    {
        None => Ok(None),
        Some(row) => Ok(Some(cap_as_u32(row.cap)?)),
    }
}

/// Every stored cap. Servers with no row are absent.
pub async fn list(db: &Database) -> Result<Vec<MusicSummonCap>> {
    let sql = format!("SELECT {PROJECTION} FROM music_summon_cap ORDER BY serverAddr ASC;");
    let mut resp = db
        .query(sql)
        .await
        .context("music_summon_cap list query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Insert or replace the one row for `server_addr`. The stored address
/// is the canonical socket, so a second spelling does not create a row.
pub async fn upsert(db: &Database, server_addr: &str, cap: u32) -> Result<()> {
    let canon = music_bot::canon_server_addr(server_addr);
    let cap = i64::from(cap);
    let rows = list(db).await?;
    if let Some(existing) = rows
        .into_iter()
        .find(|row| music_bot::canon_server_addr(&row.serverAddr) == canon)
    {
        let sql = "UPDATE music_summon_cap SET cap = $cap, serverAddr = $canon WHERE serverAddr = $serverAddr;";
        db.query(sql)
            .bind(("cap", cap))
            .bind(("canon", canon))
            .bind(("serverAddr", existing.serverAddr))
            .await
            .context("music_summon_cap update query failed")?
            .check()?;
    } else {
        let sql = "CREATE music_summon_cap CONTENT { serverAddr: $serverAddr, cap: $cap };";
        db.query(sql)
            .bind(("serverAddr", canon))
            .bind(("cap", cap))
            .await
            .context("music_summon_cap create query failed")?
            .check()?;
    }
    Ok(())
}

fn cap_as_u32(cap: i64) -> Result<u32> {
    u32::try_from(cap).map_err(|_| anyhow::anyhow!("stored summon cap {cap} is outside u32"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::connect_in_memory;

    #[tokio::test]
    async fn missing_server_is_none_and_upsert_is_one_row() {
        let db = connect_in_memory().await.expect("connect");
        crate::db::migrations::run(&db).await.expect("migrations");

        assert_eq!(get(&db, "127.0.0.1:9987").await.expect("get"), None);
        assert!(list(&db).await.expect("list").is_empty());

        upsert(&db, "127.0.0.1:9987", 1).await.expect("insert");
        upsert(&db, "127.0.0.1:9987", 4).await.expect("replace");
        upsert(&db, "127.0.0.1:9988", 0)
            .await
            .expect("other server");

        assert_eq!(get(&db, "127.0.0.1:9987").await.expect("get"), Some(4));
        assert_eq!(get(&db, "127.0.0.1:9988").await.expect("get"), Some(0));
        assert_eq!(get(&db, "10.0.0.1:9987").await.expect("get"), None);
        let rows = list(&db).await.expect("list");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].serverAddr, "127.0.0.1:9987");
        assert_eq!(rows[0].cap, 4);
        assert_eq!(rows[1].cap, 0);
    }

    #[tokio::test]
    async fn spellings_of_one_socket_are_one_row() {
        let db = connect_in_memory().await.expect("connect");
        crate::db::migrations::run(&db).await.expect("migrations");
        upsert(&db, "Voice.Example", 2).await.expect("insert");
        upsert(&db, "voice.example:9987", 3).await.expect("replace");
        assert_eq!(get(&db, "VOICE.example").await.expect("get"), Some(3));
        assert_eq!(list(&db).await.expect("list").len(), 1);
        upsert(&db, "voice.example:9988", 1)
            .await
            .expect("other port");
        assert_eq!(list(&db).await.expect("list").len(), 2);
    }
}
