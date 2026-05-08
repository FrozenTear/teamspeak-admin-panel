//! Spec §6.9 — HTTP security headers.
//!
//! Applied as a single `tower_http::set_header::SetResponseHeaderLayer` per
//! header so each rule is independent and reviewable. HSTS is gated on
//! `NODE_ENV=production` because it forces HTTPS-only at the browser level
//! — turning that on for a localhost dev server would break the dev loop.
//!
//! `X-Frame-Options: DENY` is the global default; the public widget routes
//! (spec §27) override to `SAMEORIGIN` at their handler so they remain
//! embeddable. The override path is owned by WIDGETS, not SECURITY.

use axum::http::{HeaderName, HeaderValue, header};
use tower::layer::util::Identity;
use tower::layer::util::Stack;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::NodeEnv;

/// Compose every security-header layer into one stacked Layer suitable for
/// `Router::layer(...)`.
///
/// HSTS is included only in production. The other headers are unconditional.
pub fn security_headers_stack(node_env: NodeEnv) -> SecurityHeadersStack {
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
    // CSP — directives chosen for the dx (Dioxus) WASM SPA:
    //
    //   * `script-src 'wasm-unsafe-eval'` is required by every evergreen browser
    //     (Chromium ≥ 99, Firefox ≥ 102) for `WebAssembly.instantiateStreaming`;
    //     without it the client never loads and only the SSR fallback renders.
    //   * `script-src 'unsafe-inline'` is a Phase 1 trade-off. dx injects inline
    //     hydration scripts (`window.hydrate_queue`, `window.initial_dioxus_hydration_data`)
    //     whose body contains per-request JSON, so a static hash is not stable
    //     and a per-request nonce would require hooking the dioxus-server HTML
    //     emit. See PURA-48 follow-up to migrate to nonce-based CSP. Acceptable
    //     here only because Phase 1 has no user-controlled `innerHTML` paths
    //     and the inline scripts are dx-generated. Re-evaluate before any
    //     route surfaces user-supplied HTML.
    //   * `style-src 'unsafe-inline'` covers our existing inline `<style>` block
    //     in the dx index.html template. `https://fonts.googleapis.com` is the
    //     CSS host that the Inter `@import` resolves to; `https://fonts.gstatic.com`
    //     is where the actual woff2 files are fetched from.
    //   * Defense-in-depth: `object-src 'none'`, `base-uri 'self'`,
    //     `frame-ancestors 'none'`, `form-action 'self'` reduce the blast radius
    //     of the relaxed `script-src`. `frame-ancestors 'none'` duplicates XFO
    //     DENY for clients that prefer CSP.
    let csp = SetResponseHeaderLayer::if_not_present(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; \
             img-src 'self' data:; \
             connect-src 'self' ws: wss:; \
             style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
             font-src 'self' https://fonts.gstatic.com data:; \
             script-src 'self' 'wasm-unsafe-eval' 'unsafe-inline'; \
             object-src 'none'; \
             base-uri 'self'; \
             frame-ancestors 'none'; \
             form-action 'self'",
        ),
    );
    // HSTS — production only. 6 months minimum per spec; we use 365 days.
    let hsts = if node_env.is_production() {
        Some(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("strict-transport-security"),
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
    } else {
        None
    };

    SecurityHeadersStack {
        xcto,
        xfo,
        referrer,
        csp,
        hsts,
    }
}

/// Pre-built security-header layer bundle. Apply with `router.layer(stack.into_layer())`
/// or by calling [`SecurityHeadersStack::apply`] which threads each layer
/// onto a router in turn.
pub struct SecurityHeadersStack {
    xcto: SetResponseHeaderLayer<HeaderValue>,
    xfo: SetResponseHeaderLayer<HeaderValue>,
    referrer: SetResponseHeaderLayer<HeaderValue>,
    csp: SetResponseHeaderLayer<HeaderValue>,
    hsts: Option<SetResponseHeaderLayer<HeaderValue>>,
}

impl SecurityHeadersStack {
    /// Apply every header layer to the given router. Order doesn't matter
    /// because every header has a distinct name and the layers all use
    /// `if_not_present`.
    pub fn apply(self, router: axum::Router) -> axum::Router {
        let r = router
            .layer(self.xcto)
            .layer(self.xfo)
            .layer(self.referrer)
            .layer(self.csp);
        match self.hsts {
            Some(h) => r.layer(h),
            None => r,
        }
    }
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
        let app = Router::new().route("/", get(|| async { "ok" }));
        let app = security_headers_stack(node_env).apply(app);
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap()
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
    async fn prod_emits_runnable_csp_and_hsts() {
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

        let hsts = h
            .get("strict-transport-security")
            .and_then(|v| v.to_str().ok())
            .expect("prod: HSTS header missing");
        assert!(hsts.contains("max-age="));
        assert!(hsts.contains("includeSubDomains"));
        // Spec: 6-month minimum. 31_536_000s = 365 days, comfortably above.
        assert!(hsts.contains("31536000"));
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
        assert!(r.is_err(), "predicate must reject CSP missing wasm-unsafe-eval");
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
