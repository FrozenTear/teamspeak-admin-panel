//! Spec §6.9 — HTTP security headers.
//!
//! Applied as a single `tower_http::set_header::SetResponseHeaderLayer` per
//! static header so each rule is independent and reviewable. HSTS is
//! request-gated: production **and** an actually-HTTPS request. Production
//! alone is not enough — emitting HSTS on cleartext `:3001` trains browsers
//! to demand TLS on a port with no certificate.
//!
//! HTTPS is detected from `X-Forwarded-Proto: https` only when the
//! [`crate::web::proxy`] policy trusts the header: hops > 0 **and** the
//! TCP peer is inside `TRUSTED_PROXY_CIDRS`. An absolute-form request
//! URI (`https://…`) is not treated as HTTPS — the client controls that
//! line. `TRUSTED_PROXY_HOPS=0`, or hops set with an empty CIDR list,
//! ignores client-supplied forwarding headers.
//!
//! `X-Frame-Options: DENY` is the global default; the public widget routes
//! (spec §27) override to `SAMEORIGIN` at their handler so they remain
//! embeddable. The override path is owned by WIDGETS, not SECURITY.
//!
//! ## CSP lives elsewhere (PURA-48)
//!
//! `Content-Security-Policy` is **not** set by this stack. It is owned by
//! the per-response middleware in [`crate::web::csp_nonce`] so each
//! response carries a unique `'nonce-…'` and `script-src` can drop
//! `'unsafe-inline'`. Wire that middleware *outside* this stack on the
//! router so both CSP and the static headers below land on every response.
//! The integration tests below combine the two layers to verify the
//! resulting CSP keeps the dx WASM SPA hydrating.

use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;
use tower::layer::util::Identity;
use tower::layer::util::Stack;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::NodeEnv;
use crate::web::proxy;

const HSTS: HeaderName = HeaderName::from_static("strict-transport-security");
const HSTS_VALUE: HeaderValue = HeaderValue::from_static("max-age=31536000; includeSubDomains");

