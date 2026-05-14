//! Spec Chapter 10 — outbound HTTP client to the per-server TS6 WebQuery API.
//!
//! Phase 1 (PURA-23) shipped the **read-only** subset for the §7.19 dashboard.
//! Phase 2 (PURA-68) extends this module with the full ServerQuery command
//! surface the FE needs for ops actions:
//!
//! - **Read** — `clientlist`/`clientinfo`/`clientdblist`/`clientdbinfo`,
//!   `channellist`/`channelinfo`/`channelclientlist`,
//!   `serverinfo`/`hostinfo`/`logview`,
//!   `channelclientpermlist`.
//! - **Write** — `clientkick`/`clientpoke`/`clientmove`/`clientedit` (used by
//!   the mute helper), `banadd`/`bandel`/`bandelall`.
//!
//! Cross-cutting:
//!
//! - [`WebQueryClient`] per `server_connection` row, single keep-alive socket
//!   so each upstream registers exactly one ServerQuery slot (§10.1).
//! - [`WebQueryPool`] keyed by `server_connection.id`, lazy-creating clients
//!   on first call. Boot-time pre-population, `autoStart`, the 30s health
//!   probe, and the `refreshClient` lifecycle on `PUT/DELETE /servers` are
//!   Phase 2 follow-ups owned by a separate ticket.
//! - Spec §10.5/§10.6 envelope handling: non-zero `status.code` → typed
//!   [`WebQueryError::Upstream`] which the route layer maps to `502
//!   {error: "TeamSpeak API Error", code, details}` per §7.0.2. Code `1281`
//!   (`database_empty_result`) is opt-in coerced to an empty list by
//!   list-shaped reads via [`WebQueryClient::list_or_empty`].
//! - Tracing: every request runs inside a `webquery.request` span carrying
//!   `config_id`, `method`, `path`, and emits a structured log with
//!   `latency_ms` and outcome (`ok` / `upstream_err` / `transport`).
//!
//! Out of scope (separate issues): REST endpoints exposing these actions
//! (PURA-71), WS event fan-out (separate child), SSH-based control path.
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
use std::time::{Duration, Instant};

use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method, StatusCode};
use serde::Deserialize;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};
use tracing::Instrument;

use crate::crypto;
use crate::repos::server_connections::ServerConnection;

pub use models::{
    BanAddResponse, BanEntry, ChannelClientPerm, ChannelEntry, ChannelInfo, ClientDbEntry,
    ClientEntry, ClientInfo, ConnectionInfo, HostInfo, LogEntry, ServerInfo, VersionInfo,
    VirtualServerEntry,
};

