//! Phase 2 PURA-78 — backend-agnostic control plane for read-only TS6
//! ServerQuery commands. The trait covers exactly the Phase 1 surface
//! [`crate::webquery::dashboard`] consumes; both [`WebQueryClient`] and
//! [`SshControlClient`] implement it and a per-server `controlPath`
//! flag picks one at pool construction time.
//!
//! ## What's here
//!
//! - [`ControlBackend`] — async trait with `version` / `serverlist` /
//!   `serverinfo` / `channellist` / `clientlist` /
//!   `server_connection_info`. Each method returns the same typed shape
//!   ([`VersionInfo`], [`VirtualServerEntry`], [`ServerInfo`],
//!   [`ChannelEntry`], [`ClientEntry`], [`ConnectionInfo`]) so the REST
//!   layer never needs to know which backend served the response.
//! - [`ControlBackendError`] — shape-aligned with [`WebQueryError`] and
//!   [`SshBridgeError`]; `http_status`, `upstream_code`, and
//!   `upstream_message` mirror them so the §7.0.2 envelope is preserved
//!   on either control path.
//! - `impl ControlBackend for` [`WebQueryClient`] — straight delegation.
//! - `impl ControlBackend for` [`SshControlClient`] — defined alongside
//!   the type in [`crate::sshbridge::control_client`].
//! - [`ControlBackendPool`] — keyed by `server_connection.id`; reads
//!   `connection.controlPath` on first miss to instantiate the matching
//!   client and stores it as `Arc<dyn ControlBackend>`.
//!
//! ## Why a parallel pool to [`crate::webquery::WebQueryPool`]
//!
//! The Phase 2 write surface ([`crate::routes::control`]) still talks
//! to a [`WebQueryClient`] directly — the SSH write commands land in a
//! later child issue. Keeping `WebQueryPool` in place avoids changing
//! those call sites in this slice; the new [`ControlBackendPool`] is
//! consumed only by the dashboard handler. Once the SSH write surface
//! lands, the write side migrates onto a richer trait and
//! [`WebQueryPool`] retires.
//!
//! ## Out of scope
//!
//! - SSHBridge write methods (`clientmove`, `clientkick`, `banadd`, …).
//! - Pool eviction on `PUT/DELETE /servers` — that work belongs to a
//!   later refresh-on-edit child.
//! - Bulk-fleet operations.

#![allow(dead_code)] // trait surface + pool helpers consumed by routes / future Phase 2 hooks.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use reqwest::StatusCode;
use thiserror::Error;
use tokio::sync::RwLock;

use crate::db::Database;
use crate::repos::server_connections::ServerConnection;
use crate::sshbridge::{
    control_client::SshControlClient,
    hostkey::{HostKeyConfigError, HostKeyVerifier},
    russh_channel::{connect as ssh_connect, RusshAuth, RusshConnectParams},
    transport::{spawn_with_db as spawn_transport_with_db, TransportConfig},
    SshBridgeError,
};
use zeroize::Zeroizing;
use crate::webquery::{
    models::{
        ChannelEntry, ClientEntry, ConnectionInfo, ServerInfo, VersionInfo, VirtualServerEntry,
    },
    WebQueryClient, WebQueryError,
};

/// Errors returned by [`ControlBackend`] methods. Variants are
/// shape-aligned with both [`WebQueryError`] and [`SshBridgeError`] so
/// callers map either backend's failures through the same §7.0.2 path.
#[derive(Debug, Error)]
pub enum ControlBackendError {
    /// Upstream returned a non-zero status code (`error id=…` on SSH;
    /// `status.code != 0` on WebQuery). Maps to `502 {error: "TeamSpeak
    /// API Error", code, details}` per §7.0.2.
    #[error("TS upstream error {code}: {message}")]
    Upstream { code: i64, message: String },

    /// Transport-class failure (network, TLS, SSH session). Maps to
    /// `502` with `code = -1` per §10.5.
    #[error("control transport error: {0}")]
    Transport(String),

    /// The response could not be parsed into the expected typed shape.
    /// Maps to `502` with `code = -1`.
    #[error("malformed control response: {0}")]
    InvalidResponse(String),

    /// Stored credential (apiKey ciphertext, sshPassword ciphertext)
    /// failed to decrypt. Construction-time only.
    #[error("failed to decrypt control credentials for connection #{config_id}: {message}")]
    Decrypt { config_id: i64, message: String },

