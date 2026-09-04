//! In-process HTTP liveness probe for Quadlet / OCI HEALTHCHECK.
//!
//! `Containerfile.sidecar` does not install `curl` or `wget` (unlike
//! fullstack). Operators — and the image's own `HEALTHCHECK` / Quadlet
//! `HealthCmd` — invoke the sidecar binary with `--healthcheck-url`
//! so a GET of the control-plane `/health` needs no extra packages.
//!
//! This is a localhost operator probe, not the `POST /source` SSRF
//! surface (PURA-150). The only guards here are: http(s) scheme,
//! no redirects, and a short timeout so a hung control plane fails
//! the probe before Quadlet's `HealthTimeout` SIGKILLs it.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Default `--http-listen` control plane + `/health` path. Keep in
/// lock-step with `Args::http_listen` in `main.rs`, `EXPOSE 7080` in
/// `Containerfile.sidecar`, and `deploy/kube` / Quadlet examples.
/// Historical spec default `9800` is not used.
pub const DEFAULT_URL: &str = "http://127.0.0.1:7080/health";

/// Shorter than Quadlet / OCI `HealthTimeout=5s` so the process
/// exits with a clean non-zero status instead of being SIGKILL'd.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// GET `url` and return `Ok(())` on HTTP 2xx. Any transport error,
/// non-http(s) scheme, or non-2xx status is `Err`.
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
        let err = probe(&url).await.expect_err("HTTP 503 must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("503"), "{msg}");
    }

    #[tokio::test]
    async fn probe_err_on_404() {
        let url = serve_status(StatusCode::OK).await;
        let missing = url.replacen("/health", "/nope", 1);
        let err = probe(&missing).await.expect_err("HTTP 404 must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("404"), "{msg}");
    }

    #[tokio::test]
    async fn probe_err_on_connection_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        probe(&format!("http://{addr}/health"))
            .await
            .expect_err("connection refused must fail");
    }

    #[tokio::test]
    async fn probe_rejects_non_http_scheme() {
        let err = probe("file:///etc/passwd")
            .await
            .expect_err("file:// must fail");
        let msg = format!("{err:#}");
        assert!(msg.contains("scheme"), "{msg}");
    }

    #[tokio::test]
    async fn probe_rejects_unparseable_url() {
        probe("not a url").await.expect_err("garbage URL must fail");
    }

    #[tokio::test]
    async fn probe_times_out_on_hung_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(60)).await;
        });
        let err = probe_with_timeout(&format!("http://{addr}/health"), Duration::from_millis(200))
            .await
            .expect_err("hung peer must time out");
        let msg = format!("{err:#}").to_ascii_lowercase();
        assert!(
            msg.contains("timed out") || msg.contains("timeout"),
            "expected a timeout error, got: {msg}"
        );
    }
}
