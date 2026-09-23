//! H5 — redirects and nested playback URLs stay inside the SSRF policy.
//!
//! Opus 5.5 review finding H5 ([PR #66](https://github.com/FrozenTear/teamspeak-admin-panel/pull/66),
//! `docs/reviews/2026-09-23-opus-5.5-review.md`): after `/play` accepts a
//! URL, the ICY client followed reqwest's default redirect policy and
//! ffmpeg fetched HLS segment URLs with no further check. A public URL
//! can 302 to a metadata address, and a playlist can list internal
//! segments.
//!
//! This module is the music-side leftover. Host checks go through
//! [`crate::gate::pin_remote_url`] and the process resolver in
//! [`crate::gate::process_resolver`] — the same choke point
//! [`AudioPipeline::spawn`](crate::AudioPipeline::spawn) uses. Plaintext
//! `http` with no pinned address is refused. `https` with a DNS miss
//! is allowed; TLS hostname checks (ffmpeg `-tls_verify 1`, or
//! reqwest's rustls verifier on the hops we follow) bind the name.
//!
//! Redirects are capped at [`MAX_PLAYBACK_REDIRECTS`] and every hop is
//! checked. ffmpeg itself does not re-check: remote inputs are pointed
//! at a loopback proxy ([`PlaybackProxy`]) that runs this guard on each
//! request, including HLS playlist and segment URLs. HTTP redirects are
//! followed here. HTTPS redirects are ffmpeg CONNECTs (its own cap of
//! 8); each CONNECT is authorized before the tunnel opens.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use ts6_ssrf::{PinnedTarget, Resolver, SsrfError};
use url::Url;

/// How many redirects a playback fetch may follow after the first
/// response. The next hop past this cap is refused.
pub(crate) const MAX_PLAYBACK_REDIRECTS: u8 = 3;

/// Playlist bodies larger than this are refused rather than streamed
/// unchecked. A media playlist is a few kilobytes; a megabyte is room
/// for a long VOD list and still bounded.
const MAX_PLAYLIST_BYTES: usize = 1024 * 1024;

/// Cap on URLs pulled out of one playlist. Past this the body is
/// treated as hostile.
const MAX_PLAYLIST_URLS: usize = 4096;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PROXY_HEADERS: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum GuardError {
    #[error("URL rejected by SSRF policy: {0}")]
    Ssrf(#[from] SsrfError),
    #[error("plaintext HTTP URL has no pinned address")]
    UnpinnedHttp,
    #[error("redirected too many times ({0})")]
    TooManyRedirects(u8),
    #[error("redirect missing or invalid Location")]
    BadRedirect,
    #[error("malformed playback proxy request")]
    BadRequest,
    #[error("playlist rejected: {0}")]
    BadPlaylist(&'static str),
    #[error("playlist exceeds the playback guard size cap")]
    PlaylistTooLarge,
    #[error("upstream connect failed: {0}")]
    Connect(String),
    #[error("playback fetch failed: {0}")]
    Fetch(#[from] reqwest::Error),
}

impl GuardError {
    /// Policy refusals, as opposed to a transport failure. The proxy
    /// counts these and answers 403.
    fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::Ssrf(_)
                | Self::UnpinnedHttp
                | Self::TooManyRedirects(_)
                | Self::BadRedirect
                | Self::BadPlaylist(_)
                | Self::PlaylistTooLarge
        )
    }
}

/// Who may pass a loopback target.
///
/// Production is [`FetchPolicy::production`]. The ICY unit tests bind a
/// mock server on `127.0.0.1` and opt in; that build is `cfg(test)`
/// only and is not the music-bot binary. The ffmpeg proxy always uses
/// production.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FetchPolicy {
    allow_loopback: bool,
}

impl FetchPolicy {
    pub(crate) fn production() -> Self {
        Self {
            allow_loopback: false,
        }
    }

    /// Loopback targets are allowed. ICY reconnect tests only.
    #[cfg(test)]
    pub(crate) fn allow_loopback() -> Self {
        Self {
            allow_loopback: true,
        }
    }
}

/// Check `raw` with the process resolver and the production policy.
pub(crate) async fn authorize_playback_url(raw: &str) -> Result<PinnedTarget, GuardError> {
    authorize_url(
        raw,
        FetchPolicy::production(),
        crate::gate::process_resolver(),
    )
    .await
}

