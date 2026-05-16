//! Integration + unit tests for the public moderation surface (PURA-307).
//!
//! Coverage:
//!   - per-server kill-switch (`404` when the flag is off);
//!   - the token identity gate — invalid / wrong-kind / replayed tokens
//!     all collapse to one generic `403`;
//!   - the report intake happy path writes a `pending` `moderation_report`;
//!   - the appeal happy path writes a `moderation_appeal`, moves the case
//!     `actioned → appealed`, and appends an `appeal_filed` timeline row
//!     with `actorUserId = NULL`;
//!   - the redacted case view omits moderator `note` rows (brief §6 hook 4);
//!   - one-appeal-per-case is enforced;
//!   - `X-Forwarded-For` is trusted only from a configured proxy CIDR.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use ts6_manager_shared::moderation as wire;

use super::*;
use crate::app_state::AppState;
use crate::db::{connect_in_memory, migrations};
use crate::repos::moderation_case_actions::{self, NewModerationCaseAction};
use crate::repos::moderation_cases::{self, NewModerationCase};
use crate::repos::{app_settings, moderation_appeals, moderation_reports};
use crate::routes::moderation::tokens;

async fn fresh_state() -> AppState {
    let db = connect_in_memory().await.unwrap();
    migrations::run(&db).await.unwrap();
    crate::crypto::init("test-seed-pura-307");
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
    Router::new()
        .merge(super::router(Vec::new()))
        .with_state(state)
}

/// POST a JSON body, attributing the request to a fixed loopback peer so
/// the per-IP limiter has a stable key.
fn post(uri: &str, body: &serde_json::Value) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap();
    let peer: SocketAddr = "203.0.113.10:50000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

fn get(uri: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::GET)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let peer: SocketAddr = "203.0.113.10:50000".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    req
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn enable(state: &AppState, flag: &str) {
    app_settings::put(&state.db, flag, "true").await.unwrap();
}

/// Insert a case and move it to `actioned` — the only state an appeal may
/// be filed against.
async fn actioned_case(state: &AppState, subject: &str) -> i64 {
    let case = moderation_cases::insert(
        &state.db,
        NewModerationCase {
            serverConfigId: 1,
            virtualServerId: 1,
            subjectUid: subject.into(),
            subjectNicknameSnapshot: "Subj".into(),
            origin: "operator".into(),
            originRef: None,
            reason: "banned for spam".into(),
            openedByUserId: Some(1),
        },
    )
    .await
    .unwrap();
    moderation_cases::set_status(&state.db, case.id, "actioned", None)
        .await
        .unwrap();
    case.id
}

// ---- kill-switch ------------------------------------------------------

