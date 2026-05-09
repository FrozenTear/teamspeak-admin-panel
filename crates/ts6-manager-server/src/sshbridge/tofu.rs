//! Trust-on-first-use (TOFU) capture pipeline for SSHBridge host keys
//! (PURA-100).
//!
//! TOFU is **opt-in** — gated by `TS_SSH_TOFU=1`. Default-off preserves
//! the [`HostKeyPolicy::Reject`] fail-closed posture from ADR-0002 for
//! every operator who has not actively chosen to trade the
//! "configure-first" failure mode for a "first-connect attacker wins"
//! failure mode.
//!
//! ## Why a separate worker
//!
//! `russh::client::Handler::check_server_key` is a sync (return-position
//! impl-trait) callback that fires **inside the SSH key exchange**.
//! Doing a SurrealDB write from inside that callback is wrong on two
//! axes — it would block the handshake on DB latency, and the russh
//! handler future is `!Send` in places that would not let us `.await` a
//! `Surreal<Any>` cleanly. So the verifier `try_send`s a capture
//! request onto an mpsc; this worker drains it and performs the
//! persistence write asynchronously.
//!
//! ## Concurrency model — CAS-style writeback
//!
//! The worker's UPDATE is `WHERE sshHostKeyFingerprint = NONE`, which
//! means: if any prior capture (or operator edit) already set a
//! fingerprint on this row, the new write is a no-op. Without this
//! guard, two concurrent first-connects against different MitMs (or a
//! genuine reconnect after operator pin) could each clobber each
//! other's fingerprint. The CAS turns that into "the first writer
//! wins, every later writer is a logged no-op" — failing safe.
//!
//! ## Capacity + drop policy
//!
//! The mpsc is **bounded** (32 slots — TOFU-eligible connects are
//! rare). On full, [`TofuCaptureSink::try_send`] returns `Err`; the
//! verifier logs the drop but still accepts the connect, because the
//! verifier's own per-instance `OnceLock` carries the in-process pin
//! even if persistence is lost. Subsequent process restarts re-TOFU
//! against whatever is presented at that moment — that is the
//! documented operator tradeoff.
//!
//! ## Audit
//!
//! Every capture fires a `tracing::warn!` event under target
//! `sshbridge::hostkey` with `config_id`, `host`, `port`, the observed
//! fingerprint, and the operator user-id (if known). The persistence
//! result fires a follow-up `tracing::info!` (success) or
//! `tracing::warn!` (CAS no-op / DB error) under the same target.

use std::sync::Arc;

use russh::keys::ssh_key::Fingerprint;
use tokio::sync::mpsc;

use crate::db::Database;

/// One TOFU capture event emitted by [`crate::sshbridge::hostkey::HostKeyVerifier`]
/// when its policy is [`crate::sshbridge::hostkey::HostKeyPolicy::TrustOnFirstUse`]
/// and the per-instance pin slot was empty.
#[derive(Debug, Clone)]
pub struct TofuCaptureRequest {
    /// `server_connection.id` whose row should receive the captured
    /// fingerprint.
    pub config_id: i64,
    /// Host string (logged into the audit line; not used for the DB
    /// write — `id` is the canonical key).
    pub host: String,
    /// Port (logged into the audit line; not used for the DB write).
    pub port: u16,
    /// Captured SHA-256 fingerprint, in canonical OpenSSH `SHA256:base64`
    /// form. The persisted column is `option<string>` and the
    /// strict-fingerprint loader parses this same shape via
    /// [`Fingerprint::from_str`], so round-trip is identity.
    pub fingerprint: String,
    /// Operator `user.id` whose request kicked off the connect, when
    /// reachable. The russh callback runs at handshake time; for
    /// system-driven connects (dashboard tick supervisor, WS
    /// re-subscribe) there is no operator to attribute, so this stays
    /// `None`. The audit line carries the value verbatim — `None`
    /// renders as a missing field, not the string `"None"`.
    pub user_id: Option<i64>,
}

/// Cheap-to-clone handle held by [`crate::sshbridge::hostkey::HostKeyVerifier`].
///
/// Carries the bounded mpsc sender that delivers capture events to the
/// background worker. `try_send` is the only call surface — never
/// `.send().await`, because the verifier runs inside russh's sync
/// `check_server_key` callback.
#[derive(Clone)]
pub struct TofuCaptureSink {
    tx: mpsc::Sender<TofuCaptureRequest>,
}

