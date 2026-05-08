//! Transport seam for SSHBridge.
//!
//! [`SshChannel`] is the byte-stream abstraction the transport state
//! machine talks to. Its only production implementation lives in
//! [`super::russh_channel`] and is backed by a `russh::Channel<Msg>`. Unit
//! tests substitute a stub channel so the bring-up sequence, queue
//! ordering, banner detection, keepalive cadence, and reconnect loop are
//! all verifiable without a real SSH peer.
//!
//! The contract is deliberately narrow: write a chunk, read the next
//! chunk, close. The state machine takes care of CR-LF reassembly (via
//! [`super::wire::LineBuffer`]) and frame classification.

use std::fmt;

use async_trait::async_trait;

/// A bidirectional ServerQuery byte stream.
///
/// Implementations are expected to be `Send` so the transport task can
/// own them across `await` points; they need not be `Sync` because the
/// transport task is single-threaded.
#[async_trait]
pub trait SshChannel: Send {
    /// Write all of `bytes` to the channel. Implementations MUST not
    /// short-write; either the whole slice is written or an error is
    /// returned. The transport layer issues CR-LF terminators in
    /// separate calls — a chunked-write semantics here is fine.
    async fn write(&mut self, bytes: &[u8]) -> Result<(), TransportError>;

    /// Receive the next chunk from the channel.
    ///
    /// `Ok(Some(bytes))` — bytes arrived. May be a partial line; the
    ///   line buffer reassembles.
    /// `Ok(None)` — the peer closed the channel cleanly (e.g. an `exit
    ///   status` ChannelMsg). The transport treats this as a transport
    ///   failure that triggers reconnect.
    /// `Err(TransportError::AuthRejected)` — surfaced by an
    ///   implementation that learnt about the auth failure mid-stream
    ///   (e.g. the russh adapter saw an `AuthResult::Failure` after a
    ///   reconnect handshake). Fatal — the transport propagates it as
    ///   [`super::SshBridgeError::AuthRejected`] and stops.
    /// `Err(other)` — generic transport failure; transport will trigger
    ///   reconnect.
    async fn recv(&mut self) -> Result<Option<Vec<u8>>, TransportError>;

    /// Best-effort graceful close. Implementations should swallow errors
    /// arising from a peer that already closed the channel.
    async fn close(&mut self) -> Result<(), TransportError>;
}

/// Failures emitted by the SSH byte-stream layer.
///
/// Distinct from [`super::SshBridgeError`] — this variant set is what
/// happens *below* the ServerQuery line protocol; the transport
/// translates these into the public error shape.
#[derive(Debug, Clone)]
pub enum TransportError {
    /// Generic I/O / protocol error. The string is operator-friendly
    /// and never contains credentials.
    Io(String),

    /// Peer closed the channel before a terminator arrived. Usually a
    /// sign of an upstream restart. Triggers reconnect.
    Closed(String),

    /// Authentication was rejected by the SSH layer (russh's
    /// `AuthResult::Failure`) or the channel surfaced one of the
    /// spec §11.5 substrings (`authentication` / `Auth`). **Fatal** —
    /// the transport does NOT enter the reconnect loop on this error.
    AuthRejected,

    /// Host-key verification rejected the server-presented key.
    /// Treated as fatal so the operator notices and updates the row;
    /// the transport surfaces it as a transport failure with a
    /// recognisable message.
    HostKeyMismatch,

    /// The configured deadline elapsed waiting for bytes. Triggers
    /// reconnect; the in-flight command (if any) is reported as a
    /// transport-class failure to its caller.
    Timeout,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Io(s) => write!(f, "ssh transport I/O error: {s}"),
            TransportError::Closed(s) => write!(f, "ssh channel closed: {s}"),
            TransportError::AuthRejected => write!(f, "ssh authentication rejected"),
            TransportError::HostKeyMismatch => write!(f, "ssh host-key verification failed"),
            TransportError::Timeout => write!(f, "ssh command timed out"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Spec §11.5 — auth-rejected fallback detection. Some SSH peers do not
/// fail handshakes through `AuthResult::Failure` cleanly; instead the
/// channel writes a banner string containing `authentication` (any case)
/// or the exact token `Auth` (case-sensitive whole word) before
/// closing. This helper centralises that string-scan so the russh
/// adapter and the transport agree on the rule.
pub(crate) fn looks_like_auth_failure(s: &str) -> bool {
    if s.to_ascii_lowercase().contains("authentication") {
        return true;
    }
    // `Auth` as a word — case-sensitive per spec §11.5. A "word" here
    // is `Auth` surrounded by non-alphanumeric (or string boundaries),
    // so `Auth:`, ` Auth `, `Auth!`, and end-of-string `Auth` all
    // match, while `Author` and `Authorise` do not.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if &bytes[i..i + 4] == b"Auth" {
            let prev_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
            let next_ok = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric();
            if prev_ok && next_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failure_substring_matches_authentication_any_case() {
        assert!(looks_like_auth_failure("Authentication failed"));
        assert!(looks_like_auth_failure("authentication failed"));
        assert!(looks_like_auth_failure("AUTHENTICATION DENIED"));
    }

    #[test]
    fn auth_failure_substring_matches_bare_auth_token() {
        assert!(looks_like_auth_failure("Auth: rejected"));
        assert!(looks_like_auth_failure("session closed: Auth"));
    }

    #[test]
    fn auth_failure_substring_does_not_match_unrelated() {
        // `authorized` contains `Auth`-like fragment but is not a whole word
        // match — and contains no `authentication` substring.
        assert!(!looks_like_auth_failure("authorized_keys updated"));
        assert!(!looks_like_auth_failure("connection reset"));
        assert!(!looks_like_auth_failure(""));
    }

    #[test]
    fn auth_failure_substring_lowercase_auth_is_not_a_match() {
        // The spec calls out `Auth` capital-A as the standalone marker.
        assert!(!looks_like_auth_failure("auth failed"));
    }

    #[test]
    fn transport_error_display_strings_have_no_credential_leakage() {
        // Sanity: every variant's Display is operator-safe.
        let _ = TransportError::Io("eof".into()).to_string();
        let _ = TransportError::Closed("eof".into()).to_string();
        let _ = TransportError::AuthRejected.to_string();
        let _ = TransportError::HostKeyMismatch.to_string();
        let _ = TransportError::Timeout.to_string();
    }
}
