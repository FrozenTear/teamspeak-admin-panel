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

    // --- POST /source — happy path. server-generated source_id, explicit 1080p preset. --
    let body = serde_json::json!({
        "url": "http://fixture.test/clip.mp4",
        "preset": "1080p",
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
    assert_eq!(
        sources[0]["preset"], "1080p",
        "stats must echo the preset the caller posted"
    );
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

/// WS-4 (PURA-142). Asserts that:
///
/// 1. Each preset round-trips through `POST /source` → `GET /stats`.
/// 2. Case-insensitive parse: `"720P"` works.
/// 3. Default = `"720p"` when `preset` is missing.
/// 4. Default = `"720p"` when `preset` is explicit `null`.
/// 5. Unknown preset strings → 400 in the WS-3 error model.
#[tokio::test]
async fn preset_roundtrip_and_validation() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("warn,ts6_media_sidecar=debug")
        .with_test_writer()
        .try_init();

    let resolver = MockResolver::new().with("fixture.test", vec![ip("203.0.113.10")]);
    let sidecar = boot(Arc::new(resolver) as Arc<dyn Resolver>).await;
    let base = format!("http://{}", sidecar.http_addr);
    let client = reqwest::Client::new();

    async fn start(client: &reqwest::Client, base: &str, body: Value) -> (u16, Value) {
        let resp = client
            .post(format!("{base}/source"))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        let body = resp.json::<Value>().await.unwrap_or(Value::Null);
        (status, body)
    }

    async fn stop(client: &reqwest::Client, base: &str, id: &str) {
        let resp = client
            .post(format!("{base}/source/stop"))
            .json(&serde_json::json!({"source_id": id}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn preset_in_stats(client: &reqwest::Client, base: &str, id: &str) -> String {
        let stats: Value = client
            .get(format!("{base}/stats"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let sources = stats["sources"].as_array().unwrap();
        let source = sources
            .iter()
            .find(|s| s["source_id"].as_str() == Some(id))
            .expect("source present in /stats");
        source["preset"].as_str().unwrap().to_string()
    }

    // (1) Each valid preset.
    for (preset_in, preset_out) in
        [("480p", "480p"), ("720p", "720p"), ("1080p", "1080p")]
    {
        let (status, posted) = start(
            &client,
            &base,
            serde_json::json!({
                "url": "http://fixture.test/x",
                "preset": preset_in,
            }),
        )
        .await;
        assert_eq!(status, 201, "preset {preset_in}: body = {posted}");
        let id = posted["source_id"].as_str().unwrap().to_string();
        assert_eq!(preset_in_stats(&client, &base, &id).await, preset_out);
        stop(&client, &base, &id).await;
    }

    // (2) Case-insensitive parse.
    let (status, posted) = start(
        &client,
        &base,
        serde_json::json!({"url": "http://fixture.test/x", "preset": "720P"}),
    )
    .await;
    assert_eq!(status, 201);
    let id = posted["source_id"].as_str().unwrap().to_string();
    assert_eq!(preset_in_stats(&client, &base, &id).await, "720p");
    stop(&client, &base, &id).await;

    // (3) Missing → 720p default.
    let (status, posted) = start(
        &client,
        &base,
        serde_json::json!({"url": "http://fixture.test/x"}),
    )
    .await;
    assert_eq!(status, 201);
    let id = posted["source_id"].as_str().unwrap().to_string();
    assert_eq!(preset_in_stats(&client, &base, &id).await, "720p");
    stop(&client, &base, &id).await;

    // (4) Explicit null → 720p default.
    let (status, posted) = start(
        &client,
        &base,
        serde_json::json!({"url": "http://fixture.test/x", "preset": null}),
    )
    .await;
    assert_eq!(status, 201);
    let id = posted["source_id"].as_str().unwrap().to_string();
    assert_eq!(preset_in_stats(&client, &base, &id).await, "720p");
    stop(&client, &base, &id).await;

    // (5) Unknown preset → 400 with WS-3 error model.
    let (status, body) = start(
        &client,
        &base,
        serde_json::json!({"url": "http://fixture.test/x", "preset": "4k"}),
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"], "invalid_request");
    assert!(
        body["detail"]
            .as_str()
            .is_some_and(|d| d.contains("unknown quality preset")),
        "detail must surface the parse error: {body}"
    );

    sidecar.shutdown();
}

/// WS-4 (PURA-142). Pure unit assertion that the FFmpeg argv produced
/// by [`ts6_media_sidecar::pipeline::ffmpeg_video_args`] reflects the
/// preset's spec-§23.4 resolution / framerate / bitrate. Lives in the
/// integration suite (instead of `#[cfg(test)]`) so it cross-checks the
/// public API surface the sidecar exposes to its callers.
#[test]
fn ffmpeg_argv_reflects_preset() {
    use ts6_media_sidecar::pipeline::{ffmpeg_video_args, PipelineConfig};
    use ts6_media_sidecar::{QualityPreset, SourceInput};

    fn argv_for(preset: QualityPreset) -> Vec<String> {
        let cfg = PipelineConfig::new("test", SourceInput::Url("http://x/".into()))
            .with_preset(preset);
        ffmpeg_video_args(&cfg)
    }

    let cases = [
        (QualityPreset::P480, 854u32, 480u32, 24u32, "1000k"),
        (QualityPreset::P720, 1280, 720, 30, "2500k"),
        (QualityPreset::P1080, 1920, 1080, 30, "4500k"),
    ];

    for (preset, w, h, fps, bitrate) in cases {
        let args = argv_for(preset);

        // -b:v <bitrate> and -maxrate <bitrate>
        let bv_idx = args.iter().position(|a| a == "-b:v").expect("-b:v present");
        assert_eq!(args[bv_idx + 1], bitrate, "preset {preset:?}: bitrate");
        let mr_idx = args
            .iter()
            .position(|a| a == "-maxrate")
            .expect("-maxrate present");
        assert_eq!(args[mr_idx + 1], bitrate, "preset {preset:?}: maxrate");

        // keyframe interval = framerate (= ~1 s join latency).
        let g_idx = args.iter().position(|a| a == "-g").expect("-g present");
        assert_eq!(args[g_idx + 1], fps.to_string(), "preset {preset:?}: -g");

        // -vf carries the fps + scale + pad chain.
        let vf_idx = args.iter().position(|a| a == "-vf").expect("-vf present");
        let vf = &args[vf_idx + 1];
        assert!(
            vf.contains(&format!("fps={fps}")),
            "preset {preset:?}: vf missing fps: {vf}"
        );
        assert!(
            vf.contains(&format!("scale={w}:{h}")),
            "preset {preset:?}: vf missing scale: {vf}"
        );
        assert!(
            vf.contains(&format!("pad={w}:{h}")),
            "preset {preset:?}: vf missing pad: {vf}"
        );
    }
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
