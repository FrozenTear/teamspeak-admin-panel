//! SSHBridge host-key verification — see `docs/adr/0002-host-key-verifier.md`.
//!
//! Three policies (`StrictFingerprint` / `KnownHostsFile` / `Reject`); the
//! default is `StrictFingerprint` driven by the per-server
//! `sshHostKeyFingerprint` column added in migration
//! `0005_ssh_bridge_auth.surql` (PURA-77). When the operator has not
//! supplied a fingerprint and `TS_SSH_KNOWN_HOSTS` is unset, the
//! verifier falls back to `Reject` — meets the PURA-76 requirement that
//! the default MUST NOT be unconditional accept.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use russh::keys::ssh_key::{Fingerprint, HashAlg, PublicKey};

/// What the verifier should do when a server presents a key.
#[derive(Debug, Clone)]
pub enum HostKeyPolicy {
    /// Reject every host key. Construction-time default; selected when
    /// the operator has not pinned a per-server fingerprint and the
    /// `TS_SSH_KNOWN_HOSTS` escape hatch is also unset.
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
}

/// Verifier wired into russh's `client::Handler::check_server_key` callback.
///
/// One verifier per `(config_id, host, port)` triple. Cheap to construct;
/// holds an `Arc<[Fingerprint]>` for the strict policy so it clones
/// without re-allocating.
#[derive(Debug, Clone)]
pub struct HostKeyVerifier {
    policy: HostKeyPolicy,
    config_id: i64,
    host: String,
    port: u16,
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
        }
    }

    /// Build the default-for-deployment verifier from the per-server
    /// `sshHostKeyFingerprint` column. `None` (column was NULL) and an
    /// unset `TS_SSH_KNOWN_HOSTS` collapse to `Reject` — the operator
    /// has not opted in to verification, so the bridge refuses to
    /// connect.
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
    ) -> Result<Self, HostKeyConfigError> {
        let host = host.into();
        let policy = match (stored_fingerprint, known_hosts_path) {
            (Some(s), _) => {
                let fp = Fingerprint::from_str(s.trim()).map_err(|e| {
                    HostKeyConfigError::ParseFingerprint {
                        config_id,
                        message: e.to_string(),
                    }
                })?;
                HostKeyPolicy::StrictFingerprint(Arc::from(vec![fp]))
            }
            (None, Some(path)) => HostKeyPolicy::KnownHostsFile { path },
            (None, None) => HostKeyPolicy::Reject,
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
    fn from_config_no_fp_no_path_yields_reject() {
        let v = HostKeyVerifier::from_config(7, "ts.example", 10022, None, None).unwrap();
        match v.policy {
            HostKeyPolicy::Reject => {}
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn from_config_with_fp_yields_strict() {
        let (_, fp) = fresh_key();
        let s = fp.to_string(); // SHA256:base64
        let v = HostKeyVerifier::from_config(7, "ts.example", 10022, Some(&s), None).unwrap();
        match v.policy {
            HostKeyPolicy::StrictFingerprint(_) => {}
            other => panic!("expected StrictFingerprint, got {other:?}"),
        }
    }

    #[test]
    fn from_config_rejects_malformed_fp() {
        let bad = "totally-not-a-fingerprint";
        let r = HostKeyVerifier::from_config(7, "ts.example", 10022, Some(bad), None);
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
        )
        .unwrap();
        assert!(matches!(v.policy, HostKeyPolicy::StrictFingerprint(_)));
    }
}