/// Compose every security-header layer into one stacked Layer suitable for
/// `Router::layer(...)`.
///
/// XCTO / XFO / Referrer-Policy are unconditional. HSTS is wired only in
/// production, and the middleware still suppresses it on cleartext.
pub fn security_headers_stack(
    node_env: NodeEnv,
    proxy_trust: proxy::ProxyTrust,
) -> SecurityHeadersStack {
    let xcto = SetResponseHeaderLayer::if_not_present(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    let xfo = SetResponseHeaderLayer::if_not_present(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    );
    let referrer = SetResponseHeaderLayer::if_not_present(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );

    SecurityHeadersStack {
        xcto,
        xfo,
        referrer,
        emit_hsts: node_env.is_production(),
        proxy_trust,
    }
}

/// Pre-built security-header layer bundle. Apply with `router.layer(stack.into_layer())`
/// or by calling [`SecurityHeadersStack::apply`] which threads each layer
/// onto a router in turn.
pub struct SecurityHeadersStack {
    xcto: SetResponseHeaderLayer<HeaderValue>,
    xfo: SetResponseHeaderLayer<HeaderValue>,
    referrer: SetResponseHeaderLayer<HeaderValue>,
    emit_hsts: bool,
    proxy_trust: proxy::ProxyTrust,
}

impl SecurityHeadersStack {
    /// Apply every header layer to the given router. Order doesn't matter
    /// for the static headers (distinct names, `if_not_present`). HSTS is
    /// a request-aware middleware so it can see the URI scheme and a
    /// trusted `X-Forwarded-Proto`.
    pub fn apply(self, router: axum::Router) -> axum::Router {
        let r = router.layer(self.xcto).layer(self.xfo).layer(self.referrer);
        if self.emit_hsts {
            r.layer(axum::middleware::from_fn_with_state(
                self.proxy_trust,
                emit_hsts_if_https,
            ))
        } else {
            r
        }
    }
}

/// Production HSTS (spec §6.9: ≥6 months; we use 365 days). Set only when
/// the request is actually HTTPS. `if_not_present` — do not clobber a
/// more specific value a handler already set.
async fn emit_hsts_if_https(
    State(trust): State<proxy::ProxyTrust>,
    req: Request,
    next: Next,
) -> Response {
    // Missing ConnectInfo fails closed: 0.0.0.0 is not inside a normal
    // proxy CIDR, so a spoofed X-Forwarded-Proto cannot turn HSTS on.
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
    let https = proxy::request_is_https(req.headers(), peer, &trust);
    let mut resp = next.run(req).await;
    if https {
        let headers = resp.headers_mut();
        if !headers.contains_key(&HSTS) {
            headers.insert(HSTS, HSTS_VALUE);
        }
    }
    resp
}

// `Stack` re-exports kept for callers that want to compose with their own
// middleware via `tower::layer::layer_fn` etc. Currently unused; reserved
// for the FE-PAGES integration when widget routes need the XFO override.
#[allow(dead_code)]
type _UnusedStack = Stack<Identity, Identity>;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use std::collections::HashMap;
    use tower::ServiceExt;

    async fn fetch_root(node_env: NodeEnv) -> axum::http::Response<Body> {
        fetch(node_env, 0, "/", None).await
    }

    async fn fetch(
        node_env: NodeEnv,
        trusted_proxy_hops: u8,
        uri: &str,
        forwarded_proto: Option<&str>,
    ) -> axum::http::Response<Body> {
        fetch_from(
            node_env,
            trusted_proxy_hops,
            uri,
            forwarded_proto,
            // Hop-count tests that expect the proto header to count use a
            // peer inside this CIDR. hops=0 still ignores the header.
            Some("203.0.113.7:443"),
        )
        .await
    }

    async fn fetch_from(
        node_env: NodeEnv,
        trusted_proxy_hops: u8,
        uri: &str,
        forwarded_proto: Option<&str>,
        peer: Option<&str>,
    ) -> axum::http::Response<Body> {
        // Mirror the production wiring in `main.rs`: static header stack on
        // the inside, the per-response nonce-CSP middleware on the outside.
        // CSP-shape assertions below then exercise the same layered result
        // a browser would see, not just one half of it.
        let app = Router::new().route(
            "/",
            get(|| async {
                axum::response::Response::builder()
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(Body::from("ok"))
                    .unwrap()
            }),
        );
        let trust = proxy::ProxyTrust::from_parts(
            trusted_proxy_hops,
            vec!["203.0.113.0/24".parse().unwrap()],
        );
        let app = security_headers_stack(node_env, trust).apply(app);
        let app = app.layer(axum::middleware::from_fn(
            super::super::csp_nonce::nonce_csp_middleware,
        ));
        let mut builder = Request::builder().uri(uri);
        if let Some(proto) = forwarded_proto {
            builder = builder.header("x-forwarded-proto", proto);
        }
        let mut req = builder.body(Body::empty()).unwrap();
        if let Some(peer) = peer {
            let socket: std::net::SocketAddr = peer.parse().unwrap();
            req.extensions_mut()
                .insert(axum::extract::ConnectInfo(socket));
        }
        app.oneshot(req).await.unwrap()
    }

    fn assert_hsts_present(h: &axum::http::HeaderMap) {
        let hsts = h
            .get("strict-transport-security")
            .and_then(|v| v.to_str().ok())
            .expect("HSTS header missing");
        assert!(hsts.contains("max-age="));
        assert!(hsts.contains("includeSubDomains"));
        // Spec: 6-month minimum. 31_536_000s = 365 days, comfortably above.
        assert!(hsts.contains("31536000"));
    }

    /// Parse a CSP header into a directive → sources map. Source-expressions
    /// keep their surrounding quotes (`'self'`, `'unsafe-inline'`, `'nonce-…'`)
    /// so callers can compare them as opaque tokens. Whitespace-tolerant; a
    /// trailing `;` is fine.
    fn parse_csp(csp: &str) -> HashMap<&str, Vec<&str>> {
        csp.split(';')
            .filter_map(|d| {
                let mut parts = d.split_whitespace();
                let name = parts.next()?;
                Some((name, parts.collect()))
            })
            .collect()
    }

    /// PURA-47 regression fence: assert this CSP, served as-is, would actually
    /// let the dx WASM SPA load in a browser. "Header present" is not enough
    /// — the `script-src`, `style-src`, and `font-src` directives must line
    /// up with what dx-emitted HTML actually does (WASM compile, inline
    /// hydration scripts, inline `<style>`, Google Fonts CSS + woff2). A new
    /// CSP that drops any of these fails CI before it can reach a browser.
    fn assert_csp_runs_dx_spa(label: &str, csp: &str) {
        let dirs = parse_csp(csp);

        // ---- script-src ---------------------------------------------------
        let script = dirs
            .get("script-src")
            .unwrap_or_else(|| panic!("{label}: CSP missing script-src. Got: {csp}"));

        // WebAssembly.instantiateStreaming. Both Chromium ≥ 99 and Firefox
        // ≥ 102 accept `'wasm-unsafe-eval'`; `'unsafe-eval'` is the legacy
        // umbrella. Either is sufficient.
        let allows_wasm = script
            .iter()
            .any(|s| *s == "'wasm-unsafe-eval'" || *s == "'unsafe-eval'");
        assert!(
            allows_wasm,
            "{label}: script-src must permit WebAssembly compile via \
             'wasm-unsafe-eval' or 'unsafe-eval'. Got: {csp}"
        );

        // dx-injected inline hydration scripts (`window.hydrate_queue`, …).
        // Acceptable forms: `'unsafe-inline'` (current Phase 1 trade-off,
        // PURA-48 tracks the nonce migration), a per-request `'nonce-…'`,
        // or a stable `'sha…-…'` integrity hash list.
        let allows_inline_scripts = script.iter().any(|s| is_inline_token(s));
        assert!(
            allows_inline_scripts,
            "{label}: script-src must permit dx-injected inline scripts via \
             'unsafe-inline', a 'nonce-…' source, or a 'sha…-…' hash list. \
             Got: {csp}"
        );

        // ---- style-src ----------------------------------------------------
        let style = dirs
            .get("style-src")
            .unwrap_or_else(|| panic!("{label}: CSP missing style-src. Got: {csp}"));

        // The dx index.html template ships an inline `<style>` block.
        let allows_inline_styles = style.iter().any(|s| is_inline_token(s));
        assert!(
            allows_inline_styles,
            "{label}: style-src must permit the inline <style> in dx \
             index.html via 'unsafe-inline', a 'nonce-…' source, or a \
             'sha…-…' hash list. Got: {csp}"
        );

        // The inline `<style>` `@import`s Inter from fonts.googleapis.com.
        assert!(
            allows_host(style, "https://fonts.googleapis.com"),
            "{label}: style-src must permit https://fonts.googleapis.com so \
             the Inter @import is not blocked. Got: {csp}"
        );

        // ---- font-src -----------------------------------------------------
        let font = dirs
            .get("font-src")
            .unwrap_or_else(|| panic!("{label}: CSP missing font-src. Got: {csp}"));

        // The fetched googleapis CSS in turn pulls woff2 from fonts.gstatic.com.
        assert!(
            allows_host(font, "https://fonts.gstatic.com"),
            "{label}: font-src must permit https://fonts.gstatic.com so the \
             Inter woff2 files load. Got: {csp}"
        );

        // ---- defense-in-depth pairing for the relaxed script-src ----------
        for (dir, expected) in [
            ("object-src", "'none'"),
            ("base-uri", "'self'"),
            ("frame-ancestors", "'none'"),
            ("form-action", "'self'"),
        ] {
            let sources = dirs
                .get(dir)
                .unwrap_or_else(|| panic!("{label}: CSP missing {dir}. Got: {csp}"));
            assert_eq!(
                sources.as_slice(),
                [expected].as_slice(),
                "{label}: {dir} must be exactly {expected}. Got: {csp}"
            );
        }
    }

    /// Inline-script-or-style allow tokens. `'unsafe-inline'` is the broad
    /// allow; `'nonce-…'` and `'sha256-…'`/`'sha384-…'`/`'sha512-…'` are the
    /// strict alternatives the spec accepts.
    fn is_inline_token(src: &str) -> bool {
        src == "'unsafe-inline'"
            || src.starts_with("'nonce-")
            || src.starts_with("'sha256-")
            || src.starts_with("'sha384-")
            || src.starts_with("'sha512-")
    }

    /// True if a directive's source list permits the given host. Accepts the
    /// exact host, a permissive `https:` scheme source, or `*`.
    fn allows_host(sources: &[&str], host: &str) -> bool {
        sources
            .iter()
            .any(|s| *s == host || *s == "https:" || *s == "*")
    }

    #[tokio::test]
    async fn dev_emits_runnable_csp_and_no_hsts() {
        let resp = fetch_root(NodeEnv::Development).await;
        let h = resp.headers();
        assert_eq!(
            h.get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            h.get(header::X_FRAME_OPTIONS).and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
        assert_eq!(
            h.get(header::REFERRER_POLICY).and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .expect("dev: CSP header missing");
        assert_csp_runs_dx_spa("dev", csp);
        assert!(
            h.get("strict-transport-security").is_none(),
            "HSTS must NOT be set in dev"
        );
    }

    #[tokio::test]
    async fn prod_emits_runnable_csp_without_hsts_on_cleartext() {
        // Contabo / kube host-network: phones hit http://IP:3001. Production
        // NODE_ENV must not emit HSTS on that cleartext response.
        let resp = fetch_root(NodeEnv::Production).await;
        let h = resp.headers();
        assert_eq!(
            h.get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            h.get(header::X_FRAME_OPTIONS).and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
        assert_eq!(
            h.get(header::REFERRER_POLICY).and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .expect("prod: CSP header missing");
        assert_csp_runs_dx_spa("prod", csp);
        assert!(
            h.get("strict-transport-security").is_none(),
            "HSTS must NOT be set on cleartext HTTP, even in production"
        );
    }

    #[tokio::test]
    async fn prod_emits_hsts_when_trusted_proxy_marks_https() {
        let resp = fetch(NodeEnv::Production, 1, "/", Some("https")).await;
        let h = resp.headers();
        assert_eq!(
            h.get(header::X_CONTENT_TYPE_OPTIONS)
                .and_then(|v| v.to_str().ok()),
            Some("nosniff")
        );
        assert_eq!(
            h.get(header::X_FRAME_OPTIONS).and_then(|v| v.to_str().ok()),
            Some("DENY")
        );
        assert_eq!(
            h.get(header::REFERRER_POLICY).and_then(|v| v.to_str().ok()),
            Some("no-referrer")
        );
        let csp = h
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .expect("prod proxied https: CSP header missing");
        assert_csp_runs_dx_spa("prod proxied https", csp);
        assert_hsts_present(h);
    }

    #[tokio::test]
    async fn prod_ignores_spoofed_forwarded_proto_when_untrusted() {
        // hops=0 (direct :3001): a client-supplied X-Forwarded-Proto must
        // not unlock HSTS.
        let resp = fetch(NodeEnv::Production, 0, "/", Some("https")).await;
        assert!(
            resp.headers().get("strict-transport-security").is_none(),
            "untrusted X-Forwarded-Proto must not emit HSTS"
        );
    }

    #[tokio::test]
    async fn prod_ignores_absolute_form_https_uri() {
        // Client-controlled request-target. Must not emit HSTS.
        let resp = fetch(NodeEnv::Production, 0, "https://panel.example.com/", None).await;
        assert!(
            resp.headers().get("strict-transport-security").is_none(),
            "absolute-form https URI must not emit HSTS"
        );
    }

    #[tokio::test]
    async fn prod_ignores_forwarded_proto_from_peer_outside_cidr() {
        let resp = fetch_from(
            NodeEnv::Production,
            1,
            "/",
            Some("https"),
            Some("198.51.100.8:9"),
        )
        .await;
        assert!(
            resp.headers().get("strict-transport-security").is_none(),
            "peer outside TRUSTED_PROXY_CIDRS must not unlock HSTS"
        );
    }

    #[tokio::test]
    async fn prod_no_hsts_when_trusted_proxy_marks_http() {
        let resp = fetch(NodeEnv::Production, 1, "/", Some("http")).await;
        assert!(
            resp.headers().get("strict-transport-security").is_none(),
            "trusted X-Forwarded-Proto: http is still cleartext"
        );
    }

    /// Negative controls — every shape of broken CSP we've already seen, or
    /// could plausibly see next, must trip `assert_csp_runs_dx_spa`. This is
    /// the predicate's own regression suite: if a future refactor weakens
    /// the predicate, these tests fail.
    #[test]
    fn pre_pura47_csp_is_rejected() {
        // The exact header that was live before commit 88e4c6c — only
        // `script-src 'self'`, no WASM token, no inline allowance, no fonts.
        let broken = "default-src 'self'; img-src 'self' data:; \
                      connect-src 'self' ws: wss:; \
                      style-src 'self' 'unsafe-inline'; \
                      script-src 'self'; \
                      object-src 'none'; base-uri 'self'; \
                      frame-ancestors 'none'; form-action 'self'; \
                      font-src 'self'";
        let r = std::panic::catch_unwind(|| assert_csp_runs_dx_spa("regression", broken));
        assert!(
            r.is_err(),
            "predicate must reject pre-PURA-47 CSP (no WASM, no inline, no fonts)"
        );
    }

    #[test]
    fn csp_without_wasm_eval_is_rejected() {
        // Inline scripts allowed, but no WASM eval token — WASM still blocked.
        let broken = "default-src 'self'; img-src 'self' data:; \
                      connect-src 'self' ws: wss:; \
                      style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
                      font-src 'self' https://fonts.gstatic.com data:; \
                      script-src 'self' 'unsafe-inline'; \
                      object-src 'none'; base-uri 'self'; \
                      frame-ancestors 'none'; form-action 'self'";
        let r = std::panic::catch_unwind(|| assert_csp_runs_dx_spa("regression", broken));
        assert!(
            r.is_err(),
            "predicate must reject CSP missing wasm-unsafe-eval"
        );
    }

    #[test]
    fn csp_without_inline_or_nonce_is_rejected() {
        // WASM allowed but inline scripts blocked and no nonce/hash → hydration fails.
        let broken = "default-src 'self'; img-src 'self' data:; \
                      connect-src 'self' ws: wss:; \
                      style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
                      font-src 'self' https://fonts.gstatic.com data:; \
                      script-src 'self' 'wasm-unsafe-eval'; \
                      object-src 'none'; base-uri 'self'; \
                      frame-ancestors 'none'; form-action 'self'";
        let r = std::panic::catch_unwind(|| assert_csp_runs_dx_spa("regression", broken));
        assert!(
            r.is_err(),
            "predicate must reject CSP without 'unsafe-inline' / nonce / hash for scripts"
        );
    }

    #[test]
    fn csp_without_google_fonts_hosts_is_rejected() {
        let broken = "default-src 'self'; img-src 'self' data:; \
                      connect-src 'self' ws: wss:; \
                      style-src 'self' 'unsafe-inline'; \
                      font-src 'self' data:; \
                      script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; \
                      object-src 'none'; base-uri 'self'; \
                      frame-ancestors 'none'; form-action 'self'";
        let r = std::panic::catch_unwind(|| assert_csp_runs_dx_spa("regression", broken));
        assert!(
            r.is_err(),
            "predicate must reject CSP without fonts.googleapis.com / fonts.gstatic.com"
        );
    }

    #[test]
    fn nonce_based_csp_is_accepted() {
        // PURA-48's target shape: drop 'unsafe-inline' in favour of per-request nonces.
        // The predicate must not block the nonce migration.
        let nonced = "default-src 'self'; img-src 'self' data:; \
                      connect-src 'self' ws: wss:; \
                      style-src 'self' 'nonce-abc123' https://fonts.googleapis.com; \
                      font-src 'self' https://fonts.gstatic.com data:; \
                      script-src 'self' 'wasm-unsafe-eval' 'nonce-abc123'; \
                      object-src 'none'; base-uri 'self'; \
                      frame-ancestors 'none'; form-action 'self'";
        assert_csp_runs_dx_spa("nonce", nonced);
    }
}
