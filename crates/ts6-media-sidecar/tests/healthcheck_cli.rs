//! CLI wiring for `--healthcheck-url` (Quadlet / OCI HEALTHCHECK).
//!
//! Spawns the `ts6-media-sidecar` binary against a tiny axum listener
//! (or a closed port) so we assert process exit codes without booting
//! the QUIC/TLS sidecar. The probe function itself is unit-tested in
//! `src/healthcheck.rs`; `tests/smoke.rs` hits a live sidecar `/health`.

use axum::Router;
use axum::routing::get;
use tokio::net::TcpListener;

fn bin() -> Option<&'static str> {
    // `cargo check --all-targets` / clippy do not set CARGO_BIN_EXE_*.
    // `cargo test` does. Skip the spawn when the path is absent.
    option_env!("CARGO_BIN_EXE_ts6_media_sidecar")
}

#[tokio::test]
async fn cli_exits_zero_on_http_200() {
    let Some(bin) = bin() else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route("/health", get(|| async { "ok" }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let status = std::process::Command::new(bin)
        .arg("--healthcheck-url")
        .arg(format!("http://{addr}/health"))
        .status()
        .expect("spawn ts6-media-sidecar --healthcheck-url");
    assert!(
        status.success(),
        "probe against HTTP 200 must exit 0, got {status}"
    );
}

#[tokio::test]
async fn cli_exits_nonzero_when_nothing_listens() {
    let Some(bin) = bin() else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let status = std::process::Command::new(bin)
        .arg("--healthcheck-url")
        .arg(format!("http://{addr}/health"))
        .status()
        .expect("spawn ts6-media-sidecar --healthcheck-url");
    assert!(
        !status.success(),
        "probe against a closed port must exit non-zero, got {status}"
    );
}

#[tokio::test]
async fn cli_exits_nonzero_on_http_503() {
    let Some(bin) = bin() else {
        return;
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/health",
        get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let status = std::process::Command::new(bin)
        .arg("--healthcheck-url")
        .arg(format!("http://{addr}/health"))
        .status()
        .expect("spawn ts6-media-sidecar --healthcheck-url");
    assert!(
        !status.success(),
        "probe against HTTP 503 must exit non-zero, got {status}"
    );
}
