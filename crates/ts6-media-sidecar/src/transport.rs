//! QUIC / WebTransport listener wired into `moq-native::Server`.
//!
//! Pinned to ALPN `moq-lite-04` per ADR-0007. Sessions that negotiate a
//! different MoQ version fail at the TLS handshake (their offered ALPN is
//! not in the server's advertised list). The accept loop publishes the
//! shared [`SidecarOrigin`] consumer into every accepted session so
//! browsers can subscribe to broadcasts the sidecar registers.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use moq_lite::Version;
use moq_native::{Server, ServerConfig, ServerTlsConfig};
use tracing::{debug, info, warn};

use crate::SidecarStats;
use crate::origin::SidecarOrigin;

/// Plumbing input for the QUIC/WebTransport listener.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// `host:port` to bind UDP for QUIC. `[::1]:0` works for tests.
    pub bind: SocketAddr,
    /// PEM cert chain(s). Required unless `tls_generate` is non-empty.
    pub tls_cert: Vec<PathBuf>,
    /// PEM private key(s) matching `tls_cert`.
    pub tls_key: Vec<PathBuf>,
    /// Hostnames for an in-memory self-signed cert. Mutually exclusive
    /// with `tls_cert`/`tls_key`. Useful for dev + the smoke test.
    pub tls_generate: Vec<String>,
}

impl TransportConfig {
    fn into_server_config(self) -> Result<ServerConfig> {
        if self.tls_cert.is_empty() && self.tls_generate.is_empty() {
            return Err(anyhow!(
                "transport TLS unconfigured: pass --tls-cert/--tls-key or --tls-generate <hostname>"
            ));
        }
        if !self.tls_cert.is_empty() && !self.tls_generate.is_empty() {
            return Err(anyhow!(
                "transport TLS overspecified: pass either --tls-cert/--tls-key OR --tls-generate, not both"
            ));
        }

        let mut tls = ServerTlsConfig::default();
        tls.cert = self.tls_cert;
        tls.key = self.tls_key;
        tls.generate = self.tls_generate;

        let mut cfg = ServerConfig::default();
        cfg.bind = Some(self.bind.to_string());
        cfg.tls = tls;
        // Pin ALPN to moq-lite-04 only. moq-native's `versions` field
        // empty = "all"; we want the strict opposite — reject anything
        // that doesn't speak the wire we just shipped through WS-0.
        cfg.version =
            vec![Version::from_str("moq-lite-04").expect("hard-coded valid version string")];
        Ok(cfg)
    }
}

/// Bound, not-yet-running QUIC/WebTransport listener.
pub struct Transport {
    server: Server,
    origin: Arc<SidecarOrigin>,
    stats: Arc<SidecarStats>,
}

impl Transport {
    pub fn bind(
        config: TransportConfig,
        origin: Arc<SidecarOrigin>,
        stats: Arc<SidecarStats>,
    ) -> Result<Self> {
        let server_cfg = config.into_server_config()?;
        let server = Server::new(server_cfg).context("init moq-native server")?;
        Ok(Self {
            server,
            origin,
            stats,
        })
    }

    /// UDP socket the QUIC endpoint is listening on.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.server
            .local_addr()
            .context("read transport local_addr")
    }

    /// SHA256 hex of the (first) configured certificate, suitable for the
    /// browser-side `serverCertificateHashes` flow.
    pub fn primary_fingerprint(&self) -> Option<String> {
        self.server
            .tls_info()
            .read()
            .ok()
            .and_then(|info| info.fingerprints.first().cloned())
    }

    /// Drives the accept loop until ctrl_c (handled internally by
    /// `moq-native::Server::accept`). Each session is published the
    /// shared origin's consumer — that's the read-only handle browsers
    /// subscribe through.
    pub async fn run(self) -> Result<()> {
        let Transport {
            mut server,
            origin,
            stats,
        } = self;

        let local = server.local_addr().context("read transport local_addr")?;
        info!(%local, "transport listener accepting moq-lite-04 sessions");

        while let Some(request) = server.accept().await {
            let transport = request.transport();
            let url = request.url().map(|u| u.to_string());
            debug!(transport, ?url, "incoming session");

            let request = request.with_publish(origin.consumer());
            let stats = stats.clone();
            tokio::spawn(async move {
                stats.record_session_open().await;
                match request.ok().await {
                    Ok(session) => {
                        info!(transport, ?url, "session established");
                        // Hold the session until the client closes it; the
                        // moq-native `Session` keeps its own internal task
                        // tree, so we just need to keep the value alive.
                        let _ = session.closed().await;
                        info!(transport, ?url, "session closed");
                    }
                    Err(err) => {
                        warn!(transport, ?url, %err, "session handshake failed");
                    }
                }
                stats.record_session_close().await;
            });
        }

        Ok(())
    }
}
