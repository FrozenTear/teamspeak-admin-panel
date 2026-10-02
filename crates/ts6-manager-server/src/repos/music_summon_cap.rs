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
/// no row.
pub async fn get(db: &Database, server_addr: &str) -> Result<Option<u32>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM music_summon_cap WHERE serverAddr = $serverAddr LIMIT 1;"
    );
    let mut resp = db
        .query(sql)
        .bind(("serverAddr", server_addr.to_string()))
        .await
        .context("music_summon_cap get query failed")?
        .check()?;
    let rows: Vec<MusicSummonCap> = resp.take(0)?;
    match rows.into_iter().next() {
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

/// Insert or replace the one row for `server_addr`.
pub async fn upsert(db: &Database, server_addr: &str, cap: u32) -> Result<()> {
    let cap = i64::from(cap);
    if get(db, server_addr).await?.is_some() {
        let sql = "UPDATE music_summon_cap SET cap = $cap WHERE serverAddr = $serverAddr;";
        db.query(sql)
            .bind(("cap", cap))
            .bind(("serverAddr", server_addr.to_string()))
            .await
            .context("music_summon_cap update query failed")?
            .check()?;
    } else {
        let sql = "CREATE music_summon_cap CONTENT { serverAddr: $serverAddr, cap: $cap };";
        db.query(sql)
            .bind(("serverAddr", server_addr.to_string()))
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
}
