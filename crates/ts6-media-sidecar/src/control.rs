//! WS-3 mutating control plane — `POST /source`, `POST /source/stop`,
//! `GET /track/{source_id}`. Plus the registry + DTOs that back them.
//!
//! Per-impl-plan §3.10 + PURA-141, this is the **REST replacement for
//! spec Ch.24 `/peer/*`** under deviation D6 (MoQ instead of WebRTC).
//! Subscribers don't speak HTTP here — they speak `moq-lite-04` over
//! WebTransport against the QUIC listener (`transport.rs`). The HTTP
//! plane is operator-facing only.
//!
//! Pipeline registry uses `tokio::sync::RwLock<HashMap<String, Pipeline>>`,
//! matching `SidecarOrigin::broadcasts` so we don't introduce a second
//! locking style (DashMap would mean another crate + a different
//! await-or-not contract). The reader path (`GET /track`, `GET /stats`)
//! takes a read lock and copies the small per-source projection; the
//! writer path (`POST /source` / `POST /source/stop`) takes a write lock
//! across the insert/remove so a duplicate `source_id` race is impossible.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{info, warn};
use ts6_ssrf::{Resolver, SsrfError, is_url_allowed};

use crate::http_pin::{PinProxy, PinnedTarget as PinProxyTarget};
use crate::origin::SidecarOrigin;
use crate::pipeline::{
    Pipeline, PipelineConfig, PipelineMetrics, SourceInput, TRACK_AUDIO, TRACK_VIDEO,
};
use crate::preset::QualityPreset;

/// HTTP-side shared state for the WS-3 mutating endpoints. Cheap to
/// clone (the inner [`PipelineRegistry`] is an `Arc<RwLock<…>>`).
#[derive(Clone)]
pub struct ControlPlaneState {
    pub origin: Arc<SidecarOrigin>,
    pub registry: PipelineRegistry,
    pub resolver: Arc<dyn Resolver>,
    pub ffmpeg_path: std::path::PathBuf,
    /// PURA-172 — IP-pin proxy for plaintext-HTTP FFmpeg fetches. The
    /// control plane registers one token per HTTP source against this proxy
    /// and rewrites the FFmpeg-facing URL to `http://127.0.0.1:<port>/<token>`.
    /// On `POST /source/stop` the token is burned so a leaked proxy URL
    /// can't replay past the pipeline's lifetime.
    pub pin_proxy: Arc<PinProxy>,
}

/// In-memory pipeline registry. `source_id → PipelineEntry`. The
/// `source_id` is also the broadcast name registered against
/// [`SidecarOrigin`], so the same key works on both sides of the system.
///
/// The entry carries an optional `pin_token` so `POST /source/stop` can
/// burn the proxy registration (PURA-172) without a separate map.
#[derive(Clone, Default)]
pub struct PipelineRegistry {
    inner: Arc<RwLock<HashMap<String, PipelineEntry>>>,
}

/// One row in the [`PipelineRegistry`]. Carries the live pipeline + an
/// optional PURA-172 proxy token. The token is `Some` for plaintext-HTTP
/// sources that were routed through the IP-pin proxy, `None` for HTTPS
/// (no proxy needed — TLS already pins) and synthetic lavfi sources.
pub(crate) struct PipelineEntry {
    pub(crate) pipeline: Pipeline,
    pub(crate) pin_token: Option<String>,
}

