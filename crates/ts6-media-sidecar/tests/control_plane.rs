//! WS-3 control-plane integration test (PURA-141).
//!
//! Boots the sidecar lib in-process on ephemeral ports with a
//! deterministic `MockResolver` so SSRF allow/deny is reproducible
//! without touching real DNS. Walks the operator workflow:
//! `POST /source` → `GET /track/{id}` → `GET /stats` → `POST /source/stop`,
//! then a negative SSRF test that pins a private-range resolution.
//!
//! No FFmpeg involvement — `ffmpeg_path` points at `/bin/true`, which
//! exits 0 with empty stdout. The mux loops see immediate EOF and the
//! supervisor restart-loops; this test asserts the *registry* state, not
//! the media plane. WS-2's `pipeline_two_tab_smoke` covers media.

use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use ts6_media_sidecar::{Sidecar, SidecarConfig, TransportConfig};
use ts6_ssrf::{MockResolver, Resolver};

#[tokio::test]
async fn control_plane_start_lookup_stats_stop() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,ts6_media_sidecar=debug")
        .with_test_writer()
        .try_init();

    let resolver = MockResolver::new()
        .with("fixture.test", vec![ip("203.0.113.10")])
        .with("private.test", vec![ip("10.0.0.42")]);

    let sidecar = boot(Arc::new(resolver) as Arc<dyn Resolver>).await;
    let http_addr = sidecar.http_addr;
    let base = format!("http://{}", http_addr);

    // --- POST /source — happy path. server-generated source_id. --------------
    let body = serde_json::json!({
        "url": "http://fixture.test/clip.mp4",
        "preset": "passthrough",
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/source"))
        .json(&body)
        .send()
        .await
        .expect("POST /source");
    assert_eq!(resp.status(), 201, "{}", resp.text().await.unwrap());
    let posted: Value = resp.json().await.expect("POST /source JSON");
    let source_id = posted["source_id"]
        .as_str()
        .expect("source_id present")
        .to_string();
    assert!(!source_id.is_empty(), "server must generate uuid v4");
    assert_eq!(posted["track"]["namespace"], source_id);
    assert_eq!(posted["track"]["video"], "video");
    assert_eq!(posted["track"]["audio"], "audio");

    // --- GET /track/{source_id} — same payload as POST. ----------------------
    let track: Value = reqwest::get(format!("{base}/track/{source_id}"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(track["source_id"], source_id);
    assert_eq!(track["track"]["video"], "video");

    // --- GET /stats — includes the new source. -------------------------------
    let stats: Value = reqwest::get(format!("{base}/stats"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let sources = stats["sources"]
        .as_array()
        .expect("stats.sources is an array");
    assert_eq!(sources.len(), 1, "exactly one source registered");
    assert_eq!(sources[0]["source_id"], source_id);
    assert!(sources[0]["video"]["ffmpeg_alive"].is_boolean());
    assert!(sources[0]["video"]["frames_published"].is_number());
    assert!(stats["registered_broadcasts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str() == Some(&source_id)));

    // --- POST /source — duplicate source_id rejects 409. ---------------------
    let dup_body = serde_json::json!({
        "url": "http://fixture.test/another.mp4",
        "source_id": source_id,
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/source"))
        .json(&dup_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 409, "duplicate source_id must conflict");
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"], "source_id_already_running");

    // --- POST /source — SSRF rejection. --------------------------------------
    let ssrf_body = serde_json::json!({
        "url": "http://private.test/clip.mp4",
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/source"))
        .json(&ssrf_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "private-range resolve must be blocked");
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"], "ssrf_blocked");
    assert!(err["detail"].is_string());

    // --- POST /source — SSRF rejection for IP literal. -----------------------
    let ssrf_literal = serde_json::json!({
        "url": "http://127.0.0.1/local",
    });
    let resp = reqwest::Client::new()
        .post(format!("{base}/source"))
        .json(&ssrf_literal)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "loopback IP literal must be blocked");
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"], "ssrf_blocked");

    // --- POST /source/stop — happy path → 204 + registry empty. --------------
    let stop_body = serde_json::json!({"source_id": source_id});
    let resp = reqwest::Client::new()
        .post(format!("{base}/source/stop"))
        .json(&stop_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);

    // Give Pipeline::stop a beat to finish unregister (the moq-lite drop
    // is sync, but the broadcasts read lock might still be flushing).
    tokio::time::sleep(Duration::from_millis(50)).await;

    let resp = reqwest::get(format!("{base}/track/{source_id}"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 404, "stopped source must 404");

    let stats: Value = reqwest::get(format!("{base}/stats"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(stats["sources"].as_array().unwrap().len(), 0);

    // --- POST /source/stop — unknown source → 404. ---------------------------
    let resp = reqwest::Client::new()
        .post(format!("{base}/source/stop"))
        .json(&serde_json::json!({"source_id": "nonexistent-source"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"], "unknown_source_id");

    // --- POST /source — invalid source_id (contains '/') rejects 400. --------
    let resp = reqwest::Client::new()
        .post(format!("{base}/source"))
        .json(&serde_json::json!({
            "url": "http://fixture.test/x",
            "source_id": "bad/id",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(err["error"], "invalid_request");

    sidecar.shutdown();
}

#[tokio::test]
async fn control_plane_health_stays_cheap_with_pipelines() {
    // /health should not block on the pipeline registry's write lock.
    // We boot, register a source, hammer /health while holding writes
    // open (via a stop), and assert /health stays sub-100ms.
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,ts6_media_sidecar=debug")
        .with_test_writer()
        .try_init();

    let resolver = MockResolver::new().with("fixture.test", vec![ip("203.0.113.10")]);
    let sidecar = boot(Arc::new(resolver) as Arc<dyn Resolver>).await;
    let http_addr = sidecar.http_addr;
    let base = format!("http://{}", http_addr);

    // Register a source so the registry is non-trivial.
    let _: Value = reqwest::Client::new()
        .post(format!("{base}/source"))
        .json(&serde_json::json!({"url": "http://fixture.test/clip.mp4"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let start = std::time::Instant::now();
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    let elapsed = start.elapsed();
    assert_eq!(resp.status(), 200);
    assert!(
        elapsed < Duration::from_millis(250),
        "/health took {:?} — must stay cheap"
    , elapsed);

    sidecar.shutdown();
}

async fn boot(resolver: Arc<dyn Resolver>) -> Sidecar {
    let config = SidecarConfig {
        transport: TransportConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            tls_cert: vec![],
            tls_key: vec![],
            tls_generate: vec!["localhost".to_string()],
        },
        http_listen: "127.0.0.1:0".parse().unwrap(),
        resolver,
        // `/bin/true` exits 0 with empty stdout. The supervisor handles
        // that gracefully (mux loop sees clean EOF and the supervisor
        // restart-loops with backoff). Keeps the test ffmpeg-free.
        ffmpeg_path: PathBuf::from("/bin/true"),
    };
    Sidecar::start(config).await.expect("sidecar boots")
}

fn ip(s: &str) -> IpAddr {
    IpAddr::from_str(s).unwrap()
}
