//! In-process HTTP liveness probe for the Contabo music unit.
//!
//! `Containerfile.music` does not install curl/wget. Kube probes and
//! OCI HEALTHCHECK invoke `ts6-manager-music --healthcheck-url` so
//! Podman 5.6 `httpGet` → in-container curl never runs (that path
//! restart-loops the sidecar). Same shape as `ts6-media-sidecar`.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Default loopback control plane. Must stay in lock-step with
/// `ts6-manager-music --listen`, `EXPOSE 3002`, and deploy/kube.
pub const DEFAULT_URL: &str = "http://127.0.0.1:3002/health";

/// Shorter than kube / OCI HealthTimeout=5s.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

pub async fn probe(url: &str) -> Result<()> {
    probe_with_timeout(url, DEFAULT_TIMEOUT).await
}

pub async fn probe_with_timeout(url: &str, timeout: Duration) -> Result<()> {
    let parsed = url::Url::parse(url).with_context(|| format!("invalid healthcheck URL: {url}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => bail!("healthcheck URL scheme must be http or https, got {other}"),
    }

    let client = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build healthcheck HTTP client")?;

    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    let status = resp.status();
    if !status.is_success() {
        bail!("GET {url} returned {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tokio::net::TcpListener;

    async fn serve_status(code: StatusCode) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/health", get(move || async move { code }));
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}/health")
    }

    #[tokio::test]
    async fn probe_ok_on_200() {
        let url = serve_status(StatusCode::OK).await;
        probe(&url).await.expect("HTTP 200 must succeed");
    }

    #[tokio::test]
    async fn probe_err_on_503() {
        let url = serve_status(StatusCode::SERVICE_UNAVAILABLE).await;
        probe(&url).await.expect_err("HTTP 503 must fail");
    }

    #[tokio::test]
    async fn probe_rejects_non_http_scheme() {
        let err = probe("file:///etc/passwd")
            .await
            .expect_err("file:// must fail");
        assert!(format!("{err:#}").contains("scheme"));
    }
}
