//! PURA-172 — Rust-side Host-preserving IP-pin proxy (sidecar-internal).
//!
//! Plaintext-HTTP source URLs are not safe to hand to FFmpeg directly even
//! after [`ts6_ssrf::is_url_allowed`] has accepted them: FFmpeg does its own
//! outbound DNS lookup at fetch time, so the IP the SSRF validator pinned
//! against the private-range blocklist (the [`ts6_ssrf::PinnedTarget::resolved_ip`])
//! can diverge from the IP FFmpeg actually connects to — the DNS rebinding
//! window R6 names. PURA-149 reverted the obvious "rewrite URL host to IP
//! literal" fix because it broke TLS SNI and HTTP `Host:` for every virtual-
//! hosted CDN.
//!
//! This module closes the rebinding window for both plaintext HTTP and HTTPS
//! by interposing a loopback proxy. FFmpeg never resolves the source name
//! and never follows a redirect or a nested playlist URL itself.
//!
//! 1. `POST /source` validates the URL with `ts6_ssrf`, gets back a
//!    [`ts6_ssrf::PinnedTarget`] including the resolved IP. HTTP and HTTPS
//!    with no `resolved_ip` are refused (fail closed) by the control plane.
//! 2. The control plane registers a [`PinnedTarget`] in this proxy's
//!    registry, gets back an unguessable token, and rewrites the FFmpeg
//!    argv to fetch `http://127.0.0.1:<port>/<token>` instead of the
//!    original URL. HTTPS is not handed to FFmpeg: the proxy opens TLS
//!    itself (certificate verification + original hostname as SNI) while
//!    `resolve_to_addrs` pins the socket to the SSRF-validated IP.
//! 3. The proxy receives FFmpeg's GET, looks up the token, and forwards to
//!    the upstream using a `reqwest::Client` whose
//!    [`reqwest::ClientBuilder::resolve_to_addrs`] pins resolution of
//!    `target.host` to `target.resolved_ip` — so DNS at connect time is
//!    irrelevant. The `Host:` header is preserved so virtual-hosted CDNs
//!    serve the right vhost.
//! 4. Redirects (3xx) are refused (502). FFmpeg therefore never sees a
//!    `Location` it could follow to a private or metadata address.
//! 5. HLS playlists (`#EXTM3U`) are rewritten before they reach FFmpeg.
//!    Every media URI and `URI="..."` attribute is resolved, re-checked
//!    with `ts6_ssrf`, and replaced with a child pin-proxy URL. A nested
//!    internal URL fails the whole playlist (502) so FFmpeg cannot fetch
//!    it. Child tokens are burned with the parent on `POST /source/stop`.
//!    Other nested manifests (DASH MPD, PLS) are refused rather than
//!    forwarded unrewritten.
//! 6. The parent token is invalidated on `POST /source/stop` so a leaked
//!    proxy URL cannot replay after the pipeline ends.
//!
//! The proxy binds on `127.0.0.1:0` (ephemeral, loopback-only) and is owned
//! by the [`crate::Sidecar`] handle. Subscribers don't speak this — only
//! FFmpeg, inside the same sidecar process.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures::{StreamExt, TryStreamExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use ts6_ssrf::{Resolver, is_url_allowed};

/// One registered upstream the proxy is willing to forward to. Cloned per
/// inbound request so the registry lock is released before the (possibly
/// long-running) upstream fetch begins.
#[derive(Debug, Clone)]
pub struct PinnedTarget {
    /// Full original URL the operator supplied. Path + query are forwarded
    /// verbatim to the upstream so it sees the resource it expects. Scheme
    /// is `http` or `https`. HTTPS is terminated here (reqwest verifies the
    /// certificate and sends the original hostname as SNI); FFmpeg only
    /// ever speaks plaintext HTTP to the loopback proxy, so no MITM cert
    /// is minted.
    pub upstream_url: url::Url,
    /// Hostname (no port). Set on the outbound `Host:` header so virtual-
    /// hosted CDNs continue to serve the right vhost.
    pub host: String,
    /// IP the SSRF validator accepted. Passed to `resolve_to_addrs` so the
    /// outbound socket binds to this exact peer regardless of any DNS the
    /// host's resolver would return at connect time.
    pub resolved_ip: IpAddr,
    /// Upstream port (defaults derived from scheme by `ts6-ssrf`).
    pub port: u16,
}

/// Shared registry of `token → PinnedTarget`. Cloning is cheap (`Arc`).
///
/// Nested HLS URLs are registered as children of the playlist token that
/// produced them. [`PinRegistry::deregister`] burns the whole tree so
/// `POST /source/stop` invalidates segment pins along with the source.
#[derive(Clone, Default)]
pub struct PinRegistry {
    inner: Arc<RwLock<RegistryMaps>>,
}

#[derive(Default)]
struct RegistryMaps {
    targets: HashMap<String, PinnedTarget>,
    /// Parent token → child tokens minted while rewriting its playlists.
    children: HashMap<String, Vec<String>>,
    /// `(parent token, upstream URL)` → child token. Playlist refreshes
    /// reuse the same pin instead of leaking a new token per poll.
    reused: HashMap<(String, String), String>,
}