    /// SSH-only — auth was rejected by the upstream SSH daemon. The
    /// REST layer reports this as `502` (operator sees the same
    /// envelope shape as transport errors) but the bridge surfaces a
    /// separate "credentials need attention" signal via the connection
    /// lifecycle.
    #[error("control auth rejected for connection #{config_id}")]
    AuthRejected { config_id: i64 },

    /// SSH-only — the host-key verifier rejected the server-presented
    /// key. Parallel to [`AuthRejected`] — both are fatal, fail-closed,
    /// and surface a separate operator-attention signal. The REST layer
    /// renders the operator-friendly remediation hint
    /// ("verify the new fingerprint via `ssh-keyscan` and update
    /// `sshHostKeyFingerprint` on the row") in the §7.0.2 `details`
    /// string.
    #[error("control host-key mismatch for connection #{config_id}")]
    HostKeyMismatch { config_id: i64 },

    /// Configuration-time error (host-key fingerprint malformed,
    /// required column missing). Maps to `500` because the operator
    /// row needs editing before the request can succeed.
    #[error("control backend configuration error: {0}")]
    Config(String),
}

impl ControlBackendError {
    /// HTTP status code per §7.0.2 / §10.5 — same mapping as the
    /// individual backend errors.
    pub fn http_status(&self) -> StatusCode {
        match self {
            Self::Upstream { .. }
            | Self::Transport(_)
            | Self::InvalidResponse(_)
            | Self::AuthRejected { .. }
            | Self::HostKeyMismatch { .. } => StatusCode::BAD_GATEWAY,
            Self::Decrypt { .. } | Self::Config(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Upstream code surfaced in the §7.0.2 body. Non-upstream errors
    /// report `-1`.
    pub fn upstream_code(&self) -> i64 {
        match self {
            Self::Upstream { code, .. } => *code,
            _ => -1,
        }
    }

    /// Operator-friendly `details` string for the §7.0.2 body.
    pub fn upstream_message(&self) -> String {
        match self {
            Self::Upstream { message, .. } => message.clone(),
            Self::HostKeyMismatch { config_id } => format!(
                "host-key fingerprint did not match the pinned value for connection #{config_id}; \
                 verify the new fingerprint via `ssh-keyscan` and update sshHostKeyFingerprint on the row"
            ),
            other => other.to_string(),
        }
    }
}

impl From<WebQueryError> for ControlBackendError {
    fn from(e: WebQueryError) -> Self {
        match e {
            WebQueryError::Upstream { code, message } => Self::Upstream { code, message },
            WebQueryError::Transport(s) => Self::Transport(s),
            WebQueryError::InvalidResponse(s) => Self::InvalidResponse(s),
            WebQueryError::Decrypt { config_id, source } => Self::Decrypt {
                config_id,
                message: source.to_string(),
            },
        }
    }
}

impl From<SshBridgeError> for ControlBackendError {
    fn from(e: SshBridgeError) -> Self {
        match e {
            SshBridgeError::Upstream { code, message } => Self::Upstream { code, message },
            SshBridgeError::Transport(s) => Self::Transport(s),
            SshBridgeError::InvalidResponse(s) => Self::InvalidResponse(s),
            SshBridgeError::Decrypt { config_id, source } => Self::Decrypt {
                config_id,
                message: source.to_string(),
            },
            SshBridgeError::AuthRejected { config_id } => Self::AuthRejected { config_id },
            SshBridgeError::HostKeyMismatch { config_id } => {
                Self::HostKeyMismatch { config_id }
            }
        }
    }
}

impl From<HostKeyConfigError> for ControlBackendError {
    fn from(e: HostKeyConfigError) -> Self {
        Self::Config(e.to_string())
    }
}

pub type ControlResult<T> = Result<T, ControlBackendError>;

/// The Phase 1 read-only ServerQuery surface. Both backends implement
/// the same six methods; the dashboard handler is the only consumer in
/// this slice. Trait is `dyn`-safe (object-safe) so the pool can hand
/// out `Arc<dyn ControlBackend + Send + Sync>` without an enum dispatch.
#[async_trait]
pub trait ControlBackend: Send + Sync + std::fmt::Debug {
    /// `version` — instance scope. Doubles as the cheap health probe
    /// per §10.7.
    async fn version(&self) -> ControlResult<VersionInfo>;

    /// `serverlist` — instance scope. Drives the virtual-server
    /// selector.
    async fn serverlist(&self) -> ControlResult<Vec<VirtualServerEntry>>;

    /// `serverinfo` — virtual-server scope.
    async fn serverinfo(&self, sid: i64) -> ControlResult<ServerInfo>;

    /// `channellist` — virtual-server scope. Basic projection only;
    /// flag-driven projections are a Phase 2 follow-up.
    async fn channellist(&self, sid: i64) -> ControlResult<Vec<ChannelEntry>>;

    /// `clientlist` — virtual-server scope. Basic projection only.
    async fn clientlist(&self, sid: i64) -> ControlResult<Vec<ClientEntry>>;

    /// `serverrequestconnectioninfo` — virtual-server scope.
    async fn server_connection_info(&self, sid: i64) -> ControlResult<ConnectionInfo>;
}

/// Straight delegation. Method-call resolution prefers the inherent
/// methods on [`WebQueryClient`] over the trait's, so `self.version()`
/// in the trait body does NOT recurse — Rust picks
/// `WebQueryClient::version` first. Disambiguating UFCS would only
/// add noise.
#[async_trait]
impl ControlBackend for WebQueryClient {
    async fn version(&self) -> ControlResult<VersionInfo> {
        self.version().await.map_err(Into::into)
    }

    async fn serverlist(&self) -> ControlResult<Vec<VirtualServerEntry>> {
        self.serverlist().await.map_err(Into::into)
    }

    async fn serverinfo(&self, sid: i64) -> ControlResult<ServerInfo> {
        self.serverinfo(sid).await.map_err(Into::into)
    }

    async fn channellist(&self, sid: i64) -> ControlResult<Vec<ChannelEntry>> {
        self.channellist(sid).await.map_err(Into::into)
    }

    async fn clientlist(&self, sid: i64) -> ControlResult<Vec<ClientEntry>> {
        self.clientlist(sid).await.map_err(Into::into)
    }

    async fn server_connection_info(&self, sid: i64) -> ControlResult<ConnectionInfo> {
        self.server_connection_info(sid).await.map_err(Into::into)
    }
}

/// Pool of [`ControlBackend`] clients keyed by `server_connection.id`.
///
/// Lazy build on first miss. The `connection.controlPath` flag selects
/// the backend variant — `"webquery"` (default) builds a
/// [`WebQueryClient`]; `"ssh"` builds an [`SshControlClient`] backed by
/// a russh transport. Unknown values fall back to WebQuery so a
/// future deviation in the column does not break booted servers.
#[derive(Clone)]
pub struct ControlBackendPool {
    inner: Arc<RwLock<HashMap<i64, Arc<dyn ControlBackend>>>>,
    allow_self_signed: bool,
    /// Optional path to the operator's `known_hosts` file. Sourced
    /// from `TS_SSH_KNOWN_HOSTS` at boot; `None` falls through to the
    /// per-server fingerprint column or `Reject`.
    ssh_known_hosts_path: Option<PathBuf>,
    /// PURA-79: SurrealDB handle threaded into the SSH transport
    /// supervisor via [`spawn_transport_with_db`]. Without it the
    /// dispatch loop only `tracing::info!`s audit events and
    /// `ssh_audit_log` stays empty in production. Cheap to clone — the
    /// `Surreal<Any>` handle is internally `Arc`-shared.
    db: Arc<Database>,
}

impl std::fmt::Debug for ControlBackendPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlBackendPool")
            .field("allow_self_signed", &self.allow_self_signed)
            .field(
                "ssh_known_hosts_path",
                &self
                    .ssh_known_hosts_path
                    .as_ref()
                    .map(|p| p.display().to_string()),
            )
            .finish_non_exhaustive()
    }
}

impl ControlBackendPool {
    pub fn new(allow_self_signed: bool, db: Arc<Database>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            allow_self_signed,
            ssh_known_hosts_path: None,
            db,
        }
    }

