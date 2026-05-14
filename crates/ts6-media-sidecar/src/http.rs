//! Control-plane HTTP surface (axum) — separate from the QUIC listener so
//! ops dashboards / load-balancers can probe without speaking MoQ.
//!
//! Routes:
//! - WS-1: `GET /health`, `GET /stats`, `GET /certificate.sha256`
//! - WS-3: `POST /source`, `POST /source/stop`, `GET /track/{source_id}`
//!
//! The mutating handlers live in [`crate::control`]; this module owns
//! binding + router wiring + the read-only GETs.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;
use ts6_ssrf::Resolver;

use crate::SidecarStats;
use crate::control::{self, ControlPlaneState, PipelineRegistry, SourceStatsSnapshot};
use crate::origin::SidecarOrigin;

#[derive(Clone)]
struct AppState {
    origin: Arc<SidecarOrigin>,
    stats: Arc<SidecarStats>,
    fingerprint: String,
    control_plane: ControlPlaneState,
}

/// Bound, not-yet-running axum server.
pub struct HttpServer {
    listener: TcpListener,
    router: Router,
}

impl HttpServer {
    pub async fn bind(
        bind: SocketAddr,
        origin: Arc<SidecarOrigin>,
        stats: Arc<SidecarStats>,
        fingerprint: String,
        resolver: Arc<dyn Resolver>,
        registry: PipelineRegistry,
        ffmpeg_path: PathBuf,
    ) -> Result<Self> {
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind control-plane HTTP listener on {bind}"))?;

        let control_plane = ControlPlaneState {
            origin: origin.clone(),
            registry,
            resolver,
            ffmpeg_path,
        };

        let state = AppState {
            origin,
            stats,
            fingerprint,
            control_plane: control_plane.clone(),
        };

        // axum gives each handler the state it asks for via `FromRef`
        // (see `impl FromRef<AppState> for ControlPlaneState` below), so
        // the WS-3 mutating handlers can extract `State<ControlPlaneState>`
        // off the same `AppState` the read-only handlers consume.
        let router = Router::new()
            .route("/health", get(health))
            .route("/stats", get(stats_handler))
            .route("/certificate.sha256", get(certificate_sha256))
            .route("/source", post(control::post_source))
            .route("/source/stop", post(control::post_source_stop))
            .route("/track/{source_id}", get(control::get_track))
            .fallback(not_found)
            .with_state(state);

        Ok(Self { listener, router })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("TcpListener bound by Self::bind")
    }

    pub async fn run(self) -> Result<()> {
        let local = self.local_addr();
        info!(%local, "control-plane HTTP listener up");
        axum::serve(self.listener, self.router)
            .await
            .context("axum::serve exited")
    }
}

impl axum::extract::FromRef<AppState> for ControlPlaneState {
    fn from_ref(input: &AppState) -> Self {
        input.control_plane.clone()
    }
}

#[derive(Serialize)]
struct HealthBody {
    status: &'static str,
    uptime_s: u64,
    sessions: u64,
    broadcasts: usize,
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.stats.snapshot().await;
    let broadcasts = state.origin.len().await;
    axum::Json(HealthBody {
        status: "ok",
        uptime_s: stats.uptime_s,
        sessions: stats.active_sessions,
        broadcasts,
    })
}

#[derive(Serialize)]
struct StatsBody {
    uptime_s: u64,
    active_sessions: u64,
    lifetime_sessions: u64,
    registered_broadcasts: Vec<String>,
    sources: Vec<SourceStatsSnapshot>,
}

async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.stats.snapshot().await;
    let registered_broadcasts = state.origin.names().await;
    let sources = state.control_plane.registry.snapshot().await;
    axum::Json(StatsBody {
        uptime_s: stats.uptime_s,
        active_sessions: stats.active_sessions,
        lifetime_sessions: stats.lifetime_sessions,
        registered_broadcasts,
        sources,
    })
}

async fn certificate_sha256(State(state): State<AppState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        state.fingerprint.clone(),
    )
}

async fn not_found() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}