pub(crate) async fn authorize_url(
    raw: &str,
    policy: FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<PinnedTarget, GuardError> {
    if policy.allow_loopback && is_loopback_url(raw) {
        return synthetic_loopback(raw);
    }
    match crate::gate::pin_remote_url(raw, resolver).await {
        Ok(target) => Ok(target),
        Err(crate::gate::GateError::Ssrf(err)) => Err(GuardError::Ssrf(err)),
        Err(crate::gate::GateError::UnpinnedHttp) => Err(GuardError::UnpinnedHttp),
        Err(other) => Err(GuardError::Connect(other.to_string())),
    }
}

fn is_loopback_url(raw: &str) -> bool {
    let Ok(url) = Url::parse(raw) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => {
            name.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        }
        None => false,
    }
}

fn synthetic_loopback(raw: &str) -> Result<PinnedTarget, GuardError> {
    let url = Url::parse(raw).map_err(|_| GuardError::Ssrf(SsrfError::InvalidUrlFormat))?;
    let port = url
        .port_or_known_default()
        .ok_or(GuardError::Ssrf(SsrfError::InvalidUrlFormat))?;
    let ip = match url.host() {
        Some(url::Host::Ipv6(_)) => IpAddr::V6(Ipv6Addr::LOCALHOST),
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let host = url.host_str().unwrap_or("localhost").to_string();
    Ok(PinnedTarget {
        url,
        host,
        port,
        resolved_ip: Some(ip),
    })
}

/// GET/HEAD `url`, following at most [`MAX_PLAYBACK_REDIRECTS`] hops.
/// Each hop is authorized and, when an address was pinned, connected
/// to that address (`resolve_to_addrs`). Reqwest's own redirect policy
/// is off. TLS certificates are verified (rustls default).
pub(crate) async fn guarded_request(
    method: reqwest::Method,
    url: &str,
    headers: reqwest::header::HeaderMap,
    policy: FetchPolicy,
) -> Result<reqwest::Response, GuardError> {
    guarded_request_with(
        method,
        url,
        headers,
        policy,
        crate::gate::process_resolver(),
    )
    .await
}

pub(crate) async fn guarded_request_with(
    mut method: reqwest::Method,
    url: &str,
    headers: reqwest::header::HeaderMap,
    policy: FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<reqwest::Response, GuardError> {
    let mut current = url.to_string();
    let mut followed = 0u8;
    loop {
        let target = authorize_url(&current, policy, resolver).await?;
        let client = client_for(&target)?;
        let resp = client
            .request(method.clone(), target.url.clone())
            .headers(headers.clone())
            .send()
            .await?;
        let status = resp.status();
        if !is_follow_redirect(status) {
            return Ok(resp);
        }
        if followed >= MAX_PLAYBACK_REDIRECTS {
            return Err(GuardError::TooManyRedirects(MAX_PLAYBACK_REDIRECTS));
        }
        let loc = redirect_location(resp.headers())?;
        let next = resolve_redirect(&target.url, loc)?;
        // Drop the redirect body before the next hop so the socket can close.
        drop(resp);
        if status == reqwest::StatusCode::SEE_OTHER {
            method = reqwest::Method::GET;
        }
        current = next.as_str().to_string();
        followed += 1;
    }
}

fn is_follow_redirect(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::MOVED_PERMANENTLY
            | reqwest::StatusCode::FOUND
            | reqwest::StatusCode::SEE_OTHER
            | reqwest::StatusCode::TEMPORARY_REDIRECT
            | reqwest::StatusCode::PERMANENT_REDIRECT
    )
}

fn redirect_location(headers: &reqwest::header::HeaderMap) -> Result<&str, GuardError> {
    let loc = headers
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.contains(['\r', '\n']))
        .ok_or(GuardError::BadRedirect)?;
    Ok(loc)
}

fn resolve_redirect(base: &Url, location: &str) -> Result<Url, GuardError> {
    if location.contains("://") {
        Url::parse(location).map_err(|_| GuardError::BadRedirect)
    } else {
        base.join(location).map_err(|_| GuardError::BadRedirect)
    }
}

