// TS6 voice room connection. Lifts the handshake + identity-cache
// scaffolding from `ts6-voice-prototype` (PURA-112) so the translator
// uses the same pinned tsclientlib (master @ 04aa249) and the same
// disconnect dance. Slice c will use this connection's `raw()` to
// publish audio back into the room and to read inbound `StreamItem::Audio`
// frames.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use tokio::time::sleep;
use tracing::{debug, info, warn};
use tsclientlib::{Connection, DisconnectOptions, Identity, Reason, StreamItem};

pub struct Ts6Config {
    pub server: String,
    pub name: String,
    pub identity_dir: PathBuf,
    pub connect_timeout: Duration,
}

pub struct Ts6Connection {
    con: Connection,
}

impl Ts6Connection {
    pub async fn connect(cfg: &Ts6Config) -> Result<Self> {
        tokio::fs::create_dir_all(&cfg.identity_dir)
            .await
            .with_context(|| format!("create identity dir {}", cfg.identity_dir.display()))?;
        let identity_path = cfg.identity_dir.join("identity.json");
        let identity = load_or_create_identity(&identity_path).await?;
        info!(
            uid = %identity.key().to_pub().get_uid(),
            level = identity.level(),
            "TS6 identity ready"
        );

        let mut con = Connection::build(cfg.server.as_str())
            .name(cfg.name.clone())
            .identity(identity)
            .log_commands(false)
            .log_packets(false)
            .log_udp_packets(false)
            .connect()
            .context("Connection::build()")?;

        if !wait_for_connected(&mut con, cfg.connect_timeout).await? {
            bail!(
                "TS6 handshake did not complete within {}s — fixture up?",
                cfg.connect_timeout.as_secs(),
            );
        }
        Ok(Ts6Connection { con })
    }

    /// Borrow the underlying `tsclientlib::Connection`.
    ///
    /// Callers that need to drive the event stream and call methods like
    /// `send_audio` from the same `select!` arm dance the borrow-checker
    /// the same way `ts6-voice-prototype` does: create the `events()`
    /// future inline as the arm expression so the borrow is released
    /// before the body runs.
    pub fn raw(&mut self) -> &mut Connection {
        &mut self.con
    }

    pub async fn disconnect(self, msg: &str) {
        let mut con = self.con;
        if let Err(err) = con.disconnect(
            DisconnectOptions::new()
                .reason(Reason::Clientdisconnect)
                .message(msg),
        ) {
            warn!(?err, "TS6 disconnect failed (non-fatal)");
        }
        // Drain a moment for graceful close.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let drain = con.events();
            tokio::pin!(drain);
            while drain.next().await.is_some() {}
        })
        .await;
    }
}

async fn wait_for_connected(con: &mut Connection, timeout: Duration) -> Result<bool> {
    let deadline = sleep(timeout);
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
                    info!(level, "server requires higher identity — upgrading");
                }
                Some(Ok(StreamItem::IdentityLevelIncreased)) => {
                    info!("identity upgraded — handshake resumes");
                }
                Some(Ok(other)) => debug!(?other, "stream item during handshake"),
                Some(Err(err)) => return Err(anyhow!("stream error during handshake: {err}")),
                None => return Ok(false),
            }
        }
    }
}

async fn load_or_create_identity(path: &Path) -> Result<Identity> {
    if path.exists() {
        let raw = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read identity file {}", path.display()))?;
        let identity: Identity = serde_json::from_str(raw.trim())
            .with_context(|| format!("parse identity file {}", path.display()))?;
        info!(
            path = %path.display(),
            level = identity.level(),
            "loaded cached TS6 identity"
        );
        Ok(identity)
    } else {
        let identity = Identity::create();
        let raw = serde_json::to_string(&identity)?;
        tokio::fs::write(path, &raw)
            .await
            .with_context(|| format!("write identity file {}", path.display()))?;
        info!(
            path = %path.display(),
            level = identity.level(),
            "generated new TS6 identity"
        );
        Ok(identity)
    }
}