impl std::fmt::Debug for TofuCaptureSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TofuCaptureSink").finish_non_exhaustive()
    }
}

impl TofuCaptureSink {
    /// Try to enqueue a capture event without blocking. Returns `false`
    /// if the channel is full or the worker has shut down. Callers
    /// **must not** flip the verify decision based on the return value
    /// — the verifier already tracks the in-memory pin via its own
    /// `OnceLock`, so a dropped persistence event still leaves the
    /// in-process trust intact for this verifier's lifetime.
    pub fn try_send(&self, req: TofuCaptureRequest) -> bool {
        match self.tx.try_send(req) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    target: "sshbridge::hostkey",
                    "TOFU capture channel full; persistence event dropped — \
                     in-memory pin still enforced for this verifier instance"
                );
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!(
                    target: "sshbridge::hostkey",
                    "TOFU capture worker has shut down; persistence event dropped"
                );
                false
            }
        }
    }

    /// Test-only constructor — pairs the sink with a receiver the test
    /// can drain to assert what the verifier emitted. Production code
    /// uses [`spawn_capture_worker`] instead.
    #[cfg(test)]
    pub fn for_test() -> (Self, mpsc::Receiver<TofuCaptureRequest>) {
        let (tx, rx) = mpsc::channel(32);
        (Self { tx }, rx)
    }
}

/// Spawn the background worker that drains TOFU capture events and
/// persists each fingerprint into `server_connection.sshHostKeyFingerprint`
/// with a CAS-style guard. Returns the [`TofuCaptureSink`] callers
/// pass into [`crate::sshbridge::hostkey::HostKeyVerifier::from_config`].
///
/// The worker runs until every clone of the returned sink is dropped.
/// In production that is the lifetime of the [`crate::control::ControlBackendPool`],
/// which lives for the lifetime of the process.
pub fn spawn_capture_worker(db: Arc<Database>) -> TofuCaptureSink {
    let (tx, mut rx) = mpsc::channel::<TofuCaptureRequest>(32);
    tokio::spawn(async move {
        tracing::info!(
            target: "sshbridge::hostkey",
            "TOFU capture worker started"
        );
        while let Some(req) = rx.recv().await {
            persist_capture(&db, &req).await;
        }
        tracing::info!(
            target: "sshbridge::hostkey",
            "TOFU capture worker shutting down (sink dropped)"
        );
    });
    TofuCaptureSink { tx }
}

/// CAS-style persistence: only set `sshHostKeyFingerprint` if the
/// column is currently `NONE`. A row that was already pinned (operator
/// edit, prior capture) is left untouched — the worker logs a `warn`
/// so an operator running `--log-level=warn` still sees the no-op.
///
/// On DB error, the worker logs and returns; the in-memory pin in the
/// verifier still protects this process. `try_send` from the verifier
/// is fire-and-forget by design.
async fn persist_capture(db: &Database, req: &TofuCaptureRequest) {
    let sql = "UPDATE type::record('server_connection', $id)
        MERGE { sshHostKeyFingerprint: $fp }
        WHERE sshHostKeyFingerprint = NONE
        RETURN record::id(id) AS id;";

    let bound = db
        .query(sql)
        .bind(("id", req.config_id))
        .bind(("fp", req.fingerprint.clone()))
        .await;
    let mut response = match bound.and_then(|r| r.check()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "sshbridge::hostkey",
                config_id = req.config_id,
                host = %req.host,
                port = req.port,
                user_id = ?req.user_id,
                error = %e,
                "TOFU fingerprint persistence failed; in-memory pin still \
                 enforced for this verifier instance until process restart"
            );
            return;
        }
    };

    let rows: Vec<i64> = response.take("id").unwrap_or_default();
    if rows.is_empty() {
        tracing::warn!(
            target: "sshbridge::hostkey",
            config_id = req.config_id,
            host = %req.host,
            port = req.port,
            user_id = ?req.user_id,
            "TOFU fingerprint already pinned on row (CAS no-op) — operator \
             edit or concurrent capture won; this connect's pin lives in \
             memory only"
        );
    } else {
        tracing::info!(
            target: "sshbridge::hostkey",
            config_id = req.config_id,
            host = %req.host,
            port = req.port,
            user_id = ?req.user_id,
            fingerprint = %req.fingerprint,
            "TOFU fingerprint persisted to server_connection.sshHostKeyFingerprint"
        );
    }
}