fn client_for(target: &PinnedTarget) -> Result<reqwest::Client, GuardError> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(CONNECT_TIMEOUT);
    // Pin domain names to the address the SSRF check accepted. IP
    // literals do not rebind; pinning them is redundant and some
    // reqwest versions do not match a bracketed IPv6 host.
    if let Some(ip) = target.resolved_ip
        && matches!(target.url.host(), Some(url::Host::Domain(_)))
        && let Some(host) = target.url.host_str()
    {
        builder = builder.resolve_to_addrs(host, &[SocketAddr::new(ip, target.port)]);
    }
    builder.build().map_err(GuardError::Fetch)
}

/// URLs an HLS / M3U / PLS body asks the player to open, resolved
/// against `base` (the playlist's final URL after redirects).
pub(crate) fn playlist_references(base: &Url, body: &str) -> Result<Vec<Url>, GuardError> {
    let mut out = Vec::new();
    for raw_line in body.split('\n') {
        let line = raw_line.trim().trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            push_quoted_uris(rest, base, &mut out)?;
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            continue;
        }
        // PLS `File1=http://…` / `Title1=Name`. A media URL can also
        // contain `=` in the query (`http://host/a?x=1`); those keys
        // contain `://` and stay media lines.
        if let Some((key, value)) = line.split_once('=')
            && !key.contains("://")
            && !key.contains('/')
        {
            if pls_file_key(key.trim()) {
                push_resolved(value.trim(), base, &mut out)?;
            }
            continue;
        }
        push_resolved(line, base, &mut out)?;
    }
    Ok(out)
}

fn pls_file_key(key: &str) -> bool {
    let Some(rest) = key.get(4..) else {
        return false;
    };
    key[..4].eq_ignore_ascii_case("file")
        && !rest.is_empty()
        && rest.bytes().all(|b| b.is_ascii_digit())
}

fn push_quoted_uris(line: &str, base: &Url, out: &mut Vec<Url>) -> Result<(), GuardError> {
    let lower = line.to_ascii_lowercase();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find("uri=\"") {
        let start = search_from + rel + 5;
        let rest = line
            .get(start..)
            .ok_or(GuardError::BadPlaylist("unterminated URI"))?;
        let end = rest
            .find('"')
            .ok_or(GuardError::BadPlaylist("unterminated URI"))?;
        push_resolved(&rest[..end], base, out)?;
        search_from = start + end + 1;
    }
    Ok(())
}

fn push_resolved(raw: &str, base: &Url, out: &mut Vec<Url>) -> Result<(), GuardError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(());
    }
    let url = if raw.contains("://") {
        Url::parse(raw).map_err(|_| GuardError::BadPlaylist("unparseable playlist URL"))?
    } else {
        base.join(raw)
            .map_err(|_| GuardError::BadPlaylist("unparseable relative playlist URL"))?
    };
    out.push(url);
    if out.len() > MAX_PLAYLIST_URLS {
        return Err(GuardError::PlaylistTooLarge);
    }
    Ok(())
}

async fn playlist_urls_allowed(
    base: &Url,
    body: &str,
    policy: FetchPolicy,
    resolver: &dyn Resolver,
) -> Result<(), GuardError> {
    for url in playlist_references(base, body)? {
        if let Err(err) = authorize_url(url.as_str(), policy, resolver).await {
            tracing::warn!(
                scheme = url.scheme(),
                host = url.host_str().unwrap_or(""),
                error = %err,
                "playback playlist URL refused"
            );
            return Err(err);
        }
    }
    Ok(())
}

fn is_playlist(url: &Url, content_type: Option<&reqwest::header::HeaderValue>) -> bool {
    if let Some(ct) = content_type.and_then(|v| v.to_str().ok()) {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("mpegurl") || ct.contains("mpeg-url") || ct.contains("scpls") {
            return true;
        }
    }
    match url.path().rsplit('/').next().unwrap_or("").rsplit_once('.') {
        Some((_, ext)) => matches!(ext.to_ascii_lowercase().as_str(), "m3u8" | "m3u" | "pls"),
        None => false,
    }
}

