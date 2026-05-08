//! `GET /api/servers/{configId}/vs/{sid}/logs?after=…&severity=…&lines=…`
//! — `logview` tail. PURA-71.
//!
//! - `after` — pass the previous response's `last_pos` to page forward.
//! - `lines` — capped to `MAX_LOG_LINES`. The TS upstream caps at 100 per
//!   call anyway; we hard-stop at 500 so the route never asks for a
//!   pathological page size.
//! - `severity` — substring filter on the line text. The TS `logview`
//!   upstream does not support filtering, so we filter on egress. This
//!   means `lines` is the page size BEFORE filtering — undersized
//!   responses are expected when severity is set.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use ts6_manager_shared::control::{LogLine, LogTailQuery, LogTailResponse};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;

use super::{access, translate_webquery_error};

const DEFAULT_LOG_LINES: u32 = 100;
const MAX_LOG_LINES: u32 = 500;

pub async fn tail(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
    Path((config_id, sid)): Path<(i64, i64)>,
    Query(query): Query<LogTailQuery>,
) -> Result<Json<LogTailResponse>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = state
        .webquery
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_webquery_error)?;

    let lines = query
        .lines
        .map(|n| n.min(MAX_LOG_LINES))
        .unwrap_or(DEFAULT_LOG_LINES);

    let entries = client
        .logview(sid, lines, true, false, query.after)
        .await
        .map_err(translate_webquery_error)?;

    // Carry forward `last_pos` / `file_size` from the upstream's first
    // row. The TS `logview` shape only emits these on the leading entry.
    let mut last_pos = None;
    let mut file_size = None;
    let mut out_lines = Vec::with_capacity(entries.len());
    for entry in entries {
        if last_pos.is_none() && entry.last_pos.is_some() {
            last_pos = entry.last_pos;
        }
        if file_size.is_none() && entry.file_size.is_some() {
            file_size = entry.file_size;
        }
        if !entry.l.is_empty() {
            out_lines.push(LogLine { text: entry.l });
        }
    }

    if let Some(needle) = query.severity.as_deref() {
        let needle_lower = needle.to_ascii_lowercase();
        out_lines.retain(|l| l.text.to_ascii_lowercase().contains(&needle_lower));
    }

    Ok(Json(LogTailResponse {
        last_pos,
        file_size,
        lines: out_lines,
    }))
}