    pub fn with_known_hosts(mut self, path: Option<PathBuf>) -> Self {
        self.ssh_known_hosts_path = path;
        self
    }

    /// Fetch the backend for `config_id`, building one from `connection`
    /// on first miss.  Returns `Transport`-class error when no
    /// connection row is supplied and the cache is cold (matches the
    /// dashboard's §10.7 mapping to `500 "No connection configured for
    /// server config ID"`).
    pub async fn get_or_build(
        &self,
        config_id: i64,
        connection: Option<&ServerConnection>,
    ) -> ControlResult<Arc<dyn ControlBackend>> {
        if let Some(existing) = self.inner.read().await.get(&config_id).cloned() {
            return Ok(existing);
        }
        let connection = connection.ok_or_else(|| {
            ControlBackendError::Transport(format!(
                "No connection configured for server config ID {config_id}"
            ))
        })?;
        let backend = self.build_backend(connection).await?;
        self.inner
            .write()
            .await
            .insert(config_id, backend.clone());
        Ok(backend)
    }

    /// Drop the cached backend for `config_id`. Reserved for the
    /// future refresh-on-edit hook on `PUT/DELETE /servers/:configId`.
    pub async fn remove(&self, config_id: i64) {
        self.inner.write().await.remove(&config_id);
    }

