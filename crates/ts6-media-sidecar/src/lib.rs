//! `ts6-media-sidecar` — Phase-5 production sidecar for the MoQ + WebTransport
//! video path. Scaffold only: this crate currently boots a QUIC/WebTransport
//! listener (ALPN-pinned to `moq-lite-04`), exposes an empty broadcast
//! registry, and serves a small control-plane HTTP surface.
//!
//! Real media plumbing (FFmpeg subprocess, REST `/source` plane, SSRF
//! allow-list, quality presets, Dioxus widget) ship in WS-2..WS-5 under
//! the [PURA-136](../../docs/adr/0007-moq-flavor-and-draft-pin.md) Phase-5
//! epic. The pinning rationale lives in ADR-0007.

pub mod http;
pub mod origin;
pub mod pipeline;
pub mod transport;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

pub use crate::http::HttpServer;
pub use crate::origin::SidecarOrigin;
pub use crate::pipeline::{Pipeline, PipelineConfig, SourceInput};
pub use crate::transport::TransportConfig;

/// Configuration handed to [`Sidecar::start`]. Mirrors the binary's CLI
/// surface but is pure data so the smoke test can build one inline.
#[derive(Debug, Clone)]
pub struct SidecarConfig {
    pub transport: TransportConfig,
    pub http_listen: SocketAddr,
}

/// Handle to a running sidecar. Holds the bound addresses + TLS fingerprint
/// so callers (smoke tests, ops dashboards) can introspect without
/// re-discovering them. Drop or call [`Sidecar::shutdown`] to stop.
pub struct Sidecar {
    pub http_addr: SocketAddr,
    pub transport_addr: SocketAddr,
    pub fingerprint: String,
    pub origin: Arc<SidecarOrigin>,
    transport_task: JoinHandle<anyhow::Result<()>>,
    http_task: JoinHandle<anyhow::Result<()>>,
}

impl Sidecar {
    /// Boot the QUIC/WebTransport listener + the control-plane HTTP server.
    /// Returns once both are bound; the accept loops run on background
    /// tasks owned by the returned handle.
    pub async fn start(config: SidecarConfig) -> anyhow::Result<Self> {
        let started_at = Instant::now();
        let stats = Arc::new(SidecarStats::new(started_at));
        let origin = Arc::new(SidecarOrigin::new());

        let transport = transport::Transport::bind(config.transport, origin.clone(), stats.clone())
            .context("bind QUIC/WebTransport listener")?;
        let transport_addr = transport.local_addr()?;
        let fingerprint = transport
            .primary_fingerprint()
            .context("no TLS fingerprint exposed by moq-native (configure --tls-cert/--tls-key or --tls-generate)")?;

        let http = http::HttpServer::bind(
            config.http_listen,
            origin.clone(),
            stats.clone(),
            fingerprint.clone(),
        )
        .await
        .context("bind control-plane HTTP listener")?;
        let http_addr = http.local_addr();

        let transport_task = tokio::spawn(transport.run());
        let http_task = tokio::spawn(http.run());

        Ok(Self {
            http_addr,
            transport_addr,
            fingerprint,
            origin,
            transport_task,
            http_task,
        })
    }

    /// Wait for either accept loop to exit. Useful from the binary's main
    /// alongside a `ctrl_c` future.
    pub async fn join(self) -> anyhow::Result<()> {
        tokio::select! {
            res = self.transport_task => res.context("transport task panicked")?,
            res = self.http_task => res.context("http task panicked")?,
        }
    }

    /// Abort both background tasks.
    pub fn shutdown(self) {
        self.transport_task.abort();
        self.http_task.abort();
    }
}

/// Shared, lock-free counters for the control-plane stats endpoint.
///
/// Sessions are tracked as a delta (accept ++ / close --) so `/stats` can
/// report active vs. lifetime without separate locks. Broadcasts are read
/// directly off the [`SidecarOrigin`] registry to avoid drift.
#[derive(Debug)]
pub struct SidecarStats {
    pub started_at: Instant,
    sessions: RwLock<SessionCounts>,
}

#[derive(Debug, Default, Clone, Copy)]
struct SessionCounts {
    active: u64,
    lifetime: u64,
}

impl SidecarStats {
    pub fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            sessions: RwLock::new(SessionCounts::default()),
        }
    }

    pub async fn record_session_open(&self) {
        let mut s = self.sessions.write().await;
        s.active = s.active.saturating_add(1);
        s.lifetime = s.lifetime.saturating_add(1);
    }

    pub async fn record_session_close(&self) {
        let mut s = self.sessions.write().await;
        s.active = s.active.saturating_sub(1);
    }

    pub async fn snapshot(&self) -> StatsSnapshot {
        let s = *self.sessions.read().await;
        StatsSnapshot {
            uptime_s: self.started_at.elapsed().as_secs(),
            active_sessions: s.active,
            lifetime_sessions: s.lifetime,
        }
    }
}

/// Plain-old-data view of [`SidecarStats`] that the HTTP layer serialises.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct StatsSnapshot {
    pub uptime_s: u64,
    pub active_sessions: u64,
    pub lifetime_sessions: u64,
}
