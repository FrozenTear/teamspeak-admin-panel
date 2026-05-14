//! Public widget routes (spec §7.28).
//!
//! Mounted at `/api/widget/{token}`. **No authentication** — the only
//! credential is the token in the URL. Rate-limit + CORS relax + cache
//! headers are applied per spec §7.28 / §7.29.
//!
//! Slice A shipped `/data`; Slice B ([PURA-87]) added `/image.svg`;
//! Slice C ([PURA-88]) adds `/image.png`. All three render off the same
//! 45 s `WidgetData` snapshot via [`resolve_widget_data`].
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
use crate::repos::{server_connections, video_sources, widgets as widget_repo};

use super::cache::CACHE_TTL;
use super::png;
use super::snapshot::{WidgetInputs, build_widget_data};
use super::svg;
use super::themes::theme_for;
use crate::web::widget_security::short_token;

const CACHE_CONTROL_VALUE: &str = "public, max-age=45";

/// Mount the public widget routes under `/api/widget/{token}/...`.
pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/widget/{token}/data", get(data_handler))
        .route("/api/widget/{token}/image.svg", get(svg_handler))
        .route("/api/widget/{token}/image.png", get(png_handler))
        // PURA-144 (WS-6) — derived read-only video-source view. WS-8
        // (public viewer mount) consumes this to render `<video>`
        // players for whichever sources are live on this widget's
        // server. Management fields (`url`, `created_by_user_id`,
        // `created_at`) are stripped — only the streaming-relevant
        // metadata is exposed.
        .route(
            "/api/widget/{token}/video-sources",
            get(video_sources_handler),
        )
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

/// `GET /api/widget/{token}/image.png`
///
/// Renders the Slice B SVG and rasterises it to a 400 px-wide PNG via
/// [`super::png::rasterise`]. Spec §27.4 graceful fallback: if the
/// rasteriser is unavailable (compile-time `widget-png-disabled` feature)
/// or fails at runtime, the route serves the SVG bytes at the same URL
/// with `Content-Type: image/svg+xml` and logs a `WARN`. The cache header
/// matches the JSON / SVG paths (`public, max-age=45`).
async fn png_handler(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let data = resolve_widget_data(&state, &token).await?;
    let theme = theme_for(WidgetThemeName::parse_or_default(&data.theme));
    let svg_body = svg::render(&data, theme);

    // CPU-bound work; hop off the runtime so a slow rasterise does not
    // stall other requests. The closure is `Send + 'static` since
    // `String` is the only captured payload.
    let svg_for_raster = svg_body.clone();
    let raster_result = tokio::task::spawn_blocking(move || png::rasterise(&svg_for_raster)).await;

    let cache_control = HeaderValue::from_static(CACHE_CONTROL_VALUE);
    match raster_result {
        Ok(Ok(bytes)) => {
            let mut response = (
                StatusCode::OK,
                [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
                bytes,
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, cache_control);
            Ok(response)
        }
        outcome => {
            // Spec §27.4 — fall back to SVG bytes at the PNG URL with
            // `image/svg+xml`. WARN once per call so operators can spot
            // a rasteriser regression without losing the request.
            match outcome {
                Ok(Err(e)) => {
                    tracing::warn!(
                        token_prefix = %short_token(&token),
                        error = %e,
                        "widget PNG rasterise failed; serving SVG fallback (spec §27.4)"
                    );
                }
                Err(join_err) => {
                    tracing::warn!(
                        token_prefix = %short_token(&token),
                        error = %join_err,
                        "widget PNG rasterise task panicked; serving SVG fallback (spec §27.4)"
                    );
                }
                Ok(Ok(_)) => unreachable!("Ok(Ok(_)) handled in the matching arm above"),
            }
            let mut response = (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("image/svg+xml"),
                )],
                svg_body,
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, cache_control);
            Ok(response)
        }
    }
}

// ---------------------------------------------------------------------
// PURA-144 (WS-6) — derived public video-sources view.
// ---------------------------------------------------------------------

/// One entry in the public video-sources list. Strips every management
/// field — operators consume the rich shape via `/api/video-sources`,
/// public viewers only need enough metadata to wire up a MoQ subscribe
/// and render a label.
#[derive(Debug, Serialize)]
pub struct PublicVideoSource {
    pub source_id: String,
    pub label: String,
    pub preset: String,
    pub status: String,
    pub track: PublicTrack,
}

#[derive(Debug, Serialize)]
pub struct PublicTrack {
    pub namespace: String,
    pub video: String,
    pub audio: String,
}

/// Wire shape returned by `GET /api/widget/{token}/video-sources`. The
/// `relay_url` field is the public WebTransport endpoint of the sidecar
/// (sourced from `MOQ_PUBLIC_URL`) — without it the embedded viewer has
/// no way to dial the moq-lite relay. `None` when the operator has not
/// configured a public relay URL; the public viewer falls back to
/// "No live video" in that case.
#[derive(Debug, Serialize)]
pub struct PublicVideoSourcesResponse {
    /// Public WebTransport URL for the moq-lite-04 relay
    /// (e.g. `https://stream.example.com:4443/anon`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
    pub sources: Vec<PublicVideoSource>,
}

