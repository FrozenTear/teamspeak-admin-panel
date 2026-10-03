//! One summon cap and one summon home channel per TeamSpeak server
//! address.
//!
//! The key is the bot `serverAddr` string (`host:port`). It is not a bot
//! id and not a slot. A missing row, or a row without a cap, means no
//! number is stored, and callers must not invent one. A row without a
//! home means no home channel is picked, and callers must not pick one.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

/// One server's stored summon settings. `cap` is the number the music
/// process has accepted. `homeChannel` is the channel its summon clients
/// wait in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct MusicSummonCap {
    pub serverAddr: String,
    pub cap: Option<i64>,
    pub homeChannel: Option<i64>,
}

const PROJECTION: &str = "serverAddr, cap, homeChannel";

/// The stored number for `server_addr`, or `None` when that server has
/// no row or no number. Addresses that dial the same socket share one row.
pub async fn get(db: &Database, server_addr: &str) -> Result<Option<u32>> {
    match find(db, server_addr).await?.and_then(|row| row.cap) {
        None => Ok(None),
        Some(cap) => Ok(Some(cap_as_u32(cap)?)),
    }
}

/// The stored home channel for `server_addr`, or `None` when none is
/// picked.
pub async fn get_home(db: &Database, server_addr: &str) -> Result<Option<u64>> {
    match find(db, server_addr).await?.and_then(|row| row.homeChannel) {
        None => Ok(None),
        Some(home) => Ok(Some(home_as_u64(home)?)),
    }
}

async fn find(db: &Database, server_addr: &str) -> Result<Option<MusicSummonCap>> {
    let want = music_bot::canon_server_addr(server_addr);
    if want.is_empty() {
        return Ok(None);
    }
    let rows = list(db).await?;
    Ok(rows
        .into_iter()
        .find(|row| music_bot::canon_server_addr(&row.serverAddr) == want))
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

/// Insert or replace the home channel for `server_addr`. `None` clears
/// it. The cap on the same row is left as it is; a new row has no cap.
pub async fn upsert_home(db: &Database, server_addr: &str, home: Option<u64>) -> Result<()> {
    let canon = music_bot::canon_server_addr(server_addr);
    let home = home
        .map(|home| {
            i64::try_from(home)
                .map_err(|_| anyhow::anyhow!("summon home channel {home} is outside i64"))
        })
        .transpose()?;
    let rows = list(db).await?;
    if let Some(existing) = rows
        .into_iter()
        .find(|row| music_bot::canon_server_addr(&row.serverAddr) == canon)
    {
        let sql = "UPDATE music_summon_cap SET homeChannel = $home, serverAddr = $canon WHERE serverAddr = $serverAddr;";
        db.query(sql)
            .bind(("home", home))
            .bind(("canon", canon))
            .bind(("serverAddr", existing.serverAddr))
            .await
            .context("music_summon_cap home update query failed")?
            .check()?;
    } else if home.is_some() {
        let sql =
            "CREATE music_summon_cap CONTENT { serverAddr: $serverAddr, homeChannel: $home };";
        db.query(sql)
            .bind(("serverAddr", canon))
            .bind(("home", home))
            .await
            .context("music_summon_cap home create query failed")?
            .check()?;
    }
    Ok(())
}

fn cap_as_u32(cap: i64) -> Result<u32> {
    u32::try_from(cap).map_err(|_| anyhow::anyhow!("stored summon cap {cap} is outside u32"))
}

fn home_as_u64(home: i64) -> Result<u64> {
    u64::try_from(home)
        .ok()
        .filter(|home| *home > 0)
        .ok_or_else(|| anyhow::anyhow!("stored summon home channel {home} is not a channel id"))
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
        assert_eq!(rows[0].cap, Some(4));
        assert_eq!(rows[1].cap, Some(0));
    }

    #[tokio::test]
    async fn the_home_shares_the_cap_row_and_is_not_invented() {
        let db = connect_in_memory().await.expect("connect");
        crate::db::migrations::run(&db).await.expect("migrations");

        assert_eq!(get_home(&db, "127.0.0.1:9987").await.expect("get"), None);
        // Clearing a home that was never picked writes nothing.
        upsert_home(&db, "127.0.0.1:9987", None)
            .await
            .expect("clear nothing");
        assert!(list(&db).await.expect("list").is_empty());

        // A home before any cap: one row, still no number.
        upsert_home(&db, "127.0.0.1:9987", Some(12))
            .await
            .expect("home");
        assert_eq!(get_home(&db, "127.0.0.1").await.expect("get"), Some(12));
        assert_eq!(get(&db, "127.0.0.1:9987").await.expect("cap"), None);

        // The cap lands on the same row and keeps the home.
        upsert(&db, "127.0.0.1:9987", 2).await.expect("cap");
        assert_eq!(get(&db, "127.0.0.1:9987").await.expect("cap"), Some(2));
        assert_eq!(
            get_home(&db, "127.0.0.1:9987").await.expect("get"),
            Some(12)
        );
        let rows = list(&db).await.expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].homeChannel, Some(12));

        // A new home keeps the cap. Clearing it keeps the cap too.
        upsert_home(&db, "127.0.0.1:9987", Some(30))
            .await
            .expect("move");
        assert_eq!(
            get_home(&db, "127.0.0.1:9987").await.expect("get"),
            Some(30)
        );
        upsert_home(&db, "127.0.0.1:9987", None)
            .await
            .expect("clear");
        assert_eq!(get_home(&db, "127.0.0.1:9987").await.expect("get"), None);
        assert_eq!(get(&db, "127.0.0.1:9987").await.expect("cap"), Some(2));

        assert_eq!(get_home(&db, "127.0.0.1:9988").await.expect("get"), None);
        assert!(
            upsert_home(&db, "127.0.0.1:9988", Some(u64::MAX))
                .await
                .is_err()
        );
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