/// Loopback proxy ffmpeg uses for every remote input.
///
/// Binds `127.0.0.1` only. ffmpeg is given `-http_proxy` and
/// `http_proxy` in its environment (ffmpeg 6.1 reads that variable for
/// both plaintext HTTP and the TLS `httpproxy` CONNECT path, including
/// HLS segment opens). Each absolute-form request and each CONNECT is
/// authorized. Playlist bodies are scanned and refused when they name
/// a blocked URL.
pub(crate) struct PlaybackProxy {
    addr: SocketAddr,
    /// Test-only counter. Production writes it; unit tests assert ffmpeg
    /// actually reached the proxy.
    #[allow(dead_code)]
    refusals: Arc<AtomicU64>,
    shutdown: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl PlaybackProxy {
    pub(crate) async fn start() -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let refusals = Arc::new(AtomicU64::new(0));
        let (shutdown, shutdown_rx) = watch::channel(false);
        let refusals_task = Arc::clone(&refusals);
        let task = crate::runtime::spawn_decode(async move {
            accept_loop(listener, shutdown_rx, refusals_task).await;
        });
        Ok(Self {
            addr,
            refusals,
            shutdown,
            task,
        })
    }

    pub(crate) fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    #[cfg(test)]
    pub(crate) fn refusals(&self) -> u64 {
        self.refusals.load(Ordering::Relaxed)
    }
}

impl Drop for PlaybackProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.task.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    mut shutdown_rx: watch::Receiver<bool>,
    refusals: Arc<AtomicU64>,
) {
    loop {
        tokio::select! {
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
            }
            accepted = listener.accept() => {
                let Ok((sock, _)) = accepted else {
                    break;
                };
                let refusals = Arc::clone(&refusals);
                let shutdown_rx = shutdown_rx.clone();
                tokio::spawn(async move {
                    let mut shutdown_rx = shutdown_rx;
                    tokio::select! {
                        changed = shutdown_rx.changed() => {
                            let _ = changed;
                        }
                        result = handle_conn(sock, refusals) => {
                            if let Err(err) = result {
                                tracing::debug!(error = %err, "playback proxy connection ended");
                            }
                        }
                    }
                });
            }
        }
    }
}

async fn handle_conn(mut sock: TcpStream, refusals: Arc<AtomicU64>) -> io::Result<()> {
    let _ = sock.set_nodelay(true);
    let incoming = read_incoming(&mut sock).await?;
    if incoming.method.eq_ignore_ascii_case("CONNECT") {
        handle_connect(&mut sock, &incoming, &refusals).await
    } else if incoming.method.eq_ignore_ascii_case("GET")
        || incoming.method.eq_ignore_ascii_case("HEAD")
    {
        handle_http(&mut sock, &incoming, &refusals).await
    } else {
        write_simple(&mut sock, 405, "method_not_allowed").await
    }
}

struct Incoming {
    method: String,
    target: String,
    headers: reqwest::header::HeaderMap,
}

async fn read_incoming(sock: &mut TcpStream) -> io::Result<Incoming> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + HEADER_TIMEOUT;
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_PROXY_HEADERS {
            return Err(io::Error::other("playback proxy headers too large"));
        }
        let n = tokio::time::timeout_at(deadline, sock.read(&mut tmp))
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "playback proxy header timeout")
            })??;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "playback proxy client closed",
            ));
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    parse_incoming(&buf).map_err(|err| io::Error::other(err.to_string()))
}

fn parse_incoming(buf: &[u8]) -> Result<Incoming, GuardError> {
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(GuardError::BadRequest)?;
    let text = std::str::from_utf8(&buf[..header_end]).map_err(|_| GuardError::BadRequest)?;
    let mut lines = text.split("\r\n");
    let request = lines.next().ok_or(GuardError::BadRequest)?;
    let mut parts = request.split(' ');
    let method = parts.next().ok_or(GuardError::BadRequest)?.to_string();
    let target = parts.next().ok_or(GuardError::BadRequest)?.to_string();
    if parts.next().is_none() {
        return Err(GuardError::BadRequest);
    }
    let mut headers = reqwest::header::HeaderMap::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(GuardError::BadRequest);
        };
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.trim().as_bytes()) else {
            continue;
        };
        let Ok(value) = reqwest::header::HeaderValue::from_str(value.trim()) else {
            continue;
        };
        headers.append(name, value);
    }
    Ok(Incoming {
        method,
        target,
        headers,
    })
}

