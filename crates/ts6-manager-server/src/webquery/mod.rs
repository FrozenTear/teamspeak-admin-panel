//! Spec Chapter 10 — outbound HTTP client to the per-server TS6 WebQuery API.
//!
//! Phase 1 (PURA-23) ships the **read-only** subset:
//!
//! - [`WebQueryClient`] per `server_connection` row, single keep-alive socket
//!   so each upstream registers exactly one ServerQuery slot (§10.1).
//! - [`WebQueryPool`] keyed by `server_connection.id`, lazy-creating clients
//!   on first call. Boot-time pre-population, `autoStart`, the 30s health
//!   probe, and the `refreshClient` lifecycle on `PUT/DELETE /servers` are
//!   Phase 2 follow-ups; the issue scope explicitly defers them.
//! - Typed read methods needed by the §7.19 dashboard route plus the cheap
//!   `version` probe used as a health check.
//! - Spec §10.5/§10.6 envelope handling: non-zero `status.code` → typed
//!   [`WebQueryError::Upstream`] which the route layer maps to `502
//!   {error: "TeamSpeak API Error", code, details}` per §7.0.2.
//!
//! Out of scope for Phase 1 (handed to future REST/WEBQUERY engineer):
//! write-side commands, the SSH event bridge, the WebQuery command whitelist
//! consumed by the bot flow runtime.
//!
//! ServerQuery `key=value` escaping (§10.4) lives in [`escape`]; it is **not**
//! applied here because WebQuery's URL encoder already handles the wire-side
//! transform and the spec forbids double-escaping. The escape pair is
//! exposed for the future SSH bridge.

#![allow(dead_code)] // consumed by REST routes and future WEBQUERY callers.

pub mod dashboard;
pub mod escape;
pub mod models;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

use crate::crypto;
use crate::repos::server_connections::ServerConnection;

pub use models::{ChannelEntry, ClientEntry, ConnectionInfo, ServerInfo, VersionInfo, VirtualServerEntry};

/// Spec §10.2 — fixed 15s request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Spec §10.2 — API key header name. Lowercase; reqwest normalises but the
/// header name is canonically `x-api-key`.
const API_KEY_HEADER: &str = "x-api-key";

/// Errors returned by [`WebQueryClient`]. The REST layer maps these to HTTP
/// status codes per spec §7.0.2.
#[derive(Debug, Error)]
pub enum WebQueryError {
    /// TS upstream returned a non-zero `status.code`. `502 {error: "TeamSpeak
    /// API Error", code, details}` per §7.0.2.
    #[error("TS upstream error {code}: {message}")]
    Upstream { code: i64, message: String },

    /// HTTP transport failure (connect refused, TLS rejection, DNS, timeout).
    /// Maps to `502` with `code = -1` per spec §10.5.
    #[error("transport error: {0}")]
    Transport(String),

    /// Response was reachable but not the expected `{body, status}` envelope.
    #[error("malformed WebQuery response: {0}")]
    InvalidResponse(String),

    /// `apiKey` ciphertext failed to decrypt. Construction-time only.
    #[error("failed to decrypt apiKey for connection #{config_id}: {source}")]
    Decrypt {
        config_id: i64,
        #[source]
        source: crate::crypto::AeadError,
    },
}

