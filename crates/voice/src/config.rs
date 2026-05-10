//! Bot config — PURA-118 WS-1.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::backoff::BackoffConfig;

/// Per-bot configuration handed to `BotSupervisor::spawn`. The supervisor
/// stamps a `BotId` onto each spawn; everything else comes from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    /// Display name in-channel.
    pub name: String,
    /// Server `host:port`.
    pub server_addr: String,
    /// On-disk path for the cached identity. Two simultaneous bots MUST
    /// use distinct paths (the on-disk format matches
    /// `ts6-voice-fixture::load_or_create_identity`).
    pub identity_path: PathBuf,
    /// Auto-connect on spawn. When `false`, the bot stays in
    /// `Disconnected` until a `Connect` command arrives.
    pub auto_connect: bool,
    /// Reconnect backoff schedule — applies to handshake retries AND to
    /// unexpected drops while online.
    pub backoff: BackoffConfig,
    /// Per-attempt handshake timeout. Default `30 s` matches
    /// `ts6-voice-prototype` and the fixture audio-E2E test.
    pub handshake_timeout: Duration,
    /// Capacity of the `BotEvent` broadcast channel. Larger = more
    /// late-subscriber tolerance, more memory. Default 64 events is
    /// plenty for the WS-5 / WS-6 surfaces.
    pub event_buffer: usize,
}

impl BotConfig {
    /// Helper for tests / examples — builds a config with defaults filled
    /// in around the two fields that always vary.
    pub fn new(name: impl Into<String>, identity_path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            server_addr: "127.0.0.1:9987".to_string(),
            identity_path: identity_path.into(),
            auto_connect: true,
            backoff: BackoffConfig::default(),
            handshake_timeout: Duration::from_secs(30),
            event_buffer: 64,
        }
    }

    pub fn with_server_addr(mut self, addr: impl Into<String>) -> Self {
        self.server_addr = addr.into();
        self
    }

    pub fn with_auto_connect(mut self, on: bool) -> Self {
        self.auto_connect = on;
        self
    }

    pub fn with_backoff(mut self, b: BackoffConfig) -> Self {
        self.backoff = b;
        self
    }

    pub fn with_handshake_timeout(mut self, d: Duration) -> Self {
        self.handshake_timeout = d;
        self
    }
}

/// Newtype over `u64` so the supervisor can hand back stable, copyable
/// IDs without pulling a `uuid` dependency into the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BotId(pub u64);

impl std::fmt::Display for BotId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bot-{:08x}", self.0)
    }
}
