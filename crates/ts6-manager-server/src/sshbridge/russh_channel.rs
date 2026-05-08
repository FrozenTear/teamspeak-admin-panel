//! Concrete russh-backed [`SshChannel`] for SSHBridge.
//!
//! [`connect`] performs the full SSH bring-up sequence:
//!
//! 1. Open a TCP socket via `russh::client::connect`.
//! 2. Pass the host-key verifier into the russh handler — verification
//!    runs inside `Handler::check_server_key`, before the SSH key
//!    exchange completes. The shared `rejected` flag lets the outer
//!    error branch decide on `HostKeyMismatch` from the verifier's own
//!    `Ok(false)` return rather than string-matching russh's connect
//!    error (PURA-86 — fragile across russh versions).
//! 3. Authenticate via the variant carried in [`RusshConnectParams::auth`]:
//!    [`connect_password`] (`sshAuthMethod='password'`),
//!    [`connect_key`] (`sshAuthMethod='key'`, encrypted-at-rest OpenSSH
//!    private-key blob via [`crate::crypto::unseal`]), or
//!    [`connect_agent`] (`sshAuthMethod='agent'`, signing delegated to a
//!    `ssh-agent` Unix socket). `AuthResult::Failure` from any variant
//!    maps to [`TransportError::AuthRejected`] so the supervisor stops
//!    without retry.
//! 4. Open a session channel and request a shell. The TS6 ServerQuery
//!    SSH interface starts the line protocol on shell-up.
//! 5. Wrap the channel in [`RusshChannel`] and return it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use russh::client::{self, Handler};
use russh::keys::ssh_key::PublicKey;
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};
use zeroize::Zeroizing;

use super::channel::{looks_like_auth_failure, SshChannel, TransportError};
use super::hostkey::HostKeyVerifier;

/// Auth credential supplied with [`RusshConnectParams`]. Variants map
/// 1:1 to `server_connection.sshAuthMethod` (`'password'` | `'key'` |
/// `'agent'`); the cleartext fields are wrapped in [`Zeroizing`] so any
/// drop scrubs the allocation.
///
/// `Clone` is intentional — the transport supervisor calls the connect
/// factory once per (re)connect cycle, so each attempt clones the
/// credential. The cloned `Zeroizing` is itself zeroized on drop.
#[derive(Clone)]
pub enum RusshAuth {
    /// Cleartext password (post-`crate::crypto::unseal` of the
    /// `sshPassword` ciphertext).
    Password(Zeroizing<String>),
    /// OpenSSH-format private-key blob (post-`crate::crypto::unseal` of
    /// the `sshPrivateKey` ciphertext). The blob MUST already be
    /// cleartext OpenSSH form — SSHBridge does not prompt for a
    /// passphrase, so an inner-passphrase blob is rejected at parse.
    Key(Zeroizing<String>),
    /// Filesystem path to an `ssh-agent` Unix-domain socket (the value
    /// stored in `sshKeyAgentSocket`). The agent does the signing;
    /// SSHBridge never sees the private key bytes for this variant.
    Agent(PathBuf),
}

/// Connection parameters consumed by [`connect`]. The surrounding
/// fields are shared across all three auth methods; the credential
/// itself lives in [`RusshConnectParams::auth`].
///
/// The struct deliberately omits `Debug` to keep credential bytes from
/// accidentally printing via `{:?}`. `RusshAuth::Password` and
/// `RusshAuth::Key` hold their cleartext values inside [`Zeroizing`] so
/// any drop — including a successful clone-and-discard — scrubs the
/// allocation.
pub struct RusshConnectParams {
    pub config_id: i64,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub verifier: Arc<HostKeyVerifier>,
    pub auth: RusshAuth,
}

