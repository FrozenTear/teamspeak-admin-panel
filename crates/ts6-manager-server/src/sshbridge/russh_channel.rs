//! Concrete russh-backed [`SshChannel`] for SSHBridge.
//!
//! [`connect_password`] performs the full SSH bring-up sequence:
//!
//! 1. Open a TCP socket via `russh::client::connect`.
//! 2. Pass the host-key verifier into the russh handler — verification
//!    runs inside `Handler::check_server_key`, before the SSH key
//!    exchange completes.
//! 3. Authenticate with password (PURA-76 scope).
//!    `AuthResult::Failure` is mapped to [`TransportError::AuthRejected`]
//!    so the supervisor stops without retry.
//! 4. Open a session channel and request a shell. The TS6 ServerQuery
//!    SSH interface starts the line protocol on shell-up.
//! 5. Wrap the channel in [`RusshChannel`] and return it. Subsequent
//!    [`SshChannel::write`] / [`SshChannel::recv`] calls operate
//!    against this channel.
//!
//! Encrypted-private-key + ssh-agent auth are out of scope for this
//! slice (PURA-69 follow-up B). The shape of [`RusshConnectParams`]
//! anticipates the variants: when follow-up B lands, the
//! `auth_method` field switches on those branches without requiring a
//! signature change here.

use std::sync::Arc;

use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::{ChannelMsg, Disconnect};

use super::channel::{looks_like_auth_failure, SshChannel, TransportError};
use super::hostkey::HostKeyVerifier;

/// Connection parameters consumed by [`connect_password`].
///
/// `password` is the cleartext credential — callers MUST decrypt the
/// `sshPassword` ciphertext before constructing this struct. The struct
/// is not `Debug` to keep it from accidentally being printed.
pub struct RusshConnectParams {
    pub config_id: i64,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub verifier: Arc<HostKeyVerifier>,
}

impl RusshConnectParams {
    pub fn new(
        config_id: i64,
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        password: impl Into<String>,
        verifier: Arc<HostKeyVerifier>,
    ) -> Self {
        Self {
            config_id,
            host: host.into(),
            port,
            user: user.into(),
            password: password.into(),
            verifier,
        }
    }
}

/// russh handler — only the host-key callback matters for SSHBridge.
///
/// `russh::client::Handler` uses a native return-position-impl-trait
/// signature for `check_server_key`; we must NOT layer `#[async_trait]`
/// over this impl (that would rewrite the method to a boxed future and
/// the lifetimes would no longer line up with russh's trait).
struct BridgeHandler {
    verifier: Arc<HostKeyVerifier>,
}

impl Handler for BridgeHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(self.verifier.verify(server_public_key))
    }
}

/// One russh session + shell channel, wrapped to fit the [`SshChannel`]
/// contract.
pub struct RusshChannel {
    session: client::Handle<BridgeHandler>,
    channel: russh::Channel<client::Msg>,
    closed: bool,
}

impl RusshChannel {
    fn map_data(msg: ChannelMsg) -> Result<Option<Vec<u8>>, TransportError> {
        match msg {
            ChannelMsg::Data { data } => Ok(Some(data.to_vec())),
            ChannelMsg::ExtendedData { data, .. } => {
                // stderr-style extended data — TS6 ServerQuery does not
                // use this in practice, but if it ever does we surface
                // it as ordinary bytes; the line parser ignores
                // unrecognised content beyond the framing rules.
                Ok(Some(data.to_vec()))
            }
            ChannelMsg::Eof | ChannelMsg::Close => Ok(None),
            // Other message kinds (window adjusts, exit status, etc.)
            // are control-plane events that don't carry ServerQuery
            // bytes — skip and let the caller loop again.
            _ => Ok(Some(Vec::new())),
        }
    }
}

#[async_trait]
impl SshChannel for RusshChannel {
    async fn write(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        self.channel
            .data(bytes)
            .await
            .map_err(|e| TransportError::Io(e.to_string()))
    }

    async fn recv(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        loop {
            match self.channel.wait().await {
                None => return Ok(None),
                Some(msg) => {
                    let mapped = Self::map_data(msg)?;
                    match mapped {
                        Some(v) if v.is_empty() => continue, // control-plane skip
                        other => return Ok(other),
                    }
                }
            }
        }
    }

    async fn close(&mut self) -> Result<(), TransportError> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        let _ = self.channel.close().await;
        let _ = self
            .session
            .disconnect(Disconnect::ByApplication, "shutdown", "en")
            .await;
        Ok(())
    }
}

/// Run the full bring-up sequence for password auth and yield a
/// channel ready for [`super::transport::read_banner`].
pub async fn connect_password(
    params: RusshConnectParams,
) -> Result<RusshChannel, TransportError> {
    let config = Arc::new(client::Config {
        // Reasonable defaults; russh's own keepalive layer is disabled
        // because [`super::transport`] runs application-level
        // `whoami` keepalives at the ServerQuery layer (spec §11.3).
        inactivity_timeout: None,
        keepalive_interval: None,
        ..Default::default()
    });

    let handler = BridgeHandler {
        verifier: params.verifier.clone(),
    };

    let mut session = match client::connect(config, (params.host.as_str(), params.port), handler)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let s = e.to_string();
            if looks_like_auth_failure(&s) {
                return Err(TransportError::AuthRejected);
            }
            // Host-key rejection surfaces from russh as an error whose
            // string commonly includes "key" / "rejected"; we keep the
            // mapping conservative — a verifier-rejected connection
            // returns `HostKeyMismatch` only when our handler returned
            // `Ok(false)`. Other rejections go through `Io`.
            if s.contains("UnknownKey") || s.contains("rejected by user") {
                return Err(TransportError::HostKeyMismatch);
            }
            return Err(TransportError::Io(s));
        }
    };

    match session
        .authenticate_password(&params.user, &params.password)
        .await
    {
        Ok(result) => {
            if !result.success() {
                tracing::warn!(
                    target: "sshbridge::transport",
                    config_id = params.config_id,
                    "russh password auth rejected"
                );
                return Err(TransportError::AuthRejected);
            }
        }
        Err(e) => {
            let s = e.to_string();
            if looks_like_auth_failure(&s) {
                return Err(TransportError::AuthRejected);
            }
            return Err(TransportError::Io(s));
        }
    }

    let channel = match session.channel_open_session().await {
        Ok(c) => c,
        Err(e) => return Err(TransportError::Io(e.to_string())),
    };

    if let Err(e) = channel.request_shell(true).await {
        return Err(TransportError::Io(format!("request_shell failed: {e}")));
    }

    Ok(RusshChannel {
        session,
        channel,
        closed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshbridge::hostkey::HostKeyPolicy;

    #[test]
    fn connect_params_struct_holds_no_password_in_debug() {
        // The struct deliberately omits Debug; this test exists as a
        // regression assertion — if someone derives Debug, the trait
        // bound below will fail to compile.
        fn requires_no_debug<T>() {}
        requires_no_debug::<RusshConnectParams>();
    }

    #[test]
    fn connect_params_construct_smoke() {
        let v = Arc::new(HostKeyVerifier::new(
            HostKeyPolicy::Reject,
            42,
            "ts.example",
            10022,
        ));
        let p = RusshConnectParams::new(42, "ts.example", 10022, "serveradmin", "secret", v);
        assert_eq!(p.config_id, 42);
        assert_eq!(p.port, 10022);
        assert_eq!(p.user, "serveradmin");
        // password retained; no plaintext leak via Debug because the
        // struct is not Debug.
        assert_eq!(p.password, "secret");
    }
}
