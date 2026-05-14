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
//! This module closes the rebinding window for plaintext HTTP by interposing
//! a loopback proxy:
//!
//! 1. `POST /source` validates the URL with `ts6_ssrf`, gets back a
//!    [`ts6_ssrf::PinnedTarget`] including the resolved IP.
//! 2. For plaintext-HTTP sources only (HTTPS unchanged — TLS validation
//!    already pins to the cert SAN), the control plane registers a
//!    [`PinnedTarget`] in this proxy's registry, gets back an unguessable
//!    token, and rewrites the FFmpeg argv to fetch
//!    `http://127.0.0.1:<port>/<token>` instead of the original URL.
//! 3. The proxy receives FFmpeg's GET, looks up the token, and forwards to
//!    the upstream using a `reqwest::Client` whose
//!    [`reqwest::ClientBuilder::resolve_to_addrs`] pins resolution of
//!    `target.host` to `target.resolved_ip` — so DNS at connect time is
//!    irrelevant. The `Host:` header is preserved so virtual-hosted CDNs
//!    serve the right vhost.
//! 4. Redirects (3xx) are refused (502) — v1 keeps SSRF surface minimal;
//!    re-running `ts6-ssrf` per hop is deferred to v2 (see PURA-150).
//! 5. The token is invalidated on `POST /source/stop` so a leaked proxy URL
//!    cannot replay after the pipeline ends.
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
use futures::TryStreamExt;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// One registered upstream the proxy is willing to forward to. Cloned per
/// inbound request so the registry lock is released before the (possibly
/// long-running) upstream fetch begins.
#[derive(Debug, Clone)]
pub struct PinnedTarget {
    /// Full original URL the operator supplied. Path + query are forwarded
    /// verbatim to the upstream so it sees the resource it expects. Scheme
    /// MUST be `http` — HTTPS upstream is out of scope for v1 (would need
    /// MITM cert mint).
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
#[derive(Clone, Default)]
pub struct PinRegistry {
    inner: Arc<RwLock<HashMap<String, PinnedTarget>>>,
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
        guard.insert(token.clone(), target);
        token
    }

    /// Drop `token` from the registry. Returns `true` if a target was
    /// removed, `false` if the token was unknown (idempotent — repeated
    /// burns are fine).
    pub async fn deregister(&self, token: &str) -> bool {
        let mut guard = self.inner.write().await;
        guard.remove(token).is_some()
    }

    async fn lookup(&self, token: &str) -> Option<PinnedTarget> {
        let guard = self.inner.read().await;
        guard.get(token).cloned()
    }

    /// Number of currently registered tokens. Used by integration tests
    /// to assert wiring (HTTP register → token added, stop → burned,
    /// HTTPS → no token). Cheap (one read lock + map.len()).
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// True iff no tokens are currently registered.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
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

impl PinProxy {
    /// Bind the proxy on `127.0.0.1:0` (ephemeral port, loopback-only) and
    /// start its accept loop. The returned [`PinProxy`] owns the task; call
    /// [`Self::shutdown`] to abort it explicitly, or drop the proxy to let
    /// `Drop` abort it.
    pub async fn start() -> anyhow::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("bind PinProxy on 127.0.0.1:0")?;
        let local_addr = listener.local_addr().context("PinProxy local_addr")?;
        let registry = PinRegistry::new();
        let router = Router::new()
            .route("/{token}", any(handle))
            .fallback(not_found)
            .with_state(registry.clone());
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
    State(registry): State<PinRegistry>,
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

    let Some(target) = registry.lookup(&token).await else {
        debug!(token = %short(&token), "PinProxy: unknown token");
        return StatusCode::NOT_FOUND.into_response();
    };

    // Only plaintext HTTP upstream is in scope for v1. Defense-in-depth: if
    // a caller ever registers an https target, refuse to proxy rather than
    // silently downgrading or attempting an unvalidated TLS handshake.
    if target.upstream_url.scheme() != "http" {
        warn!(
            scheme = target.upstream_url.scheme(),
            host = %target.host,
            "PinProxy: refusing non-http upstream",
        );
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // Build a per-request reqwest client pinned to the SSRF-validated IP.
    // `resolve_to_addrs` overrides DNS for `target.host` only; `.no_proxy()`
    // makes sure no HTTP_PROXY env vars can re-route the connect through
    // an external proxy that would defeat the pin; `.redirect(none)` so
    // any 3xx Location lands on us, not on a re-resolved follow-up.
    let client_build = reqwest::Client::builder()
        .resolve_to_addrs(
            &target.host,
            &[SocketAddr::new(target.resolved_ip, target.port)],
        )
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        // Long enough for large media fetches; tighter than infinite so a
        // wedged upstream can't pin a sidecar task forever.
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300));
    let client = match client_build.build() {
        Ok(c) => c,
        Err(err) => {
            warn!(%err, host = %target.host, "PinProxy: client build failed");
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
            warn!(%err, host = %target.host, ip = %target.resolved_ip, "PinProxy: upstream send failed");
            return StatusCode::BAD_GATEWAY.into_response();
        }
    };

    let status = upstream_resp.status();

    // v1 refuses redirects outright — re-running SSRF per hop is v2 work
    // (see issue body). Any 3xx becomes a 502 to the inbound client.
    if status.is_redirection() {
        let location = upstream_resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("<no location>");
        warn!(
            %status,
            host = %target.host,
            location = %location,
            "PinProxy: upstream attempted redirect; refusing (v1 no re-validation)",
        );
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // Strip hop-by-hop response headers and pass everything else through.
    let mut builder = Response::builder().status(status.as_u16());
    let response_headers = builder
        .headers_mut()
        .expect("Response::builder always has headers on a fresh builder");
    for (name, value) in upstream_resp.headers().iter() {
        if is_hop_by_hop(name.as_str()) {
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

    // HEAD never carries a body; for GET, stream the upstream body so we
    // don't buffer 5 GB media files. `bytes_stream` yields `Bytes` chunks.
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let stream = upstream_resp.bytes_stream().map_err(std::io::Error::other);
        Body::from_stream(stream)
    };

    match builder.body(body) {
        Ok(resp) => resp,
        Err(err) => {
            warn!(%err, "PinProxy: build response failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
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
        let proxy = PinProxy::start().await.expect("proxy start");
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

        let proxy = PinProxy::start().await.expect("proxy start");
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

        let proxy = PinProxy::start().await.unwrap();
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

        let proxy = PinProxy::start().await.unwrap();
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

        let proxy = PinProxy::start().await.unwrap();
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
        let proxy = PinProxy::start().await.unwrap();
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

        let proxy = PinProxy::start().await.unwrap();
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
}