async fn handle_connect(
    sock: &mut TcpStream,
    incoming: &Incoming,
    refusals: &AtomicU64,
) -> io::Result<()> {
    let url = match connect_target_url(&incoming.target) {
        Ok(url) => url,
        Err(err) => {
            note_refusal(refusals, &err);
            return write_simple(sock, 400, "bad_request").await;
        }
    };
    let target = match authorize_playback_url(&url).await {
        Ok(target) => target,
        Err(err) => {
            note_refusal(refusals, &err);
            tracing::warn!(url = %host_for_log(&url), error = %err, "playback proxy refused CONNECT");
            return write_simple(sock, 403, "ssrf_blocked").await;
        }
    };
    let mut upstream = match connect_pinned(&target).await {
        Ok(stream) => stream,
        Err(err) => {
            tracing::warn!(url = %host_for_log(&url), error = %err, "playback proxy CONNECT failed");
            return write_simple(sock, 502, "bad_gateway").await;
        }
    };
    sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    let _ = tokio::io::copy_bidirectional(sock, &mut upstream).await;
    Ok(())
}

fn connect_target_url(target: &str) -> Result<String, GuardError> {
    let (host, port) = split_host_port(target).ok_or(GuardError::BadRequest)?;
    // CONNECT is the TLS tunnel ffmpeg opens for https. Port 80 is the
    // odd plaintext case; every other port is checked as https so a DNS
    // miss fails the same way as an https playback URL.
    let scheme = if port == 80 { "http" } else { "https" };
    if host.contains(':') {
        Ok(format!("{scheme}://[{host}]:{port}/"))
    } else {
        Ok(format!("{scheme}://{host}:{port}/"))
    }
}

fn split_host_port(target: &str) -> Option<(&str, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = after.strip_prefix(':')?.parse().ok()?;
        return Some((host, port));
    }
    let (host, port) = target.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }
    Some((host, port.parse().ok()?))
}

async fn connect_pinned(target: &PinnedTarget) -> Result<TcpStream, GuardError> {
    let connect = async {
        if let Some(ip) = target.resolved_ip {
            TcpStream::connect(SocketAddr::new(ip, target.port)).await
        } else {
            let host = target.url.host_str().unwrap_or(target.host.as_str());
            TcpStream::connect((host, target.port)).await
        }
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(GuardError::Connect(err.to_string())),
        Err(_) => Err(GuardError::Connect("timed out".to_string())),
    }
}

async fn handle_http(
    sock: &mut TcpStream,
    incoming: &Incoming,
    refusals: &AtomicU64,
) -> io::Result<()> {
    let url = match request_url(incoming) {
        Ok(url) => url,
        Err(err) => {
            note_refusal(refusals, &err);
            return write_simple(sock, 400, "bad_request").await;
        }
    };
    let method = if incoming.method.eq_ignore_ascii_case("HEAD") {
        reqwest::Method::HEAD
    } else {
        reqwest::Method::GET
    };
    let headers = forward_headers(&incoming.headers);
    match guarded_request(method, &url, headers, FetchPolicy::production()).await {
        Ok(resp) => write_upstream(sock, resp, refusals).await,
        Err(err) => {
            note_refusal(refusals, &err);
            if err.is_refusal() {
                tracing::warn!(url = %host_for_log(&url), error = %err, "playback proxy refused URL");
                write_simple(sock, 403, "ssrf_blocked").await
            } else {
                tracing::warn!(url = %host_for_log(&url), error = %err, "playback proxy fetch failed");
                write_simple(sock, 502, "bad_gateway").await
            }
        }
    }
}

fn request_url(incoming: &Incoming) -> Result<String, GuardError> {
    let target = incoming.target.as_str();
    let scheme = target.split(':').next().unwrap_or("").to_ascii_lowercase();
    if scheme == "http" || scheme == "https" {
        return Ok(target.to_string());
    }
    if let Some(path) = target.strip_prefix('/') {
        let host = incoming
            .headers
            .get(reqwest::header::HOST)
            .and_then(|v| v.to_str().ok())
            .ok_or(GuardError::BadRequest)?;
        return Ok(format!("http://{host}/{path}"));
    }
    // CONNECT is handled elsewhere. Anything else is not a URL we can check.
    Err(GuardError::BadRequest)
}

fn forward_headers(incoming: &reqwest::header::HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for name in ["range", "user-agent", "accept", "icy-metadata"] {
        if let Some(value) = incoming.get(name) {
            out.insert(
                reqwest::header::HeaderName::from_static(name),
                value.clone(),
            );
        }
    }
    if !out.contains_key(reqwest::header::USER_AGENT) {
        out.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("music-bot-audio/0.0 (PURA-119)"),
        );
    }
    out
}

