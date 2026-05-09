//! Shared helpers for the headless TS6 voice-client fixture.
//!
//! Two consumers:
//!
//! 1. The connect-only `ts6-voice-fixture` binary (PURA-106) — drives V4/V7
//!    visual e2e against a live TS6 server fixture.
//! 2. The `tests/audio_e2e.rs` integration test (PURA-110) — asserts that
//!    Opus frames flow end-to-end through the same fixture.
//!
//! Anything specific to one of those two consumers stays in its own file;
//! this module only holds the bits both need (identity persistence,
//! connection bring-up, handshake wait).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::StreamExt;
use tokio::fs;
use tracing::{debug, info};
use tsclientlib::{Connection, StreamItem};

pub use tsclientlib;

/// Drive the event stream until the first `BookEvents` arrives (= the server
/// has accepted us and sent its initial book) or `timeout` fires.
///
/// Returns `Ok(true)` on success, `Ok(false)` on stream-end-without-connect
/// (or timeout), and `Err` only when the stream itself errored.
///
/// Per `tsclientlib`'s contract the connection does nothing until its event
/// stream is polled; this is the single canonical "wait until connected"
/// loop the fixture uses everywhere.
pub async fn wait_for_connected(con: &mut Connection, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let events = con.events();
    tokio::pin!(events);

    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => return Ok(false),
            ev = events.next() => match ev {
                Some(Ok(StreamItem::BookEvents(_))) => return Ok(true),
                Some(Ok(StreamItem::IdentityLevelIncreasing(level))) => {
                    info!(level, "server requires higher identity level — upgrading");
                }
                Some(Ok(StreamItem::IdentityLevelIncreased)) => {
                    info!("identity upgraded — handshake will resume");
                }
                Some(Ok(other)) => debug!(?other, "stream item during handshake"),
                Some(Err(err)) => {
                    return Err(anyhow::anyhow!("stream error during handshake: {err}"));
                }
                None => return Ok(false),
            }
        }
    }
}

/// Read a JSON-serialized `Identity` from `path`, or generate a fresh
/// level-8 one and persist it. Same on-disk format the connect-only binary
/// has used since PURA-106.
pub async fn load_or_create_identity(path: &Path) -> Result<tsclientlib::Identity> {
    if path.exists() {
        let raw = fs::read_to_string(path)
            .await
            .with_context(|| format!("read identity file {}", path.display()))?;
        let identity: tsclientlib::Identity = serde_json::from_str(raw.trim())
            .with_context(|| format!("parse identity file {}", path.display()))?;
        info!(path = %path.display(), level = identity.level(), "loaded cached identity");
        Ok(identity)
    } else {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create identity dir {}", parent.display()))?;
        }
        let identity = tsclientlib::Identity::create();
        let raw = serde_json::to_string(&identity).context("serialize identity to json")?;
        fs::write(path, &raw)
            .await
            .with_context(|| format!("write identity file {}", path.display()))?;
        info!(path = %path.display(), level = identity.level(), "generated new identity");
        Ok(identity)
    }
}