/// Convenience: render a russh [`Fingerprint`] in canonical
/// `SHA256:base64` form for the persisted column.
pub fn fingerprint_to_storage(fp: &Fingerprint) -> String {
    fp.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::keys::ssh_key::{HashAlg, PrivateKey};

    fn fresh_fp(seed_byte: u8) -> Fingerprint {
        let mut seed = [0u8; 32];
        seed[0] = seed_byte;
        let kp = Ed25519Keypair::from_seed(&seed);
        let priv_key = PrivateKey::from(kp);
        priv_key.public_key().fingerprint(HashAlg::Sha256)
    }

    #[tokio::test]
    async fn capture_worker_persists_fingerprint_on_null_row() {
        let db = crate::db::connect_in_memory()
            .await
            .expect("in-memory connect");
        crate::db::migrations::run(&db).await.expect("migrations run");

        let new = crate::repos::server_connections::NewServerConnection {
            name: "tofu-row".into(),
            host: "ts.example".into(),
            webqueryPort: 10080,
            apiKey: "k".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: Some("serveradmin".into()),
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: Some("ssh".into()),
            sshAuthMethod: Some("agent".into()),
            sshPrivateKey: None,
            sshKeyAgentSocket: Some("/run/ssh-agent.sock".into()),
            sshHostKeyFingerprint: None,
        };
        let row = crate::repos::server_connections::insert(&db, new)
            .await
            .expect("insert row");
        assert!(row.sshHostKeyFingerprint.is_none());

        let sink = spawn_capture_worker(db.clone());
        let fp = fresh_fp(1);
        let fp_str = fp.to_string();
        assert!(sink.try_send(TofuCaptureRequest {
            config_id: row.id,
            host: row.host.clone(),
            port: 10022,
            fingerprint: fp_str.clone(),
            user_id: Some(42),
        }));

        // Worker is async — poll for at most a few seconds.
        for _ in 0..200 {
            let after = crate::repos::server_connections::find_by_id(&db, row.id)
                .await
                .unwrap()
                .unwrap();
            if let Some(stored) = after.sshHostKeyFingerprint {
                assert_eq!(stored, fp_str);
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("TOFU worker did not persist the captured fingerprint within 4s");
    }

    #[tokio::test]
    async fn capture_worker_does_not_overwrite_already_pinned_row() {
        let db = crate::db::connect_in_memory()
            .await
            .expect("in-memory connect");
        crate::db::migrations::run(&db).await.expect("migrations run");

        let pinned = fresh_fp(7).to_string();
        let new = crate::repos::server_connections::NewServerConnection {
            name: "already-pinned".into(),
            host: "ts.example".into(),
            webqueryPort: 10080,
            apiKey: "k".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: Some("serveradmin".into()),
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: Some("ssh".into()),
            sshAuthMethod: Some("agent".into()),
            sshPrivateKey: None,
            sshKeyAgentSocket: Some("/run/ssh-agent.sock".into()),
            sshHostKeyFingerprint: Some(pinned.clone()),
        };
        let row = crate::repos::server_connections::insert(&db, new)
            .await
            .expect("insert row");

        let sink = spawn_capture_worker(db.clone());
        let attacker_fp = fresh_fp(99).to_string();
        assert!(sink.try_send(TofuCaptureRequest {
            config_id: row.id,
            host: row.host.clone(),
            port: 10022,
            fingerprint: attacker_fp.clone(),
            user_id: None,
        }));

        // Wait long enough for the worker to drain and (correctly)
        // no-op. 200ms is well past worker latency on any sane CI box.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let after = crate::repos::server_connections::find_by_id(&db, row.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            after.sshHostKeyFingerprint,
            Some(pinned),
            "TOFU worker MUST NOT overwrite an already-pinned fingerprint — \
             the CAS guard is the only thing standing between an attacker who \
             can race the writer and a permanent pin against the wrong key"
        );
    }

    #[test]
    fn for_test_returns_paired_sink_and_receiver() {
        let (sink, mut rx) = TofuCaptureSink::for_test();
        let fp = fresh_fp(3).to_string();
        assert!(sink.try_send(TofuCaptureRequest {
            config_id: 1,
            host: "h".into(),
            port: 10022,
            fingerprint: fp.clone(),
            user_id: None,
        }));
        let got = rx.try_recv().expect("event arrived");
        assert_eq!(got.config_id, 1);
        assert_eq!(got.fingerprint, fp);
    }
}