impl PinRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `target` and return its unguessable token. Token entropy is
    /// 122 bits (UUID v4) — same surface area as the operator-visible
    /// `source_id` server-generated path, so we don't introduce a second
    /// random-source contract here.
    pub async fn register(&self, target: PinnedTarget) -> String {
        let token = uuid::Uuid::new_v4().to_string();
        let mut guard = self.inner.write().await;
        guard.targets.insert(token.clone(), target);
        token
    }

    /// Register `target` as a nested fetch of `parent` (an HLS segment,
    /// key, or variant playlist). Returns `None` when `parent` is already
    /// gone. The same upstream URL under the same parent reuses one token.
    pub async fn register_nested(&self, parent: &str, target: PinnedTarget) -> Option<String> {
        let mut guard = self.inner.write().await;
        if !guard.targets.contains_key(parent) {
            return None;
        }
        let key = (parent.to_string(), target.upstream_url.as_str().to_string());
        if let Some(existing) = guard.reused.get(&key)
            && guard.targets.contains_key(existing)
        {
            return Some(existing.clone());
        }
        let token = uuid::Uuid::new_v4().to_string();
        guard
            .children
            .entry(parent.to_string())
            .or_default()
            .push(token.clone());
        guard.reused.insert(key, token.clone());
        guard.targets.insert(token.clone(), target);
        Some(token)
    }

    /// Drop `token` and every nested token minted from it. Returns `true`
    /// if a target was removed, `false` if the token was unknown
    /// (idempotent — repeated burns are fine).
    pub async fn deregister(&self, token: &str) -> bool {
        let mut guard = self.inner.write().await;
        if !guard.targets.contains_key(token) {
            return false;
        }
        let mut stack = vec![token.to_string()];
        let mut removed = false;
        while let Some(tok) = stack.pop() {
            if guard.targets.remove(&tok).is_some() {
                removed = true;
            }
            if let Some(kids) = guard.children.remove(&tok) {
                stack.extend(kids);
            }
        }
        let stale: Vec<(String, String)> = guard
            .reused
            .iter()
            .filter(|((parent, _), child)| {
                !guard.targets.contains_key(parent) || !guard.targets.contains_key(*child)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in stale {
            guard.reused.remove(&key);
        }
        removed
    }

    pub async fn lookup(&self, token: &str) -> Option<PinnedTarget> {
        let guard = self.inner.read().await;
        guard.targets.get(token).cloned()
    }

    /// Schemes of currently registered upstreams, sorted. Tests use this
    /// to tell a pinned HTTPS source from a pinned plaintext source.
    pub async fn upstream_schemes(&self) -> Vec<String> {
        let guard = self.inner.read().await;
        let mut schemes: Vec<String> = guard
            .targets
            .values()
            .map(|target| target.upstream_url.scheme().to_string())
            .collect();
        schemes.sort();
        schemes
    }

    /// Number of currently registered tokens, including nested playlist
    /// pins. Used by integration tests to assert wiring (register → token
    /// added, stop → burned). Cheap (one read lock + map.len()).
    pub async fn len(&self) -> usize {
        self.inner.read().await.targets.len()
    }

    /// True iff no tokens are currently registered.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.targets.is_empty()
    }
}

/// Handle to the running proxy. Holds the bound loopback address so callers
/// (control plane, tests) can build proxy URLs without re-discovering the
/// port, plus the join handle so [`PinProxy::shutdown`] can abort it.
///
/// Lives behind an `Arc` in the sidecar (the control plane needs a clone
/// for axum state extraction, the tests need a clone to peek at the
/// registry). `shutdown` takes `&self` so any holder of the Arc can tear
/// it down without having to be the unique owner.
pub struct PinProxy {
    pub local_addr: SocketAddr,
    pub registry: PinRegistry,
    task: std::sync::Mutex<Option<JoinHandle<anyhow::Result<()>>>>,
}

#[derive(Clone)]
struct ProxyState {
    registry: PinRegistry,
    resolver: Arc<dyn Resolver>,
    local_addr: SocketAddr,
}

impl PinProxy {
    /// Bind the proxy on `127.0.0.1:0` (ephemeral port, loopback-only) and
    /// start its accept loop. `resolver` re-checks nested HLS URLs with
    /// the same SSRF validator `POST /source` used. The returned
    /// [`PinProxy`] owns the task; call [`Self::shutdown`] to abort it
    /// explicitly, or drop the proxy to let `Drop` abort it.
    pub async fn start(resolver: Arc<dyn Resolver>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind PinProxy on 127.0.0.1:0")?;
        let local_addr = listener.local_addr().context("PinProxy local_addr")?;
        let registry = PinRegistry::new();
        let state = ProxyState {
            registry: registry.clone(),
            resolver,
            local_addr,
        };
        let router = Router::new()
            .route("/{token}", any(handle))
            .fallback(not_found)
            .with_state(state);
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .context("PinProxy axum::serve exited")
        });
        info!(%local_addr, "PinProxy listening");
        Ok(Self {
            local_addr,
            registry,
            task: std::sync::Mutex::new(Some(task)),
        })
    }

    /// Format a proxy URL for `token` that callers (control plane) hand to
    /// FFmpeg. Loopback always, so no SSRF surface against the outside.
    pub fn proxy_url(&self, token: &str) -> String {
        format!("http://{}/{}", self.local_addr, token)
    }

    /// Abort the proxy task. Idempotent (safe to call after `Drop`).
    pub fn shutdown(&self) {
        if let Some(task) = self.task.lock().expect("PinProxy task mutex").take() {
            task.abort();
        }
    }
}