impl WebQueryError {
    /// HTTP status code for this error per §7.0.2. Used by the REST layer
    /// to translate uniformly.
    pub fn http_status(&self) -> StatusCode {
        match self {
            WebQueryError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            WebQueryError::Transport(_) => StatusCode::BAD_GATEWAY,
            WebQueryError::InvalidResponse(_) => StatusCode::BAD_GATEWAY,
            WebQueryError::Decrypt { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// TS upstream code surfaced in the §7.0.2 body. Non-upstream errors
    /// report `-1` per spec §10.5.
    pub fn upstream_code(&self) -> i64 {
        match self {
            WebQueryError::Upstream { code, .. } => *code,
            _ => -1,
        }
    }

    /// Operator-friendly `details` string for the §7.0.2 body.
    pub fn upstream_message(&self) -> String {
        match self {
            WebQueryError::Upstream { message, .. } => message.clone(),
            other => other.to_string(),
        }
    }
}

pub type WebQueryResult<T> = Result<T, WebQueryError>;

/// One WebQuery client per `server_connection` row (§10.1).
///
/// Single-socket invariant: enforced by `pool_max_idle_per_host(1)` plus an
/// async [`Mutex`] around the request-issuing path. Concurrent dashboard
/// loads queue rather than open a second socket, so the upstream's
/// ServerQuery clientlist sees exactly one `serveradmin*` slot per managed
/// server (verifiable via §10.9).
pub struct WebQueryClient {
    /// `server_connection.id` — debug only, never logged with credentials.
    config_id: i64,
    base_url: String,
    api_key: String,
    inner: Client,
    /// Serialises requests on a single permit so concurrent callers queue.
    request_lock: Mutex<()>,
}

impl std::fmt::Debug for WebQueryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebQueryClient")
            .field("config_id", &self.config_id)
            .field("base_url", &self.base_url)
            // never expose api_key
            .finish_non_exhaustive()
    }
}

impl WebQueryClient {
    /// Build a client from the decrypted parameters. Most callers use
    /// [`WebQueryClient::from_connection`] which decrypts the apiKey via
    /// [`crate::crypto::unseal`] before delegating here.
    pub fn new(
        config_id: i64,
        host: &str,
        port: u16,
        use_https: bool,
        api_key: String,
        allow_self_signed: bool,
    ) -> WebQueryResult<Self> {
        let scheme = if use_https { "https" } else { "http" };
        let base_url = format!("{scheme}://{host}:{port}");

        let mut builder = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            // §10.1 single-socket invariant.
            .pool_max_idle_per_host(1)
            .http1_only()
            .pool_idle_timeout(Some(Duration::from_secs(90)));

        if allow_self_signed {
            // `TS_ALLOW_SELF_SIGNED` is a deliberate self-host escape hatch
            // (operator-controlled trust). Only honoured when the env flag
            // was true at boot — see [`crate::config::Config`].
            builder = builder.danger_accept_invalid_certs(true);
        }

        let inner = builder
            .build()
            .map_err(|e| WebQueryError::Transport(e.to_string()))?;

        Ok(Self {
            config_id,
            base_url,
            api_key,
            inner,
            request_lock: Mutex::new(()),
        })
    }

    /// Build a client from a `ServerConnection` row. Decrypts `apiKey` via
    /// [`crate::crypto::unseal`] (legacy plaintext rows pass through per the
    /// crypto module's pass-through rule).
    pub fn from_connection(
        connection: &ServerConnection,
        allow_self_signed: bool,
    ) -> WebQueryResult<Self> {
        let api_key = crypto::unseal(&connection.apiKey).map_err(|e| WebQueryError::Decrypt {
            config_id: connection.id,
            source: e,
        })?;
        Self::new(
            connection.id,
            &connection.host,
            connection.webqueryPort.try_into().unwrap_or(10080),
            connection.useHttps,
            api_key,
            allow_self_signed,
        )
    }

    /// Returns the connection's database row id. Useful for log fields.
    pub fn config_id(&self) -> i64 {
        self.config_id
    }

    /// Issue a GET request and parse the spec §10.5 envelope.
    ///
    /// `path` is everything after the host:port — e.g. `/version` for an
    /// instance-scoped command, `/3/serverinfo` for a sid-scoped one.
    /// `params` are URL-encoded by reqwest; per §10.4 we do **not** apply
    /// ServerQuery escaping here.
    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        self.request(Method::GET, path, params).await
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        // §10.1 — serialise to the single keep-alive socket.
        let _permit = self.request_lock.lock().await;

        let url = format!("{}{}", self.base_url, path);

        let mut headers = HeaderMap::new();
        let key = HeaderValue::from_str(&self.api_key)
            .map_err(|_| WebQueryError::Transport("apiKey is not a valid HTTP header".into()))?;
        headers.insert(API_KEY_HEADER, key);

        let response = self
            .inner
            .request(method, &url)
            .headers(headers)
            .query(params)
            .send()
            .await
            .map_err(|e| WebQueryError::Transport(e.to_string()))?;

        // §10.5 — body is the canonical signal even on non-2xx; only fall
        // back to status-only error messaging when the body fails to parse.
        let bytes = response
            .bytes()
            .await
            .map_err(|e| WebQueryError::Transport(e.to_string()))?;

        let envelope: Envelope<T> = serde_json::from_slice(&bytes).map_err(|e| {
            WebQueryError::InvalidResponse(format!("envelope parse failed: {e}"))
        })?;
        envelope.into_body()
    }

    /// `version` (instance scope). Phase 1 health probe per §10.7.
    pub async fn version(&self) -> WebQueryResult<VersionInfo> {
        self.get::<VersionInfo>("/version", &[]).await
    }

    /// Returns true if [`Self::version`] succeeds. Suitable for the cheap
    /// dashboard health gate.
    pub async fn health(&self) -> bool {
        self.version().await.is_ok()
    }

