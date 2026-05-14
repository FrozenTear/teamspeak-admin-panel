//! Sidecar HTTP client — PURA-144 (WS-6).
//!
//! Thin wrapper around `reqwest::Client` that speaks the WS-3 control
//! plane shipped by [`ts6-media-sidecar`] (commit 153d26a):
//!
//! - `POST /source`        → start an FFmpeg pipeline.
//! - `POST /source/stop`   → tear it down.
//! - `GET  /stats`         → snapshot of every pipeline's frames/bytes/
//!   ffmpeg_alive counters.
//! - `GET  /health`        → liveness probe.
//!
//! The sidecar is reachable on the operator's private network — the
//! manager-side SSRF validator runs separately on the operator-supplied
//! `url` field BEFORE the sidecar is asked to start a pipeline (defence
//! in depth; the sidecar runs its own validator too).
//!
//! No retry policy here: callers translate transport errors into 5xx and
//! the operator retries from the FE. The sidecar's contract is that
//! `POST /source` is idempotent on `source_id` collision (returns 409),
//! so a manager-side retry after a partial failure is safe.

use std::time::Duration;

use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum SidecarClientError {
    /// `reqwest` failed to issue the request (transport / DNS / TLS /
    /// timeout). The manager-side route maps this to `502 Bad Gateway`.
    #[error("sidecar transport error: {0}")]
    Transport(String),

    /// The sidecar returned a non-2xx status. `body` is the raw response
    /// payload — passed through to operators for diagnostics. The wire
    /// shape from the sidecar is `{ error, detail? }` per
    /// [`ts6_media_sidecar::control::ApiError`].
    #[error("sidecar returned {status}: {body}")]
    Upstream { status: StatusCode, body: String },

    /// 2xx response but the body did not deserialise into the expected
    /// shape. Distinct from `Transport` so the route layer can log it
    /// at WARN — an upstream contract drift, not a network blip.
    #[error("malformed sidecar response: {0}")]
    Malformed(String),
}

pub type SidecarResult<T> = Result<T, SidecarClientError>;

// ---------------------------------------------------------------------------
// Wire DTOs — must stay shape-identical to the sidecar's `control.rs`.
// ---------------------------------------------------------------------------

