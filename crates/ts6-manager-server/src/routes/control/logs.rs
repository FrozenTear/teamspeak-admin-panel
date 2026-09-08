//! `GET /api/servers/{configId}/vs/{sid}/logs?after=…&severity=…&lines=…`
//! — `logview` tail. PURA-71.
//!
//! - `after` — pass the previous response's `last_pos` to page forward.
//! - `lines` — capped to `TS_LOGVIEW_MAX_LINES` (100). TeamSpeak's
//!   `logview` rejects anything larger with error 1541 (`invalid
//!   parameter size`); REST never asks TS for more than that.
//! - `severity` — substring filter on the line text. The TS `logview`
//!   upstream does not support filtering, so we filter on egress. This
//!   means `lines` is the page size BEFORE filtering — undersized
//!   responses are expected when severity is set.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::response::Response;
use ts6_manager_shared::control::{LogLine, LogTailQuery, LogTailResponse};

use crate::app_state::AppState;
use crate::auth::extractors::RequireServerAccess;

use super::{access, translate_control_error};

/// TeamSpeak `logview` rejects `lines` above 100 with error 1541
/// (`invalid parameter size`). REST never asks TS for more than this.
const TS_LOGVIEW_MAX_LINES: u32 = 100;
const DEFAULT_LOG_LINES: u32 = TS_LOGVIEW_MAX_LINES;
/// REST query cap. Same as `TS_LOGVIEW_MAX_LINES` so the documented
/// page size matches what we actually send upstream.
const MAX_LOG_LINES: u32 = TS_LOGVIEW_MAX_LINES;

fn clamp_logview_lines(requested: Option<u32>) -> u32 {
    requested
        .unwrap_or(DEFAULT_LOG_LINES)
        .min(MAX_LOG_LINES)
        .min(TS_LOGVIEW_MAX_LINES)
}

pub async fn tail(
    State(state): State<AppState>,
    RequireServerAccess { user, .. }: RequireServerAccess,
    Path((config_id, sid)): Path<(i64, i64)>,
    Query(query): Query<LogTailQuery>,
) -> Result<Json<LogTailResponse>, Response> {
    let connection = access::check_read(&state, &user, config_id).await?;
    let client = state
        .control
        .get_or_build(connection.id, Some(&connection))
        .await
        .map_err(translate_control_error)?;

    let lines = clamp_logview_lines(query.lines);

    let entries = client
        .logview(
            sid,
            lines.min(TS_LOGVIEW_MAX_LINES),
            true,
            false,
            query.after,
        )
        .await
        .map_err(translate_control_error)?;

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

#[cfg(test)]
mod clamp_tests {
    use super::*;

    #[test]
    fn defaults_to_ts_logview_max() {
        assert_eq!(clamp_logview_lines(None), TS_LOGVIEW_MAX_LINES);
    }

    #[test]
    fn passes_through_values_at_or_below_max() {
        assert_eq!(clamp_logview_lines(Some(1)), 1);
        assert_eq!(
            clamp_logview_lines(Some(TS_LOGVIEW_MAX_LINES)),
            TS_LOGVIEW_MAX_LINES
        );
    }

    #[test]
    fn clamps_oversize_to_ts_logview_max() {
        // The Contabo Logs page used to request 200; TS rejects that
        // with 1541 (`invalid parameter size`).
        assert_eq!(clamp_logview_lines(Some(200)), TS_LOGVIEW_MAX_LINES);
        assert_eq!(clamp_logview_lines(Some(500)), TS_LOGVIEW_MAX_LINES);
        assert_eq!(clamp_logview_lines(Some(u32::MAX)), TS_LOGVIEW_MAX_LINES);
    }
}
