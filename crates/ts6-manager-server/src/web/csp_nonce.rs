//! PURA-48 — Phase 2 nonce-based Content-Security-Policy.
//!
//! Generates a per-request 128-bit hex nonce, rewrites every inline
//! `<script>` open tag in HTML responses to `<script nonce="…">`, and
//! sets the `Content-Security-Policy` header per-response with that
//! nonce baked into `script-src`. This replaces the Phase 1 (PURA-47)
//! `'unsafe-inline'` trade-off.
//!
//! ## Why a body-rewriting middleware
//!
//! `dioxus-server` 0.7.7 hardcodes `<script>` (literal, no attributes)
//! in three SSR call sites (`render_before_body` / INITIALIZE_STREAMING_JS,
//! `render_after_main` / `window.initial_dioxus_hydration_data="…"`, and
//! `replace_placeholder` / `window.dx_hydrate(…)`) and exposes no nonce
//! hook on `ServeConfig`. Until upstream grows one, the body rewrite is
//! the only seam available that lets us drop `'unsafe-inline'`.
//!
//! ## What gets rewritten
//!
//! Only the literal byte sequence `<script>` (open tag with no attributes)
//! is replaced with `<script nonce="…">`. External scripts in the dx-CLI
//! prod bundle use `<script type="module" async src="…"></script>` and
//! stay covered by `script-src 'self'` — the trailing `>` ensures we
//! don't touch them. The dev-mode toast template's inline `<script>` is
//! also rewritten, which is correct.
//!
//! ## What this does NOT protect against
//!
//! If a future code path emits user-supplied HTML containing the literal
//! substring `<script>` (eg. unescaped channel descriptions or welcome
//! messages), the rewriter will nonce that script too — letting it
//! execute despite the CSP. Phase 1 has no such surface; any new surface
//! MUST HTML-escape `<` to `&lt;` before reaching the SSR pipeline. This
//! is the standard rule for nonce-based CSP, but it is load-bearing and
//! worth the sign-off from SecurityEngineer before any user-controlled
//! HTML lands.
//!
//! ## Buffering vs streaming
//!
//! HTML responses are fully buffered before rewrite (cap: 4 MiB). This
//! sacrifices dioxus-server's progressive suspense streaming on HTML
//! routes. Phase 1 has no `use_server_future` boundaries on user-facing
//! pages, so the response is already produced in one chunk and the loss
//! is theoretical. Revisit with a stateful streaming rewriter (carry
//! buffer of `needle.len() - 1` bytes across chunk boundaries) if a
//! future page starts relying on streaming SSR.

