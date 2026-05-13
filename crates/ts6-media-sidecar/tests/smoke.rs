//! WS-1 smoke — boot the sidecar lib in-process on ephemeral ports, hit the
//! control-plane HTTP surface, assert JSON shape + that the QUIC listener
//! actually bound a UDP socket. No real subscribe flow yet — that's WS-2.
//!
//! We exercise the lib API rather than spawning the binary because the lib
//! IS what the binary runs (`main.rs` is a thin clap → SidecarConfig
//! adapter), and an in-process boot is deterministic about ephemeral
//! port discovery.

use std::net::SocketAddr;

use serde_json::Value;
use ts6_media_sidecar::{Sidecar, SidecarConfig, TransportConfig};

#[tokio::test]
async fn smoke_health_stats_certificate_endpoints() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,ts6_media_sidecar=debug")
        .with_test_writer()
        .try_init();

    let config = SidecarConfig {
        transport: TransportConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            tls_cert: vec![],
            tls_key: vec![],
            tls_generate: vec!["localhost".to_string()],
        },
        http_listen: "127.0.0.1:0".parse().unwrap(),
    };

    let sidecar = Sidecar::start(config).await.expect("sidecar boots");

    // QUIC listener bound a real UDP socket.
    assert_ne!(
        sidecar.transport_addr.port(),
        0,
        "transport must bind a real port"
    );

    // SHA256 fingerprint is 64 hex chars (32 bytes hex-encoded).
    assert_eq!(
        sidecar.fingerprint.len(),
        64,
        "fingerprint must be hex-encoded SHA256, got '{}'",
        sidecar.fingerprint,
    );
    assert!(
        sidecar.fingerprint.chars().all(|c| c.is_ascii_hexdigit()),
        "fingerprint must be hex, got '{}'",
        sidecar.fingerprint,
    );

    let http_addr = sidecar.http_addr;
    let expected_fp = sidecar.fingerprint.clone();

    // /health
    let health: Value = get_json(http_addr, "/health").await;
    assert_eq!(health["status"], "ok");
    assert!(health["uptime_s"].is_number());
    assert_eq!(health["sessions"], 0);
    assert_eq!(health["broadcasts"], 0);

    // /stats
    let stats: Value = get_json(http_addr, "/stats").await;
    assert_eq!(stats["active_sessions"], 0);
    assert_eq!(stats["lifetime_sessions"], 0);
    assert_eq!(stats["registered_broadcasts"].as_array().unwrap().len(), 0);

    // Register a broadcast and re-check that /stats reflects it.
    let _producer = sidecar
        .origin
        .register_broadcast("smoke/test")
        .await
        .expect("register smoke broadcast");
    let stats_after: Value = get_json(http_addr, "/stats").await;
    let names: Vec<String> = stats_after["registered_broadcasts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["smoke/test".to_string()]);

    // /certificate.sha256 — text/plain, 64 hex chars matching `tls_info()`.
    let resp = reqwest::get(format!("http://{}/certificate.sha256", http_addr))
        .await
        .expect("certificate.sha256 GET");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("text/plain"),
        "expected text/plain content-type, got '{ct}'"
    );
    let body = resp.text().await.unwrap();
    assert_eq!(body.trim(), expected_fp);

    // Unknown route → 404.
    let resp = reqwest::get(format!("http://{}/source", http_addr))
        .await
        .expect("404 GET");
    assert_eq!(resp.status(), 404, "unknown routes must 404");

    sidecar.shutdown();
}

async fn get_json(http_addr: SocketAddr, path: &str) -> Value {
    let url = format!("http://{}{}", http_addr, path);
    let resp = reqwest::get(&url).await.unwrap_or_else(|err| {
        panic!("GET {url}: {err}");
    });
    assert_eq!(resp.status(), 200, "{} returned {}", path, resp.status());
    resp.json().await.expect("valid json")
}