/// TS upstream code for `database_empty_result`. List reads opt into mapping
/// this to an empty `Vec` via [`WebQueryClient::list_or_empty`] per §10.6.
pub const DATABASE_EMPTY_RESULT: i64 = 1281;

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

    /// Issue a GET request and parse the spec §10.5 envelope as a list-shaped
    /// body. Use [`Self::get_one`] for inherently single-row commands; the
    /// TS6 wire wraps those in a one-element array (see [`OneOrSingleton`]).
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

    /// Singleton variant of [`Self::get`]. TS6's HTTP query interface wraps
    /// `body` in a one-element JSON array even for inherently single-row
    /// commands (`serverinfo`, `hostinfo`, `version`, `clientinfo`,
    /// `channelinfo`, `banadd`, …). This helper accepts either `body: {...}`
    /// (the legacy TS3 shape preserved for older fixtures) or `body: [{...}]`
    /// (the TS6-beta9 shape captured in the field) and yields one `T`.
    ///
    /// Note: the dispatch is done on `serde_json::Value` rather than via an
    /// `#[serde(untagged)]` wrapper. Some response models (`HostInfo`,
    /// `ConnectionInfo`) declare every field with `#[serde(default)]`, which
    /// causes the auto-derived `visit_seq` path to accept a JSON array as a
    /// positional struct and silently default every field. Going through
    /// `Value` makes the array-vs-object decision before model decoding.
    async fn get_one<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        let body: serde_json::Value = self.request(Method::GET, path, params).await?;
        let one = unwrap_singleton_body(body)?;
        serde_json::from_value(one)
            .map_err(|e| WebQueryError::InvalidResponse(format!("singleton body decode: {e}")))
    }

    async fn request<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<T> {
        let span = tracing::info_span!(
            "webquery.request",
            config_id = self.config_id,
            method = %method,
            path,
        );
        async move {
            // §10.1 — serialise to the single keep-alive socket.
            let _permit = self.request_lock.lock().await;

            let url = format!("{}{}", self.base_url, path);

            let mut headers = HeaderMap::new();
            let key = HeaderValue::from_str(&self.api_key).map_err(|_| {
                WebQueryError::Transport("apiKey is not a valid HTTP header".into())
            })?;
            headers.insert(API_KEY_HEADER, key);

            let started = Instant::now();
            let send_result = self
                .inner
                .request(method.clone(), &url)
                .headers(headers)
                .query(params)
                .send()
                .await;

            let response = match send_result {
                Ok(resp) => resp,
                Err(e) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    tracing::warn!(latency_ms, outcome = "transport", error = %e);
                    return Err(WebQueryError::Transport(e.to_string()));
                }
            };

            // §10.5 — body is the canonical signal even on non-2xx; only fall
            // back to status-only error messaging when the body fails to parse.
            let bytes = match response.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    let latency_ms = started.elapsed().as_millis() as u64;
                    tracing::warn!(latency_ms, outcome = "transport", error = %e);
                    return Err(WebQueryError::Transport(e.to_string()));
                }
            };

            let latency_ms = started.elapsed().as_millis() as u64;

            let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|e| {
                tracing::warn!(latency_ms, outcome = "invalid_response", error = %e);
                WebQueryError::InvalidResponse(format!("envelope parse failed: {e}"))
            })?;

            match envelope.into_body::<T>() {
                Ok(body) => {
                    tracing::debug!(latency_ms, outcome = "ok");
                    Ok(body)
                }
                Err(WebQueryError::Upstream { code, message }) => {
                    tracing::info!(latency_ms, outcome = "upstream_err", code, %message);
                    Err(WebQueryError::Upstream { code, message })
                }
                Err(other) => {
                    tracing::warn!(latency_ms, outcome = "invalid_response", error = %other);
                    Err(other)
                }
            }
        }
        .instrument(span)
        .await
    }

    /// Run a list-shaped read and coerce TS upstream code `1281`
    /// (`database_empty_result`) to an empty `Vec` per spec §10.6. Used by
    /// `clientpermlist` / `channelclientpermlist` / `ftgetfilelist`-style
    /// commands where "no rows" arrives as an upstream error rather than a
    /// `body: []`.
    async fn list_or_empty<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        params: &[(&str, &str)],
    ) -> WebQueryResult<Vec<T>> {
        match self.get::<Vec<T>>(path, params).await {
            Ok(v) => Ok(v),
            Err(WebQueryError::Upstream { code, .. }) if code == DATABASE_EMPTY_RESULT => {
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        }
    }

    /// `version` (instance scope). Phase 1 health probe per §10.7.
    pub async fn version(&self) -> WebQueryResult<VersionInfo> {
        self.get_one::<VersionInfo>("/version", &[]).await
    }

    /// Returns true if [`Self::version`] succeeds. Suitable for the cheap
    /// dashboard health gate.
    pub async fn health(&self) -> bool {
        self.version().await.is_ok()
    }

    /// `serverlist` (instance scope) — drives `vs:sid` enumeration in the
    /// virtual-server selector.
    pub async fn serverlist(&self) -> WebQueryResult<Vec<VirtualServerEntry>> {
        self.get::<Vec<VirtualServerEntry>>("/serverlist", &[])
            .await
    }

    /// `serverinfo` (sid scope).
    pub async fn serverinfo(&self, sid: i64) -> WebQueryResult<ServerInfo> {
        self.get_one::<ServerInfo>(&format!("/{sid}/serverinfo"), &[])
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
        self.get_one::<ConnectionInfo>(&format!("/{sid}/serverrequestconnectioninfo"), &[])
            .await
    }

    // =====================================================================
    // Phase 2 (PURA-68) — full ServerQuery command surface
    // =====================================================================

    /// `clientlist` with the §7.8 flag set. Pass an empty slice for the
    /// minimal `clid`/`cid`/`client_database_id`/`client_type`/`client_nickname`
    /// projection. Standard flags: `-uid -away -voice -times -groups -info
    /// -country`; `-ip` is admin-only and the route layer is responsible for
    /// gating it.
    pub async fn clientlist_with_flags(
        &self,
        sid: i64,
        flags: &[&str],
    ) -> WebQueryResult<Vec<ClientEntry>> {
        let path = format!("/{sid}/clientlist{}", flag_suffix(flags));
        self.get::<Vec<ClientEntry>>(&path, &[]).await
    }

    /// `clientinfo` (sid scope).
    pub async fn clientinfo(&self, sid: i64, clid: i64) -> WebQueryResult<ClientInfo> {
        let clid_s = clid.to_string();
        self.get_one::<ClientInfo>(&format!("/{sid}/clientinfo"), &[("clid", clid_s.as_str())])
            .await
    }

    /// `clientdblist` (sid scope) — paginated. Defaults per §7.8: `start=0`,
    /// `duration=100`. The route layer enforces operator-supplied bounds.
    pub async fn clientdblist(
        &self,
        sid: i64,
        start: i64,
        duration: i64,
    ) -> WebQueryResult<Vec<ClientDbEntry>> {
        let start_s = start.to_string();
        let dur_s = duration.to_string();
        self.get::<Vec<ClientDbEntry>>(
            &format!("/{sid}/clientdblist"),
            &[("start", start_s.as_str()), ("duration", dur_s.as_str())],
        )
        .await
    }

    /// `clientdbinfo` (sid scope).
    pub async fn clientdbinfo(&self, sid: i64, cldbid: i64) -> WebQueryResult<ClientDbEntry> {
        let cldbid_s = cldbid.to_string();
        self.get_one::<ClientDbEntry>(
            &format!("/{sid}/clientdbinfo"),
            &[("cldbid", cldbid_s.as_str())],
        )
        .await
    }

    /// `channellist` with optional flags. Pass an empty slice for the minimal
    /// projection (`cid`/`channel_name`/`pid`/`channel_order`). §7.7 mandates
    /// `-topic -flags -voice -limits -icon -secondsempty` at the REST layer.
    pub async fn channellist_with_flags(
        &self,
        sid: i64,
        flags: &[&str],
    ) -> WebQueryResult<Vec<ChannelEntry>> {
        let path = format!("/{sid}/channellist{}", flag_suffix(flags));
        self.get::<Vec<ChannelEntry>>(&path, &[]).await
    }

    /// `channelinfo` (sid scope).
    pub async fn channelinfo(&self, sid: i64, cid: i64) -> WebQueryResult<ChannelInfo> {
        let cid_s = cid.to_string();
        self.get_one::<ChannelInfo>(&format!("/{sid}/channelinfo"), &[("cid", cid_s.as_str())])
            .await
    }

    /// `channelclientlist` — clients in a specific channel.
    pub async fn channelclientlist(&self, sid: i64, cid: i64) -> WebQueryResult<Vec<ClientEntry>> {
        let cid_s = cid.to_string();
        self.get::<Vec<ClientEntry>>(
            &format!("/{sid}/channelclientlist"),
            &[("cid", cid_s.as_str())],
        )
        .await
    }

    /// `hostinfo` (instance scope).
    pub async fn hostinfo(&self) -> WebQueryResult<HostInfo> {
        self.get_one::<HostInfo>("/hostinfo", &[]).await
    }

    /// `logview` (sid scope) — paginated log retrieval. Defaults follow
    /// §7.17: `lines=100`, `reverse=1`, `instance=0`. `begin_pos` is omitted
    /// when `None` (initial fetch); pass the previous response's `last_pos`
    /// to page forward.
    pub async fn logview(
        &self,
        sid: i64,
        lines: u32,
        reverse: bool,
        instance: bool,
        begin_pos: Option<i64>,
    ) -> WebQueryResult<Vec<LogEntry>> {
        let lines_s = lines.to_string();
        let reverse_s = if reverse { "1" } else { "0" };
        let instance_s = if instance { "1" } else { "0" };
        let mut params: Vec<(&str, &str)> = vec![
            ("lines", lines_s.as_str()),
            ("reverse", reverse_s),
            ("instance", instance_s),
        ];
        let begin_s;
        if let Some(pos) = begin_pos {
            begin_s = pos.to_string();
            params.push(("begin_pos", begin_s.as_str()));
        }
        self.get::<Vec<LogEntry>>(&format!("/{sid}/logview"), &params)
            .await
    }

    /// `clientkick` (sid scope) — moderator action. `reasonid` defaults to
    /// `5` (server kick) per §7.8 / §14.1.
    pub async fn clientkick(
        &self,
        sid: i64,
        clid: i64,
        reasonid: i64,
        reasonmsg: Option<&str>,
    ) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        let reasonid_s = reasonid.to_string();
        let mut params: Vec<(&str, &str)> =
            vec![("clid", clid_s.as_str()), ("reasonid", reasonid_s.as_str())];
        if let Some(msg) = reasonmsg {
            params.push(("reasonmsg", msg));
        }
        self.get::<UnitBody>(&format!("/{sid}/clientkick"), &params)
            .await?;
        Ok(())
    }

    /// `clientpoke` (sid scope) — fire a popup at the targeted client.
    pub async fn clientpoke(&self, sid: i64, clid: i64, msg: &str) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        self.get::<UnitBody>(
            &format!("/{sid}/clientpoke"),
            &[("clid", clid_s.as_str()), ("msg", msg)],
        )
        .await?;
        Ok(())
    }

    /// `clientmove` (sid scope) — force-move client to channel `cid`. `cpw`
    /// is the optional channel password.
    pub async fn clientmove(
        &self,
        sid: i64,
        clid: i64,
        cid: i64,
        cpw: Option<&str>,
    ) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        let cid_s = cid.to_string();
        let mut params: Vec<(&str, &str)> =
            vec![("clid", clid_s.as_str()), ("cid", cid_s.as_str())];
        if let Some(pw) = cpw {
            params.push(("cpw", pw));
        }
        self.get::<UnitBody>(&format!("/{sid}/clientmove"), &params)
            .await?;
        Ok(())
    }

    /// `clientedit` (sid scope) — flexible primitive for property changes
    /// (e.g. `CLIENT_DESCRIPTION`, `CLIENT_IS_TALKER`). Used by
    /// [`Self::client_set_muted`].
    pub async fn clientedit_raw(
        &self,
        sid: i64,
        clid: i64,
        props: &[(&str, &str)],
    ) -> WebQueryResult<()> {
        let clid_s = clid.to_string();
        let mut params: Vec<(&str, &str)> = Vec::with_capacity(props.len() + 1);
        params.push(("clid", clid_s.as_str()));
        params.extend_from_slice(props);
        self.get::<UnitBody>(&format!("/{sid}/clientedit"), &params)
            .await?;
        Ok(())
    }

    /// Mute helper — sets `CLIENT_INPUT_MUTED` and/or `CLIENT_OUTPUT_MUTED`
    /// on the client via `clientedit`. `None` leaves the field unchanged.
    pub async fn client_set_muted(
        &self,
        sid: i64,
        clid: i64,
        input_muted: Option<bool>,
        output_muted: Option<bool>,
    ) -> WebQueryResult<()> {
        let mut props: Vec<(&str, &str)> = Vec::with_capacity(2);
        let in_s;
        let out_s;
        if let Some(v) = input_muted {
            in_s = bool_to_int(v);
            props.push(("CLIENT_INPUT_MUTED", in_s));
        }
        if let Some(v) = output_muted {
            out_s = bool_to_int(v);
            props.push(("CLIENT_OUTPUT_MUTED", out_s));
        }
        if props.is_empty() {
            // Caller asked for no-op; avoid a wasted upstream round-trip.
            return Ok(());
        }
        self.clientedit_raw(sid, clid, &props).await
    }

    /// `banlist` (sid scope).
    pub async fn banlist(&self, sid: i64) -> WebQueryResult<Vec<BanEntry>> {
        // Empty banlist surfaces as upstream 1281 on some upstreams.
        self.list_or_empty::<BanEntry>(&format!("/{sid}/banlist"), &[])
            .await
    }

    /// `banadd` (sid scope) — returns the new ban id. Per §7.12 the route
    /// forwards `ip` / `uid` / `mytsid` / `name` / `banreason` / `time`;
    /// `time = 0` is permanent.
    pub async fn banadd(&self, sid: i64, params: &BanAddParams<'_>) -> WebQueryResult<i64> {
        let mut q: Vec<(&str, &str)> = Vec::with_capacity(6);
        if let Some(v) = params.ip {
            q.push(("ip", v));
        }
        if let Some(v) = params.uid {
            q.push(("uid", v));
        }
        if let Some(v) = params.mytsid {
            q.push(("mytsid", v));
        }
        if let Some(v) = params.name {
            q.push(("name", v));
        }
        if let Some(v) = params.banreason {
            q.push(("banreason", v));
        }
        let time_s;
        if let Some(t) = params.time {
            time_s = t.to_string();
            q.push(("time", time_s.as_str()));
        }
        let resp: BanAddResponse = self.get_one(&format!("/{sid}/banadd"), &q).await?;
        Ok(resp.banid)
    }

    /// `bandel` (sid scope) — drop a single ban by id.
    pub async fn bandel(&self, sid: i64, banid: i64) -> WebQueryResult<()> {
        let banid_s = banid.to_string();
        self.get::<UnitBody>(&format!("/{sid}/bandel"), &[("banid", banid_s.as_str())])
            .await?;
        Ok(())
    }

    /// `bandelall` (sid scope) — drop every ban on this virtual server.
    pub async fn bandelall(&self, sid: i64) -> WebQueryResult<()> {
        self.get::<UnitBody>(&format!("/{sid}/bandelall"), &[])
            .await?;
        Ok(())
    }

    /// `channelclientpermlist` — list a client's per-channel permission rows.
    /// TS upstream code 1281 (`database_empty_result`) is mapped to `[]`
    /// per §10.6 to match the route-layer contract.
    pub async fn channelclientpermlist(
        &self,
        sid: i64,
        cid: i64,
        cldbid: i64,
    ) -> WebQueryResult<Vec<ChannelClientPerm>> {
        let cid_s = cid.to_string();
        let cldbid_s = cldbid.to_string();
        // `-permsid` toggles the symbolic name in the response (matches the
        // §7.7 channelpermlist projection). Same query-string flag rule as
        // `clientlist`/`channellist` — TS6 rejects path-suffix concatenation.
        self.list_or_empty::<ChannelClientPerm>(
            &format!("/{sid}/channelclientpermlist{}", flag_suffix(&["permsid"])),
            &[("cid", cid_s.as_str()), ("cldbid", cldbid_s.as_str())],
        )
        .await
    }
}