/// `GET /api/widget/{token}/video-sources`. No auth — the widget token
/// is the only credential, exactly like `/data` and `/image.*`. Returns
/// an empty list when the token resolves but no sources are configured
/// for the underlying server; 404 only when the token itself is
/// unknown / revoked.
async fn video_sources_handler(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let widget = match widget_repo::find_by_token(&state.db, &token).await {
        Ok(Some(w)) => w,
        Ok(None) => {
            tracing::debug!(token_prefix = %short_token(&token), "widget token miss");
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

    let rows = video_sources::list_for_server(&state.db, widget.serverConfigId)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, server_id = widget.serverConfigId, "video_sources list failed");
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorBody {
                    error: "Internal server error".into(),
                    code: None,
                    details: None,
                },
            )
        })?;

    let public: Vec<PublicVideoSource> = rows
        .into_iter()
        .map(|r| PublicVideoSource {
            track: PublicTrack {
                namespace: r.sourceId.clone(),
                video: "video".into(),
                audio: "audio".into(),
            },
            source_id: r.sourceId,
            label: r.label,
            preset: r.preset,
            status: r.status,
        })
        .collect();

    let response_body = PublicVideoSourcesResponse {
        relay_url: state.moq_public_url.clone(),
        sources: public,
    };
    let body = serde_json::to_vec(&response_body).map_err(|e| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorBody {
                error: "Internal server error".into(),
                code: None,
                details: Some(format!("video_sources serialise failed: {e}")),
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

/// Shared cache+upstream lookup driver. Slice B (`image.svg`) and Slice C
/// (`image.png`) consume the same path so all three formats render from
/// one snapshot — and a single 45 s TTL covers the JSON view, the SVG, and
/// the rasterised PNG together.
pub async fn resolve_widget_data(state: &AppState, token: &str) -> Result<WidgetData, Response> {
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
    state
        .widget_cache
        .insert(widget.token.clone(), data.clone())
        .await;

    // Reference `CACHE_TTL` so a future tweak forces this module to be
    // re-checked alongside the constant.
    let _: std::time::Duration = CACHE_TTL;

    Ok(data)
}

// Spec §26.1 — never log full tokens above DEBUG. The redaction helper
// `short_token` lives in [`crate::web::widget_security`] (Slice F) and is
// reused here for INFO/WARN log lines. Tests for the helper itself live
// alongside the implementation.

#[cfg(test)]
mod tests {
    //! PURA-146 (WS-8) — assert that the public video-sources surface
    //! never leaks management metadata. The internal `/api/video-sources`
    //! shape (`VideoSourceView`) carries `url`, `created_by_user_id`, and
    //! `created_at`; the public wrapper here must drop all three.
    use super::*;

    #[test]
    fn public_video_source_serialisation_omits_url_and_management_fields() {
        let row = PublicVideoSource {
            source_id: "src-42".into(),
            label: "Lobby cam".into(),
            preset: "720p".into(),
            status: "live".into(),
            track: PublicTrack {
                namespace: "src-42".into(),
                video: "video".into(),
                audio: "audio".into(),
            },
        };
        let response = PublicVideoSourcesResponse {
            relay_url: Some("https://stream.example.com:4443/anon".into()),
            sources: vec![row],
        };
        let json = serde_json::to_string(&response).expect("serialise");
        // None of the operator-side fields may appear in the wire shape.
        for forbidden in [
            "\"url\":",
            "\"createdByUserId\":",
            "\"created_by_user_id\":",
            "\"createdAt\":",
            "\"created_at\":",
            // The internal numeric PK never leaks either — public callers
            // identify a stream by `source_id`, not by `id`.
            "\"id\":",
        ] {
            assert!(
                !json.contains(forbidden),
                "public JSON unexpectedly contains `{forbidden}` field: {json}"
            );
        }
        // Spot-check the fields that MUST be present.
        for required in [
            "\"relay_url\":",
            "\"sources\":",
            "\"source_id\":\"src-42\"",
            "\"label\":\"Lobby cam\"",
            "\"track\":",
            "\"namespace\":\"src-42\"",
            "\"video\":\"video\"",
            "\"audio\":\"audio\"",
        ] {
            assert!(
                json.contains(required),
                "public JSON missing expected `{required}`: {json}"
            );
        }
    }

    #[test]
    fn relay_url_is_omitted_when_none() {
        let response = PublicVideoSourcesResponse {
            relay_url: None,
            sources: vec![],
        };
        let json = serde_json::to_string(&response).expect("serialise");
        // `serde(skip_serializing_if = "Option::is_none")` — the key must
        // not appear at all so the FE can default to "No live video".
        assert!(
            !json.contains("relay_url"),
            "relay_url must be elided when None (got `{json}`)"
        );
        assert!(json.contains("\"sources\":[]"));
    }
}
