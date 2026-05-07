//! `RadioStation` repo (spec §4.2.14).
//!
//! The SSRF guard on `url` runs in the REST handler (§9). The repo treats
//! the URL as an opaque string.

#![allow(non_snake_case)]

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct RadioStation {
    pub id: i64,
    pub name: String,
    pub url: String,
    pub genre: Option<String>,
    pub imageUrl: Option<String>,
    pub serverConfigId: i64,
    pub createdAt: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewRadioStation {
    pub name: String,
    pub url: String,
    pub genre: Option<String>,
    pub imageUrl: Option<String>,
    pub serverConfigId: i64,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    name,
    url,
    genre,
    imageUrl,
    serverConfigId,
    createdAt
";

pub async fn insert(db: &Database, new: NewRadioStation) -> Result<RadioStation> {
    let sql = format!(
        "CREATE type::record('radio_station', sequence::nextval('radio_station_id'))
            CONTENT {{
                name: $name,
                url: $url,
                genre: $genre,
                imageUrl: $imageUrl,
                serverConfigId: $serverConfigId
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("name", new.name))
        .bind(("url", new.url))
        .bind(("genre", new.genre))
        .bind(("imageUrl", new.imageUrl))
        .bind(("serverConfigId", new.serverConfigId))
        .await
        .context("radio_station insert query failed")?
        .check()?;
    let row: Option<RadioStation> = resp.take(0)?;
    row.context("radio_station insert returned no row")
}

pub async fn find_by_id(db: &Database, id: i64) -> Result<Option<RadioStation>> {
    let sql = format!("SELECT {PROJECTION} FROM type::record('radio_station', $id);");
    let mut resp = db.query(sql).bind(("id", id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn list_for_server(db: &Database, server_config_id: i64) -> Result<Vec<RadioStation>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM radio_station WHERE serverConfigId = $sid ORDER BY id ASC;"
    );
    let mut resp = db.query(sql).bind(("sid", server_config_id)).await?.check()?;
    Ok(resp.take(0)?)
}

pub async fn delete(db: &Database, id: i64) -> Result<()> {
    let sql = "DELETE type::record('radio_station', $id);";
    db.query(sql).bind(("id", id)).await?.check()?;
    Ok(())
}