use axum::body::Body;
use axum::extract::Request;
use axum::http::header::{self, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;
use rand::RngCore;

/// Per-request CSP nonce. Stored in request extensions so handlers or
/// server functions can read it if they ever need to emit their own
/// inline scripts (eg. JSON-LD structured data). Phase 2 has no such
/// caller — the rewrite middleware handles every dx-emitted script —
/// but exposing the nonce keeps that escape hatch open without forcing
/// callers to plumb their own.
#[derive(Clone, Debug)]
pub struct CspNonce(pub String);

/// Build the full CSP header string with the supplied nonce baked into
/// `script-src`. Every other directive is unchanged from PURA-47; see
/// [`crate::web::headers`] for the per-directive rationale.
pub fn csp_header(nonce: &str) -> String {
    format!(
        "default-src 'self'; \
         img-src 'self' data:; \
         connect-src 'self' ws: wss:; \
         style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; \
         font-src 'self' https://fonts.gstatic.com data:; \
         script-src 'self' 'wasm-unsafe-eval' 'nonce-{nonce}'; \
         object-src 'none'; \
         base-uri 'self'; \
         frame-ancestors 'none'; \
         form-action 'self'"
    )
}

/// 128 bits is the OWASP-recommended floor for CSP nonces. We render as
/// hex (32 chars) rather than base64 to avoid pulling in a base64 dep
/// for a single call site — hex is ASCII-safe in CSP nonce values per
/// RFC 7636/CSP3 (any visible ASCII that survives header parsing).
const NONCE_BYTES: usize = 16;

fn make_nonce() -> String {
    let mut buf = [0u8; NONCE_BYTES];
    rand::thread_rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Cap on the buffered HTML response size. dx hydration data is a base64
/// VDOM blob — typically <10 KB even for the busiest dashboard route.
/// 4 MiB is generous; if a route ever blows through it we want to know.
const MAX_HTML_SIZE: usize = 4 * 1024 * 1024;

/// axum middleware: generates the nonce, rewrites HTML, sets CSP.
pub async fn nonce_csp_middleware(mut req: Request, next: Next) -> Response {
    let nonce = make_nonce();
    req.extensions_mut().insert(CspNonce(nonce.clone()));

    let mut resp = next.run(req).await;

    if response_is_html(&resp) {
        let (parts, body) = resp.into_parts();
        match axum::body::to_bytes(body, MAX_HTML_SIZE).await {
            Ok(bytes) => {
                let new_body = rewrite_inline_scripts(&bytes, &nonce);
                resp = Response::from_parts(parts, Body::from(new_body));
            }
            Err(err) => {
                // Either the body was bigger than MAX_HTML_SIZE or there
                // was a transport error draining it. We can't ship the
                // un-rewritten body — every inline script would then fail
                // the CSP nonce check and the page would render blank.
                // 500 surfaces the issue immediately instead of silently
                // shipping a broken page.
                tracing::error!(%err, "nonce-csp: failed to buffer HTML response for rewrite");
                let mut error_resp = Response::from_parts(parts, Body::from("Internal Server Error"));
                *error_resp.status_mut() = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
                resp = error_resp;
            }
        }
    }

    let value = HeaderValue::from_str(&csp_header(&nonce))
        .expect("CSP header value is ASCII (nonce is hex, directives are static)");
    resp.headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, value);

    resp
}

fn response_is_html(resp: &Response) -> bool {
    resp.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("text/html"))
        .unwrap_or(false)
}