/// Sentinel for write commands that only carry a `status` envelope. TS
/// returns `body: {}`, `body: []`, or `body: null` on these; we accept any
/// of them because [`Envelope::into_body`] forwards `null` as
/// [`serde_json::Value::Null`] and `Object(Value)` here is the identity
/// deserializer, while `Tuple(())` covers historical empty-array fixtures.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UnitBody {
    Object(serde_json::Value),
    Tuple(()),
}

/// Render `&[&str]` flags into the query-string suffix TS6 WebQuery uses
/// for command flags (e.g. `?-uid&-away` for `clientlist`). TS3-era path
/// concatenation (`clientlist-uid-away`) is rejected by TS6 with code 1538
/// (`invalid parameter`); the bare-key query form is what the upstream's
/// own `curl -G` examples produce. An empty slice returns the empty string
/// so callers can unconditionally append it.
fn flag_suffix(flags: &[&str]) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(flags.iter().map(|f| f.len() + 2).sum::<usize>() + 1);
    out.push('?');
    let mut first = true;
    for flag in flags {
        if !first {
            out.push('&');
        }
        first = false;
        out.push('-');
        out.push_str(flag.trim_start_matches('-'));
    }
    out
}

fn bool_to_int(v: bool) -> &'static str {
    if v { "1" } else { "0" }
}

