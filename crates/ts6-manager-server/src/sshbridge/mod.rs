//! PURA-69 — Phase 2 SSHBridge: parallel control path that issues TS6
//! ServerQuery commands over the TeamSpeak SSH ServerQuery interface
//! (default port `10022`, spec Chapter 11).
//!
//! ## What lands in this slice (foundation)
//!
//! - [`SshBridgeError`] — shape-compatible with [`crate::webquery::WebQueryError`]
//!   so the REST layer can swap backends per server flag without rewriting
//!   error mapping. `Upstream`, `Transport`, `InvalidResponse`, `Decrypt`
//!   variants align 1:1 across the two backends. `http_status`,
//!   `upstream_code`, and `upstream_message` mirror the WebQuery API so the
//!   `§7.0.2` error-envelope shape is preserved on either control path.
//! - [`wire`] — line-protocol parser (CR-LF reassembly, `error id=…` envelope
//!   detection, `notify*` event splitting, `key=value` record extraction
//!   with §10.4 unescape applied). Pure parsing — no I/O.
//! - [`audit`] — structured audit-log entry per command (`AuditEntry`) +
//!   `tracing::info!` emission under target `sshbridge::audit`.
//! - The typed response shapes for the read-only Phase 1 command surface
//!   are re-exported from [`crate::webquery::models`] so SSHBridge yields
//!   the same Rust types as WebQuery (`VersionInfo`, `VirtualServerEntry`,
//!   `ServerInfo`, `ChannelEntry`, `ClientEntry`, `ConnectionInfo`).
//!
//! ## What lands in follow-up child issues under PURA-69
//!
//! - **`russh` transport.** Open SSH session, shell channel, banner detect,
//!   per-command queue, application-level keepalive (`whoami` every 30 s),
//!   reconnect with exponential backoff capped at 30 s. Spec §11.3 / §11.5.
//! - **Auth model.** Password (existing `sshPassword` ciphertext column),
//!   ssh-agent socket, encrypted-at-rest private key (`enc:` envelope via
//!   [`crate::crypto`]). PURA-69 explicitly defaults to ssh-agent or
//!   encrypted private key; password stays as a fallback. Schema deviation
//!   adds `sshAuthMethod` + `sshPrivateKey` + `sshKeyAgentSocket` columns.
//! - **Host-key verification.** `known_hosts` vs strict-fingerprint vs TOFU.
//!   Explicit SecurityEngineer review item — the foundation slice does NOT
//!   ship a verifier yet.
//! - **`ControlBackend` trait** — landed in PURA-78. The common
//!   interface lives in [`crate::control`]; both `WebQueryClient` and
//!   [`control_client::SshControlClient`] implement it, and the
//!   per-server `controlPath` flag (`'webquery'` default, `'ssh'`
//!   opt-in) selects the impl at pool construction.
//! - **Env-gated integration test** against a containerised TS SSH
//!   ServerQuery target (skipped without `TS6_SSH_INTEGRATION`) — also
//!   landed under PURA-78. The container itself is wired into
//!   `podman-compose.yml` under the `ssh-integration` profile so it
//!   does not run on every dev start.
//! - **Persistent audit table** (DB-backed) once the field list is signed
//!   off by SecurityEngineer.
//!
//! ## ADR
//!
//! Crate-selection rationale lives in `docs/adr/0001-ssh-client-russh.md`.
//!
//! ## Cleanroom + upstream-PR posture
//!
//! Inherited from PURA-66/PURA-69: if a russh bug or missing feature
//! surfaces, document internally → draft locally → post the exact draft on
//! the relevant Paperclip thread → wait for explicit board ack → only then
//! file. **No upstream PR/FR/bug under the board's identity without that
//! round-trip.**

#![allow(dead_code)] // consumed by REST routes + russh transport (follow-up children).

pub mod audit;
pub mod channel;
pub mod control_client;
pub mod hostkey;
pub mod retention;
pub mod russh_channel;
pub mod tofu;
pub mod transport;
pub mod wire;

