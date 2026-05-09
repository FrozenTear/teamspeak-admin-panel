//! SSHBridge host-key verification — see `docs/adr/0002-host-key-verifier.md`.
//!
//! Four policies (`StrictFingerprint` / `KnownHostsFile` / `TrustOnFirstUse`
//! / `Reject`); the default is `StrictFingerprint` driven by the
//! per-server `sshHostKeyFingerprint` column added in migration
//! `0005_ssh_bridge_auth.surql` (PURA-77). When the operator has not
//! supplied a fingerprint and `TS_SSH_KNOWN_HOSTS` is unset, the
//! verifier falls back to `Reject` — meets the PURA-76 requirement that
//! the default MUST NOT be unconditional accept.
//!
//! `TrustOnFirstUse` is opt-in via `TS_SSH_TOFU=1` (PURA-100); the
//! capture pipeline lives in [`super::tofu`]. TOFU only takes over when
//! the operator has actively chosen the tradeoff *and* neither of the
//! stricter policies applies.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use russh::keys::ssh_key::{Fingerprint, HashAlg, PublicKey};

use super::tofu::{TofuCaptureRequest, TofuCaptureSink};

/// What the verifier should do when a server presents a key.
#[derive(Debug, Clone)]
pub enum HostKeyPolicy {
    /// Reject every host key. Construction-time default; selected when
    /// the operator has not pinned a per-server fingerprint, the
    /// `TS_SSH_KNOWN_HOSTS` escape hatch is unset, **and** TOFU was not
    /// opted into via `TS_SSH_TOFU=1`.
    Reject,

    /// Accept only host keys whose SHA-256 fingerprint matches one of
    /// the entries. An empty list rejects every key (semantically
    /// equivalent to `Reject` but keeps the policy variant stable for
    /// callers who hold an empty fingerprint list).
    StrictFingerprint(Arc<[Fingerprint]>),

    /// Defer to russh's OpenSSH `known_hosts` parser. The path is the
    /// resolved location of `TS_SSH_KNOWN_HOSTS` (or another operator-
    /// supplied path); the verifier passes through whatever russh
    /// reports for `(host, port, key)` from that file.
    KnownHostsFile { path: PathBuf },

    /// PURA-100 — Trust-on-first-use. **Opt-in only**, gated by
    /// `TS_SSH_TOFU=1`. Selected when:
    ///
    /// - the operator has not pinned a per-server fingerprint,
    /// - `TS_SSH_KNOWN_HOSTS` is unset, AND
    /// - the operator has actively opted in to TOFU.
    ///
    /// On the first `verify` call the policy:
    ///
    /// 1. Computes the SHA-256 fingerprint of the offered key.
    /// 2. Stores it in the verifier's per-instance pin slot
    ///    ([`HostKeyVerifier::tofu_pin`]).
    /// 3. Fires a non-blocking [`TofuCaptureRequest`] onto the worker
    ///    (see [`super::tofu`]) so the fingerprint is persisted onto
    ///    `server_connection.sshHostKeyFingerprint`.
    /// 4. Returns `true` (accept).
    ///
    /// On every subsequent call, the verifier compares the offered key
    /// against the pinned fingerprint and rejects mismatches — same
    /// behaviour as [`HostKeyPolicy::StrictFingerprint`] from that
    /// point forward, even if persistence has not yet completed.
    ///
    /// **Security tradeoff.** TOFU pins whatever the verifier sees
    /// first. If a MitM is in place during the first connect, the
    /// wrong key gets pinned permanently — both in memory for this
    /// process and (via the worker) on disk. Operators who can extract
    /// the fingerprint out-of-band SHOULD use [`HostKeyPolicy::StrictFingerprint`]
    /// instead.
    TrustOnFirstUse { sink: TofuCaptureSink },
}