impl PipelineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> Vec<SourceStatsSnapshot> {
        let guard = self.inner.read().await;
        let mut out: Vec<SourceStatsSnapshot> = guard
            .iter()
            .map(|(id, entry)| {
                SourceStatsSnapshot::from_pipeline(
                    id,
                    entry.pipeline.preset(),
                    &entry.pipeline.metrics(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.source_id.cmp(&b.source_id));
        out
    }

    pub async fn lookup_track(&self, source_id: &str) -> Option<TrackDescriptor> {
        let guard = self.inner.read().await;
        guard
            .get(source_id)
            .map(|_| TrackDescriptor::for_source(source_id))
    }
}

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

/// `POST /source` body. `source_id` is server-generated when absent;
/// `preset` selects an encoding profile (WS-4 / PURA-142). Wire type is
/// a raw string so an unknown value lands as an [`ApiError::InvalidRequest`]
/// (HTTP 400 in the WS-3 error model) instead of an Axum
/// `JsonRejection` wrapped in a different body shape. The handler maps
/// the string to [`QualityPreset`] via case-insensitive parse, defaulting
/// to [`QualityPreset::DEFAULT`] (= `"720p"`) when the field is missing
/// or `null`.
#[derive(Debug, Deserialize)]
pub struct StartSourceRequest {
    pub url: String,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub preset: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StartSourceResponse {
    pub source_id: String,
    pub track: TrackDescriptor,
}

/// `POST /source/stop` body.
#[derive(Debug, Deserialize)]
pub struct StopSourceRequest {
    pub source_id: String,
}

/// Wire shape for "where do I subscribe to this source's media tracks".
///
/// `namespace` mirrors the moq-lite broadcast path; `video` + `audio` are
/// the moq-lite track names inside that broadcast (`pipeline.rs`
/// hardcodes them so the WS-0 reference player can subscribe without
/// configuration).
#[derive(Debug, Clone, Serialize)]
pub struct TrackDescriptor {
    pub namespace: String,
    pub video: String,
    pub audio: String,
}

impl TrackDescriptor {
    pub fn for_source(source_id: &str) -> Self {
        Self {
            namespace: source_id.to_string(),
            video: TRACK_VIDEO.to_string(),
            audio: TRACK_AUDIO.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// /stats projection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SourceStatsSnapshot {
    pub source_id: String,
    /// Encoding preset this source was started with. Serialises as
    /// `"480p"`, `"720p"`, or `"1080p"` — same string the operator
    /// passed (or defaulted into) on `POST /source`.
    pub preset: QualityPreset,
    pub video: TrackStatsSnapshot,
    pub audio: TrackStatsSnapshot,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TrackStatsSnapshot {
    pub frames_published: u64,
    pub bytes_published: u64,
    pub ffmpeg_alive: bool,
}

impl SourceStatsSnapshot {
    fn from_pipeline(source_id: &str, preset: QualityPreset, metrics: &PipelineMetrics) -> Self {
        Self {
            source_id: source_id.to_string(),
            preset,
            video: TrackStatsSnapshot {
                frames_published: metrics.video.frames_published.load(Ordering::Relaxed),
                bytes_published: metrics.video.bytes_published.load(Ordering::Relaxed),
                ffmpeg_alive: metrics.video.ffmpeg_alive.load(Ordering::Relaxed),
            },
            audio: TrackStatsSnapshot {
                frames_published: metrics.audio.frames_published.load(Ordering::Relaxed),
                bytes_published: metrics.audio.bytes_published.load(Ordering::Relaxed),
                ffmpeg_alive: metrics.audio.ffmpeg_alive.load(Ordering::Relaxed),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("ssrf_blocked")]
    SsrfBlocked(#[source] SsrfError),
    #[error("invalid_request: {0}")]
    InvalidRequest(String),
    #[error("source_id_already_running")]
    AlreadyRunning,
    #[error("unknown_source_id")]
    UnknownSource,
    #[error("internal: {0}")]
    Internal(#[source] anyhow::Error),
}

#[derive(Debug, Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            ApiError::SsrfBlocked(_) => StatusCode::BAD_REQUEST,
            ApiError::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::AlreadyRunning => StatusCode::CONFLICT,
            ApiError::UnknownSource => StatusCode::NOT_FOUND,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_code(&self) -> &'static str {
        match self {
            ApiError::SsrfBlocked(_) => "ssrf_blocked",
            ApiError::InvalidRequest(_) => "invalid_request",
            ApiError::AlreadyRunning => "source_id_already_running",
            ApiError::UnknownSource => "unknown_source_id",
            ApiError::Internal(_) => "internal",
        }
    }

    fn detail(&self) -> Option<String> {
        match self {
            ApiError::SsrfBlocked(e) => Some(e.to_string()),
            ApiError::InvalidRequest(d) => Some(d.clone()),
            ApiError::AlreadyRunning | ApiError::UnknownSource => None,
            ApiError::Internal(e) => Some(e.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status();
        let body = ErrorBody {
            error: self.error_code(),
            detail: self.detail(),
        };
        (status, Json(body)).into_response()
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn post_source(
    State(state): State<ControlPlaneState>,
    Json(req): Json<StartSourceRequest>,
) -> Result<(StatusCode, Json<StartSourceResponse>), ApiError> {
    if req.url.trim().is_empty() {
        return Err(ApiError::InvalidRequest("url is required".into()));
    }

    let source_id = req
        .source_id
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    if let Some(err) = invalid_source_id(&source_id) {
        return Err(ApiError::InvalidRequest(err));
    }

    let pinned = is_url_allowed(&req.url, state.resolver.as_ref())
        .await
        .map_err(ApiError::SsrfBlocked)?;

    // PURA-149 → PURA-172: closing the rebinding window for plaintext HTTP.
    //
    // PURA-149 reverted "rewrite URL host to IP literal" because FFmpeg's
    // own DNS at fetch time can diverge from the IP `ts6-ssrf` validated
    // (the DNS rebinding window R6 names), AND because IP-literal SNI /
    // `Host:` breaks every virtual-hosted CDN.
    //
    // For HTTPS we leave the URL alone: TLS hostname validation already
    // binds the connection to the cert SAN, which is a stronger guarantee
    // than IP-pinning.
    //
    // For plaintext HTTP we route FFmpeg through the sidecar-internal
    // IP-pin proxy (`crate::http_pin::PinProxy`): the proxy's reqwest
    // client uses `resolve_to_addrs` to force the upstream socket to
    // `pinned.resolved_ip` while preserving the original `Host:` header.
    // FFmpeg receives `http://127.0.0.1:<port>/<token>` and never speaks
    // to the outside resolver again. The token is single-use across the
    // pipeline lifetime — `POST /source/stop` burns it.
    let (ffmpeg_url, pin_token) = match pin_token_for(&pinned, &state.pin_proxy).await {
        Some((url, tok)) => (url, Some(tok)),
        None => (pinned.url.to_string(), None),
    };

    let mut guard = state.registry.inner.write().await;
    if guard.contains_key(&source_id) {
        // Burn the token we just registered — the pipeline isn't going to
        // start, so the proxy must not hold a stale entry.
        if let Some(tok) = pin_token {
            state.pin_proxy.registry.deregister(&tok).await;
        }
        return Err(ApiError::AlreadyRunning);
    }

    // Default = 720p when the caller omits `preset` or sends `null`
    // (spec §23.4 / PURA-142 AC). Unknown strings fail with the WS-3
    // error model, not Axum's default JSON-rejection shape.
    let preset = match req.preset.as_deref() {
        None => QualityPreset::DEFAULT,
        Some(s) => s.parse::<QualityPreset>().map_err(|e| {
            // Same cleanup as AlreadyRunning — we eagerly registered
            // the token before validating the preset.
            let tok = pin_token.clone();
            let proxy = state.pin_proxy.clone();
            if let Some(t) = tok {
                tokio::spawn(async move {
                    proxy.registry.deregister(&t).await;
                });
            }
            ApiError::InvalidRequest(e.to_string())
        })?,
    };
    let cfg = PipelineConfig::new(source_id.clone(), SourceInput::Url(ffmpeg_url.clone()))
        .with_ffmpeg_path(state.ffmpeg_path.clone())
        .with_preset(preset);

    let pipeline = match Pipeline::start(cfg, state.origin.clone()).await {
        Ok(p) => p,
        Err(err) => {
            if let Some(tok) = pin_token.as_deref() {
                state.pin_proxy.registry.deregister(tok).await;
            }
            return Err(ApiError::Internal(err));
        }
    };

    guard.insert(
        source_id.clone(),
        PipelineEntry {
            pipeline,
            pin_token: pin_token.clone(),
        },
    );
    info!(
        %source_id,
        url = %req.url,
        ffmpeg_url = %ffmpeg_url,
        proxied = %pin_token.is_some(),
        %preset,
        "pipeline registered"
    );

    Ok((
        StatusCode::CREATED,
        Json(StartSourceResponse {
            source_id: source_id.clone(),
            track: TrackDescriptor::for_source(&source_id),
        }),
    ))
}

pub async fn post_source_stop(
    State(state): State<ControlPlaneState>,
    Json(req): Json<StopSourceRequest>,
) -> Result<StatusCode, ApiError> {
    let source_id = req.source_id.trim().to_string();
    if source_id.is_empty() {
        return Err(ApiError::InvalidRequest("source_id is required".into()));
    }

    let entry = {
        let mut guard = state.registry.inner.write().await;
        guard.remove(&source_id)
    };

    match entry {
        Some(entry) => {
            // PURA-172 — burn the proxy token before stopping the
            // pipeline so a leaked `http://127.0.0.1:<port>/<token>` URL
            // cannot replay after the pipeline ends (the single-use AC).
            // Done first so even if `Pipeline::stop` panics or wedges, the
            // token is already invalidated.
            if let Some(tok) = entry.pin_token.as_deref() {
                state.pin_proxy.registry.deregister(tok).await;
            }
            entry.pipeline.stop().await;
            info!(%source_id, "pipeline stopped");
            Ok(StatusCode::NO_CONTENT)
        }
        None => {
            warn!(%source_id, "stop requested for unknown source_id");
            Err(ApiError::UnknownSource)
        }
    }
}

/// Decide whether `pinned` should be routed through the IP-pin proxy and
/// return the FFmpeg-facing URL + the proxy token, or `None` if the
/// caller should use `pinned.url` directly.
///
/// HTTPS sources are passed through unchanged — TLS hostname validation
/// already binds the connection to the cert SAN, which is a stronger
/// guarantee than IP-pinning. Plaintext HTTP sources with a `resolved_ip`
/// from `ts6-ssrf` are proxied. HTTP sources whose host could not be
/// resolved (`resolved_ip` is `None` — spec §9.3 NXDOMAIN passthrough)
/// also fall back to the direct URL because there is no IP to pin to.
async fn pin_token_for(
    pinned: &ts6_ssrf::PinnedTarget,
    proxy: &PinProxy,
) -> Option<(String, String)> {
    if pinned.url.scheme() != "http" {
        return None;
    }
    let resolved_ip = pinned.resolved_ip?;
    let target = PinProxyTarget {
        upstream_url: pinned.url.clone(),
        host: pinned.host.clone(),
        resolved_ip,
        port: pinned.port,
    };
    let token = proxy.registry.register(target).await;
    let url = proxy.proxy_url(&token);
    Some((url, token))
}

pub async fn get_track(
    State(state): State<ControlPlaneState>,
    Path(source_id): Path<String>,
) -> Result<Json<StartSourceResponse>, ApiError> {
    match state.registry.lookup_track(&source_id).await {
        Some(track) => Ok(Json(StartSourceResponse { source_id, track })),
        None => Err(ApiError::UnknownSource),
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Reject operator-supplied source IDs containing characters that would
/// break MoQ broadcast paths or URL routing (`/`, whitespace, `..`).
/// Server-generated UUIDs always pass.
fn invalid_source_id(id: &str) -> Option<String> {
    if id.is_empty() {
        return Some("source_id must not be empty".into());
    }
    if id.len() > 128 {
        return Some("source_id must be at most 128 characters".into());
    }
    if id.chars().any(|c| {
        c == '/' || c == '\\' || c == '?' || c == '#' || c.is_whitespace() || c.is_control()
    }) {
        return Some(
            "source_id may not contain slashes, '?', '#', whitespace, or control characters".into(),
        );
    }
    if id == "." || id == ".." {
        return Some("source_id may not be '.' or '..'".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::str::FromStr;
    use ts6_ssrf::MockResolver;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[tokio::test]
    async fn ssrf_blocks_loopback_literal() {
        let resolver = MockResolver::new();
        let err = is_url_allowed("http://127.0.0.1/", &resolver)
            .await
            .expect_err("loopback must be rejected");
        let api_err = ApiError::SsrfBlocked(err);
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(api_err.error_code(), "ssrf_blocked");
    }

    #[tokio::test]
    async fn ssrf_blocks_dns_rebinder() {
        let resolver = MockResolver::new().with("rebind.test", vec![ip("10.0.0.1")]);
        let err = is_url_allowed("http://rebind.test/", &resolver)
            .await
            .expect_err("rebinder to private IP must be rejected");
        let api_err = ApiError::SsrfBlocked(err);
        assert_eq!(api_err.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn ssrf_allows_public_host() {
        let resolver = MockResolver::new().with("example.com", vec![ip("93.184.216.34")]);
        let pinned = is_url_allowed("http://example.com/", &resolver)
            .await
            .expect("public host must be allowed");
        assert_eq!(pinned.resolved_ip, Some(ip("93.184.216.34")));
    }

    // PURA-149: URL host must round-trip to FFmpeg unchanged. The
    // previous implementation rewrote the host to the resolved IP,
    // which broke TLS SNI / HTTP Host header for virtual-hosted CDNs
    // (Cloudflare, googleapis, samplelib, …) and caused FFmpeg to
    // immediately exit with "IVF stream EOF before header".
    #[tokio::test]
    async fn pinned_url_preserves_https_hostname_for_cdn_sources() {
        let resolver =
            MockResolver::new().with("download.samplelib.com", vec![ip("188.227.84.172")]);
        let pinned = is_url_allowed(
            "https://download.samplelib.com/mp4/sample-5s.mp4",
            &resolver,
        )
        .await
        .expect("public CDN host must pass SSRF");

        let url_for_ffmpeg = pinned.url.to_string();
        assert!(
            url_for_ffmpeg.contains("download.samplelib.com"),
            "https URL passed to ffmpeg lost its hostname: {url_for_ffmpeg}"
        );
        assert!(
            !url_for_ffmpeg.contains("188.227.84.172"),
            "https URL passed to ffmpeg was rewritten to IP literal — \
             breaks TLS SNI / Host on virtual-hosted CDNs: {url_for_ffmpeg}"
        );
    }

    #[tokio::test]
    async fn pinned_url_preserves_http_hostname() {
        let resolver = MockResolver::new().with("example.com", vec![ip("93.184.216.34")]);
        let pinned = is_url_allowed("http://example.com/sample.mp4", &resolver)
            .await
            .expect("public host must be allowed");

        let url_for_ffmpeg = pinned.url.to_string();
        assert!(
            url_for_ffmpeg.contains("example.com"),
            "http URL passed to ffmpeg lost its hostname: {url_for_ffmpeg}"
        );
    }

    #[test]
    fn source_id_validator_rejects_unsafe_chars() {
        assert!(invalid_source_id("camera/1").is_some());
        assert!(invalid_source_id("camera 1").is_some());
        assert!(invalid_source_id("..").is_some());
        assert!(invalid_source_id("").is_some());
        let too_long = "a".repeat(129);
        assert!(invalid_source_id(&too_long).is_some());
    }

    #[test]
    fn source_id_validator_accepts_normal() {
        assert!(invalid_source_id("camera-1").is_none());
        assert!(invalid_source_id("01HXY-source").is_none());
        assert!(invalid_source_id(&uuid::Uuid::new_v4().to_string()).is_none());
    }
}