// Re-exports for the eventual REST seam (PURA-69 follow-up C). Until
// that lands the rest of the crate references these through their
// module path; the explicit allow keeps the unused-imports lint quiet
// without weakening visibility.
#[allow(unused_imports)]
pub use channel::{SshChannel, TransportError};
#[allow(unused_imports)]
pub use hostkey::{HostKeyConfigError, HostKeyPolicy, HostKeyVerifier};
#[allow(unused_imports)]
pub use russh_channel::{RusshChannel, RusshConnectParams, connect_password};
#[allow(unused_imports)]
pub use transport::{
    CommandOutcome, SessionResult, TransportConfig, TransportHandle, next_backoff,
    spawn as spawn_transport, spawn_with_db as spawn_transport_with_db,
};

use reqwest::StatusCode;
use thiserror::Error;

// SSHBridge yields the same Rust shapes as WebQuery — re-export so callers
// import them through one path and the REST layer never has to know which
// backend produced a value. The russh transport (follow-up child issue) is
// the first internal consumer; the explicit allow suppresses the
// dead-on-arrival warning until the transport lands.
#[allow(unused_imports)]
pub use crate::webquery::models::{
    ChannelEntry, ClientEntry, ConnectionInfo, ServerInfo, VersionInfo, VirtualServerEntry,
};

/// Errors from the SSH bridge. Variants are shape-aligned with
/// [`crate::webquery::WebQueryError`] so REST handlers can map either
/// backend's failures through the same §7.0.2 path.
#[derive(Debug, Error)]
pub enum SshBridgeError {
    /// Upstream returned `error id=<n>` with `n != 0`. Maps to `502
    /// {error: "TeamSpeak API Error", code, details}` per spec §7.0.2.
    #[error("TS upstream error {code}: {message}")]
    Upstream { code: i64, message: String },

    /// SSH transport / channel failure (connect refused, auth, lost session,
    /// timeout). Maps to `502` with `code = -1`.
    #[error("SSH transport error: {0}")]
    Transport(String),

    /// A wire frame did not match the expected ServerQuery shape (e.g. a
    /// command response could not be parsed into the typed model). Maps to
    /// `502` with `code = -1`.
    #[error("malformed SSH response: {0}")]
    InvalidResponse(String),

    /// Stored SSH credential (password ciphertext, encrypted private key)
    /// failed to decrypt. Construction-time only.
    #[error("failed to decrypt SSH credential for connection #{config_id}: {source}")]
    Decrypt {
        config_id: i64,
        #[source]
        source: crate::crypto::AeadError,
    },

    /// Auth was rejected by the upstream SSH server. Per spec §11.5, an
    /// authentication failure is **fatal**: the bridge MUST NOT enter a
    /// reconnect loop on this error. Caller flips the operator-visible
    /// "credentials need attention" signal and waits for the row to be
    /// updated before retrying.
    #[error("SSH auth rejected for connection #{config_id}")]
    AuthRejected { config_id: i64 },

    /// Host-key verifier rejected the server-presented key. Fail-closed:
    /// no command runs against an unverified host. Like
    /// [`SshBridgeError::AuthRejected`] this is fatal — the supervisor
    /// short-circuits the reconnect loop so an operator who legitimately
    /// re-keyed the upstream sees a typed signal instead of an unending
    /// backoff/retry storm. Per ADR-0002 §Consequences the operator
    /// remediates by verifying the new fingerprint via `ssh-keyscan` and
    /// updating `sshHostKeyFingerprint` on the row.
    #[error("SSH host-key fingerprint mismatch for connection #{config_id}")]
    HostKeyMismatch { config_id: i64 },
}

