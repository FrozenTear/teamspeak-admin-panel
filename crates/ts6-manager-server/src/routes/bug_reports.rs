//! `POST /api/bug-reports` — operator bug report → private GitHub Issue.
//!
//! Auth: [`RequireAuth`] (any signed-in operator). Admin is not required.
//! Misconfigured sink (token/repo unset) → 503 with the shared
//! `{ "error": "..." }` envelope so Contabo can enable the feature later
//! without a boot crash.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::Utc;
use ts6_manager_shared::auth::ErrorResponse;
use ts6_manager_shared::bug_reports::{
    CreateBugReportRequest, CreateBugReportResponse, error_strings,
};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::bug_reports::{IssueDraft, Reporter, SinkError, build_issue};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/bug-reports", post(create_bug_report))
}

async fn create_bug_report(
    RequireAuth(user): RequireAuth,
    State(state): State<AppState>,
    Json(mut body): Json<CreateBugReportRequest>,
) -> Response {
    // Voice seat bag — last-known wire marks + short in-process tail.
    // Does not overwrite Panel / Music Bot keys already on `context`.
    let dest = body.context.get_or_insert_with(Default::default);
    music_bot::merge_voice_bug_context(dest);
    if dest.is_empty() {
        body.context = None;
    }

    let report = match body.validate() {
        Ok(v) => v,
        Err(msg) => return err(StatusCode::BAD_REQUEST, msg),
    };

    if !state.bug_reports.is_configured() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            error_strings::SINK_UNCONFIGURED,
        );
    }

    let draft: IssueDraft = build_issue(
        &report,
        &Reporter {
            id: user.id,
            username: user.username.clone(),
            display_name: user.display_name.clone(),
        },
        Utc::now(),
    );

    match state.bug_reports.create_issue(draft).await {
        Ok(created) => (
            StatusCode::CREATED,
            Json(CreateBugReportResponse {
                issue_url: created.html_url,
                issue_number: created.number,
            }),
        )
            .into_response(),
        Err(SinkError::Unconfigured) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            error_strings::SINK_UNCONFIGURED,
        ),
        Err(SinkError::Upstream) => err(StatusCode::BAD_GATEWAY, error_strings::SINK_FAILED),
    }
}