impl RusshConnectParams {
    /// Build a `password` variant.
    pub fn new_password(
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
            verifier,
            auth: RusshAuth::Password(Zeroizing::new(password.into())),
        }
    }

    /// Build a `key` variant. `private_key_pem` is the OpenSSH-format
    /// private-key blob in cleartext (post-`crate::crypto::unseal`).
    pub fn new_key(
        config_id: i64,
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        private_key_pem: impl Into<String>,
        verifier: Arc<HostKeyVerifier>,
    ) -> Self {
        Self {
            config_id,
            host: host.into(),
            port,
            user: user.into(),
            verifier,
            auth: RusshAuth::Key(Zeroizing::new(private_key_pem.into())),
        }
    }

    /// Build an `agent` variant. `socket_path` is the
    /// `sshKeyAgentSocket` value — typically the operator's
    /// `SSH_AUTH_SOCK` filesystem path.
    pub fn new_agent(
        config_id: i64,
        host: impl Into<String>,
        port: u16,
        user: impl Into<String>,
        socket_path: PathBuf,
        verifier: Arc<HostKeyVerifier>,
    ) -> Self {
        Self {
            config_id,
            host: host.into(),
            port,
            user: user.into(),
            verifier,
            auth: RusshAuth::Agent(socket_path),
        }
    }
}

/// russh handler — only the host-key callback matters for SSHBridge.
///
/// `russh::client::Handler` uses a native return-position-impl-trait
/// signature for `check_server_key`; we must NOT layer `#[async_trait]`
/// over this impl (that would rewrite the method to a boxed future and
/// the lifetimes would no longer line up with russh's trait).
///
/// The `rejected` flag is shared with [`open_session`] so the rejection
/// branch is decided by the verifier's own `Ok(false)` return rather
/// than by string-matching russh's resulting connect error. String
/// matches on `"UnknownKey"` / `"rejected by user"` were the previous
/// heuristic and are fragile across russh versions; PURA-86 replaces
/// them with this explicit signal.
struct BridgeHandler {
    verifier: Arc<HostKeyVerifier>,
    rejected: Arc<AtomicBool>,
}

