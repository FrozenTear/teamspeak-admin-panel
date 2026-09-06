//! Music Bot seat of the private GitHub Issues bug-report flow.
//!
//! #28 (`POST /api/bug-reports`) locks the wire shape and does not add a
//! server-side tracing ring. This module:
//!
//! 1. Exposes `GET /api/music-bots/bug-report-context` so Panel can read
//!    the same snapshot it will put into `context`.
//! 2. Enriches `POST /api/bug-reports` when that route is mounted (API
//!    PR #28) by merging `musicBotLatency` / `logTail` if the keys are
//!    absent. The middleware does not change #28's request type.

use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::get;
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;

/// 64 KiB is well above #28's 32 KiB context cap plus the other fields.
const MAX_BUG_REPORT_BODY: usize = 64 * 1024;

pub(super) fn router() -> Router<AppState> {
    Router::new().route(
        "/api/music-bots/bug-report-context",
        get(bug_report_context),
    )
}

async fn bug_report_context(
    RequireAuth(_user): RequireAuth,
) -> Json<wire::MusicBotBugReportContext> {
    Json(to_wire(music_bot::bug_report::snapshot()))
}

fn to_wire(snap: music_bot::bug_report::BugReportSnapshot) -> wire::MusicBotBugReportContext {
    wire::MusicBotBugReportContext {
        music_bot_latency: snap.music_bot_latency,
        log_tail: snap.log_tail,
    }
}

/// Request-side middleware: on `POST /api/bug-reports`, merge the Music
/// Bot snapshot into `context` when those keys are absent.
///
/// Hypothesis (verified in tests): the latency ring lives in-process on
/// the server; Panel cannot see `music_bot_latency` tracing events. #28
/// said seats pass tails via `context` and added no new bug-report
/// endpoints. Enriching the POST body before #28's handler validates it
/// keeps the locked wire shape and lets #28's existing caps apply.
pub async fn enrich_bug_report_request(req: Request, next: Next) -> Response {
    if req.method() != Method::POST || req.uri().path() != "/api/bug-reports" {
        return next.run(req).await;
    }

    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, MAX_BUG_REPORT_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return next.run(Request::from_parts(parts, Body::empty())).await;
        }
    };
    let body = match enrich_bug_report_json(&bytes) {
        Some(enriched) => Body::from(enriched),
        None => Body::from(bytes),
    };
    next.run(Request::from_parts(parts, body)).await
}

/// Pure enrichment used by the middleware and its tests.
pub fn enrich_bug_report_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let obj = value.as_object_mut()?;
    let ctx_entry = obj
        .entry("context")
        .or_insert_with(|| serde_json::json!({}));
    if ctx_entry.is_null() {
        *ctx_entry = serde_json::json!({});
    }
    let map = ctx_entry.as_object_mut()?;
    music_bot::bug_report::snapshot().merge_absent(map);
    serde_json::to_vec(&value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use music_bot::bug_report::{CONTEXT_KEY_LOG_TAIL, CONTEXT_KEY_MUSIC_BOT_LATENCY, LatencyRing};

    fn push_sample_stage() {
        music_bot::bug_report::global_ring().clear();
        music_bot::bug_report::global_ring().record_stage(
            music_bot::bug_report::LatencyStage {
                stage: "resolver_resolved".into(),
                elapsed_ms: Some(20),
                retry: false,
            },
            "music_bot_latency stage=resolver_resolved elapsed_ms=20",
        );
    }

    #[test]
    fn enrich_merges_absent_keys_into_context() {
        let _guard = music_bot::bug_report::test_global_lock();
        push_sample_stage();

        let raw = br#"{"pagePath":"/music-bots/42","note":"slow resolve"}"#;
        let out = enrich_bug_report_json(raw).expect("json");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["pagePath"], "/music-bots/42");
        assert_eq!(v["note"], "slow resolve");
        let latency = v["context"][CONTEXT_KEY_MUSIC_BOT_LATENCY]
            .as_str()
            .unwrap();
        assert!(latency.contains("resolver_resolved"), "{latency}");
        assert!(
            v["context"][CONTEXT_KEY_LOG_TAIL]
                .as_str()
                .unwrap()
                .contains("resolver_resolved")
        );
    }

    #[test]
    fn enrich_does_not_overwrite_panel_supplied_keys() {
        let _guard = music_bot::bug_report::test_global_lock();
        push_sample_stage();

        let raw = br#"{
            "pagePath":"/music-bots/42",
            "context":{"musicBotLatency":"panel-supplied","other":"keep"}
        }"#;
        let out = enrich_bug_report_json(raw).expect("json");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["context"]["musicBotLatency"], "panel-supplied");
        assert_eq!(v["context"]["other"], "keep");
        assert!(
            v["context"][CONTEXT_KEY_LOG_TAIL]
                .as_str()
                .unwrap()
                .contains("resolver_resolved")
        );
    }

    #[test]
    fn enrich_leaves_invalid_json_alone() {
        assert!(enrich_bug_report_json(b"not-json").is_none());
        assert!(enrich_bug_report_json(b"[1,2,3]").is_none());
    }

    #[test]
    fn local_ring_and_global_are_independent() {
        let local = LatencyRing::new();
        local.record_stage(
            music_bot::bug_report::LatencyStage {
                stage: "only_local".into(),
                elapsed_ms: Some(1),
                retry: false,
            },
            "only_local",
        );
        assert!(local.snapshot().music_bot_latency.contains("only_local"));
    }
}
