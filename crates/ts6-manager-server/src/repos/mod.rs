//! Typed CRUD per spec Chapter 4 entity (PURA-10 thin slice).
//!
//! Each module wraps the SurrealDB client with a typed surface that matches
//! the wire-shape JSON keys verbatim per Chapter 7 §7.1. Repos own no state;
//! they accept `&Database` and return owned data.
//!
//! Slice 1 ships the four entities [PURA-4](/PURA/issues/PURA-4) needs to
//! start refresh-token reuse-detection-by-family work. The remaining ~13
//! entities land via the slice-2 follow-up.

#![allow(dead_code)] // consumed by SECURITY (PURA-4) follow-up slices and integration tests

use anyhow::{Result, bail};
use surrealdb::types::{RecordId, RecordIdKey};

pub mod app_settings;
pub mod bot_execution_logs;
pub mod bot_executions;
pub mod bot_flow_runs;
pub mod bot_flows;
pub mod bot_variables;
pub mod music_bots;
pub mod music_requests;
pub mod playlist_songs;
pub mod playlists;
pub mod radio_stations;
pub mod refresh_tokens;
pub mod server_connections;
pub mod server_user_grants;
pub mod setup;
pub mod songs;
pub mod ssh_audit_log;
pub mod stream_sessions;
pub mod users;
pub mod video_sources;
pub mod widgets;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_chapter4;

/// Pull the integer portion out of a SurrealDB record id (e.g. the `1` in
/// `user:1`). Spec §4.1 mandates `int` primary keys; the migrations pin each
/// new record's id to a `sequence::nextval(...)` int, so this conversion is
/// total in practice. Returns an error if the row's id is not numeric — that
/// would only happen for hand-written records that bypass the repo layer.
fn record_id_to_i64(id: &RecordId) -> Result<i64> {
    match &id.key {
        RecordIdKey::Number(n) => Ok(*n),
        other => bail!(
            "record id `{id:?}` has non-int key (got {other:?}); priority-slice tables must use sequence::nextval"
        ),
    }
}
