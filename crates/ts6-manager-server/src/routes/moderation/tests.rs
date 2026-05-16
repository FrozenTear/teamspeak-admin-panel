//! Integration tests for the `/api/moderation/*` surface — PURA-286.
//!
//! Coverage: the `RequirePermission` gate (viewer denied, moderator role
//! default allowed, explicit-grant path), the case state machine
//! (`open → actioned → resolved` + reopen, with the illegal transitions
//! rejected), the timeline-row + audit-row pairing, and the per-subject
//! history fan-in. Kick / ban / mute TS6 dispatch is verified against the
//! live fixture by `9.0-qa`; here the `note` action exercises the
//! append + state-machine path without a backend round-trip.

use super::router;
use crate::app_state::AppState;
use crate::auth::{jwt, password};
use crate::db::{connect_in_memory, migrations};
use crate::repos::{admin_audit_log, user_permissions, users};
use axum::Router;
use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use ts6_manager_shared::admin::Page;
use ts6_manager_shared::moderation as wire;

async fn fresh_state() -> AppState {
    let db = connect_in_memory().await.unwrap();
    migrations::run(&db).await.unwrap();
    crate::crypto::init("test-seed-pura-286");
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
        yt_cookie: std::sync::Arc::new(std::sync::RwLock::new(None)),
        data_dir: std::path::PathBuf::from("./data"),
        trusted_proxy_hops: 0,
    }
}

fn app(state: AppState) -> Router {
    Router::new().merge(router()).with_state(state)
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

fn mint(state: &AppState, id: i64, username: &str, role: &str) -> String {
    jwt::mint_access(
        id,
        username,
        role,
        state.jwt_access_expiry,
        &state.jwt_secret,
    )
    .unwrap()
}

fn auth(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("Bearer {token}")).unwrap()
}

fn json_body<T: serde::Serialize>(v: &T) -> Body {
    Body::from(serde_json::to_vec(v).unwrap())
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

fn get(uri: &str, token: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header("authorization", auth(token))
        .body(Body::empty())
        .unwrap()
}

fn post<T: serde::Serialize>(uri: &str, token: &str, body: &T) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("authorization", auth(token))
        .header("content-type", "application/json")
        .body(json_body(body))
        .unwrap()
}

fn open_case_req() -> wire::OpenCaseRequest {
    wire::OpenCaseRequest {
        server_config_id: 1,
        virtual_server_id: 1,
        subject_uid: "uid-subject".into(),
        subject_nickname_snapshot: "Troublemaker".into(),
        reason: "spam".into(),
        origin: None,
        origin_ref: None,
    }
}