/// Verifier wired into russh's `client::Handler::check_server_key` callback.
///
/// One verifier per `(config_id, host, port)` triple. Cheap to construct;
/// holds an `Arc<[Fingerprint]>` for the strict policy so it clones
/// without re-allocating.
///
/// `tofu_pin` is the in-process pin slot for [`HostKeyPolicy::TrustOnFirstUse`].
/// It is unused for every other policy. The slot is filled on the
/// first successful `verify` call; subsequent calls in the same
/// process compare against the pinned fingerprint and reject
/// mismatches even if the TOFU worker has not yet persisted the
/// capture (and even if persistence is permanently lost — the row
/// stays at rest until either the worker or an operator pins it).
#[derive(Debug, Clone)]
pub struct HostKeyVerifier {
    policy: HostKeyPolicy,
    config_id: i64,
    host: String,
    port: u16,
    tofu_pin: Arc<OnceLock<Fingerprint>>,
}

impl HostKeyVerifier {
    pub fn new(
        policy: HostKeyPolicy,
        config_id: i64,
        host: impl Into<String>,
        port: u16,
    ) -> Self {
        Self {
            policy,
            config_id,
            host: host.into(),
            port,
            tofu_pin: Arc::new(OnceLock::new()),
        }
    }

    /// Build the default-for-deployment verifier from the per-server
    /// `sshHostKeyFingerprint` column.
    ///
    /// Selection rules (highest priority first):
    ///
    /// 1. `stored_fingerprint = Some(_)` → [`HostKeyPolicy::StrictFingerprint`].
    /// 2. `known_hosts_path = Some(_)` → [`HostKeyPolicy::KnownHostsFile`].
    /// 3. `tofu_sink = Some(_)` (i.e. operator set `TS_SSH_TOFU=1` AND
    ///    the manager spawned the capture worker) → [`HostKeyPolicy::TrustOnFirstUse`].
    /// 4. otherwise → [`HostKeyPolicy::Reject`].
    ///
    /// The strict-fingerprint and known-hosts paths take precedence
    /// over TOFU even when TOFU is enabled — an operator who pinned a
    /// row gets strict verification, period. TOFU only fills the gap
    /// for rows that have never been pinned.
    ///
    /// Returns `Err` only if the stored fingerprint cannot be parsed
    /// (malformed `SHA256:…` shape); the caller surfaces this as a
    /// configuration error rather than letting a typo silently disable
    /// verification.
    pub fn from_config(
        config_id: i64,
        host: impl Into<String>,
        port: u16,
        stored_fingerprint: Option<&str>,
        known_hosts_path: Option<PathBuf>,
        tofu_sink: Option<TofuCaptureSink>,
    ) -> Result<Self, HostKeyConfigError> {
        let host = host.into();
        let policy = match (stored_fingerprint, known_hosts_path, tofu_sink) {
            (Some(s), _, _) => {
                let fp = Fingerprint::from_str(s.trim()).map_err(|e| {
                    HostKeyConfigError::ParseFingerprint {
                        config_id,
                        message: e.to_string(),
                    }
                })?;
                HostKeyPolicy::StrictFingerprint(Arc::from(vec![fp]))
            }
            (None, Some(path), _) => HostKeyPolicy::KnownHostsFile { path },
            (None, None, Some(sink)) => HostKeyPolicy::TrustOnFirstUse { sink },
            (None, None, None) => HostKeyPolicy::Reject,
        };
        Ok(Self::new(policy, config_id, host, port))
    }