async fn write_upstream(
    sock: &mut TcpStream,
    resp: reqwest::Response,
    refusals: &AtomicU64,
) -> io::Result<()> {
    let status = resp.status();
    let final_url = resp.url().clone();
    let headers = resp.headers().clone();
    let playlist = is_playlist(&final_url, headers.get(reqwest::header::CONTENT_TYPE));
    if playlist {
        let bytes = match read_capped(resp, MAX_PLAYLIST_BYTES).await {
            Ok(bytes) => bytes,
            Err(err) => return refuse_playlist(sock, refusals, err).await,
        };
        let text = String::from_utf8_lossy(&bytes);
        if let Err(err) = playlist_urls_allowed(
            &final_url,
            &text,
            FetchPolicy::production(),
            crate::gate::process_resolver(),
        )
        .await
        {
            return refuse_playlist(sock, refusals, err).await;
        }
        write_bytes(sock, status, &headers, Some(&bytes)).await
    } else {
        write_stream(sock, status, &headers, resp).await
    }
}

async fn refuse_playlist(
    sock: &mut TcpStream,
    refusals: &AtomicU64,
    err: GuardError,
) -> io::Result<()> {
    note_refusal(refusals, &err);
    tracing::warn!(error = %err, "playback playlist refused");
    let status = if err.is_refusal() { 403 } else { 502 };
    let body = if status == 403 {
        "ssrf_blocked"
    } else {
        "bad_gateway"
    };
    write_simple(sock, status, body).await
}