fn err(status: StatusCode, message: &str) -> Response {
    (status, Json(ErrorResponse::new(message))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{jwt, password};
    use crate::bug_reports::RecordingSink;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::users;
    use axum::body::Body;
    use axum::http::{HeaderValue, Method, Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    async fn fresh_state() -> AppState {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        crate::crypto::init("test-seed-bug-reports");
        let control = crate::control::ControlBackendPool::new(false, db.clone());
        AppState {
            db,
            jwt_secret: Arc::new(b"test-secret-bytes-please-32-or-more".to_vec()),
            jwt_access_expiry: Duration::from_secs(900),
            jwt_refresh_expiry: Duration::from_secs(7 * 24 * 3600),
            setup_lock: Arc::new(tokio::sync::Mutex::new(())),
            webquery: crate::webquery::WebQueryPool::new(false),
            control,
            ws_hub: crate::ws::Hub::new(),
            widget_cache: crate::widgets::WidgetCache::new(),
            music_bots: crate::music_bots::MusicBotService::default_for_tests(),
            sidecar: None,
            ssrf_resolver: Arc::new(ts6_ssrf::MockResolver::new()),
            moq_public_url: None,
            yt_cookie: Arc::new(std::sync::RwLock::new(None)),
            yt_api_key: Arc::new(std::sync::RwLock::new(None)),
            data_dir: std::path::PathBuf::from("./data"),
            trusted_proxy_hops: 0,
            bug_reports: crate::bug_reports::unconfigured_sink(),
        }
    }

    fn app(state: AppState) -> Router {
        Router::new().merge(router()).with_state(state)
    }

    fn json_body<T: serde::Serialize>(value: &T) -> Body {
        Body::from(serde_json::to_vec(value).unwrap())
    }

    async fn read_json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "expected JSON, got {:?}: {e}",
                String::from_utf8_lossy(&bytes)
            )
        })
    }

    async fn seed_user(state: &AppState, username: &str, role: &str) -> i64 {
        let pw = "Hunter2!ok".to_string();
        let hash = tokio::task::spawn_blocking(move || password::hash_new(&pw))
            .await
            .unwrap()
            .unwrap();
        users::insert(
            &state.db,
            users::NewUser {
                username: username.into(),
                passwordHash: hash,
                displayName: username.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    fn mint_token(state: &AppState, id: i64, username: &str, role: &str) -> String {
        jwt::mint_access(
            id,
            username,
            role,
            state.jwt_access_expiry,
            &state.jwt_secret,
        )
        .unwrap()
    }

    fn auth_header(token: &str) -> HeaderValue {
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
    }

    fn sample_body() -> CreateBugReportRequest {
        CreateBugReportRequest {
            page_path: "/music-bots/42".into(),
            server_id: Some(1),
            note: Some("optional operator text".into()),
            toasts: Some(vec!["Failed to fetch".into()]),
            ws_errors: Some(vec!["SSE closed".into()]),
            release: Some("v1.6.9".into()),
            context: Some(serde_json::Map::from_iter([(
                "musicBotLatency".into(),
                serde_json::json!("resolve=20s retry=1"),
            )])),
        }
    }

    async fn post_report(
        app: Router,
        token: Option<&str>,
        body: &CreateBugReportRequest,
    ) -> axum::http::Response<Body> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/api/bug-reports")
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header("authorization", auth_header(token));
        }
        app.oneshot(builder.body(json_body(body)).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn unauthenticated_is_401() {
        let state = fresh_state().await;
        let resp = post_report(app(state), None, &sample_body()).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let envelope: ErrorResponse = read_json(resp).await;
        assert_eq!(envelope.error, "No token provided");
    }

    #[tokio::test]
    async fn empty_page_path_is_400() {
        let state = fresh_state().await;
        let uid = seed_user(&state, "viewer1", "viewer").await;
        let token = mint_token(&state, uid, "viewer1", "viewer");
        let mut body = sample_body();
        body.page_path = "   ".into();
        let resp = post_report(app(state), Some(&token), &body).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let envelope: ErrorResponse = read_json(resp).await;
        assert_eq!(envelope.error, error_strings::PAGE_PATH_REQUIRED);
    }

    #[tokio::test]
    async fn missing_page_path_is_400() {
        let state = fresh_state().await;
        let uid = seed_user(&state, "viewer2", "viewer").await;
        let token = mint_token(&state, uid, "viewer2", "viewer");
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/bug-reports")
            .header("authorization", auth_header(&token))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"note":"no path"}"#))
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let envelope: ErrorResponse = read_json(resp).await;
        assert_eq!(envelope.error, error_strings::PAGE_PATH_REQUIRED);
    }

    #[tokio::test]
    async fn unconfigured_sink_is_503() {
        let state = fresh_state().await;
        let uid = seed_user(&state, "viewer3", "viewer").await;
        let token = mint_token(&state, uid, "viewer3", "viewer");
        let resp = post_report(app(state), Some(&token), &sample_body()).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let envelope: ErrorResponse = read_json(resp).await;
        assert_eq!(envelope.error, error_strings::SINK_UNCONFIGURED);
    }

    #[tokio::test]
    async fn happy_path_viewer_creates_issue_via_mock() {
        let mut state = fresh_state().await;
        let recorder = RecordingSink::new(
            "https://github.com/FrozenTear/teamspeak-admin-panel/issues/99",
            99,
        );
        let recorded = recorder.issues.clone();
        state.bug_reports = recorder.handle();

        let uid = seed_user(&state, "viewer4", "viewer").await;
        let token = mint_token(&state, uid, "viewer4", "viewer");
        let resp = post_report(app(state), Some(&token), &sample_body()).await;
        assert_eq!(resp.status(), StatusCode::CREATED);
        let body: CreateBugReportResponse = read_json(resp).await;
        assert_eq!(
            body.issue_url,
            "https://github.com/FrozenTear/teamspeak-admin-panel/issues/99"
        );
        assert_eq!(body.issue_number, 99);

        let drafts = recorded.lock().unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].title,
            "[bug-report] /music-bots/42 — optional operator text"
        );
        assert!(drafts[0].body.contains("viewer4"));
        assert!(drafts[0].body.contains("/music-bots/42"));
        assert!(drafts[0].body.contains("Failed to fetch"));
        assert!(drafts[0].body.contains("SSE closed"));
        assert!(drafts[0].body.contains("v1.6.9"));
        assert!(drafts[0].body.contains("musicBotLatency"));
        assert!(drafts[0].body.contains("resolve=20s retry=1"));
    }

    #[tokio::test]
    async fn voice_context_is_merged_into_the_issue_without_clobbering_music_bot() {
        let _voice = music_bot::acquire_test_lock();
        music_bot::seed_for_tests();
        let mut state = fresh_state().await;
        let recorder = RecordingSink::new(
            "https://github.com/FrozenTear/teamspeak-admin-panel/issues/101",
            101,
        );
        let recorded = recorder.issues.clone();
        state.bug_reports = recorder.handle();

        let uid = seed_user(&state, "viewer5", "viewer").await;
        let token = mint_token(&state, uid, "viewer5", "viewer");
        let resp = post_report(app(state), Some(&token), &sample_body()).await;
        music_bot::reset_for_tests();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let drafts = recorded.lock().unwrap();
        assert_eq!(drafts.len(), 1);
        let body = &drafts[0].body;
        // Music Bot keys from the Panel body survive.
        assert!(body.contains("**musicBotLatency**"));
        assert!(body.contains("resolve=20s retry=1"));
        // Voice seat keys (camelCase) land in ### Context.
        assert!(body.contains("**firstFrameOnWireMs**"));
        assert!(body.contains("1842"));
        assert!(body.contains("**handshakeDropped**"));
        assert!(body.contains("**connectedLoopStall**"));
        assert!(body.contains("**frameUnderrun**"));
        assert!(body.contains("**voiceState**"));
        assert!(body.contains("**voiceLogTail**"));
        assert!(body.contains("first_frame_on_wire elapsed_ms=1842"));
        // Seat-scoped tail — do not steal Music Bot's `logTail` key.
        assert!(!body.contains("**logTail**"));
    }
}
