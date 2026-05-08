//! Public widget routes (spec §7.28).
//!
//! Mounted at `/api/widget/{token}`. **No authentication** — the only
//! credential is the token in the URL. Rate-limit + CORS relax + cache
//! headers are applied per spec §7.28 / §7.29.
//!
//! Slice A (this commit) ships only `GET /api/widget/{token}/data`. The
//! `image.svg` and `image.png` endpoints land in [PURA-72-B] and [PURA-72-C].
//!
//! Caching:
//!
//! - 45 s in-memory TTL keyed by token (see [`super::cache::WidgetCache`]).
//! - Response carries `Cache-Control: public, max-age=45` so a downstream
//!   CDN can cache as well. Operator mutation (`PATCH` / `DELETE` /
//!   `regenerate-token`) invalidates the in-memory entry; the CDN window
//!   may serve stale data for up to 45 s and that is the spec-mandated
//!   behaviour.
//! - 404 on unknown / revoked tokens.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Serialize;
use ts6_manager_shared::widgets::{WidgetData, WidgetThemeName};

use crate::app_state::AppState;
use crate::control::ControlBackendError;
use crate::repos::{server_connections, widgets as widget_repo};

use super::cache::CACHE_TTL;
use super::snapshot::{WidgetInputs, build_widget_data};
use super::svg;
use super::themes::theme_for;

const CACHE_CONTROL_VALUE: &str = "public, max-age=45";

/// Mount the public widget routes under `/api/widget/{token}/...`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/widget/{token}/data", get(data_handler))
        .route("/api/widget/{token}/image.svg", get(svg_handler))
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<String>,
}

fn error_response(status: StatusCode, body: ErrorBody) -> Response {
    (status, axum::Json(body)).into_response()
}

fn translate_control_error(err: ControlBackendError) -> Response {
    let status = err.http_status();
    let body = match status {
        StatusCode::BAD_GATEWAY => ErrorBody {
            error: "TeamSpeak API Error".into(),
            code: Some(err.upstream_code()),
            details: Some(err.upstream_message()),
        },
        _ => ErrorBody {
            error: "Internal server error".into(),
            code: None,
            details: None,
        },
    };
    error_response(status, body)
}

/// `GET /api/widget/{token}/data`
async fn data_handler(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let data = resolve_widget_data(&state, &token).await?;
    let body = serde_json::to_vec(&data).map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorBody {
                error: "Internal server error".into(),
                code: None,
                details: Some(format!("widget JSON serialise failed: {e}")),
            },
        )
    })?;
    let mut response = (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_VALUE),
    );
    Ok(response)
}

/// `GET /api/widget/{token}/image.svg`
///
/// Renders the same `WidgetData` snapshot that backs `/data`, so JSON and
/// SVG share a single 45 s cache window. Theme is resolved via
/// [`super::themes::theme_for`] from the snapshot's `theme` string. The
/// renderer emits a self-contained SVG document with the SPA's font stack
/// embedded as a `font-family` attribute on the root `<svg>`.
async fn svg_handler(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let data = resolve_widget_data(&state, &token).await?;
    let theme = theme_for(WidgetThemeName::parse_or_default(&data.theme));
    let body = svg::render(&data, theme);
    let mut response = (
        StatusCode::OK,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/svg+xml"),
        )],
        body,
    )
        .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_VALUE),
    );
    Ok(response)
}

/// Shared cache+upstream lookup driver. Slice B (`image.svg`) and Slice C
/// (`image.png`) consume the same path so all three formats render from
/// one snapshot — and a single 45 s TTL covers the JSON view, the SVG, and
/// the rasterised PNG together.
pub async fn resolve_widget_data(
    state: &AppState,
    token: &str,
) -> Result<WidgetData, Response> {
    if let Some(cached) = state.widget_cache.get(token).await {
        return Ok(cached);
    }

    // 1. Resolve the token. Unknown / revoked → 404. Logged at DEBUG only —
    //    spec §26.1: "MUST NOT log full tokens at any level above debug".
    let widget = match widget_repo::find_by_token(&state.db, token).await {
        Ok(Some(w)) => w,
        Ok(None) => {
            tracing::debug!(token_prefix = %short_token(token), "widget token miss");
            return Err(error_response(
                StatusCode::NOT_FOUND,
                ErrorBody {
                    error: "Not found".into(),
                    code: None,
                    details: None,
                },
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "widget repo lookup failed");
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody {
                    error: "Internal server error".into(),
                    code: None,
                    details: None,
                },
            ));
        }
    };

    // 2. Resolve the underlying server connection. A widget that points at
    //    a deleted server returns 404 (the operator deleted the upstream;
    //    the public surface should not leak that distinction).
    let connection = match server_connections::find_by_id(&state.db, widget.serverConfigId).await {
        Ok(Some(c)) => c,
        Ok(None) => {
            tracing::debug!(
                widget_id = widget.id,
                server_config_id = widget.serverConfigId,
                "widget points at deleted server connection"
            );
            return Err(error_response(
                StatusCode::NOT_FOUND,
                ErrorBody {
                    error: "Not found".into(),
                    code: None,
                    details: None,
                },
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "server_connections lookup failed");
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody {
                    error: "Internal server error".into(),
                    code: None,
                    details: None,
                },
            ));
        }
    };

    let backend = state
        .control
        .get_or_build(widget.serverConfigId, Some(&connection))
        .await
        .map_err(translate_control_error)?;

    // 3. Fan out the three §7.19-style upstream calls in parallel.
    let sid = widget.virtualServerId;
    let (server, channels, clients) = tokio::try_join!(
        backend.serverinfo(sid),
        backend.channellist(sid),
        backend.clientlist(sid),
    )
    .map_err(translate_control_error)?;

    let inputs = WidgetInputs {
        server,
        channels,
        clients,
    };
    let data = build_widget_data(&widget, inputs);

    // 4. Cache for [`CACHE_TTL`] (45 s) keyed by the widget's *current* token.
    //    A regenerate-token call invalidates this entry by the old token, so
    //    the new token starts cold (correct — different URL, fresh state).
    state.widget_cache.insert(widget.token.clone(), data.clone()).await;

    // Reference `CACHE_TTL` so a future tweak forces this module to be
    // re-checked alongside the constant.
    let _: std::time::Duration = CACHE_TTL;

    Ok(data)
}

/// Spec §26.1 — never log full tokens above DEBUG. This helper renders the
/// first 4 chars + "…" so tracing fields stay searchable without leaking
/// the credential. Used by INFO/WARN log lines.
fn short_token(token: &str) -> String {
    let mut chars: Vec<char> = token.chars().take(4).collect();
    if token.chars().count() > 4 {
        chars.push('…');
    }
    chars.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_token_truncates_long_inputs() {
        assert_eq!(short_token("abcdefgh"), "abcd…");
    }

    #[test]
    fn short_token_passes_short_inputs_through() {
        assert_eq!(short_token("abc"), "abc");
        assert_eq!(short_token("abcd"), "abcd");
    }
}
