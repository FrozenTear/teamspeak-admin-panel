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
    use tower::ServiceExt;

    async fn fetch_root(node_env: NodeEnv) -> axum::http::Response<Body> {
        let app = Router::new().route("/", get(|| async { "ok" }));
        let app = security_headers_stack(node_env).apply(app);
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        app.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn dev_emits_xcto_xfo_referrer_csp_but_no_hsts() {
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
        assert!(h.get(header::CONTENT_SECURITY_POLICY).is_some());
        assert!(
            h.get("strict-transport-security").is_none(),
            "HSTS must NOT be set in dev"
        );
    }

    /// PURA-47 regression — every directive that had to land for the dx WASM
    /// SPA to hydrate. Each `assert!` is the load-bearing check for one
    /// concrete browser-side error from the original repro: WASM compile
    /// blocked, dx hydration scripts blocked, Google Fonts CSS blocked,
    /// Google Fonts woff2 blocked. If any of these regress, the SPA stops
    /// loading in Chromium and Firefox.
    #[tokio::test]
    async fn csp_allows_wasm_and_dx_hydration_and_fonts() {
        for env in [NodeEnv::Development, NodeEnv::Production] {
            let resp = fetch_root(env).await;
            let csp = resp
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_else(|| panic!("CSP missing in {env:?}"));

            // WASM compile/instantiate (both Chromium and Firefox).
            assert!(
                csp.contains("'wasm-unsafe-eval'"),
                "{env:?}: script-src must include 'wasm-unsafe-eval' so \
                 WebAssembly.instantiateStreaming is not blocked. Got: {csp}"
            );

            // dx-injected inline hydration scripts. Phase 1 trade-off; revisit
            // when nonce-based CSP lands.
            assert!(
                csp.contains("script-src") && csp.contains("'unsafe-inline'"),
                "{env:?}: script-src must include 'unsafe-inline' until \
                 nonce-based CSP lands; otherwise dx hydration scripts are \
                 rejected. Got: {csp}"
            );

            // Google Fonts CSS host (the @import target inside the inline
            // <style>). `style-src` already had 'unsafe-inline' for the
            // inline block itself; this allows the *fetched* CSS file.
            assert!(
                csp.contains("https://fonts.googleapis.com"),
                "{env:?}: style-src must include https://fonts.googleapis.com \
                 so the Inter @import is not blocked. Got: {csp}"
            );

            // Google Fonts woff2 host (referenced from the fetched CSS).
            assert!(
                csp.contains("font-src") && csp.contains("https://fonts.gstatic.com"),
                "{env:?}: font-src must include https://fonts.gstatic.com so \
                 the Inter woff2 files load. Got: {csp}"
            );

            // Defense-in-depth additions paired with the relaxed script-src.
            assert!(
                csp.contains("object-src 'none'"),
                "{env:?}: object-src 'none' missing. Got: {csp}"
            );
            assert!(
                csp.contains("base-uri 'self'"),
                "{env:?}: base-uri 'self' missing. Got: {csp}"
            );
            assert!(
                csp.contains("frame-ancestors 'none'"),
                "{env:?}: frame-ancestors 'none' missing. Got: {csp}"
            );
            assert!(
                csp.contains("form-action 'self'"),
                "{env:?}: form-action 'self' missing. Got: {csp}"
            );
        }
    }

    #[tokio::test]
    async fn prod_adds_hsts() {
        let resp = fetch_root(NodeEnv::Production).await;
        let hsts = resp
            .headers()
            .get("strict-transport-security")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(hsts.contains("max-age="));
        assert!(hsts.contains("includeSubDomains"));
        // Spec: 6-month minimum. 31_536_000s = 365 days, comfortably above.
        assert!(hsts.contains("31536000"));
    }
}