/// Parameters for [`WebQueryClient::banadd`]. All fields are optional —
/// callers send the subset that applies (e.g. ban-by-IP vs. ban-by-uid).
#[derive(Debug, Default, Clone)]
pub struct BanAddParams<'a> {
    pub ip: Option<&'a str>,
    pub uid: Option<&'a str>,
    pub mytsid: Option<&'a str>,
    pub name: Option<&'a str>,
    pub banreason: Option<&'a str>,
    /// Ban duration in seconds. `Some(0)` is permanent per §7.12; `None`
    /// omits the field entirely (upstream defaults apply).
    pub time: Option<i64>,
}

/// Unwrap the TS6 singleton-wrap shape into a bare JSON value suitable for
/// model decoding. Accepts either `{...}` (the legacy TS3 shape kept around
/// for older fixtures) or `[{...}]` (TS6 wire — `6.0.0-beta9` captured).
/// Multi-element arrays and empty arrays are rejected as `InvalidResponse`
/// to surface wiring mistakes (e.g. a list-shaped command routed through
/// [`WebQueryClient::get_one`] by accident).
fn unwrap_singleton_body(body: serde_json::Value) -> WebQueryResult<serde_json::Value> {
    match body {
        serde_json::Value::Array(mut arr) if arr.len() == 1 => Ok(arr.pop().unwrap()),
        serde_json::Value::Array(arr) if arr.is_empty() => Err(WebQueryError::InvalidResponse(
            "expected single-element body, got empty array".into(),
        )),
        serde_json::Value::Array(arr) => Err(WebQueryError::InvalidResponse(format!(
            "expected single-element body, got {}-element array",
            arr.len()
        ))),
        other => Ok(other),
    }
}

