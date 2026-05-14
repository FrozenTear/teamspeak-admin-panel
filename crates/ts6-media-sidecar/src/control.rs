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
}

/// In-memory pipeline registry. `source_id → Pipeline`. The `source_id`
/// is also the broadcast name registered against [`SidecarOrigin`], so
/// the same key works on both sides of the system.
#[derive(Clone, Default)]
pub struct PipelineRegistry {
    inner: Arc<RwLock<HashMap<String, Pipeline>>>,
}

impl PipelineRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> Vec<SourceStatsSnapshot> {
        let guard = self.inner.read().await;
        let mut out: Vec<SourceStatsSnapshot> = guard
            .iter()
            .map(|(id, p)| SourceStatsSnapshot::from_pipeline(id, p.preset(), &p.metrics()))
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

    // PURA-149: we used to rewrite the URL host to `pinned.resolved_ip`
    // here to defeat DNS rebinding (ts6-ssrf's documented contract for
    // outbound clients). For FFmpeg-driven outbound that's actively
    // harmful — every virtual-hosted CDN (Cloudflare, googleapis,
    // samplelib's nginx) needs the original hostname in TLS SNI and the
    // `Host:` header, and IP-literal SNI is undefined / always wrong.
    //
    // What replaces the URL-level pin for v1:
    //   - HTTPS: TLS hostname validation binds the connection to the
    //     certificate, which is a stronger guarantee than IP-pinning
    //     (a DNS rebinder substituting a private-range IP gets a TLS
    //     handshake failure on the cert's SAN check).
    //   - HTTP: we accept a small TOCTTOU window between SSRF resolve
    //     and FFmpeg connect. `ts6-ssrf` has already rejected the
    //     metadata-host list and any answer that lands in a private
    //     range, so the rebinder needs a moving public→private answer
    //     in that window. A future iteration can route plaintext HTTP
    //     through a Rust-side reqwest proxy that pins the IP while
    //     preserving the Host header.
    let pinned_url = pinned.url.to_string();

    let mut guard = state.registry.inner.write().await;
    if guard.contains_key(&source_id) {
        return Err(ApiError::AlreadyRunning);
    }

    // Default = 720p when the caller omits `preset` or sends `null`
    // (spec §23.4 / PURA-142 AC). Unknown strings fail with the WS-3
    // error model, not Axum's default JSON-rejection shape.
    let preset = match req.preset.as_deref() {
        None => QualityPreset::DEFAULT,
        Some(s) => s
            .parse::<QualityPreset>()
            .map_err(|e| ApiError::InvalidRequest(e.to_string()))?,
    };
    let cfg = PipelineConfig::new(source_id.clone(), SourceInput::Url(pinned_url))
        .with_ffmpeg_path(state.ffmpeg_path.clone())
        .with_preset(preset);

    let pipeline = Pipeline::start(cfg, state.origin.clone())
        .await
        .map_err(ApiError::Internal)?;

    guard.insert(source_id.clone(), pipeline);
    info!(
        %source_id,
        url = %req.url,
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

    let pipeline = {
        let mut guard = state.registry.inner.write().await;
        guard.remove(&source_id)
    };

    match pipeline {
        Some(p) => {
            p.stop().await;
            info!(%source_id, "pipeline stopped");
            Ok(StatusCode::NO_CONTENT)
        }
        None => {
            warn!(%source_id, "stop requested for unknown source_id");
            Err(ApiError::UnknownSource)
        }
    }
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