#[tokio::test]
async fn reports_404_when_flag_disabled() {
    let state = fresh_state().await;
    let resp = app(state)
        .oneshot(post(
            "/api/public/moderation/reports",
            &serde_json::json!({
                "token": "x.y",
                "serverConfigId": 1,
                "virtualServerId": 1,
                "subjectUidOrNickname": "bad-actor",
                "category": "spam",
                "statement": "spamming the lobby"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn appeals_404_when_flag_disabled() {
    let state = fresh_state().await;
    let resp = app(state)
        .oneshot(get("/api/public/moderation/case?token=x.y"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- report intake ----------------------------------------------------

#[tokio::test]
async fn report_with_invalid_token_is_forbidden() {
    let state = fresh_state().await;
    enable(&state, FLAG_REPORTS_ENABLED).await;
    let resp = app(state)
        .oneshot(post(
            "/api/public/moderation/reports",
            &serde_json::json!({
                "token": "deadbeef.cafe",
                "serverConfigId": 1,
                "virtualServerId": 1,
                "subjectUidOrNickname": "bad-actor",
                "category": "spam",
                "statement": "spamming the lobby"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn report_happy_path_writes_pending_row() {
    let state = fresh_state().await;
    enable(&state, FLAG_REPORTS_ENABLED).await;
    let minted = tokens::mint_report_challenge(&state.db, "reporter-uid-1")
        .await
        .unwrap();

    let resp = app(state.clone())
        .oneshot(post(
            "/api/public/moderation/reports",
            &serde_json::json!({
                "token": minted.plaintext,
                "serverConfigId": 1,
                "virtualServerId": 1,
                "subjectUidOrNickname": "bad-actor",
                "category": "harassment",
                "statement": "was abusive in voice",
                "evidenceUrl": "https://example.com/clip"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = body_json(resp).await;
    assert_eq!(body["status"], "pending");

    // The row exists, keyed to the *token's* reporter UID, not the body.
    let pending = moderation_reports::list_by_status(&state.db, "pending")
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].reporterUid, "reporter-uid-1");
    assert_eq!(pending[0].subjectUidOrNickname, "bad-actor");
    assert!(
        !pending[0].sourceIpHash.is_empty(),
        "source IP is hashed in"
    );
}

#[tokio::test]
async fn report_token_is_single_use() {
    let state = fresh_state().await;
    enable(&state, FLAG_REPORTS_ENABLED).await;
    let minted = tokens::mint_report_challenge(&state.db, "reporter-uid-2")
        .await
        .unwrap();
    let body = serde_json::json!({
        "token": minted.plaintext,
        "serverConfigId": 1,
        "virtualServerId": 1,
        "subjectUidOrNickname": "bad-actor",
        "category": "spam",
        "statement": "spam"
    });

    let first = app(state.clone())
        .oneshot(post("/api/public/moderation/reports", &body))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let replay = app(state)
        .oneshot(post("/api/public/moderation/reports", &body))
        .await
        .unwrap();
    assert_eq!(
        replay.status(),
        StatusCode::FORBIDDEN,
        "a consumed report token must not verify again"
    );
}

#[tokio::test]
async fn appeal_token_is_rejected_on_the_report_route() {
    let state = fresh_state().await;
    enable(&state, FLAG_REPORTS_ENABLED).await;
    let case_id = actioned_case(&state, "subj-x").await;
    let minted = tokens::mint_appeal(&state.db, case_id).await.unwrap();

    let resp = app(state)
        .oneshot(post(
            "/api/public/moderation/reports",
            &serde_json::json!({
                "token": minted.plaintext,
                "serverConfigId": 1,
                "virtualServerId": 1,
                "subjectUidOrNickname": "bad-actor",
                "category": "spam",
                "statement": "spam"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "an appeal token must not authorise a report",
    );
}

// ---- appeals ----------------------------------------------------------

#[tokio::test]
async fn redacted_case_view_omits_moderator_notes() {
    let state = fresh_state().await;
    enable(&state, FLAG_APPEALS_ENABLED).await;
    let case_id = actioned_case(&state, "subj-appeal").await;

    // A visible enforcement action and an operator note on the same case.
    for (kind, reason) in [
        ("ban", "ban: repeated spam"),
        ("note", "INTERNAL: alt of user 7"),
    ] {
        moderation_case_actions::insert(
            &state.db,
            NewModerationCaseAction {
                caseId: case_id,
                actorUserId: Some(1),
                actorUsernameSnapshot: "mod1".into(),
                actionKind: kind.into(),
                reason: reason.into(),
                tsRef: None,
                payload: None,
            },
        )
        .await
        .unwrap();
    }

    let minted = tokens::mint_appeal(&state.db, case_id).await.unwrap();
    let resp = app(state)
        .oneshot(get(&format!(
            "/api/public/moderation/case?token={}",
            minted.plaintext
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let view: wire::RedactedCase = serde_json::from_value(body_json(resp).await).unwrap();

    assert_eq!(view.case_id, case_id);
    assert!(
        view.appealable,
        "an actioned case with no appeal is appealable"
    );
    assert_eq!(
        view.timeline.len(),
        1,
        "only the enforcement action is shown"
    );
    assert_eq!(view.timeline[0].action_kind, "ban");
    let serialized = serde_json::to_string(&view).unwrap();
    assert!(
        !serialized.contains("INTERNAL"),
        "the redacted view leaked a moderator note: {serialized}"
    );
}

#[tokio::test]
async fn redacted_case_view_is_non_consuming() {
    // The view must not burn the token — the subject reloads, then appeals.
    let state = fresh_state().await;
    enable(&state, FLAG_APPEALS_ENABLED).await;
    let case_id = actioned_case(&state, "subj-reload").await;
    let minted = tokens::mint_appeal(&state.db, case_id).await.unwrap();
    let uri = format!("/api/public/moderation/case?token={}", minted.plaintext);

    for _ in 0..3 {
        let resp = app(state.clone()).oneshot(get(&uri)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "view stays usable on reload");
    }
}

#[tokio::test]
async fn appeal_happy_path_transitions_case_and_appends_marker() {
    let state = fresh_state().await;
    enable(&state, FLAG_APPEALS_ENABLED).await;
    let case_id = actioned_case(&state, "subj-y").await;
    let minted = tokens::mint_appeal(&state.db, case_id).await.unwrap();

    let resp = app(state.clone())
        .oneshot(post(
            "/api/public/moderation/appeals",
            &serde_json::json!({
                "token": minted.plaintext,
                "statement": "it was not me — my account was shared"
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Appeal row.
    let appeals = moderation_appeals::list_for_case(&state.db, case_id)
        .await
        .unwrap();
    assert_eq!(appeals.len(), 1);
    assert_eq!(appeals[0].status, "pending");
    assert_eq!(appeals[0].submitterUid, "subj-y");

    // Case transitioned `actioned → appealed`.
    let case = moderation_cases::find_by_id(&state.db, case_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(case.status, "appealed");

    // `appeal_filed` timeline row, `actorUserId = NULL`.
    let timeline = moderation_case_actions::list_for_case(&state.db, case_id)
        .await
        .unwrap();
    let filed = timeline
        .iter()
        .find(|a| a.actionKind == "appeal_filed")
        .expect("an appeal_filed row was appended");
    assert!(filed.actorUserId.is_none(), "appellant is not an operator");
}

#[tokio::test]
async fn second_appeal_on_a_case_is_conflict() {
    let state = fresh_state().await;
    enable(&state, FLAG_APPEALS_ENABLED).await;
    let case_id = actioned_case(&state, "subj-z").await;

    let first_token = tokens::mint_appeal(&state.db, case_id).await.unwrap();
    let first = app(state.clone())
        .oneshot(post(
            "/api/public/moderation/appeals",
            &serde_json::json!({ "token": first_token.plaintext, "statement": "first appeal" }),
        ))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    // A fresh token, same case — the case is now `appealed`, not `actioned`.
    let second_token = tokens::mint_appeal(&state.db, case_id).await.unwrap();
    let second = app(state)
        .oneshot(post(
            "/api/public/moderation/appeals",
            &serde_json::json!({ "token": second_token.plaintext, "statement": "second appeal" }),
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn appeal_rejects_an_overlong_statement() {
    let state = fresh_state().await;
    enable(&state, FLAG_APPEALS_ENABLED).await;
    let case_id = actioned_case(&state, "subj-long").await;
    let minted = tokens::mint_appeal(&state.db, case_id).await.unwrap();

    let resp = app(state)
        .oneshot(post(
            "/api/public/moderation/appeals",
            &serde_json::json!({
                "token": minted.plaintext,
                "statement": "x".repeat(MAX_TEXT_LEN + 1)
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---- resolve_client_ip (CIDR proxy trust) -----------------------------

#[test]
fn xff_is_ignored_when_no_proxy_cidr_is_configured() {
    // Default-deny: an empty allow-list means XFF is never trusted.
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.9"));
    let peer: SocketAddr = "203.0.113.7:5000".parse().unwrap();
    let ip = resolve_client_ip(&headers, peer, &[]);
    assert_eq!(
        ip.to_string(),
        "203.0.113.7",
        "no CIDR ⇒ trust only the peer"
    );
}

#[test]
fn xff_is_ignored_when_peer_is_not_a_trusted_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.9"));
    let peer: SocketAddr = "203.0.113.7:5000".parse().unwrap();
    let trusted = vec!["10.0.0.0/8".parse().unwrap()];
    let ip = resolve_client_ip(&headers, peer, &trusted);
    assert_eq!(
        ip.to_string(),
        "203.0.113.7",
        "peer outside the proxy CIDR ⇒ XFF untrusted",
    );
}

#[test]
fn xff_rightmost_entry_is_trusted_when_peer_is_a_proxy() {
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-forwarded-for",
        HeaderValue::from_static("evil-claim, 198.51.100.9"),
    );
    let peer: SocketAddr = "10.1.2.3:5000".parse().unwrap();
    let trusted = vec!["10.0.0.0/8".parse().unwrap()];
    let ip = resolve_client_ip(&headers, peer, &trusted);
    assert_eq!(
        ip.to_string(),
        "198.51.100.9",
        "peer inside the proxy CIDR ⇒ take the rightmost XFF entry",
    );
}

// ---- hash_source_ip ---------------------------------------------------

#[test]
fn source_ip_hash_is_deterministic_and_keyed() {
    let ip = "198.51.100.42".parse().unwrap();
    let a = hash_source_ip(ip, b"secret-key-one");
    let b = hash_source_ip(ip, b"secret-key-one");
    let c = hash_source_ip(ip, b"secret-key-two");
    assert_eq!(a, b, "same IP + key ⇒ same hash (abuse correlation)");
    assert_ne!(a, c, "the hash is keyed on the server secret");
    assert_ne!(a, "198.51.100.42", "the raw IP is never the stored value");
    assert_eq!(a.len(), 64, "SHA-256 hex");
}