    /// Run the policy against `server_key`. Logs every decision under
    /// target `sshbridge::hostkey` so the audit stream sees both
    /// accepts and rejects.
    pub fn verify(&self, server_key: &PublicKey) -> bool {
        match &self.policy {
            HostKeyPolicy::Reject => {
                tracing::warn!(
                    target: "sshbridge::hostkey",
                    config_id = self.config_id,
                    host = %self.host,
                    port = self.port,
                    "host-key verification: REJECT policy active (no fingerprint configured)"
                );
                false
            }
            HostKeyPolicy::StrictFingerprint(fps) => {
                let observed = server_key.fingerprint(HashAlg::Sha256);
                let ok = fps.iter().any(|fp| *fp == observed);
                if ok {
                    tracing::info!(
                        target: "sshbridge::hostkey",
                        config_id = self.config_id,
                        host = %self.host,
                        port = self.port,
                        fingerprint = %observed,
                        "host-key verified (strict fingerprint)"
                    );
                } else {
                    tracing::warn!(
                        target: "sshbridge::hostkey",
                        config_id = self.config_id,
                        host = %self.host,
                        port = self.port,
                        observed = %observed,
                        "host-key REJECTED (strict fingerprint mismatch)"
                    );
                }
                ok
            }
            HostKeyPolicy::KnownHostsFile { path } => {
                let res = russh::keys::known_hosts::check_known_hosts_path(
                    &self.host,
                    self.port,
                    server_key,
                    path,
                );
                let observed = server_key.fingerprint(HashAlg::Sha256);
                match res {
                    Ok(true) => {
                        tracing::info!(
                            target: "sshbridge::hostkey",
                            config_id = self.config_id,
                            host = %self.host,
                            port = self.port,
                            fingerprint = %observed,
                            known_hosts = %path.display(),
                            "host-key verified (known_hosts)"
                        );
                        true
                    }
                    Ok(false) => {
                        tracing::warn!(
                            target: "sshbridge::hostkey",
                            config_id = self.config_id,
                            host = %self.host,
                            port = self.port,
                            observed = %observed,
                            known_hosts = %path.display(),
                            "host-key REJECTED (no matching known_hosts entry)"
                        );
                        false
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "sshbridge::hostkey",
                            config_id = self.config_id,
                            host = %self.host,
                            port = self.port,
                            error = %e,
                            known_hosts = %path.display(),
                            "host-key verification failed (known_hosts read error) — rejecting"
                        );
                        false
                    }
                }
            }
            HostKeyPolicy::TrustOnFirstUse { sink } => self.verify_tofu(server_key, sink),
        }
    }

    /// PURA-100 — TOFU verify. First call captures the fingerprint
    /// into the per-instance pin slot and fires a non-blocking
    /// persistence request onto the worker; every subsequent call
    /// compares against the pinned value and rejects mismatches. The
    /// in-memory pin is the load-bearing invariant: even if persistence
    /// is delayed or fails, this verifier instance will never accept a
    /// different key after the first one is observed.
    fn verify_tofu(&self, server_key: &PublicKey, sink: &TofuCaptureSink) -> bool {
        let observed = server_key.fingerprint(HashAlg::Sha256);
        if let Some(pinned) = self.tofu_pin.get() {
            // Subsequent calls — strict-equality enforcement against
            // the in-memory pin.
            if *pinned == observed {
                tracing::info!(
                    target: "sshbridge::hostkey",
                    config_id = self.config_id,
                    host = %self.host,
                    port = self.port,
                    fingerprint = %observed,
                    "host-key verified (TOFU in-memory pin match)"
                );
                return true;
            }
            tracing::warn!(
                target: "sshbridge::hostkey",
                config_id = self.config_id,
                host = %self.host,
                port = self.port,
                observed = %observed,
                pinned = %pinned,
                "host-key REJECTED (TOFU pin mismatch — server key changed mid-process)"
            );
            return false;
        }

        // First call — race-resolved by `OnceLock::set`. If two russh
        // handshakes race into this branch concurrently (unusual but
        // possible — same verifier shared across reconnect attempts),
        // exactly one wins the `set` and the other observes a present
        // value via the `Err(prior)` branch and falls back to strict
        // comparison.
        match self.tofu_pin.set(observed) {
            Ok(()) => {
                tracing::warn!(
                    target: "sshbridge::hostkey",
                    config_id = self.config_id,
                    host = %self.host,
                    port = self.port,
                    fingerprint = %observed,
                    "host-key TRUST-ON-FIRST-USE — pinning observed fingerprint \
                     for this server. Operators who can extract the fingerprint \
                     out-of-band MUST pin sshHostKeyFingerprint manually instead; \
                     TOFU's first-connect window is the MitM exposure"
                );
                let req = TofuCaptureRequest {
                    config_id: self.config_id,
                    host: self.host.clone(),
                    port: self.port,
                    fingerprint: super::tofu::fingerprint_to_storage(&observed),
                    user_id: None,
                };
                let _ = sink.try_send(req);
                true
            }
            Err(prior) => {
                if prior == observed {
                    tracing::info!(
                        target: "sshbridge::hostkey",
                        config_id = self.config_id,
                        host = %self.host,
                        port = self.port,
                        fingerprint = %observed,
                        "host-key verified (TOFU pin race resolved — same key)"
                    );
                    true
                } else {
                    tracing::warn!(
                        target: "sshbridge::hostkey",
                        config_id = self.config_id,
                        host = %self.host,
                        port = self.port,
                        observed = %observed,
                        pinned = %prior,
                        "host-key REJECTED (TOFU pin race resolved — different key)"
                    );
                    false
                }
            }
        }
    }

    /// Identify this verifier for log fields. Useful when the russh
    /// handler implementation needs to attach the connection's id to a
    /// log line outside `verify`.
    pub fn config_id(&self) -> i64 {
        self.config_id
    }
}

