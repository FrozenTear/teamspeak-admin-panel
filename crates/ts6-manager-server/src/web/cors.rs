//! Spec §6.10 — CORS allowlist driven by `FRONTEND_URL`.
//!
//! Single allowed origin, no wildcards. Credentials are enabled so the
//! cross-origin dev configuration (Dioxus client on `:5173`, axum server on
//! `:3001`) can include the refresh token in the request body — see spec
//! §6.5 for the bearer-in-body refresh flow.

use axum::http::{HeaderName, HeaderValue, Method, header};
use tower_http::cors::{AllowOrigin, CorsLayer};
use url::Url;

/// Build a CORS layer that allow-lists exactly `frontend_url` as the origin.
///
/// `frontend_url` is parsed with `url::Url`; only the scheme + authority is
/// kept (any path is dropped — CORS origins are scheme + host + port). If
/// parsing fails, the function returns a layer that allow-lists nothing —
/// production callers SHOULD have already validated the env var at boot.
pub fn cors_layer(frontend_url: &str) -> CorsLayer {
    let origin_value: Option<HeaderValue> = Url::parse(frontend_url).ok().and_then(|u| {
        let port_part = match (u.port(), u.scheme()) {
            (Some(p), _) => format!(":{p}"),
            // No explicit port — use scheme default; for http(s) leave it blank
            // so origin compares against the browser's default-port-elided form.
            _ => String::new(),
        };
        u.host_str()
            .map(|host| format!("{}://{host}{}", u.scheme(), port_part))
            .and_then(|s| HeaderValue::from_str(&s).ok())
    });

    let allow_origin = match origin_value {
        Some(v) => AllowOrigin::exact(v),
        // Fail closed: no origins allowed. Browser pre-flight will fail; the
        // operator sees an obvious error in the network tab.
        None => AllowOrigin::predicate(|_, _| false),
    };

    CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_credentials(true)
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-requested-with"),
        ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn drive_preflight(layer: CorsLayer, origin: &str) -> axum::http::Response<Body> {
        let app = Router::new()
            .route("/x", get(|| async { "ok" }))
            .layer(layer);
        let req = Request::builder()
            .method(Method::OPTIONS)
            .uri("/x")
            .header(header::ORIGIN, origin)
            .header("access-control-request-method", "POST")
            .body(Body::empty())
            .unwrap();
        app.oneshot(req).await.unwrap()
    }

    #[tokio::test]
    async fn allows_exact_frontend_origin() {
        let layer = cors_layer("http://localhost:5173");
        let resp = drive_preflight(layer, "http://localhost:5173").await;
        let h = resp.headers();
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .map(|v| v.to_str().unwrap()),
            Some("http://localhost:5173")
        );
        assert_eq!(
            h.get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .map(|v| v.to_str().unwrap()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn rejects_other_origin() {
        let layer = cors_layer("http://localhost:5173");
        let resp = drive_preflight(layer, "https://evil.example").await;
        // The browser blocks if ACAO does not echo the requesting origin.
        // tower-http may emit no header, or emit the configured origin —
        // either way, the requesting origin is NOT blessed.
        let acao = resp
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .and_then(|v| v.to_str().ok());
        assert_ne!(
            acao,
            Some("https://evil.example"),
            "must not echo a non-allowed origin back"
        );
        assert_ne!(
            acao,
            Some("*"),
            "wildcard origin would defeat the allowlist"
        );
    }

    #[tokio::test]
    async fn malformed_frontend_url_yields_fail_closed_layer() {
        let layer = cors_layer("not a url");
        let resp = drive_preflight(layer, "http://localhost:5173").await;
        assert!(
            resp.headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none()
        );
        // Drain body to silence any unused-warning.
        let _ = resp.into_body().collect().await;
    }
}