/// Spec §10.5 envelope. Always parsed, regardless of HTTP status.
///
/// `body` is captured as raw [`serde_json::Value`] so the target type's
/// deserializer decides whether `null` (the empty-body success shape used
/// by no-return mutations — `clientkick`, `clientmove`, `clientedit`,
/// `bandel`/`bandelall`, `servernotifyregister`/`servernotifyunregister`,
/// some `sendtextmessage` variants) is acceptable. [`UnitBody`] accepts it;
/// list/struct models reject and surface as [`WebQueryError::InvalidResponse`].
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    body: Option<serde_json::Value>,
    status: EnvelopeStatus,
}

#[derive(Debug, Deserialize)]
struct EnvelopeStatus {
    code: i64,
    message: String,
}

impl Envelope {
    fn into_body<T>(self) -> WebQueryResult<T>
    where
        T: for<'de> Deserialize<'de>,
    {
        if self.status.code != 0 {
            return Err(WebQueryError::Upstream {
                code: self.status.code,
                message: self.status.message,
            });
        }
        let value = self.body.unwrap_or(serde_json::Value::Null);
        serde_json::from_value(value)
            .map_err(|e| WebQueryError::InvalidResponse(format!("body decode failed: {e}")))
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
    pub async fn upsert(
        &self,
        connection: &ServerConnection,
    ) -> WebQueryResult<Arc<WebQueryClient>> {
        let client = Arc::new(WebQueryClient::from_connection(
            connection,
            self.allow_self_signed,
        )?);
        self.inner
            .write()
            .await
            .insert(connection.id, client.clone());
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