/// `POST /source` body. The sidecar generates `source_id` when omitted;
/// for v1 the manager always lets the sidecar pick it.
#[derive(Debug, Clone, Serialize)]
pub struct StartSourceRequest {
    pub url: String,
    /// Selected encoding preset. The sidecar treats unknown strings as a
    /// 400; the manager validates against [`KNOWN_PRESETS`] before
    /// forwarding so the FE error stays close to the request.
    pub preset: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StartSourceResponse {
    pub source_id: String,
    pub track: TrackDescriptor,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrackDescriptor {
    pub namespace: String,
    pub video: String,
    pub audio: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StopSourceRequest {
    pub source_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StatsResponse {
    #[serde(default)]
    pub uptime_s: u64,
    #[serde(default)]
    pub active_sessions: u64,
    #[serde(default)]
    pub lifetime_sessions: u64,
    #[serde(default)]
    pub registered_broadcasts: Vec<String>,
    #[serde(default)]
    pub sources: Vec<SourceStats>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SourceStats {
    pub source_id: String,
    #[serde(default)]
    pub preset: String,
    pub video: TrackStats,
    pub audio: TrackStats,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct TrackStats {
    #[serde(default)]
    pub frames_published: u64,
    #[serde(default)]
    pub bytes_published: u64,
    #[serde(default)]
    pub ffmpeg_alive: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    #[serde(default)]
    pub uptime_s: u64,
    #[serde(default)]
    pub sessions: u64,
    #[serde(default)]
    pub broadcasts: usize,
}

/// Presets the FE and sidecar both understand. The sidecar's
/// `QualityPreset` enum is the source of truth — see `preset.rs` in the
/// sidecar crate. Kept as plain strings here so the manager doesn't
/// pull the entire moq-spike workspace into its build.
pub const KNOWN_PRESETS: &[&str] = &["480p", "720p", "1080p"];

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct SidecarClient {
    base_url: String,
    http: Client,
}

impl SidecarClient {
    /// Construct a client pointing at `base_url`. Reuse a single
    /// instance across the process — the inner `reqwest::Client` owns
    /// the connection pool.
    pub fn new(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        let http = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest::Client default build succeeds");
        Self { base_url, http }
    }

    /// Construct a client around an existing `reqwest::Client`. Tests
    /// use this so a single bound mock listener is reused.
    pub fn with_client(base_url: impl Into<String>, http: Client) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self { base_url, http }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn start_source(
        &self,
        req: &StartSourceRequest,
    ) -> SidecarResult<StartSourceResponse> {
        let url = format!("{}/source", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(SidecarClientError::Upstream {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        serde_json::from_slice(&bytes).map_err(|e| {
            SidecarClientError::Malformed(format!(
                "start_source: {e} (raw={:?})",
                String::from_utf8_lossy(&bytes)
            ))
        })
    }

    /// Stop a pipeline. The sidecar returns `204 No Content` on success
    /// and `404` when the source is unknown — both shapes resolve to
    /// `Ok(())` so the route layer's DELETE handler can be idempotent
    /// without an extra match on the error variant.
    pub async fn stop_source(&self, req: &StopSourceRequest) -> SidecarResult<()> {
        let url = format!("{}/source/stop", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        let status = resp.status();
        if status.is_success() || status == StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = resp
            .bytes()
            .await
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();
        Err(SidecarClientError::Upstream { status, body })
    }

    pub async fn get_stats(&self) -> SidecarResult<StatsResponse> {
        let url = format!("{}/stats", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(SidecarClientError::Upstream {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| SidecarClientError::Malformed(format!("get_stats: {e}")))
    }

    pub async fn get_health(&self) -> SidecarResult<HealthResponse> {
        let url = format!("{}/health", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| SidecarClientError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(SidecarClientError::Upstream {
                status,
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| SidecarClientError::Malformed(format!("get_health: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode as AxStatus,
        routing::{get, post},
    };
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tokio::net::TcpListener;

    #[derive(Clone, Default)]
    struct MockState {
        start_calls: Arc<Mutex<Vec<serde_json::Value>>>,
        stop_calls: Arc<Mutex<Vec<serde_json::Value>>>,
        stop_known: Arc<Mutex<bool>>,
    }

    async fn boot_mock_sidecar() -> (String, MockState) {
        let state = MockState {
            stop_known: Arc::new(Mutex::new(true)),
            ..Default::default()
        };
        let app = Router::new()
            .route("/source", post(handle_start))
            .route("/source/stop", post(handle_stop))
            .route("/stats", get(handle_stats))
            .route("/health", get(handle_health))
            .with_state(state.clone());
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}"), state)
    }

    async fn handle_start(
        State(state): State<MockState>,
        Json(body): Json<serde_json::Value>,
    ) -> impl axum::response::IntoResponse {
        state.start_calls.lock().unwrap().push(body);
        (
            AxStatus::CREATED,
            Json(json!({
                "source_id": "src-42",
                "track": {
                    "namespace": "src-42",
                    "video": "video",
                    "audio": "audio"
                }
            })),
        )
    }

    async fn handle_stop(
        State(state): State<MockState>,
        Json(body): Json<serde_json::Value>,
    ) -> AxStatus {
        state.stop_calls.lock().unwrap().push(body);
        if *state.stop_known.lock().unwrap() {
            AxStatus::NO_CONTENT
        } else {
            AxStatus::NOT_FOUND
        }
    }

    async fn handle_stats(State(_): State<MockState>) -> Json<serde_json::Value> {
        Json(json!({
            "uptime_s": 12,
            "active_sessions": 0,
            "lifetime_sessions": 0,
            "registered_broadcasts": ["src-42"],
            "sources": [{
                "source_id": "src-42",
                "preset": "720p",
                "video": {"frames_published": 100, "bytes_published": 5000, "ffmpeg_alive": true},
                "audio": {"frames_published": 200, "bytes_published": 2000, "ffmpeg_alive": true}
            }]
        }))
    }

    async fn handle_health(State(_): State<MockState>) -> Json<serde_json::Value> {
        Json(json!({"status": "ok", "uptime_s": 12, "sessions": 0, "broadcasts": 1}))
    }

    #[tokio::test]
    async fn start_source_round_trip() {
        let (base, mock) = boot_mock_sidecar().await;
        let client = SidecarClient::new(base);
        let resp = client
            .start_source(&StartSourceRequest {
                url: "https://example.com/stream".into(),
                preset: Some("720p".into()),
            })
            .await
            .unwrap();
        assert_eq!(resp.source_id, "src-42");
        assert_eq!(resp.track.namespace, "src-42");
        let calls = mock.start_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["url"], "https://example.com/stream");
        assert_eq!(calls[0]["preset"], "720p");
    }

    #[tokio::test]
    async fn stop_source_treats_404_as_ok() {
        let (base, mock) = boot_mock_sidecar().await;
        *mock.stop_known.lock().unwrap() = false;
        let client = SidecarClient::new(base);
        client
            .stop_source(&StopSourceRequest {
                source_id: "src-gone".into(),
            })
            .await
            .expect("404 must be treated as success for idempotency");
    }

    #[tokio::test]
    async fn get_stats_parses_response() {
        let (base, _mock) = boot_mock_sidecar().await;
        let client = SidecarClient::new(base);
        let stats = client.get_stats().await.unwrap();
        assert_eq!(stats.sources.len(), 1);
        assert_eq!(stats.sources[0].source_id, "src-42");
        assert!(stats.sources[0].video.ffmpeg_alive);
    }

    #[tokio::test]
    async fn transport_error_when_no_listener() {
        let client = SidecarClient::new("http://127.0.0.1:1");
        let err = client.get_health().await.unwrap_err();
        assert!(matches!(err, SidecarClientError::Transport(_)));
    }
}