/// Replace every literal `<script>` (the open tag with no attributes) with
/// `<script nonce="{nonce}">`. Byte-level so we don't need a UTF-8 round
/// trip and chunk boundaries can't produce invalid str slices. Operates on
/// the fully buffered body so we don't have to worry about needle splits
/// across boundaries.
fn rewrite_inline_scripts(body: &[u8], nonce: &str) -> Vec<u8> {
    const NEEDLE: &[u8] = b"<script>";
    let replacement = format!("<script nonce=\"{nonce}\">");
    let replacement = replacement.as_bytes();

    let mut out = Vec::with_capacity(body.len() + 32);
    let mut i = 0;
    while i + NEEDLE.len() <= body.len() {
        if &body[i..i + NEEDLE.len()] == NEEDLE {
            out.extend_from_slice(replacement);
            i += NEEDLE.len();
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&body[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request as HttpRequest, StatusCode, header};
    use axum::middleware::from_fn;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn router() -> Router {
        Router::new()
            .route(
                "/html",
                get(|| async {
                    let body = "<!doctype html><html><body>\
                        <script>window.initial_dioxus_hydration_data=\"abc\";</script>\
                        <script type=\"module\" src=\"/assets/main.js\"></script>\
                        <script>window.dx_hydrate([0], \"def\")</script>\
                        </body></html>";
                    axum::response::Response::builder()
                        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                        .body(Body::from(body))
                        .unwrap()
                }),
            )
            .route(
                "/json",
                get(|| async {
                    axum::response::Response::builder()
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"hello":"<script>"}"#))
                        .unwrap()
                }),
            )
            .layer(from_fn(nonce_csp_middleware))
    }

    async fn fetch(path: &str) -> axum::http::Response<Body> {
        let req = HttpRequest::builder()
            .uri(path)
            .body(Body::empty())
            .unwrap();
        router().oneshot(req).await.unwrap()
    }

    fn extract_nonce(csp: &str) -> &str {
        let needle = "'nonce-";
        let start = csp.find(needle).expect("CSP must contain a nonce") + needle.len();
        let end = csp[start..]
            .find('\'')
            .expect("CSP nonce must be quote-terminated")
            + start;
        &csp[start..end]
    }

    /// Extract the source list of a single CSP directive. Returns the
    /// space-separated tokens between the directive name and the next `;`.
    fn directive<'a>(csp: &'a str, name: &str) -> &'a str {
        let segment = csp
            .split(';')
            .find(|d| d.split_whitespace().next() == Some(name))
            .unwrap_or_else(|| panic!("CSP missing {name}: {csp}"));
        segment.trim().strip_prefix(name).unwrap_or(segment).trim()
    }

    #[tokio::test]
    async fn html_response_has_nonce_in_csp_and_in_inline_scripts() {
        let resp = fetch("/html").await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Pull the CSP and the nonce out as owned values before consuming
        // `resp` for the body. Borrows from `resp.headers()` would prevent
        // the subsequent `into_body()` move.
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|v| v.to_str().ok())
            .expect("CSP must be set")
            .to_owned();

        // PURA-48 acceptance #1: 'unsafe-inline' is gone from script-src.
        // (style-src may still carry it — Phase 2 does not touch styles.)
        let script_src = directive(&csp, "script-src");
        assert!(
            !script_src.contains("'unsafe-inline'"),
            "PURA-48 regression: script-src must NOT contain 'unsafe-inline'. Got: {csp}"
        );
        assert!(
            script_src.contains("'wasm-unsafe-eval'"),
            "regression: WASM compile still needs 'wasm-unsafe-eval'. Got: {csp}"
        );
        assert!(
            script_src.contains("'nonce-"),
            "script-src must include a 'nonce-…' source. Got: {csp}"
        );

        let nonce = extract_nonce(&csp).to_owned();
        assert_eq!(nonce.len(), 32, "expected 128-bit hex nonce");
        assert!(
            nonce.chars().all(|c| c.is_ascii_hexdigit()),
            "nonce must be hex"
        );

        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        let body = String::from_utf8(body).unwrap();
        let expected_attr = format!("<script nonce=\"{nonce}\">");
        assert!(
            body.contains(&format!("{expected_attr}window.initial_dioxus_hydration_data")),
            "render_after_main script must carry the nonce. Body: {body}"
        );
        assert!(
            body.contains(&format!("{expected_attr}window.dx_hydrate")),
            "replace_placeholder script must carry the nonce. Body: {body}"
        );
        // External scripts (with src=) must NOT be touched.
        assert!(
            body.contains(r#"<script type="module" src="/assets/main.js"></script>"#),
            "external scripts must remain unchanged. Body: {body}"
        );
    }

    /// Acceptance criterion: two consecutive responses get *different* nonces.
    #[tokio::test]
    async fn consecutive_responses_get_unique_nonces() {
        let r1 = fetch("/html").await;
        let r2 = fetch("/html").await;
        let csp1 = r1
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let csp2 = r2
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let n1 = extract_nonce(&csp1);
        let n2 = extract_nonce(&csp2);
        assert_ne!(n1, n2, "nonces must rotate per request");
    }

    /// Non-HTML responses (eg. /api JSON) still receive the CSP header but
    /// their bodies are NOT rewritten — `<script>` inside JSON is data, not
    /// markup.
    #[tokio::test]
    async fn json_response_carries_csp_but_body_is_unchanged() {
        let resp = fetch("/json").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get(header::CONTENT_SECURITY_POLICY).is_some(),
            "JSON responses still need CSP set"
        );
        let body = resp
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        assert_eq!(body, br#"{"hello":"<script>"}"#);
    }

    #[test]
    fn rewrite_handles_overlapping_and_trailing_partial_needles() {
        // Tag at end-of-buffer (no trailing bytes).
        let out = rewrite_inline_scripts(b"hi <script>", "abc");
        assert_eq!(out, br#"hi <script nonce="abc">"#);

        // Multiple tags, mixed with content that contains `<` but not `<script>`.
        let out = rewrite_inline_scripts(b"a<scrip<script>b<script>c", "x");
        assert_eq!(out, br#"a<scrip<script nonce="x">b<script nonce="x">c"#);

        // No tags at all.
        let out = rewrite_inline_scripts(b"plain text", "x");
        assert_eq!(out, b"plain text");
    }
}