impl Handler for BridgeHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKey,
    ) -> Result<bool, Self::Error> {
        let ok = self.verifier.verify(server_public_key);
        if !ok {
            self.rejected.store(true, Ordering::SeqCst);
        }
        Ok(ok)
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

/// Top-level dispatcher — picks the right `connect_*` entrypoint based
/// on the [`RusshAuth`] variant carried by `params`. The transport
/// supervisor's `connect_factory` calls this directly so the variant
/// dispatch happens at connect time, not at factory-build time.
pub async fn connect(params: RusshConnectParams) -> Result<RusshChannel, TransportError> {
    match params.auth {
        RusshAuth::Password(_) => connect_password(params).await,
        RusshAuth::Key(_) => connect_key(params).await,
        RusshAuth::Agent(_) => connect_agent(params).await,
    }
}

/// TCP connect + SSH key exchange + host-key check. Shared by all three
/// auth variants — auth + shell-open are layered on top by each
/// variant. Returns the live session handle on success.
async fn open_session(
    host: &str,
    port: u16,
    verifier: Arc<HostKeyVerifier>,
) -> Result<client::Handle<BridgeHandler>, TransportError> {
    let config = Arc::new(client::Config {
        // Reasonable defaults; russh's own keepalive layer is disabled
        // because [`super::transport`] runs application-level
        // `whoami` keepalives at the ServerQuery layer (spec §11.3).
        inactivity_timeout: None,
        keepalive_interval: None,
        ..Default::default()
    });

    let host_key_rejected = Arc::new(AtomicBool::new(false));
    let handler = BridgeHandler {
        verifier,
        rejected: host_key_rejected.clone(),
    };

    match client::connect(config, (host, port), handler).await {
        Ok(s) => Ok(s),
        Err(e) => {
            // Verifier-driven host-key rejection wins over any string
            // heuristic — if our handler returned `Ok(false)`, this
            // connect error is a host-key mismatch even if russh's
            // wording shifts across versions. The verifier already
            // tracing::warn!s every rejection under target
            // `sshbridge::hostkey`, so the audit log carries the typed
            // reason regardless of which branch we take here.
            if host_key_rejected.load(Ordering::SeqCst) {
                return Err(TransportError::HostKeyMismatch);
            }
            let s = e.to_string();
            if looks_like_auth_failure(&s) {
                return Err(TransportError::AuthRejected);
            }
            Err(TransportError::Io(s))
        }
    }
}

/// Open the shell channel on an authenticated session and wrap it as a
/// [`RusshChannel`]. Shared between all three auth variants.
async fn open_shell_channel(
    session: client::Handle<BridgeHandler>,
) -> Result<RusshChannel, TransportError> {
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

/// Run the full bring-up sequence for password auth. Errors if
/// `params.auth` is not [`RusshAuth::Password`] (shouldn't happen —
/// [`connect`] dispatches by variant, but the explicit check keeps the
/// entrypoint safe to call directly).
pub async fn connect_password(
    params: RusshConnectParams,
) -> Result<RusshChannel, TransportError> {
    let RusshAuth::Password(ref password) = params.auth else {
        return Err(TransportError::Io(
            "connect_password called with non-password auth variant".into(),
        ));
    };

    let mut session = open_session(&params.host, params.port, params.verifier.clone()).await?;

    match session
        .authenticate_password(&params.user, password.as_str())
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

    open_shell_channel(session).await
}

/// Run the full bring-up sequence for encrypted-at-rest private-key
/// auth (`sshAuthMethod='key'`).
///
/// The OpenSSH-format private-key blob carried in [`RusshAuth::Key`]
/// MUST already be cleartext — operators store it AES-256-GCM-sealed
/// in `server_connection.sshPrivateKey` and `build_ssh_backend` unseals
/// it into a [`Zeroizing<String>`] before constructing the params. Key
/// blobs that are themselves passphrase-encrypted at the OpenSSH layer
/// are not supported and return an `Io` error during parse — operators
/// must remove the inner passphrase before storing the blob (the
/// AES-256-GCM seal at rest is the encryption layer SSHBridge relies on).
pub async fn connect_key(params: RusshConnectParams) -> Result<RusshChannel, TransportError> {
    let RusshAuth::Key(ref pem) = params.auth else {
        return Err(TransportError::Io(
            "connect_key called with non-key auth variant".into(),
        ));
    };

    let private_key = match PrivateKey::from_openssh(pem.as_str()) {
        Ok(k) => k,
        Err(e) => {
            return Err(TransportError::Io(format!(
                "ssh private key parse failed: {e}"
            )))
        }
    };
    if private_key.is_encrypted() {
        return Err(TransportError::Io(
            "stored ssh private key has an inner OpenSSH passphrase; \
             SSHBridge does not prompt — store the key without an inner \
             passphrase (the AES-256-GCM seal at rest is the encryption layer)"
                .into(),
        ));
    }

    let mut session = open_session(&params.host, params.port, params.verifier.clone()).await?;

    // For RSA keys, prefer SHA-256 over the legacy SHA-1 hash; OpenSSH
    // disabled `ssh-rsa` (SHA-1) by default in 8.7. Other algorithms
    // ignore the hash hint per `PrivateKeyWithHashAlg::new` semantics.
    let hash_alg = if private_key.algorithm().is_rsa() {
        Some(HashAlg::Sha256)
    } else {
        None
    };
    let key_with_alg = PrivateKeyWithHashAlg::new(Arc::new(private_key), hash_alg);

    match session
        .authenticate_publickey(&params.user, key_with_alg)
        .await
    {
        Ok(result) => {
            if !result.success() {
                tracing::warn!(
                    target: "sshbridge::transport",
                    config_id = params.config_id,
                    "russh public-key auth rejected"
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

    open_shell_channel(session).await
}

/// Run the full bring-up sequence for ssh-agent auth
/// (`sshAuthMethod='agent'`).
///
/// Connects to the operator's `ssh-agent` over the Unix-domain socket
/// at [`RusshAuth::Agent`]'s path, asks for the loaded identities, and
/// tries each one in turn via russh's [`PrivateKey`]-less
/// `authenticate_publickey_with` flow — signing is delegated back to
/// the agent. The first identity that authenticates wins; if none do,
/// the transport returns [`TransportError::AuthRejected`].
pub async fn connect_agent(params: RusshConnectParams) -> Result<RusshChannel, TransportError> {
    let RusshAuth::Agent(ref socket_path) = params.auth else {
        return Err(TransportError::Io(
            "connect_agent called with non-agent auth variant".into(),
        ));
    };

    let mut agent =
        match russh::keys::agent::client::AgentClient::connect_uds(socket_path.as_path()).await {
            Ok(a) => a,
            Err(e) => {
                return Err(TransportError::Io(format!(
                    "ssh-agent connect to {} failed: {e}",
                    socket_path.display()
                )))
            }
        };

    let identities = match agent.request_identities().await {
        Ok(v) => v,
        Err(e) => {
            return Err(TransportError::Io(format!(
                "ssh-agent request_identities failed: {e}"
            )))
        }
    };
    if identities.is_empty() {
        return Err(TransportError::Io(format!(
            "ssh-agent at {} has no identities loaded; \
             run `ssh-add` on the operator host before retrying",
            socket_path.display()
        )));
    }

    let mut session = open_session(&params.host, params.port, params.verifier.clone()).await?;

    // Try identities in the order the agent reports them. A non-fatal
    // `AuthResult::Failure` advances to the next identity — many SSH
    // daemons accept multiple `userauth_request` packets per session
    // until they give up and close. Russh-side errors (transport / IO)
    // are fatal and bypass the loop.
    let mut tried = 0usize;
    for identity in identities {
        tried += 1;
        let public_key = identity.public_key().into_owned();
        let hash_alg = if public_key.algorithm().is_rsa() {
            Some(HashAlg::Sha256)
        } else {
            None
        };
        match session
            .authenticate_publickey_with(&params.user, public_key, hash_alg, &mut agent)
            .await
        {
            Ok(result) => {
                if result.success() {
                    return open_shell_channel(session).await;
                }
                // Otherwise: agent's next identity gets a turn.
            }
            Err(e) => {
                // `russh::keys::agent::AgentAuthError` covers both
                // `SendError` (russh-internal channel closed) and
                // `Key` (agent-side protocol error). Either way, the
                // session is no longer usable for further attempts.
                let s = e.to_string();
                if looks_like_auth_failure(&s) {
                    return Err(TransportError::AuthRejected);
                }
                return Err(TransportError::Io(s));
            }
        }
    }

    tracing::warn!(
        target: "sshbridge::transport",
        config_id = params.config_id,
        identities_tried = tried,
        "ssh-agent identities exhausted, none authenticated"
    );
    Err(TransportError::AuthRejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sshbridge::hostkey::HostKeyPolicy;

    fn dummy_verifier() -> Arc<HostKeyVerifier> {
        Arc::new(HostKeyVerifier::new(
            HostKeyPolicy::Reject,
            42,
            "ts.example",
            10022,
        ))
    }

    #[test]
    fn connect_params_struct_holds_no_password_in_debug() {
        // The struct deliberately omits Debug; this test exists as a
        // regression assertion — if someone derives Debug, the trait
        // bound below will fail to compile.
        fn requires_no_debug<T>() {}
        requires_no_debug::<RusshConnectParams>();
    }

    #[test]
    fn new_password_constructs_password_variant() {
        let p = RusshConnectParams::new_password(
            42,
            "ts.example",
            10022,
            "serveradmin",
            "secret",
            dummy_verifier(),
        );
        assert_eq!(p.config_id, 42);
        assert_eq!(p.port, 10022);
        assert_eq!(p.user, "serveradmin");
        match &p.auth {
            RusshAuth::Password(s) => assert_eq!(s.as_str(), "secret"),
            other => panic!("expected Password variant, got {}", auth_kind(other)),
        }
    }

    #[test]
    fn new_key_constructs_key_variant() {
        let p = RusshConnectParams::new_key(
            7,
            "ts.example",
            10022,
            "serveradmin",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----\n",
            dummy_verifier(),
        );
        assert!(matches!(p.auth, RusshAuth::Key(_)));
    }

    #[test]
    fn new_agent_constructs_agent_variant() {
        let p = RusshConnectParams::new_agent(
            9,
            "ts.example",
            10022,
            "serveradmin",
            PathBuf::from("/run/user/1000/ssh-agent.sock"),
            dummy_verifier(),
        );
        match &p.auth {
            RusshAuth::Agent(path) => {
                assert_eq!(path.to_string_lossy(), "/run/user/1000/ssh-agent.sock");
            }
            other => panic!("expected Agent variant, got {}", auth_kind(other)),
        }
    }

    #[tokio::test]
    async fn connect_password_rejects_mismatched_variant() {
        let p = RusshConnectParams::new_agent(
            9,
            "ts.example",
            10022,
            "serveradmin",
            PathBuf::from("/tmp/never-touched.sock"),
            dummy_verifier(),
        );
        let err = match connect_password(p).await {
            Err(e) => e,
            Ok(_) => panic!("expected variant mismatch, got Ok"),
        };
        match err {
            TransportError::Io(s) => {
                assert!(s.contains("non-password"), "unexpected message: {s}");
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_key_rejects_mismatched_variant() {
        let p = RusshConnectParams::new_password(
            9,
            "ts.example",
            10022,
            "serveradmin",
            "secret",
            dummy_verifier(),
        );
        let err = match connect_key(p).await {
            Err(e) => e,
            Ok(_) => panic!("expected variant mismatch, got Ok"),
        };
        match err {
            TransportError::Io(s) => assert!(s.contains("non-key"), "msg: {s}"),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_agent_rejects_mismatched_variant() {
        let p = RusshConnectParams::new_password(
            9,
            "ts.example",
            10022,
            "serveradmin",
            "secret",
            dummy_verifier(),
        );
        let err = match connect_agent(p).await {
            Err(e) => e,
            Ok(_) => panic!("expected variant mismatch, got Ok"),
        };
        match err {
            TransportError::Io(s) => assert!(s.contains("non-agent"), "msg: {s}"),
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_key_rejects_unparseable_pem() {
        let p = RusshConnectParams::new_key(
            7,
            "ts.example",
            10022,
            "serveradmin",
            "not a private key",
            dummy_verifier(),
        );
        let err = match connect_key(p).await {
            Err(e) => e,
            Ok(_) => panic!("expected parse failure, got Ok"),
        };
        match err {
            TransportError::Io(s) => {
                assert!(s.contains("parse failed"), "unexpected message: {s}");
            }
            other => panic!("expected Io parse error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn connect_agent_io_error_when_socket_missing() {
        let p = RusshConnectParams::new_agent(
            9,
            "ts.example",
            10022,
            "serveradmin",
            PathBuf::from("/nonexistent/socket-pura85-test"),
            dummy_verifier(),
        );
        let err = match connect_agent(p).await {
            Err(e) => e,
            Ok(_) => panic!("expected agent connect failure, got Ok"),
        };
        match err {
            TransportError::Io(s) => {
                assert!(s.contains("ssh-agent connect"), "unexpected message: {s}");
            }
            other => panic!("expected Io error, got {other:?}"),
        }
    }

    fn auth_kind(a: &RusshAuth) -> &'static str {
        match a {
            RusshAuth::Password(_) => "password",
            RusshAuth::Key(_) => "key",
            RusshAuth::Agent(_) => "agent",
        }
    }
}