/// Errors at verifier-construction time.
#[derive(Debug, thiserror::Error)]
pub enum HostKeyConfigError {
    #[error("malformed sshHostKeyFingerprint for connection #{config_id}: {message}")]
    ParseFingerprint { config_id: i64, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::keys::ssh_key::PrivateKey;
    use std::sync::atomic::{AtomicU8, Ordering};

    /// Build a deterministic test key from a 32-byte seed. The seed
    /// counter is incremented per call so tests that need *two distinct*
    /// keys (verifier accepts A but not B) get distinguishable
    /// fingerprints. Tests don't need real entropy — just real
    /// public-key bytes that fingerprint correctly.
    fn fresh_key() -> (PublicKey, Fingerprint) {
        static COUNTER: AtomicU8 = AtomicU8::new(1);
        let mut seed = [0u8; 32];
        seed[0] = COUNTER.fetch_add(1, Ordering::SeqCst);
        let kp = Ed25519Keypair::from_seed(&seed);
        let priv_key = PrivateKey::from(kp);
        let pub_key = priv_key.public_key().clone();
        let fp = pub_key.fingerprint(HashAlg::Sha256);
        (pub_key, fp)
    }

    #[test]
    fn reject_policy_rejects_every_key() {
        let (key, _) = fresh_key();
        let v = HostKeyVerifier::new(HostKeyPolicy::Reject, 1, "ts.example", 10022);
        assert!(!v.verify(&key));
    }

    #[test]
    fn strict_fingerprint_accepts_matching_key() {
        let (key, fp) = fresh_key();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::StrictFingerprint(Arc::from(vec![fp])),
            1,
            "ts.example",
            10022,
        );
        assert!(v.verify(&key));
    }