impl Drop for PinProxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

async fn not_found() -> impl IntoResponse {
    StatusCode::NOT_FOUND
}

/// Handle a single inbound proxy request. The token is the only path
/// component (we never embed the original path here — the original URL is
/// stored alongside the target).
async fn handle(
    State(state): State<ProxyState>,
    Path(token): Path<String>,
    req: Request<Body>,
) -> Response {
    // Per impl-plan, this proxy is for FFmpeg's GET (with HEAD as a sane
    // sibling for HTTP probes). Everything else is refused to keep the
    // attack surface — a leaked token mustn't become a generic write
    // primitive against the pinned upstream.
    let method = req.method().clone();
    if !matches!(method, Method::GET | Method::HEAD) {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }

    let Some(target) = state.registry.lookup(&token).await else {
        debug!(token = %short(&token), "PinProxy: unknown token");
        return StatusCode::NOT_FOUND.into_response();
    };

    // HTTP and HTTPS only. Anything else (file, data, …) must not be
    // fetched even if a caller registered it.
    if target.upstream_url.scheme() != "http" && target.upstream_url.scheme() != "https" {
        warn!(
            scheme = target.upstream_url.scheme(),
            host = %log_host(&target.host),
            "PinProxy: refusing non-http(s) upstream",
        );
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // Build a per-request reqwest client pinned to the SSRF-validated IP.
    // `resolve_to_addrs` overrides DNS for `target.host` only; `.no_proxy()`
    // makes sure no HTTP_PROXY env vars can re-route the connect through
    // an external proxy that would defeat the pin; `.redirect(none)` so
    // any 3xx Location lands on us, not on a re-resolved follow-up.
    // rustls verifies the HTTPS certificate against `target.host` (SNI);
    // we never call `danger_accept_invalid_certs`.
    let client_build = reqwest::Client::builder()
        .resolve_to_addrs(
            &target.host,
            &[SocketAddr::new(target.resolved_ip, target.port)],
        )
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none());
    let client_build = apply_upstream_timeouts(client_build);
    let client_build = match apply_extra_tls_roots(client_build) {
        Ok(builder) => builder,
        Err(err) => {
            warn!(%err, host = %log_host(&target.host), "PinProxy: TLS root load failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };
    let client = match client_build.build() {
        Ok(c) => c,
        Err(err) => {
            warn!(%err, host = %log_host(&target.host), "PinProxy: client build failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let mut upstream_req = client.request(method.clone(), target.upstream_url.clone());

    // Forward inbound headers minus hop-by-hop, then force `Host:` to the
    // original hostname so virtual-hosted CDNs keep working. Inbound `Host:`
    // points at the loopback proxy and would route to the wrong vhost.
    let mut header_map = reqwest::header::HeaderMap::new();
    for (name, value) in req.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        if name.as_str().eq_ignore_ascii_case("host") {
            continue;
        }
        let Ok(reqwest_name) = reqwest::header::HeaderName::from_bytes(name.as_ref()) else {
            continue;
        };
        let Ok(reqwest_value) = reqwest::header::HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        header_map.insert(reqwest_name, reqwest_value);
    }
    if let Ok(host_val) = reqwest::header::HeaderValue::from_str(&target.host) {
        header_map.insert(reqwest::header::HOST, host_val);
    }
    upstream_req = upstream_req.headers(header_map);

    // GET/HEAD bodies are by spec empty; we drop the inbound body here to
    // keep the proxy a fetch-only primitive.
    drop(req);

    let upstream_resp = match upstream_req.send().await {
        Ok(r) => r,
        Err(err) => {
            warn!(%err, host = %log_host(&target.host), ip = %target.resolved_ip, "PinProxy: upstream send failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();

    // Redirect policy: refuse every 3xx. FFmpeg must not be handed a
    // Location it would follow to a private or metadata address. The
    // operator supplies the final URL.
    if status.is_redirection() {
        let location = upstream_resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<no location>");
        warn!(
            %status,
            host = %log_host(&target.host),
            location = %location,
            "PinProxy: upstream attempted redirect; refusing",
        );
        return StatusCode::BAD_GATEWAY.into_response();
    }

    if method == Method::HEAD {
        return forward_response(status, upstream_resp.headers(), Body::empty(), false);
    }

    let headers = upstream_resp.headers().clone();
    let decoded = content_encoding_decoded(&headers);
    let declared_playlist = declared_hls_playlist(&target.upstream_url, &headers);
    let mut stream = upstream_resp.bytes_stream();
    let mut prefix = Vec::new();
    loop {
        if prefix.len() >= PLAYLIST_SNIFF_BYTES {
            break;
        }
        match stream.try_next().await {
            Ok(Some(chunk)) => prefix.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(err) => {
                warn!(%err, host = %log_host(&target.host), "PinProxy: upstream body read failed");
                return StatusCode::BAD_GATEWAY.into_response();
            }
        }
    }

    if looks_like_unrewritten_manifest(&prefix) {
        warn!(
            host = %log_host(&target.host),
            "PinProxy: refusing nested manifest that is not rewritten HLS",
        );
        return StatusCode::BAD_GATEWAY.into_response();
    }

    if starts_with_hls(&prefix) || declared_playlist {
        let mut body = prefix;
        while let Some(next) = stream.try_next().await.transpose() {
            let chunk = match next {
                Ok(chunk) => chunk,
                Err(err) => {
                    warn!(%err, host = %log_host(&target.host), "PinProxy: playlist read failed");
                    return StatusCode::BAD_GATEWAY.into_response();
                }
            };
            if body.len().saturating_add(chunk.len()) > PLAYLIST_BODY_LIMIT {
                warn!(
                    host = %log_host(&target.host),
                    "PinProxy: playlist exceeds size cap; refusing",
                );
                return StatusCode::BAD_GATEWAY.into_response();
            }
            body.extend_from_slice(&chunk);
        }
        if looks_like_unrewritten_manifest(&body) && !starts_with_hls(&body) {
            warn!(
                host = %log_host(&target.host),
                "PinProxy: refusing nested manifest that is not rewritten HLS",
            );
            return StatusCode::BAD_GATEWAY.into_response();
        }
        if !starts_with_hls(&body) {
            warn!(
                host = %log_host(&target.host),
                "PinProxy: declared playlist is not HLS; refusing",
            );
            return StatusCode::BAD_GATEWAY.into_response();
        }
        let Ok(text) = std::str::from_utf8(&body) else {
            warn!(host = %log_host(&target.host), "PinProxy: HLS playlist is not UTF-8");
            return StatusCode::BAD_GATEWAY.into_response();
        };
        let rewritten = match rewrite_hls_playlist(text, &target.upstream_url, &token, &state).await
        {
            Ok(body) => body,
            Err(reason) => {
                warn!(
                    host = %log_host(&target.host),
                    reason,
                    "PinProxy: HLS playlist rewrite failed",
                );
                return StatusCode::BAD_GATEWAY.into_response();
            }
        };
        return forward_response(status, &headers, Body::from(rewritten), true);
    }

    let prefix_bytes = bytes::Bytes::from(prefix);
    let chained = futures::stream::once(async move { Ok(prefix_bytes) })
        .chain(stream.map_err(std::io::Error::other));
    forward_response(status, &headers, Body::from_stream(chained), decoded)
}

fn forward_response(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Body,
    drop_length: bool,
) -> Response {
    let mut builder = Response::builder().status(status.as_u16());
    let response_headers = builder
        .headers_mut()
        .expect("Response::builder always has headers on a fresh builder");
    for (name, value) in headers.iter() {
        if skip_forwarded_header(name.as_str(), value, drop_length) {
            continue;
        }
        let Ok(axum_name) = HeaderName::from_bytes(name.as_ref()) else {
            continue;
        };
        let Ok(axum_value) = HeaderValue::from_bytes(value.as_bytes()) else {
            continue;
        };
        response_headers.insert(axum_name, axum_value);
    }
    match builder.body(body) {
        Ok(resp) => resp,
        Err(err) => {
            warn!(%err, "PinProxy: build response failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

fn skip_forwarded_header(name: &str, value: &HeaderValue, drop_length: bool) -> bool {
    if is_hop_by_hop(name) {
        return true;
    }
    if drop_length && name.eq_ignore_ascii_case("content-length") {
        return true;
    }
    if name.eq_ignore_ascii_case("content-encoding") && content_encoding_value_decoded(value) {
        return true;
    }
    false
}

fn content_encoding_decoded(headers: &reqwest::header::HeaderMap) -> bool {
    headers
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(content_encoding_value_decoded)
}

fn content_encoding_value_decoded(value: &HeaderValue) -> bool {
    let Ok(text) = value.to_str() else {
        return false;
    };
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "gzip" | "x-gzip" | "deflate" | "br"
    )
}

/// How many leading bytes we inspect before deciding a body is a playlist.
const PLAYLIST_SNIFF_BYTES: usize = 2048;

/// HLS playlists are small text. Anything larger is refused rather than
/// truncated (a cut-off playlist could hide a later internal URL).
const PLAYLIST_BODY_LIMIT: usize = 1024 * 1024;

fn declared_hls_playlist(url: &url::Url, headers: &reqwest::header::HeaderMap) -> bool {
    let path = url.path().to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        return true;
    }
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let lc = value.to_ascii_lowercase();
            lc.contains("mpegurl") || lc.contains("mpeg-url")
        })
}

fn strip_bom(body: &[u8]) -> &[u8] {
    body.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(body)
}

fn trim_ascii_start(body: &[u8]) -> &[u8] {
    let start = body
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(body.len());
    &body[start..]
}

fn starts_with_hls(body: &[u8]) -> bool {
    let body = trim_ascii_start(strip_bom(body));
    body.len() >= 7 && body[..7].eq_ignore_ascii_case(b"#EXTM3U")
}

/// Manifests we do not rewrite. Forwarding them would let FFmpeg fetch
/// whatever absolute URL they list. HLS is handled separately.
fn looks_like_unrewritten_manifest(body: &[u8]) -> bool {
    let body = trim_ascii_start(strip_bom(body));
    let head = &body[..body.len().min(64)];
    let lower = head.to_ascii_lowercase();
    lower.starts_with(b"<?xml") || lower.starts_with(b"<mpd") || lower.starts_with(b"[playlist]")
}

async fn rewrite_hls_playlist(
    body: &str,
    base: &url::Url,
    parent_token: &str,
    state: &ProxyState,
) -> Result<String, &'static str> {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while !rest.is_empty() {
        let (line, newline, next) = split_playlist_line(rest);
        if is_media_uri_line(line) {
            let rewritten = pin_playlist_uri(line.trim(), base, parent_token, state).await?;
            out.push_str(&rewritten);
        } else if line.trim_start().starts_with('#') {
            let rewritten = rewrite_tag_line(line, base, parent_token, state).await?;
            out.push_str(&rewritten);
        } else {
            out.push_str(line);
        }
        out.push_str(newline);
        rest = next;
    }
    Ok(out)
}

fn split_playlist_line(input: &str) -> (&str, &str, &str) {
    match input.find('\n') {
        Some(idx) => {
            let (raw, rest) = input.split_at(idx + 1);
            let newline = if raw.ends_with("\r\n") { "\r\n" } else { "\n" };
            let content = raw.trim_end_matches(['\r', '\n']);
            (content, newline, rest)
        }
        None => (input, "", ""),
    }
}

fn is_media_uri_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && !trimmed.starts_with('#')
}

async fn rewrite_tag_line(
    line: &str,
    base: &url::Url,
    parent_token: &str,
    state: &ProxyState,
) -> Result<String, &'static str> {
    let mut out = String::new();
    let mut cursor = 0;
    while let Some(attr_at) = find_uri_attribute(line, cursor) {
        out.push_str(&line[cursor..attr_at]);
        let mut value_at = attr_at + 4;
        let bytes = line.as_bytes();
        while value_at < bytes.len() && bytes[value_at].is_ascii_whitespace() {
            value_at += 1;
        }
        if value_at >= bytes.len() || (bytes[value_at] != b'"' && bytes[value_at] != b'\'') {
            return Err("malformed URI attribute");
        }
        let quote = bytes[value_at] as char;
        let raw_start = value_at + 1;
        let Some(end_rel) = line[raw_start..].find(quote) else {
            return Err("unterminated URI attribute");
        };
        let raw = &line[raw_start..raw_start + end_rel];
        let rewritten = pin_playlist_uri(raw, base, parent_token, state).await?;
        out.push_str("URI=");
        out.push(quote);
        out.push_str(&rewritten);
        out.push(quote);
        cursor = raw_start + end_rel + 1;
    }
    out.push_str(&line[cursor..]);
    Ok(out)
}

fn find_uri_attribute(line: &str, from: usize) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut index = from;
    while index + 4 <= bytes.len() {
        if bytes[index].eq_ignore_ascii_case(&b'u')
            && bytes[index + 1].eq_ignore_ascii_case(&b'r')
            && bytes[index + 2].eq_ignore_ascii_case(&b'i')
            && bytes[index + 3] == b'='
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

async fn pin_playlist_uri(
    raw: &str,
    base: &url::Url,
    parent_token: &str,
    state: &ProxyState,
) -> Result<String, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty playlist uri");
    }
    let absolute = match url::Url::parse(raw) {
        Ok(parsed) => parsed,
        Err(_) => base.join(raw).map_err(|_| "unresolvable playlist uri")?,
    };
    if absolute.scheme() != "http" && absolute.scheme() != "https" {
        warn!(
            scheme = absolute.scheme(),
            "PinProxy: nested playlist URL scheme rejected"
        );
        return Err("disallowed playlist scheme");
    }
    let pinned = match is_url_allowed(absolute.as_str(), state.resolver.as_ref()).await {
        Ok(pinned) => pinned,
        Err(_) => {
            warn!(
                host = %log_host(absolute.host_str().unwrap_or("?")),
                "PinProxy: nested playlist URL blocked"
            );
            return Err("nested playlist url blocked");
        }
    };
    let Some(resolved_ip) = pinned.resolved_ip else {
        warn!(
            host = %log_host(&pinned.host),
            "PinProxy: nested playlist URL has no address to pin"
        );
        return Err("nested playlist url unpinned");
    };
    let target = PinnedTarget {
        upstream_url: pinned.url,
        host: pinned.host,
        resolved_ip,
        port: pinned.port,
    };
    let Some(token) = state.registry.register_nested(parent_token, target).await else {
        return Err("parent pin gone");
    };
    Ok(format!("http://{}/{token}", state.local_addr))
}

/// `TS6_TLS_CA_FILE`, when set, adds that PEM bundle to the proxy's trust
/// anchors. FFmpeg used to receive the same path as `-ca_file` on direct
/// HTTPS inputs; those inputs now terminate TLS inside this proxy.
fn apply_extra_tls_roots(
    mut builder: reqwest::ClientBuilder,
) -> anyhow::Result<reqwest::ClientBuilder> {
    let Ok(path) = std::env::var("TS6_TLS_CA_FILE") else {
        return Ok(builder);
    };
    if path.is_empty() {
        return Ok(builder);
    }
    let pem = std::fs::read(&path).with_context(|| format!("read TS6_TLS_CA_FILE {path}"))?;
    let mut found = false;
    for certificate in pem_certificates(&pem) {
        let cert = reqwest::Certificate::from_pem(certificate)
            .with_context(|| format!("parse certificate in TS6_TLS_CA_FILE {path}"))?;
        builder = builder.add_root_certificate(cert);
        found = true;
    }
    if !found {
        anyhow::bail!("TS6_TLS_CA_FILE {path} contains no PEM certificates");
    }
    Ok(builder)
}

fn pem_certificates(pem: &[u8]) -> Vec<&[u8]> {
    let marker = b"-----END CERTIFICATE-----";
    let mut certs = Vec::new();
    let mut rest = pem;
    while let Some(end) = rest
        .windows(marker.len())
        .position(|window| window == marker)
    {
        let split = end + marker.len();
        certs.push(&rest[..split]);
        rest = &rest[split..];
    }
    certs
}

fn log_host(host: &str) -> String {
    if host.parse::<IpAddr>().is_ok() {
        "[ip]".to_string()
    } else {
        host.to_string()
    }
}

/// Connect deadline for the pinned upstream. Independent of body length.
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Idle-read deadline. Resets after each successful read, so a live or
/// long VOD body is not killed, while a wedged upstream still fails.
///
/// This is deliberately not [`reqwest::ClientBuilder::timeout`]. That
/// setting is a total deadline from connect until the body finishes, and
/// a 5-minute cap aborts every relay that outlives it.
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(30);

fn apply_upstream_timeouts(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    // Do not add `.timeout(...)` here. It includes the response body.
    builder
        .connect_timeout(UPSTREAM_CONNECT_TIMEOUT)
        .read_timeout(UPSTREAM_READ_TIMEOUT)
}

/// RFC 7230 §6.1 + Proxy-* family — headers that MUST NOT be forwarded
/// across a proxy hop because they describe the connection, not the
/// message.
fn is_hop_by_hop(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "connection" | "keep-alive" | "te" | "trailers" | "transfer-encoding" | "upgrade"
    ) || lc.starts_with("proxy-")
}

fn short(token: &str) -> &str {
    let cutoff = token.len().min(8);
    &token[..cutoff]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use axum::Router;
    use axum::http::{HeaderName, HeaderValue, StatusCode};
    use axum::routing::any;
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use ts6_ssrf::MockResolver;

    fn test_resolver() -> Arc<dyn Resolver> {
        Arc::new(MockResolver::new())
    }

    fn resolver_with(host: &str, ip: IpAddr) -> Arc<dyn Resolver> {
        Arc::new(MockResolver::new().with(host, vec![ip]))
    }

    /// Helper — build a tiny axum upstream that the proxy will fetch from.
    /// Returns its listening address and a join handle.
    async fn spawn_upstream(router: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.ok();
        });
        (addr, task)
    }

    /// Replay attempts (no register) → 404.
    #[tokio::test]
    async fn unknown_token_returns_404() {
        let proxy = PinProxy::start(test_resolver()).await.expect("proxy start");
        let url = proxy.proxy_url("does-not-exist");
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.expect("proxy GET");
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
        proxy.shutdown();
    }

    /// Acceptance criterion: `Single-use token: replaying the proxy URL
    /// after pipeline stop returns 404 from the proxy.`
    #[tokio::test]
    async fn deregister_burns_token_replay_404() {
        // Trivial upstream that returns "ok".
        let (upstream_addr, upstream_task) =
            spawn_upstream(Router::new().route("/foo", any(|| async { (StatusCode::OK, "ok") })))
                .await;

        let proxy = PinProxy::start(test_resolver()).await.expect("proxy start");
        let target = PinnedTarget {
            upstream_url: url::Url::parse(&format!(
                "http://example.test:{}/foo",
                upstream_addr.port()
            ))
            .unwrap(),
            host: "example.test".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: upstream_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);

        let client = reqwest::Client::builder().no_proxy().build().unwrap();

        // First fetch — token live → upstream OK.
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // Burn — simulating `POST /source/stop` calling deregister.
        assert!(proxy.registry.deregister(&token).await);
        assert_eq!(proxy.registry.len().await, 0);

        // Replay → 404.
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "post-stop replay must 404 — PURA-172 single-use AC"
        );

        proxy.shutdown();
        upstream_task.abort();
    }

    /// Acceptance criterion: `upstream returns 302 Location: http://10.0.0.1/,
    /// proxy refuses; client gets a 502.`
    #[tokio::test]
    async fn redirect_refusal_returns_502() {
        let redirect_router = Router::new().route(
            "/follow",
            any(|| async {
                let mut resp = Response::builder()
                    .status(StatusCode::FOUND)
                    .body(Body::empty())
                    .unwrap();
                resp.headers_mut().insert(
                    axum::http::header::LOCATION,
                    HeaderValue::from_static("http://10.0.0.1/leaked"),
                );
                resp
            }),
        );
        let (upstream_addr, upstream_task) = spawn_upstream(redirect_router).await;

        let proxy = PinProxy::start(test_resolver()).await.unwrap();
        let target = PinnedTarget {
            upstream_url: url::Url::parse(&format!(
                "http://example.test:{}/follow",
                upstream_addr.port()
            ))
            .unwrap(),
            host: "example.test".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: upstream_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);

        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_GATEWAY,
            "302 from upstream MUST become 502 from proxy — PURA-172 redirect-refusal AC",
        );

        proxy.shutdown();
        upstream_task.abort();
    }

    /// Acceptance criterion: `Host: example.test preserved`. Upstream echoes
    /// the inbound Host header in the body; assert the proxy forwarded the
    /// vhost name, not the proxy's loopback Host.
    #[tokio::test]
    async fn preserves_host_header_for_virtual_hosted_upstream() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let captured_clone = captured.clone();
        let router = Router::new().route(
            "/vhost",
            any(move |headers: axum::http::HeaderMap| {
                let captured = captured_clone.clone();
                async move {
                    let host = headers
                        .get(axum::http::header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("<missing>")
                        .to_string();
                    *captured.lock().await = Some(host.clone());
                    (StatusCode::OK, host)
                }
            }),
        );
        let (upstream_addr, upstream_task) = spawn_upstream(router).await;

        let proxy = PinProxy::start(test_resolver()).await.unwrap();
        let target = PinnedTarget {
            upstream_url: url::Url::parse(&format!(
                "http://download.samplelib.com:{}/vhost",
                upstream_addr.port()
            ))
            .unwrap(),
            host: "download.samplelib.com".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: upstream_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let body = client.get(&url).send().await.unwrap().text().await.unwrap();
        assert!(
            body.starts_with("download.samplelib.com"),
            "Host header lost or rewritten: got {body:?}"
        );

        let captured = captured.lock().await;
        assert_eq!(
            captured.as_deref(),
            Some("download.samplelib.com"),
            "captured Host header did not match the registered vhost",
        );

        proxy.shutdown();
        upstream_task.abort();
    }

    /// Hop-by-hop response headers MUST be stripped before forwarding to
    /// the inbound client (RFC 7230 §6.1 + Proxy-* family).
    #[tokio::test]
    async fn strips_hop_by_hop_response_headers() {
        let router = Router::new().route(
            "/hop",
            any(|| async {
                let mut resp = Response::builder()
                    .status(StatusCode::OK)
                    .body(Body::from("hi"))
                    .unwrap();
                let h = resp.headers_mut();
                h.insert(
                    HeaderName::from_static("connection"),
                    HeaderValue::from_static("close"),
                );
                h.insert(
                    HeaderName::from_static("proxy-authenticate"),
                    HeaderValue::from_static("Basic"),
                );
                h.insert(
                    HeaderName::from_static("x-keep-this"),
                    HeaderValue::from_static("yes"),
                );
                resp
            }),
        );
        let (upstream_addr, upstream_task) = spawn_upstream(router).await;

        let proxy = PinProxy::start(test_resolver()).await.unwrap();
        let target = PinnedTarget {
            upstream_url: url::Url::parse(&format!(
                "http://example.test:{}/hop",
                upstream_addr.port()
            ))
            .unwrap(),
            host: "example.test".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: upstream_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert!(
            resp.headers().get("proxy-authenticate").is_none(),
            "Proxy-Authenticate must be stripped: {:?}",
            resp.headers()
        );
        assert_eq!(
            resp.headers()
                .get("x-keep-this")
                .and_then(|v| v.to_str().ok()),
            Some("yes"),
            "non-hop headers must pass through",
        );

        proxy.shutdown();
        upstream_task.abort();
    }

    /// PUT / POST etc. must not reach upstream — the proxy is a fetch
    /// primitive for FFmpeg, not a generic forwarding hop. Defense in
    /// depth: leaked tokens shouldn't become a write surface.
    #[tokio::test]
    async fn rejects_non_get_methods() {
        let proxy = PinProxy::start(test_resolver()).await.unwrap();
        let target = PinnedTarget {
            upstream_url: url::Url::parse("http://example.test/x").unwrap(),
            host: "example.test".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 80,
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.put(&url).body("evil").send().await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED,
            "PUT must be refused outright",
        );

        let resp = client.post(&url).body("evil").send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::METHOD_NOT_ALLOWED);

        proxy.shutdown();
    }

    /// `resolve_to_addrs` MUST pin the upstream socket to the registered
    /// IP — DNS at connect time is irrelevant. We register a target whose
    /// `resolved_ip` is a port the test reserved (127.0.0.1:<port_a>) and
    /// confirm the proxy talks to that listener even though the URL says
    /// `bogus.example`.
    #[tokio::test]
    async fn pins_upstream_socket_to_resolved_ip() {
        let router = Router::new().route("/ok", any(|| async { (StatusCode::OK, "pinned-here") }));
        let (real_addr, upstream_task) = spawn_upstream(router).await;

        let proxy = PinProxy::start(test_resolver()).await.unwrap();
        let target = PinnedTarget {
            // Hostname the proxy will set in Host: header — does not
            // resolve via system DNS in CI. The pin is what makes the
            // connect actually land somewhere.
            upstream_url: url::Url::parse(&format!("http://bogus.example:{}/ok", real_addr.port()))
                .unwrap(),
            host: "bogus.example".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: real_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);

        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.expect("proxy GET");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert_eq!(body, "pinned-here");

        proxy.shutdown();
        upstream_task.abort();
    }

    /// The proxy used to set `ClientBuilder::timeout(300s)`, which is a
    /// total deadline through the end of the body. A live relay dies at
    /// that cap and the FFmpeg supervisor restart-loops. Timeouts are
    /// connect + idle-read only.
    #[test]
    fn upstream_timeouts_do_not_cap_the_body() {
        assert_eq!(UPSTREAM_CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(UPSTREAM_READ_TIMEOUT, Duration::from_secs(30));
        let _ = apply_upstream_timeouts(reqwest::Client::builder());
    }

    async fn playlist_proxy(
        resolver: Arc<dyn Resolver>,
        playlist: &'static str,
    ) -> (PinProxy, String, tokio::task::JoinHandle<()>) {
        let router = Router::new().route(
            "/live/index.m3u8",
            any(move || async move {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/vnd.apple.mpegurl",
                    )],
                    playlist,
                )
            }),
        );
        let (upstream_addr, upstream_task) = spawn_upstream(router).await;
        let proxy = PinProxy::start(resolver).await.unwrap();
        let target = PinnedTarget {
            upstream_url: url::Url::parse(&format!(
                "http://cdn.test:{}/live/index.m3u8",
                upstream_addr.port()
            ))
            .unwrap(),
            host: "cdn.test".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: upstream_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);
        (proxy, url, upstream_task)
    }

    /// An HLS media URI that points at the cloud metadata address must
    /// not be forwarded to FFmpeg. The plaintext parent pin still fetches
    /// the playlist; the nested URL is what fails closed.
    #[tokio::test]
    async fn hls_nested_metadata_url_is_blocked() {
        let (proxy, url, upstream_task) = playlist_proxy(
            test_resolver(),
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nhttp://169.254.169.254/latest/meta-data/\n",
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
        let body = resp.text().await.unwrap();
        assert!(
            !body.contains("169.254.169.254"),
            "blocked metadata URL leaked to ffmpeg: {body}"
        );
        assert!(!body.contains("meta-data"), "{body}");
        proxy.shutdown();
        upstream_task.abort();
    }

    /// `URI=` attributes (keys, maps, variant renditions) are the same
    /// fetch surface as media lines.
    #[tokio::test]
    async fn hls_key_uri_to_private_ip_is_blocked() {
        let (proxy, url, upstream_task) = playlist_proxy(
            test_resolver(),
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"http://10.0.0.8/key.bin\"\n#EXTINF:10,\nseg.ts\n",
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
        let body = resp.text().await.unwrap();
        assert!(!body.contains("10.0.0.8"), "{body}");
        assert!(!body.contains("key.bin"), "{body}");
        proxy.shutdown();
        upstream_task.abort();
    }

    /// Loopback inside a playlist is an SSRF target even though the pin
    /// proxy itself listens on loopback.
    #[tokio::test]
    async fn hls_nested_loopback_url_is_blocked() {
        let (proxy, url, upstream_task) = playlist_proxy(
            test_resolver(),
            "#EXTM3U\n#EXTINF:10,\nhttp://127.0.0.1/secret\n",
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
        let body = resp.text().await.unwrap();
        assert!(!body.contains("127.0.0.1/secret"), "{body}");
        proxy.shutdown();
        upstream_task.abort();
    }

    /// Relative segment URLs are re-pinned to a child token. FFmpeg must
    /// not resolve them against the loopback proxy URL. Stopping the
    /// parent burns the child (plaintext pin single-use still holds).
    #[tokio::test]
    async fn hls_relative_segment_is_repinned_and_burned_with_parent() {
        let public_ip = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9));
        let (proxy, url, upstream_task) = playlist_proxy(
            resolver_with("cdn.test", public_ip),
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\nseg0.ts\n",
        )
        .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(
            !body.contains("seg0.ts"),
            "relative segment must not reach ffmpeg: {body}"
        );
        assert!(
            !body.contains("cdn.test"),
            "upstream host must not reach ffmpeg: {body}"
        );
        assert!(
            body.contains("http://127.0.0.1:"),
            "segment must be rewritten to the pin proxy: {body}"
        );
        assert_eq!(
            proxy.registry.len().await,
            2,
            "parent playlist pin + one child segment pin"
        );
        let child = body
            .lines()
            .find(|line| line.starts_with("http://127.0.0.1:"))
            .expect("rewritten segment line");
        let child_token = child.rsplit('/').next().unwrap();
        let stored = proxy.registry.lookup(child_token).await.expect("child pin");
        assert_eq!(stored.resolved_ip, public_ip);
        assert_eq!(stored.host, "cdn.test");
        assert!(
            stored.upstream_url.path().ends_with("/seg0.ts"),
            "child upstream lost the resolved segment path: {}",
            stored.upstream_url
        );

        let parent_token = url.rsplit('/').next().unwrap().to_string();
        assert!(proxy.registry.deregister(&parent_token).await);
        assert_eq!(proxy.registry.len().await, 0, "stop burns nested pins");
        let replay = client.get(child).send().await.unwrap();
        assert_eq!(replay.status(), reqwest::StatusCode::NOT_FOUND);

        proxy.shutdown();
        upstream_task.abort();
    }

    /// DASH (and any other XML manifest) is refused. Rewriting only HLS
    /// and then forwarding MPD would let a BaseURL point ffmpeg at an
    /// internal address.
    #[tokio::test]
    async fn dash_manifest_with_internal_baseurl_is_blocked() {
        let router = Router::new().route(
            "/manifest.mpd",
            any(|| async {
                (
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "application/dash+xml",
                    )],
                    "<?xml version=\"1.0\"?><MPD><BaseURL>http://169.254.169.254/latest/</BaseURL></MPD>",
                )
            }),
        );
        let (upstream_addr, upstream_task) = spawn_upstream(router).await;
        let proxy = PinProxy::start(test_resolver()).await.unwrap();
        let target = PinnedTarget {
            upstream_url: url::Url::parse(&format!(
                "http://cdn.test:{}/manifest.mpd",
                upstream_addr.port()
            ))
            .unwrap(),
            host: "cdn.test".into(),
            resolved_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: upstream_addr.port(),
        };
        let token = proxy.registry.register(target).await;
        let url = proxy.proxy_url(&token);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::BAD_GATEWAY);
        let body = resp.text().await.unwrap();
        assert!(!body.contains("169.254.169.254"), "{body}");
        proxy.shutdown();
        upstream_task.abort();
    }
}