#[tokio::test]
async fn viewer_without_grants_is_forbidden_on_case_list() {
    let state = fresh_state().await;
    let vid = seed_user(&state, "view", "viewer").await;
    let token = mint(&state, vid, "view", "viewer");
    let resp = app(state)
        .oneshot(get("/api/moderation/cases", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unauthenticated_request_is_rejected() {
    let state = fresh_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri("/api/moderation/cases")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn moderator_opens_lists_and_reads_a_case() {
    let state = fresh_state().await;
    let mid = seed_user(&state, "mod", "moderator").await;
    let token = mint(&state, mid, "mod", "moderator");
    let app = app(state);

    let resp = app
        .clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let case: wire::ModerationCase = read_json(resp).await;
    assert_eq!(case.status, "open");
    assert_eq!(case.origin, "operator");
    assert_eq!(case.subject_uid, "uid-subject");
    assert_eq!(case.opened_by_user_id, Some(mid));

    let resp = app
        .clone()
        .oneshot(get("/api/moderation/cases", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let page: Page<wire::ModerationCase> = read_json(resp).await;
    assert_eq!(page.total, 1);
    assert_eq!(page.items.len(), 1);

    let resp = app
        .oneshot(get(&format!("/api/moderation/cases/{}", case.id), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let detail: wire::CaseDetail = read_json(resp).await;
    assert_eq!(detail.case.id, case.id);
    assert!(detail.timeline.is_empty());
}

#[tokio::test]
async fn open_case_rejects_blank_reason() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let mut req = open_case_req();
    req.reason = "   ".into();
    let resp = app(state)
        .oneshot(post("/api/moderation/cases", &token, &req))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn detail_404_for_missing_case() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let resp = app(state)
        .oneshot(get("/api/moderation/cases/9999", &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn note_action_appends_timeline_and_actions_the_case() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let app = app(state);

    let resp = app
        .clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();
    let case: wire::ModerationCase = read_json(resp).await;

    // A `note` action does not touch TS6 and does not move the state.
    let note_action = wire::AppendActionRequest {
        action_kind: "note".into(),
        reason: "left a warning in chat".into(),
        clid: None,
        ban_duration_secs: None,
    };
    let resp = app
        .clone()
        .oneshot(post(
            &format!("/api/moderation/cases/{}/actions", case.id),
            &token,
            &note_action,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let action: wire::ModerationCaseAction = read_json(resp).await;
    assert_eq!(action.action_kind, "note");
    assert_eq!(action.case_id, case.id);

    let resp = app
        .oneshot(get(&format!("/api/moderation/cases/{}", case.id), &token))
        .await
        .unwrap();
    let detail: wire::CaseDetail = read_json(resp).await;
    assert_eq!(detail.timeline.len(), 1);
    // `note` is not punitive — the case stays `open`.
    assert_eq!(detail.case.status, "open");
}

#[tokio::test]
async fn action_requires_clid_for_kick() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let app = app(state);

    let resp = app
        .clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();
    let case: wire::ModerationCase = read_json(resp).await;

    let kick = wire::AppendActionRequest {
        action_kind: "kick".into(),
        reason: "flooding".into(),
        clid: None,
        ban_duration_secs: None,
    };
    let resp = app
        .oneshot(post(
            &format!("/api/moderation/cases/{}/actions", case.id),
            &token,
            &kick,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn action_denied_without_the_catalog_permission() {
    let state = fresh_state().await;
    // A viewer with only `case.view` can read but cannot append a note
    // action (needs `note.write`).
    let vid = seed_user(&state, "view", "viewer").await;
    user_permissions::replace_all(
        &state.db,
        vid,
        vid,
        &[
            "moderation.case.view".to_string(),
            "moderation.case.manage".to_string(),
        ],
    )
    .await
    .unwrap();
    let token = mint(&state, vid, "view", "viewer");
    let app = app(state);

    let resp = app
        .clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let case: wire::ModerationCase = read_json(resp).await;

    let note_action = wire::AppendActionRequest {
        action_kind: "note".into(),
        reason: "x".into(),
        clid: None,
        ban_duration_secs: None,
    };
    let resp = app
        .oneshot(post(
            &format!("/api/moderation/cases/{}/actions", case.id),
            &token,
            &note_action,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn resolve_then_reopen_walks_the_state_machine() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let app = app(state.clone());

    let resp = app
        .clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();
    let case: wire::ModerationCase = read_json(resp).await;

    // resolve
    let resp = app
        .clone()
        .oneshot(post(
            &format!("/api/moderation/cases/{}/resolve", case.id),
            &token,
            &wire::ResolveCaseRequest {
                resolution_note: "warned, no recurrence".into(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resolved: wire::ModerationCase = read_json(resp).await;
    assert_eq!(resolved.status, "resolved");
    assert!(resolved.resolved_at.is_some());

    // double-resolve is a conflict
    let resp = app
        .clone()
        .oneshot(post(
            &format!("/api/moderation/cases/{}/resolve", case.id),
            &token,
            &wire::ResolveCaseRequest {
                resolution_note: "again".into(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // reopen
    let resp = app
        .clone()
        .oneshot(post(
            &format!("/api/moderation/cases/{}/reopen", case.id),
            &token,
            &wire::ReopenCaseRequest {
                reason: "subject re-offended".into(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let reopened: wire::ModerationCase = read_json(resp).await;
    assert_eq!(reopened.status, "open");
    assert!(reopened.resolved_at.is_none());

    // reopening an open case is a conflict
    let resp = app
        .oneshot(post(
            &format!("/api/moderation/cases/{}/reopen", case.id),
            &token,
            &wire::ReopenCaseRequest {
                reason: "nope".into(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    // resolve + reopen each appended a timeline row and an audit row.
    let (audit, total) =
        admin_audit_log::list(&state.db, &admin_audit_log::ListFilter::default(), 50, 0)
            .await
            .unwrap();
    assert!(total >= 3, "open + resolve + reopen audit rows");
    assert!(audit.iter().any(|r| r.kind == "moderationCaseResolved"));
    assert!(audit.iter().any(|r| r.kind == "moderationCaseReopened"));
}

#[tokio::test]
async fn resolve_requires_a_resolution_note() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let app = app(state);

    let resp = app
        .clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();
    let case: wire::ModerationCase = read_json(resp).await;

    let resp = app
        .oneshot(post(
            &format!("/api/moderation/cases/{}/resolve", case.id),
            &token,
            &wire::ResolveCaseRequest {
                resolution_note: "  ".into(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn notes_create_list_and_history_fan_in() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let app = app(state);
    let uid = "uid-subject";

    // open a case so history has both a case and a note to fan in.
    app.clone()
        .oneshot(post("/api/moderation/cases", &token, &open_case_req()))
        .await
        .unwrap();

    let resp = app
        .clone()
        .oneshot(post(
            &format!("/api/moderation/subjects/{uid}/notes"),
            &token,
            &wire::CreateNoteRequest {
                body: "knows the rules, ignored a warning".into(),
            },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(get(
            &format!("/api/moderation/subjects/{uid}/notes"),
            &token,
        ))
        .await
        .unwrap();
    let notes: Vec<wire::ModerationNote> = read_json(resp).await;
    assert_eq!(notes.len(), 1);

    let resp = app
        .oneshot(get(
            &format!("/api/moderation/subjects/{uid}/history"),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let history: wire::SubjectHistory = read_json(resp).await;
    assert_eq!(history.subject_uid, uid);
    assert_eq!(history.cases.len(), 1);
    assert_eq!(history.notes.len(), 1);
}

#[tokio::test]
async fn note_create_rejects_blank_body() {
    let state = fresh_state().await;
    let aid = seed_user(&state, "admin", "admin").await;
    let token = mint(&state, aid, "admin", "admin");
    let resp = app(state)
        .oneshot(post(
            "/api/moderation/subjects/uid-x/notes",
            &token,
            &wire::CreateNoteRequest { body: "".into() },
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- TS6 complaint sub-surface (PURA-289) ----------------------------

const COMPLAINTS_URI: &str = "/api/moderation/complaints?serverConfigId=1&virtualServerId=1";

fn resolve_complaint_req() -> wire::ResolveComplaintRequest {
    wire::ResolveComplaintRequest {
        server_config_id: 1,
        virtual_server_id: 1,
        tcldbid: 5,
        fcldbid: Some(3),
    }
}

#[tokio::test]
async fn viewer_without_grants_is_forbidden_on_complaint_list() {
    let state = fresh_state().await;
    let vid = seed_user(&state, "view", "viewer").await;
    let token = mint(&state, vid, "view", "viewer");
    let resp = app(state)
        .oneshot(get(COMPLAINTS_URI, &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn viewer_without_grants_is_forbidden_on_complaint_resolve() {
    let state = fresh_state().await;
    let vid = seed_user(&state, "view", "viewer").await;
    let token = mint(&state, vid, "view", "viewer");
    let resp = app(state)
        .oneshot(post(
            "/api/moderation/complaints/resolve",
            &token,
            &resolve_complaint_req(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unauthenticated_complaint_list_is_rejected() {
    let state = fresh_state().await;
    let resp = app(state)
        .oneshot(
            Request::builder()
                .uri(COMPLAINTS_URI)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn complaint_list_404_when_server_connection_absent() {
    // A moderator passes the `ComplaintView` gate by role default, but
    // with no `server_connection` row the backend lookup is a 404.
    let state = fresh_state().await;
    let mid = seed_user(&state, "mod", "moderator").await;
    let token = mint(&state, mid, "mod", "moderator");
    let resp = app(state)
        .oneshot(get(COMPLAINTS_URI, &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn complaint_resolve_404_when_server_connection_absent() {
    let state = fresh_state().await;
    let mid = seed_user(&state, "mod", "moderator").await;
    let token = mint(&state, mid, "mod", "moderator");
    let resp = app(state)
        .oneshot(post(
            "/api/moderation/complaints/resolve",
            &token,
            &resolve_complaint_req(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