impl SshBridgeError {
    /// HTTP status code per §7.0.2 / §10.5 — same mapping as WebQuery.
    pub fn http_status(&self) -> StatusCode {
        match self {
            SshBridgeError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            SshBridgeError::Transport(_) => StatusCode::BAD_GATEWAY,
            SshBridgeError::InvalidResponse(_) => StatusCode::BAD_GATEWAY,
            SshBridgeError::Decrypt { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            // Auth-rejected is reported as a transport-class failure to the
            // operator. The bridge surfaces a separate "credentials need
            // attention" signal via the connection lifecycle, but the REST
            // response stays in the §7.0.2 envelope shape.
            SshBridgeError::AuthRejected { .. } => StatusCode::BAD_GATEWAY,
            // Host-key mismatch is also surfaced as a transport-class
            // 502 — the dashboard / control routes render it as
            // "the host key changed; verify and update" via the
            // operator-visible details string. Parallel to AuthRejected
            // — both are fatal and flip a per-connection operator-
            // attention signal.
            SshBridgeError::HostKeyMismatch { .. } => StatusCode::BAD_GATEWAY,
        }
    }

    /// Upstream code surfaced in the §7.0.2 body. Non-upstream errors
    /// report `-1` — same convention as WebQuery.
    pub fn upstream_code(&self) -> i64 {
        match self {
            SshBridgeError::Upstream { code, .. } => *code,
            _ => -1,
        }
    }

    /// Operator-friendly `details` string for the §7.0.2 body.
    pub fn upstream_message(&self) -> String {
        match self {
            SshBridgeError::Upstream { message, .. } => message.clone(),
            SshBridgeError::HostKeyMismatch { config_id } => format!(
                "host-key fingerprint did not match the pinned value for connection #{config_id}"
            ),
            other => other.to_string(),
        }
    }
}

pub type SshBridgeResult<T> = Result<T, SshBridgeError>;

/// Convert a parsed [`wire::ErrorFrame`] into the typed result.
///
/// `id == 0` is success; a body parser is responsible for turning the
/// accumulated body lines into the typed shape (separate function — this
/// only encodes the success/failure split). `id != 0` becomes
/// [`SshBridgeError::Upstream`].
pub fn frame_to_result(frame: wire::ErrorFrame) -> SshBridgeResult<()> {
    if frame.id == 0 {
        Ok(())
    } else {
        Err(SshBridgeError::Upstream {
            code: frame.id,
            message: frame.msg,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_aligns_with_webquery() {
        let upstream = SshBridgeError::Upstream {
            code: 2568,
            message: "x".into(),
        };
        assert_eq!(upstream.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(upstream.upstream_code(), 2568);

        let transport = SshBridgeError::Transport("boom".into());
        assert_eq!(transport.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(transport.upstream_code(), -1);

        let auth = SshBridgeError::AuthRejected { config_id: 7 };
        assert_eq!(auth.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(auth.upstream_code(), -1);

        let host_key = SshBridgeError::HostKeyMismatch { config_id: 9 };
        assert_eq!(host_key.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(host_key.upstream_code(), -1);
        // The `details` string carries the config-id — the UI uses this to
        // tell the operator which row to verify and update.
        let details = host_key.upstream_message();
        assert!(
            details.contains("#9") && details.contains("fingerprint"),
            "host-key mismatch upstream_message lost the config-id or fingerprint hint: {details}"
        );

        // No public `AeadError` constructor; only check the variants we can
        // build directly.
    }

    #[test]
    fn frame_to_result_zero_id_is_ok() {
        let f = wire::ErrorFrame {
            id: 0,
            msg: "ok".into(),
        };
        assert!(frame_to_result(f).is_ok());
    }

    #[test]
    fn frame_to_result_nonzero_id_is_upstream_err() {
        let f = wire::ErrorFrame {
            id: 2568,
            msg: "insufficient client permissions".into(),
        };
        let r = frame_to_result(f);
        match r {
            Err(SshBridgeError::Upstream { code, message }) => {
                assert_eq!(code, 2568);
                assert_eq!(message, "insufficient client permissions");
            }
            other => panic!("expected Upstream error, got {other:?}"),
        }
    }
}