    #[test]
    fn strict_fingerprint_rejects_non_matching_key() {
        let (_, decoy_fp) = fresh_key();
        let (other, _) = fresh_key();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::StrictFingerprint(Arc::from(vec![decoy_fp])),
            1,
            "ts.example",
            10022,
        );
        assert!(!v.verify(&other));
    }

    #[test]
    fn empty_fingerprint_list_rejects_every_key() {
        let (key, _) = fresh_key();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::StrictFingerprint(Arc::from(Vec::<Fingerprint>::new())),
            1,
            "ts.example",
            10022,
        );
        assert!(!v.verify(&key));
    }

    #[test]
    fn from_config_no_fp_no_path_no_tofu_yields_reject() {
        let v = HostKeyVerifier::from_config(7, "ts.example", 10022, None, None, None).unwrap();
        match v.policy {
            HostKeyPolicy::Reject => {}
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn from_config_with_fp_yields_strict() {
        let (_, fp) = fresh_key();
        let s = fp.to_string(); // SHA256:base64
        let v =
            HostKeyVerifier::from_config(7, "ts.example", 10022, Some(&s), None, None).unwrap();
        match v.policy {
            HostKeyPolicy::StrictFingerprint(_) => {}
            other => panic!("expected StrictFingerprint, got {other:?}"),
        }
    }

    #[test]
    fn from_config_rejects_malformed_fp() {
        let bad = "totally-not-a-fingerprint";
        let r = HostKeyVerifier::from_config(7, "ts.example", 10022, Some(bad), None, None);
        assert!(matches!(
            r,
            Err(HostKeyConfigError::ParseFingerprint { config_id: 7, .. })
        ));
    }

    #[test]
    fn from_config_path_only_yields_known_hosts_file() {
        let v = HostKeyVerifier::from_config(
            7,
            "ts.example",
            10022,
            None,
            Some(PathBuf::from("/etc/ts6-manager/ssh_known_hosts")),
            None,
        )
        .unwrap();
        match v.policy {
            HostKeyPolicy::KnownHostsFile { .. } => {}
            other => panic!("expected KnownHostsFile, got {other:?}"),
        }
    }

    #[test]
    fn from_config_fp_takes_precedence_over_known_hosts() {
        // If the operator pinned a per-server fingerprint, the row's
        // value wins — the global TS_SSH_KNOWN_HOSTS is for servers
        // that have no row-level fingerprint.
        let (_, fp) = fresh_key();
        let s = fp.to_string();
        let v = HostKeyVerifier::from_config(
            7,
            "ts.example",
            10022,
            Some(&s),
            Some(PathBuf::from("/etc/ts6-manager/ssh_known_hosts")),
            None,
        )
        .unwrap();
        assert!(matches!(v.policy, HostKeyPolicy::StrictFingerprint(_)));
    }

    // -----------------------------------------------------------------
    // PURA-100 — TOFU policy selection + verify behaviour.
    //
    // These tests are the regression belt for the security-sensitive
    // surface: TOFU MUST stay opt-in, MUST be subordinate to both
    // strict-fingerprint and known-hosts paths, and MUST pin the first
    // observed key in-memory so a second key offered later in the same
    // verifier's lifetime is rejected even if persistence has not yet
    // succeeded.
    // -----------------------------------------------------------------

    fn tofu_sink() -> (TofuCaptureSink, tokio::sync::mpsc::Receiver<TofuCaptureRequest>) {
        TofuCaptureSink::for_test()
    }

    #[test]
    fn from_config_tofu_only_when_sink_supplied_and_no_other_policy() {
        let (sink, _rx) = tofu_sink();
        let v =
            HostKeyVerifier::from_config(7, "ts.example", 10022, None, None, Some(sink)).unwrap();
        assert!(matches!(v.policy, HostKeyPolicy::TrustOnFirstUse { .. }));
    }

    #[test]
    fn from_config_strict_fp_takes_precedence_over_tofu() {
        // An operator who pinned a fingerprint for this row MUST get
        // strict verification regardless of whether TS_SSH_TOFU is on.
        // Otherwise a row that was once TOFU'd into the wrong key
        // could keep being trusted via TOFU even after the operator
        // edited the row to reflect the right key.
        let (_, fp) = fresh_key();
        let s = fp.to_string();
        let (sink, _rx) = tofu_sink();
        let v = HostKeyVerifier::from_config(
            7,
            "ts.example",
            10022,
            Some(&s),
            None,
            Some(sink),
        )
        .unwrap();
        assert!(matches!(v.policy, HostKeyPolicy::StrictFingerprint(_)));
    }

    #[test]
    fn from_config_known_hosts_takes_precedence_over_tofu() {
        // TS_SSH_KNOWN_HOSTS is also a deterministic policy; if it is
        // set, we use it instead of TOFU.
        let (sink, _rx) = tofu_sink();
        let v = HostKeyVerifier::from_config(
            7,
            "ts.example",
            10022,
            None,
            Some(PathBuf::from("/etc/ts6-manager/ssh_known_hosts")),
            Some(sink),
        )
        .unwrap();
        assert!(matches!(v.policy, HostKeyPolicy::KnownHostsFile { .. }));
    }

    #[tokio::test]
    async fn tofu_first_call_accepts_and_fires_capture() {
        let (key, fp) = fresh_key();
        let (sink, mut rx) = tofu_sink();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::TrustOnFirstUse { sink },
            7,
            "ts.example",
            10022,
        );
        assert!(v.verify(&key));
        let captured = rx.try_recv().expect("capture event was sent");
        assert_eq!(captured.config_id, 7);
        assert_eq!(captured.fingerprint, fp.to_string());
    }

    #[tokio::test]
    async fn tofu_second_call_with_same_key_accepts_and_does_not_recapture() {
        let (key, _fp) = fresh_key();
        let (sink, mut rx) = tofu_sink();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::TrustOnFirstUse { sink },
            7,
            "ts.example",
            10022,
        );
        assert!(v.verify(&key));
        let _ = rx.try_recv().expect("first call sends capture");
        // Subsequent call with the same key must accept without re-sending.
        assert!(v.verify(&key));
        assert!(
            rx.try_recv().is_err(),
            "second TOFU verify with same key MUST NOT re-fire a capture event"
        );
    }

    #[tokio::test]
    async fn tofu_second_call_with_different_key_is_rejected() {
        // PURA-100 / R6 regression — once the verifier has pinned a
        // key, any later offer of a *different* key MUST be rejected
        // even though the persistence write may still be in flight.
        // This is the load-bearing TOCTOU guarantee that turns TOFU's
        // exposure window from "until process restart" into "the
        // single first-connect packet". Without the in-memory pin,
        // every reconnect would re-TOFU and silently re-pin against
        // whatever the upstream presents — exactly what the PURA-99
        // thread warned about.
        let (key_a, _) = fresh_key();
        let (key_b, _) = fresh_key();
        let (sink, _rx) = tofu_sink();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::TrustOnFirstUse { sink },
            7,
            "ts.example",
            10022,
        );
        assert!(v.verify(&key_a));
        assert!(
            !v.verify(&key_b),
            "TOFU MUST reject a second-offered key that does not match the \
             in-memory pin — silently re-pinning would let a MitM swap the \
             key on a reconnect within the same process"
        );
    }

    #[tokio::test]
    async fn tofu_drop_on_full_channel_does_not_break_in_memory_pin() {
        // Worst case: the TOFU worker has fallen behind and the
        // bounded channel is full. The verifier MUST still enforce
        // the in-memory pin — the dropped persistence event only
        // means the row stays NULL on disk, not that the verifier
        // forgets the key.
        let (sink, mut rx) = tofu_sink();
        // Fill the channel by spamming `try_send` directly; we don't
        // care what gets stored — we just want subsequent verifier
        // emissions to drop. The for-test sink has capacity 32.
        for _ in 0..32 {
            assert!(sink.try_send(TofuCaptureRequest {
                config_id: 1,
                host: "filler".into(),
                port: 1,
                fingerprint: "SHA256:filler".into(),
                user_id: None,
            }));
        }

        let (key_a, _) = fresh_key();
        let (key_b, _) = fresh_key();
        let v = HostKeyVerifier::new(
            HostKeyPolicy::TrustOnFirstUse { sink },
            7,
            "ts.example",
            10022,
        );
        // First verify still succeeds (the verifier sets its OnceLock
        // before try_send returns, and a dropped persistence event
        // does not flip the verify decision).
        assert!(v.verify(&key_a));
        // Mismatch on the next call still fails — in-memory pin
        // protects this process even with no disk-side persistence.
        assert!(!v.verify(&key_b));
        // Drain the receiver so the sink doesn't drop noisy.
        while rx.try_recv().is_ok() {}
    }
}