async fn read_capped(resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, GuardError> {
    if resp.content_length().is_some_and(|len| len > cap as u64) {
        return Err(GuardError::PlaylistTooLarge);
    }
    let mut out = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len().saturating_add(chunk.len()) > cap {
            return Err(GuardError::PlaylistTooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn write_stream(
    sock: &mut TcpStream,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    resp: reqwest::Response,
) -> io::Result<()> {
    let head = header_block(status, headers, None);
    sock.write_all(head.as_bytes()).await?;
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| io::Error::other(err.to_string()))?;
        sock.write_all(&chunk).await?;
    }
    Ok(())
}

async fn write_bytes(
    sock: &mut TcpStream,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Option<&[u8]>,
) -> io::Result<()> {
    let head = header_block(status, headers, body.map(|b| b.len()));
    sock.write_all(head.as_bytes()).await?;
    if let Some(body) = body {
        sock.write_all(body).await?;
    }
    Ok(())
}

fn header_block(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    body_len: Option<usize>,
) -> String {
    let reason = status.canonical_reason().unwrap_or("OK");
    let mut out = format!("HTTP/1.1 {} {reason}\r\n", status.as_u16());
    const SKIP: &[&str] = &[
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailers",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ];
    for (name, value) in headers.iter() {
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        let Ok(value) = value.to_str() else {
            continue;
        };
        out.push_str(name.as_str());
        out.push_str(": ");
        out.push_str(value);
        out.push_str("\r\n");
    }
    if let Some(len) = body_len {
        out.push_str(&format!("Content-Length: {len}\r\n"));
    } else if let Some(len) = headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
    {
        out.push_str("Content-Length: ");
        out.push_str(len);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    out
}

async fn write_simple(sock: &mut TcpStream, status: u16, body: &str) -> io::Result<()> {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        _ => "Error",
    };
    let bytes = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(bytes.as_bytes()).await
}

fn note_refusal(refusals: &AtomicU64, err: &GuardError) {
    if err.is_refusal() {
        refusals.fetch_add(1, Ordering::Relaxed);
    }
}

fn host_for_log(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| format!("{}://{host}", url.scheme()))
        })
        .unwrap_or_else(|| "invalid-url".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use ts6_ssrf::MockResolver;

    fn public_resolver() -> MockResolver {
        MockResolver::new().with("example.com", vec![IpAddr::from([203, 0, 113, 10])])
    }

    #[tokio::test]
    async fn production_policy_refuses_metadata_private_and_loopback() {
        let resolver = MockResolver::new();
        for url in [
            "http://169.254.169.254/latest/meta-data",
            "http://10.1.2.3/secret",
            "http://127.0.0.1/admin",
            "http://[::1]/",
            "https://metadata.google.internal/computeMetadata/v1/",
            "http://metadata.google.internal./",
        ] {
            let err = authorize_url(url, FetchPolicy::production(), &resolver)
                .await
                .expect_err(url);
            assert!(
                matches!(err, GuardError::Ssrf(_)),
                "{url} should be SSRF-blocked, got {err}"
            );
        }
        let err = authorize_url(
            "http://missing.example/a.mp3",
            FetchPolicy::production(),
            &MockResolver::new(),
        )
        .await
        .expect_err("unpinned http");
        assert!(
            matches!(err, GuardError::UnpinnedHttp),
            "plaintext HTTP with no pin must fail closed, got {err}"
        );
    }

    #[tokio::test]
    async fn production_policy_allows_public_and_unpinned_https() {
        let resolver = public_resolver();
        let https = authorize_url(
            "https://example.com/a.mp3",
            FetchPolicy::production(),
            &resolver,
        )
        .await
        .expect("public https");
        assert_eq!(https.resolved_ip, Some(IpAddr::from([203, 0, 113, 10])));
        authorize_url(
            "http://203.0.113.10/a.mp3",
            FetchPolicy::production(),
            &MockResolver::new(),
        )
        .await
        .expect("public ip literal");
        authorize_url(
            "https://missing.example/a.mp3",
            FetchPolicy::production(),
            &MockResolver::new(),
        )
        .await
        .expect("https DNS miss is allowed; TLS binds the name");
    }

    #[tokio::test]
    async fn redirect_to_metadata_and_private_is_refused() {
        let resolver = MockResolver::new();
        let base = Url::parse("https://example.com/a.m3u8").unwrap();
        for location in [
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.5/secret",
            "http://metadata.google.internal/computeMetadata/v1/",
            "//169.254.169.254/latest/meta-data",
        ] {
            let next = resolve_redirect(&base, location).unwrap();
            let err = authorize_url(next.as_str(), FetchPolicy::production(), &resolver)
                .await
                .expect_err(location);
            assert!(
                matches!(err, GuardError::Ssrf(_)),
                "redirect {location} -> {next} must be refused, got {err}"
            );
        }
    }

    #[test]
    fn playlist_collects_segments_keys_and_pls_entries() {
        let base = Url::parse("https://example.com/live/index.m3u8").unwrap();
        let body = "\
#EXTM3U
#EXT-X-KEY:METHOD=AES-128,URI=\"keys/a.key\"
#EXTINF:10,
seg.ts
#EXT-X-MEDIA:TYPE=AUDIO,URI=\"https://example.com/alt.m3u8\"
";
        let urls = playlist_references(&base, body).unwrap();
        let rendered: Vec<_> = urls.iter().map(|u| u.as_str().to_string()).collect();
        assert!(
            rendered.iter().any(|u| u.ends_with("/live/keys/a.key")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|u| u.ends_with("/live/seg.ts")),
            "{rendered:?}"
        );
        assert!(
            rendered.iter().any(|u| u == "https://example.com/alt.m3u8"),
            "{rendered:?}"
        );

        let pls = Url::parse("http://203.0.113.10/stations.pls").unwrap();
        let urls = playlist_references(
            &pls,
            "[playlist]\nFile1=http://203.0.113.10/a.mp3\nTitle1=A\n",
        )
        .unwrap();
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0].as_str(), "http://203.0.113.10/a.mp3");
    }

    #[tokio::test]
    async fn playlist_segment_on_metadata_is_refused() {
        let base = Url::parse("https://example.com/index.m3u8").unwrap();
        let body = "#EXTM3U\nhttp://169.254.169.254/latest/meta-data\n";
        let err =
            playlist_urls_allowed(&base, body, FetchPolicy::production(), &MockResolver::new())
                .await
                .expect_err("metadata segment");
        assert!(matches!(err, GuardError::Ssrf(_)), "{err}");

        let err = playlist_urls_allowed(
            &base,
            "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"file:///etc/passwd\"\n",
            FetchPolicy::production(),
            &MockResolver::new(),
        )
        .await
        .expect_err("file key");
        assert!(matches!(err, GuardError::Ssrf(_)), "{err}");

        playlist_urls_allowed(
            &base,
            "#EXTM3U\nhttps://example.com/a.ts\n",
            FetchPolicy::production(),
            &public_resolver(),
        )
        .await
        .expect("public segment");
    }

    async fn spawn_script(
        script: Vec<Vec<u8>>,
    ) -> (String, tokio::task::JoinHandle<()>, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits_task = Arc::clone(&hits);
        let handle = tokio::spawn(async move {
            let mut script = script.into_iter();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                hits_task.fetch_add(1, Ordering::Relaxed);
                let Some(body) = script.next() else {
                    break;
                };
                let mut buf = vec![0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            }
        });
        (format!("http://{addr}/"), handle, hits)
    }

    fn response(status_and_headers: &str, body: &str) -> Vec<u8> {
        format!("{status_and_headers}\r\n\r\n{body}").into_bytes()
    }

    #[tokio::test]
    async fn follower_stops_on_redirect_to_metadata() {
        let (url, _server, hits) = spawn_script(vec![response(
            "HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data\r\nContent-Length: 0\r\nConnection: close",
            "",
        )])
        .await;
        let err = tokio::time::timeout(
            Duration::from_secs(2),
            guarded_request_with(
                reqwest::Method::GET,
                &url,
                reqwest::header::HeaderMap::new(),
                FetchPolicy::allow_loopback(),
                &MockResolver::new(),
            ),
        )
        .await
        .expect("refusal must not connect to the metadata address")
        .expect_err("redirect to metadata");
        assert!(matches!(err, GuardError::Ssrf(_)), "{err}");
        assert_eq!(hits.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn follower_caps_redirect_loops() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/loop");
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_task = Arc::clone(&hits);
        let location = url.clone();
        let _server = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                hits_task.fetch_add(1, Ordering::Relaxed);
                let mut buf = vec![0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let body = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = sock.write_all(body.as_bytes()).await;
            }
        });
        let err = guarded_request_with(
            reqwest::Method::GET,
            &url,
            reqwest::header::HeaderMap::new(),
            FetchPolicy::allow_loopback(),
            &MockResolver::new(),
        )
        .await
        .expect_err("redirect loop");
        assert!(
            matches!(err, GuardError::TooManyRedirects(MAX_PLAYBACK_REDIRECTS)),
            "{err}"
        );
        // Initial request plus one request per followed redirect. The
        // response that would exceed the cap is seen and not followed.
        assert_eq!(
            hits.load(Ordering::Relaxed),
            usize::from(MAX_PLAYBACK_REDIRECTS) + 1
        );
    }

    #[tokio::test]
    async fn follower_resolves_a_relative_redirect() {
        let (url, _server, _) = spawn_script(vec![
            response(
                "HTTP/1.1 302 Found\r\nLocation: /ok\r\nContent-Length: 0\r\nConnection: close",
                "",
            ),
            response(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 2\r\nConnection: close",
                "ok",
            ),
        ])
        .await;
        let resp = guarded_request_with(
            reqwest::Method::GET,
            &url,
            reqwest::header::HeaderMap::new(),
            FetchPolicy::allow_loopback(),
            &MockResolver::new(),
        )
        .await
        .expect("relative redirect on loopback");
        let body = resp.bytes().await.unwrap();
        assert_eq!(&body[..], b"ok");
    }

    async fn proxy_exchange(request: &str) -> String {
        let proxy = PlaybackProxy::start().await.unwrap();
        let mut sock = TcpStream::connect(proxy.addr).await.unwrap();
        sock.write_all(request.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), sock.read_to_end(&mut buf)).await;
        drop(sock);
        // Give the handler a moment to bump the counter before we read it.
        tokio::task::yield_now().await;
        let refusals = proxy.refusals();
        let text = String::from_utf8_lossy(&buf).into_owned();
        assert!(refusals >= 1, "proxy should count a refusal, body={text:?}");
        text
    }

    #[tokio::test]
    async fn proxy_refuses_metadata_private_and_connect() {
        for request in [
            "GET http://169.254.169.254/latest/meta-data HTTP/1.1\r\nHost: 169.254.169.254\r\n\r\n",
            "GET http://10.1.2.3/secret HTTP/1.1\r\nHost: 10.1.2.3\r\n\r\n",
            "GET http://metadata.google.internal/computeMetadata/v1/ HTTP/1.1\r\nHost: metadata.google.internal\r\n\r\n",
            "GET http://127.0.0.1/latest HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
            "CONNECT 169.254.169.254:443 HTTP/1.1\r\nHost: 169.254.169.254:443\r\n\r\n",
            "CONNECT metadata.google.internal:443 HTTP/1.1\r\nHost: metadata.google.internal:443\r\n\r\n",
        ] {
            let body = proxy_exchange(request).await;
            assert!(
                body.starts_with("HTTP/1.1 403") && body.contains("ssrf_blocked"),
                "request {request:?} got {body:?}"
            );
        }
    }
}