    /// Inject a backend for `config_id` (test-only). The PURA-81
    /// dashboard-tick suite uses this to bypass the WebQuery / SSH
    /// builders and exercise the supervisor + worker logic against a
    /// hand-rolled [`ControlBackend`] fake.
    #[cfg(test)]
    pub(crate) async fn insert_for_test(
        &self,
        config_id: i64,
        backend: Arc<dyn ControlBackend>,
    ) {
        self.inner.write().await.insert(config_id, backend);
    }

    async fn build_backend(
        &self,
        connection: &ServerConnection,
    ) -> ControlResult<Arc<dyn ControlBackend>> {
        match connection.controlPath.as_str() {
            "ssh" => self.build_ssh_backend(connection).await,
            // "webquery" and any unknown value: default to WebQuery.
            _ => self.build_webquery_backend(connection),
        }
    }

    fn build_webquery_backend(
        &self,
        connection: &ServerConnection,
    ) -> ControlResult<Arc<dyn ControlBackend>> {
        let client = WebQueryClient::from_connection(connection, self.allow_self_signed)?;
        Ok(Arc::new(client))
    }

    async fn build_ssh_backend(
        &self,
        connection: &ServerConnection,
    ) -> ControlResult<Arc<dyn ControlBackend>> {
        let user = connection.sshUsername.clone().ok_or_else(|| {
            ControlBackendError::Config(format!(
                "ssh control path selected for connection #{} but sshUsername is null",
                connection.id
            ))
        })?;

        // PURA-85 — branch on `sshAuthMethod` (D-SSH-AUTH, PURA-77). Each
        // method picks the row column it needs and unseals it; an
        // unknown method short-circuits with `Config` so an operator who
        // typoes the value sees a single explicit error rather than a
        // `Decrypt` ("required column null") deeper in the stack. The
        // unseal happens here so the `Zeroizing<String>` lives only as
        // long as the closure capture below.
        let auth = match connection.sshAuthMethod.as_str() {
            "password" => RusshAuth::Password(unseal_for(
                connection,
                connection.sshPassword.as_deref(),
                "sshPassword",
                "password",
            )?),
            "key" => RusshAuth::Key(unseal_for(
                connection,
                connection.sshPrivateKey.as_deref(),
                "sshPrivateKey",
                "key",
            )?),
            "agent" => {
                let socket = connection.sshKeyAgentSocket.clone().ok_or_else(|| {
                    ControlBackendError::Config(format!(
                        "sshAuthMethod='agent' for connection #{} but sshKeyAgentSocket is null",
                        connection.id
                    ))
                })?;
                RusshAuth::Agent(PathBuf::from(socket))
            }
            method => {
                return Err(ControlBackendError::Config(format!(
                    "sshAuthMethod={method:?} not recognised; expected 'password', 'key', or 'agent'"
                )));
            }
        };

        let port: u16 = connection.sshPort.try_into().unwrap_or(10022);
        let verifier = Arc::new(HostKeyVerifier::from_config(
            connection.id,
            connection.host.clone(),
            port,
            connection.sshHostKeyFingerprint.as_deref(),
            self.ssh_known_hosts_path.clone(),
        )?);

        let cfg = TransportConfig::for_connection(connection.id);
        let host = connection.host.clone();
        let user_owned = user;
        let verifier_clone = verifier;
        let config_id = connection.id;
        let auth_owned = auth;

        // The connect factory clones the credential per attempt — the
        // supervisor calls it once per (re)connect cycle. Cloning a
        // `Zeroizing<String>` produces a fresh allocation that is
        // itself zeroized on drop, matching the bring-up cost of always
        // producing fresh secret bytes per connect attempt. The
        // [`ssh_connect`] dispatcher picks `connect_password` /
        // `connect_key` / `connect_agent` from the variant.
        let factory = move || {
            let h = host.clone();
            let u = user_owned.clone();
            let v = verifier_clone.clone();
            let a = auth_owned.clone();
            async move {
                let params = RusshConnectParams {
                    config_id,
                    host: h,
                    port,
                    user: u,
                    verifier: v,
                    auth: a,
                };
                ssh_connect(params).await
            }
        };

        let handle = spawn_transport_with_db(cfg, factory, self.db.clone());
        let client = SshControlClient::new(connection.id, handle);
        Ok(Arc::new(client))
    }
}

