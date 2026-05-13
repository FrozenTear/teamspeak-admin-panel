//! Control-plane HTTP surface (axum) — separate from the QUIC listener so
//! ops dashboards / load-balancers can probe without speaking MoQ.
//!
//! WS-1 ships only the read-only endpoints (`/health`, `/stats`,
//! `/certificate.sha256`). The mutating `/source` plane lands in WS-3.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::SidecarStats;
use crate::origin::SidecarOrigin;

#[derive(Clone)]
struct AppState {
    origin: Arc<SidecarOrigin>,
    stats: Arc<SidecarStats>,
    fingerprint: String,
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
    ) -> Result<Self> {
        let listener = TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind control-plane HTTP listener on {bind}"))?;

        let state = AppState {
            origin,
            stats,
            fingerprint,
        };

        let router = Router::new()
            .route("/health", get(health))
            .route("/stats", get(stats_handler))
            .route("/certificate.sha256", get(certificate_sha256))
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
}

async fn stats_handler(State(state): State<AppState>) -> impl IntoResponse {
    let stats = state.stats.snapshot().await;
    let registered_broadcasts = state.origin.names().await;
    axum::Json(StatsBody {
        uptime_s: stats.uptime_s,
        active_sessions: stats.active_sessions,
        lifetime_sessions: stats.lifetime_sessions,
        registered_broadcasts,
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