    /// `serverlist` (instance scope) — drives `vs:sid` enumeration in the
    /// virtual-server selector.
    pub async fn serverlist(&self) -> WebQueryResult<Vec<VirtualServerEntry>> {
        self.get::<Vec<VirtualServerEntry>>("/serverlist", &[]).await
    }

    /// `serverinfo` (sid scope).
    pub async fn serverinfo(&self, sid: i64) -> WebQueryResult<ServerInfo> {
        self.get::<ServerInfo>(&format!("/{sid}/serverinfo"), &[])
            .await
    }

    /// `channellist` (sid scope).
    pub async fn channellist(&self, sid: i64) -> WebQueryResult<Vec<ChannelEntry>> {
        self.get::<Vec<ChannelEntry>>(&format!("/{sid}/channellist"), &[])
            .await
    }

    /// `clientlist` (sid scope).
    pub async fn clientlist(&self, sid: i64) -> WebQueryResult<Vec<ClientEntry>> {
        self.get::<Vec<ClientEntry>>(&format!("/{sid}/clientlist"), &[])
            .await
    }

    /// `serverrequestconnectioninfo` (sid scope).
    pub async fn server_connection_info(&self, sid: i64) -> WebQueryResult<ConnectionInfo> {
        self.get::<ConnectionInfo>(
            &format!("/{sid}/serverrequestconnectioninfo"),
            &[],
        )
        .await
    }
}

/// Spec §10.5 envelope. Always parsed, regardless of HTTP status.
#[derive(Debug, Deserialize)]
struct Envelope<T> {
    body: Option<T>,
    status: EnvelopeStatus,
}

#[derive(Debug, Deserialize)]
struct EnvelopeStatus {
    code: i64,
    message: String,
}

impl<T> Envelope<T> {
    fn into_body(self) -> WebQueryResult<T> {
        if self.status.code != 0 {
            return Err(WebQueryError::Upstream {
                code: self.status.code,
                message: self.status.message,
            });
        }
        self.body
            .ok_or_else(|| WebQueryError::InvalidResponse("status.code=0 but body is null".into()))
    }
}

/// Pool of WebQuery clients keyed by `server_connection.id` (spec §10.7).
///
/// Phase 1 ships lazy creation only: the dashboard route asks for a client
/// and the pool builds it on first miss. Boot-time pre-population, the 30s
/// `version`-probe health loop, and the `refreshClient` hook on
/// `PUT /servers/:configId` are Phase 2 follow-ups (deferred per the
/// PURA-23 issue scope).
#[derive(Clone)]
pub struct WebQueryPool {
    inner: Arc<RwLock<HashMap<i64, Arc<WebQueryClient>>>>,
    allow_self_signed: bool,
}

impl std::fmt::Debug for WebQueryPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebQueryPool")
            .field("allow_self_signed", &self.allow_self_signed)
            .finish_non_exhaustive()
    }
}

impl WebQueryPool {
    pub fn new(allow_self_signed: bool) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            allow_self_signed,
        }
    }

    /// Insert or replace the client for `connection`. Used by Phase 2 boot
    /// pre-population and by `PUT /servers/:configId` once that route lands.
    pub async fn upsert(&self, connection: &ServerConnection) -> WebQueryResult<Arc<WebQueryClient>> {
        let client = Arc::new(WebQueryClient::from_connection(
            connection,
            self.allow_self_signed,
        )?);
        self.inner.write().await.insert(connection.id, client.clone());
        Ok(client)
    }

    /// Fetch the client for `config_id`, building one from `connection` on
    /// first miss. Returns `None` from the cache only if `connection` is also
    /// absent — callers passing `None` get a `Transport` error so the
    /// dashboard route can map it to `500 "No connection configured for
    /// server config ID"` per §10.7.
    pub async fn get_or_build(
        &self,
        config_id: i64,
        connection: Option<&ServerConnection>,
    ) -> WebQueryResult<Arc<WebQueryClient>> {
        if let Some(existing) = self.inner.read().await.get(&config_id).cloned() {
            return Ok(existing);
        }
        let connection = connection.ok_or_else(|| {
            WebQueryError::Transport(format!(
                "No connection configured for server config ID {config_id}"
            ))
        })?;
        self.upsert(connection).await
    }

    /// Drop the cached client (if any). Called by `DELETE /servers/:configId`
    /// once that route lands.
    pub async fn remove(&self, config_id: i64) {
        self.inner.write().await.remove(&config_id);
    }
}

#[cfg(test)]
mod tests;