/// Helper for the `'password'` and `'key'` branches of
/// [`ControlBackendPool::build_ssh_backend`] — unseals the named
/// ciphertext column and wraps the cleartext in [`Zeroizing`]. Returns
/// `Config` if the column is null and `Decrypt` if AES-GCM unseal fails.
fn unseal_for(
    connection: &ServerConnection,
    column_value: Option<&str>,
    column_name: &str,
    method: &str,
) -> ControlResult<Zeroizing<String>> {
    let ct = column_value.ok_or_else(|| {
        ControlBackendError::Config(format!(
            "sshAuthMethod='{method}' for connection #{} but {column_name} is null",
            connection.id
        ))
    })?;
    let cleartext =
        crate::crypto::unseal(ct).map_err(|e| ControlBackendError::Decrypt {
            config_id: connection.id,
            message: e.to_string(),
        })?;
    Ok(Zeroizing::new(cleartext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_aligns_with_per_backend_mapping() {
        let upstream = ControlBackendError::Upstream {
            code: 2568,
            message: "x".into(),
        };
        assert_eq!(upstream.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(upstream.upstream_code(), 2568);

        let transport = ControlBackendError::Transport("boom".into());
        assert_eq!(transport.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(transport.upstream_code(), -1);

        let auth = ControlBackendError::AuthRejected { config_id: 7 };
        assert_eq!(auth.http_status(), StatusCode::BAD_GATEWAY);

        let cfg = ControlBackendError::Config("missing column".into());
        assert_eq!(cfg.http_status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn webquery_error_round_trips() {
        let we = WebQueryError::Upstream {
            code: 1281,
            message: "empty".into(),
        };
        let ce: ControlBackendError = we.into();
        assert!(matches!(ce, ControlBackendError::Upstream { code: 1281, .. }));

        let we = WebQueryError::Transport("dns".into());
        let ce: ControlBackendError = we.into();
        assert!(matches!(ce, ControlBackendError::Transport(_)));
    }

    #[test]
    fn ssh_error_round_trips() {
        let se = SshBridgeError::AuthRejected { config_id: 9 };
        let ce: ControlBackendError = se.into();
        assert!(matches!(
            ce,
            ControlBackendError::AuthRejected { config_id: 9 }
        ));

        let se = SshBridgeError::Upstream {
            code: 2568,
            message: "permissions".into(),
        };
        let ce: ControlBackendError = se.into();
        assert!(matches!(ce, ControlBackendError::Upstream { code: 2568, .. }));

        // PURA-86: HostKeyMismatch round-trips with config-id preserved
        // and the §7.0.2 envelope carries the operator remediation hint.
        let se = SshBridgeError::HostKeyMismatch { config_id: 42 };
        let ce: ControlBackendError = se.into();
        assert!(matches!(
            ce,
            ControlBackendError::HostKeyMismatch { config_id: 42 }
        ));
        assert_eq!(ce.http_status(), StatusCode::BAD_GATEWAY);
        assert_eq!(ce.upstream_code(), -1);
        let details = ce.upstream_message();
        assert!(
            details.contains("#42") && details.contains("ssh-keyscan"),
            "host-key mismatch details must carry the row id and remediation \
             hint: {details}"
        );
    }

    #[tokio::test]
    async fn pool_returns_transport_error_when_connection_missing() {
        let db = crate::db::connect_in_memory()
            .await
            .expect("in-memory connect");
        let pool = ControlBackendPool::new(false, db);
        let err = pool.get_or_build(99, None).await.unwrap_err();
        match err {
            ControlBackendError::Transport(s) => {
                assert!(s.contains("99"), "expected config id in error: {s}");
            }
            other => panic!("expected Transport, got {other:?}"),
        }
    }

    /// Build a minimal `ServerConnection` with `controlPath='ssh'` for the
    /// PURA-85 build-time guard tests. Each test overrides only the
    /// auth-related fields it cares about.
    fn ssh_connection(
        id: i64,
        ssh_auth_method: &str,
        ssh_password: Option<&str>,
        ssh_private_key: Option<&str>,
        ssh_key_agent_socket: Option<&str>,
    ) -> ServerConnection {
        use chrono::Utc;
        ServerConnection {
            id,
            name: format!("test-{id}"),
            host: "ts.example".into(),
            webqueryPort: 10080,
            apiKey: "enc:ignored".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: Some("serveradmin".into()),
            sshPassword: ssh_password.map(str::to_owned),
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            createdAt: Utc::now(),
            updatedAt: Utc::now(),
            controlPath: "ssh".into(),
            sshAuthMethod: ssh_auth_method.into(),
            sshPrivateKey: ssh_private_key.map(str::to_owned),
            sshKeyAgentSocket: ssh_key_agent_socket.map(str::to_owned),
            sshHostKeyFingerprint: None,
        }
    }

    /// PURA-85 AC1 — an unrecognised `sshAuthMethod` short-circuits with
    /// a `Config` error that names the offending value, not a `Decrypt`
    /// or transport error from somewhere deeper in the stack.
    #[tokio::test]
    async fn build_ssh_backend_rejects_unknown_auth_method() {
        let db = crate::db::connect_in_memory().await.expect("in-memory");
        let pool = ControlBackendPool::new(false, db);
        let conn = ssh_connection(1, "totp", Some("enc:ignored"), None, None);
        let err = pool.build_ssh_backend(&conn).await.unwrap_err();
        match err {
            ControlBackendError::Config(s) => {
                assert!(
                    s.contains("\"totp\"") && s.contains("not recognised"),
                    "expected unknown-method message, got: {s}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    /// PURA-85 AC1/AC2 — `sshAuthMethod='agent'` with a null
    /// `sshKeyAgentSocket` returns an explicit `Config` error rather
    /// than failing later inside `connect_agent`.
    #[tokio::test]
    async fn build_ssh_backend_agent_requires_socket_path() {
        let db = crate::db::connect_in_memory().await.expect("in-memory");
        let pool = ControlBackendPool::new(false, db);
        let conn = ssh_connection(2, "agent", None, None, None);
        let err = pool.build_ssh_backend(&conn).await.unwrap_err();
        match err {
            ControlBackendError::Config(s) => {
                assert!(
                    s.contains("sshAuthMethod='agent'") && s.contains("sshKeyAgentSocket"),
                    "unexpected message: {s}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    /// PURA-85 AC1/AC2 — `sshAuthMethod='key'` with a null
    /// `sshPrivateKey` returns an explicit `Config` error before the
    /// `Zeroizing<String>` path is even constructed.
    #[tokio::test]
    async fn build_ssh_backend_key_requires_private_key() {
        let db = crate::db::connect_in_memory().await.expect("in-memory");
        let pool = ControlBackendPool::new(false, db);
        let conn = ssh_connection(3, "key", None, None, None);
        let err = pool.build_ssh_backend(&conn).await.unwrap_err();
        match err {
            ControlBackendError::Config(s) => {
                assert!(
                    s.contains("sshAuthMethod='key'") && s.contains("sshPrivateKey"),
                    "unexpected message: {s}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }

    /// PURA-85 AC1 — `sshAuthMethod='password'` with a null
    /// `sshPassword` still surfaces a `Config` error (mirrors the prior
    /// behaviour, retained for regression). The message now also names
    /// the auth method so an operator who switched to `'password'` from
    /// another method sees the same explicit signal.
    #[tokio::test]
    async fn build_ssh_backend_password_requires_password() {
        let db = crate::db::connect_in_memory().await.expect("in-memory");
        let pool = ControlBackendPool::new(false, db);
        let conn = ssh_connection(4, "password", None, None, None);
        let err = pool.build_ssh_backend(&conn).await.unwrap_err();
        match err {
            ControlBackendError::Config(s) => {
                assert!(
                    s.contains("sshAuthMethod='password'") && s.contains("sshPassword"),
                    "unexpected message: {s}"
                );
            }
            other => panic!("expected Config, got {other:?}"),
        }
    }
}
