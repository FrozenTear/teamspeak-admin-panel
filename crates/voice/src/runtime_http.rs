//! Loopback HTTP control plane for the Contabo music unit.
//!
//! Fullstack proxies Panel/API here. This process owns the only
//! `BotSupervisor` / decode → Opus → wire send loop. No Surreal.

use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{delete, get, post};
use futures::stream::{Stream, StreamExt};
use tokio_stream::wrappers::BroadcastStream;
use tracing::warn;

use crate::config::BotId;
use crate::runtime_api::{
    BugReportContextResponse, HealthResponse, ListResponse, MutateOp, SendRequest, SettingsRequest,
    SpawnRequest, SpawnResponse, StoreOp, WireError, store_err_to_wire,
};
use crate::store::{LibraryEntryId, PlaylistName, StoreError, TrackId};
use crate::supervisor::BotSupervisor;

#[derive(Clone)]
pub struct RuntimeState {
    pub supervisor: Arc<BotSupervisor>,
    pub yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    pub yt_api_key: Arc<RwLock<Option<String>>>,
}

impl RuntimeState {
    pub fn new() -> Self {
        Self {
            supervisor: Arc::new(BotSupervisor::new()),
            yt_cookie: Arc::new(RwLock::new(None)),
            yt_api_key: Arc::new(RwLock::new(None)),
        }
    }
}

impl Default for RuntimeState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn router(state: RuntimeState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/bots", get(list_bots).post(spawn_bot))
        .route("/v1/bots/{id}", delete(shutdown_bot))
        .route("/v1/bots/{id}/command", post(send_command))
        .route("/v1/bots/{id}/events", get(events_sse))
        .route("/v1/settings", post(update_settings))
        .route("/v1/bug-report-context", get(bug_report_context))
        .route("/v1/mutate", post(mutate))
        .route("/v1/store", post(store_op))
        .with_state(state)
}

async fn health(State(state): State<RuntimeState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        bots: state.supervisor.list().await.len(),
        send_loop: "owned".into(),
    })
}

async fn list_bots(State(state): State<RuntimeState>) -> Json<ListResponse> {
    Json(ListResponse {
        bots: state.supervisor.list().await,
    })
}

async fn spawn_bot(
    State(state): State<RuntimeState>,
    Json(req): Json<SpawnRequest>,
) -> Result<Json<SpawnResponse>, Response> {
    let id = if let Some(id) = req.id {
        state
            .supervisor
            .spawn_with_id(
                BotId(id),
                req.config,
                Arc::clone(&state.yt_cookie),
                Arc::clone(&state.yt_api_key),
            )
            .await
    } else {
        state
            .supervisor
            .spawn(
                req.config,
                Arc::clone(&state.yt_cookie),
                Arc::clone(&state.yt_api_key),
            )
            .await
    };
    Ok(Json(SpawnResponse { id }))
}

async fn shutdown_bot(
    State(state): State<RuntimeState>,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    state
        .supervisor
        .shutdown_bot(BotId(id))
        .await
        .map_err(send_err)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn send_command(
    State(state): State<RuntimeState>,
    Path(id): Path<u64>,
    Json(req): Json<SendRequest>,
) -> Result<StatusCode, Response> {
    state
        .supervisor
        .send(BotId(id), req.command)
        .await
        .map_err(send_err)?;
    Ok(StatusCode::ACCEPTED)
}

async fn events_sse(
    State(state): State<RuntimeState>,
    Path(id): Path<u64>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    let rx = state
        .supervisor
        .subscribe(BotId(id))
        .await
        .ok_or_else(|| status_err(StatusCode::NOT_FOUND, "bot not found"))?;
    let stream = BroadcastStream::new(rx).filter_map(|item| async move {
        match item {
            Ok(ev) => match serde_json::to_string(&ev) {
                Ok(json) => Some(Ok(Event::default().data(json))),
                Err(err) => {
                    warn!(?err, "failed to serialise BotEvent for music-runtime SSE");
                    None
                }
            },
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}

async fn update_settings(
    State(state): State<RuntimeState>,
    Json(req): Json<SettingsRequest>,
) -> StatusCode {
    if let Some(cookie) = req.yt_cookie {
        *state.yt_cookie.write().unwrap_or_else(|e| e.into_inner()) = cookie;
    }
    if let Some(key) = req.yt_api_key {
        *state.yt_api_key.write().unwrap_or_else(|e| e.into_inner()) = key;
    }
    StatusCode::NO_CONTENT
}

async fn bug_report_context() -> Json<BugReportContextResponse> {
    let snap = crate::bug_report::snapshot();
    Json(BugReportContextResponse {
        music_bot_latency: snap.music_bot_latency,
        log_tail: snap.log_tail,
    })
}

async fn mutate(
    State(state): State<RuntimeState>,
    Json(op): Json<MutateOp>,
) -> Result<Json<serde_json::Value>, Response> {
    let value = match op {
        MutateOp::PlaylistCreate { bot, name } => {
            state
                .supervisor
                .playlist_create(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        MutateOp::PlaylistRename { bot, old, new } => {
            state
                .supervisor
                .playlist_rename(BotId(bot), PlaylistName(old), PlaylistName(new))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        MutateOp::PlaylistDelete { bot, name } => {
            state
                .supervisor
                .playlist_delete(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        MutateOp::PlaylistAddTrack { bot, name, track } => {
            let stored = state
                .supervisor
                .playlist_add_track(BotId(bot), &PlaylistName(name), track)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::PlaylistRemoveTrack { bot, name, id } => {
            let changed = state
                .supervisor
                .playlist_remove_track(BotId(bot), &PlaylistName(name), TrackId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        MutateOp::PlaylistList { bot } => {
            let names = state
                .supervisor
                .playlist_list(BotId(bot))
                .await
                .map_err(store_err)?;
            serde_json::to_value(names).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::PlaylistListTracks { bot, name } => {
            let tracks = state
                .supervisor
                .playlist_list_tracks(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::EnqueuePlaylist { bot, name } => {
            let tracks = state
                .supervisor
                .store()
                .enqueue_playlist(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::LibraryAdd { bot, entry } => {
            let stored = state
                .supervisor
                .library_add(BotId(bot), entry)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::LibraryRemove { bot, id } => {
            let changed = state
                .supervisor
                .library_remove(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        MutateOp::LibraryLookup { bot, id } => {
            let entry = state
                .supervisor
                .library_lookup(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::to_value(entry).unwrap_or(serde_json::Value::Null)
        }
        MutateOp::LibraryList { bot, tag } => {
            let entries = state
                .supervisor
                .library_list(BotId(bot), tag.as_deref())
                .await
                .map_err(store_err)?;
            serde_json::to_value(entries).unwrap_or(serde_json::Value::Null)
        }
    };
    Ok(Json(value))
}

async fn store_op(
    State(state): State<RuntimeState>,
    Json(op): Json<StoreOp>,
) -> Result<Json<serde_json::Value>, Response> {
    let store = state.supervisor.store();
    let value = match op {
        StoreOp::QueueEnqueue { bot, track } => {
            let stored = store
                .queue_enqueue(BotId(bot), track)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueueDequeueHead { bot } => {
            let head = store
                .queue_dequeue_head(BotId(bot))
                .await
                .map_err(store_err)?;
            serde_json::to_value(head).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueuePeek { bot } => {
            let q = store.queue_peek(BotId(bot)).await.map_err(store_err)?;
            serde_json::to_value(q).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueueClear { bot } => {
            store.queue_clear(BotId(bot)).await.map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::QueueReorder { bot, order } => {
            store
                .queue_reorder(BotId(bot), order.into_iter().map(TrackId).collect())
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::QueueRemove { bot, id } => {
            let changed = store
                .queue_remove(BotId(bot), TrackId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        StoreOp::QueueCurrent { bot } => {
            let cur = store.queue_current(BotId(bot)).await.map_err(store_err)?;
            serde_json::to_value(cur).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::QueueSetHeadTitle { bot, title } => {
            let head = store
                .queue_set_head_title(BotId(bot), title)
                .await
                .map_err(store_err)?;
            serde_json::to_value(head).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::PlaylistCreate { bot, name } => {
            store
                .playlist_create(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::PlaylistRename { bot, old, new } => {
            store
                .playlist_rename(BotId(bot), PlaylistName(old), PlaylistName(new))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::PlaylistDelete { bot, name } => {
            store
                .playlist_delete(BotId(bot), PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::json!(null)
        }
        StoreOp::PlaylistAddTrack { bot, name, track } => {
            let stored = store
                .playlist_add_track(BotId(bot), &PlaylistName(name), track)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::PlaylistRemoveTrack { bot, name, id } => {
            let changed = store
                .playlist_remove_track(BotId(bot), &PlaylistName(name), TrackId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        StoreOp::PlaylistListTracks { bot, name } => {
            let tracks = store
                .playlist_list_tracks(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::PlaylistList { bot } => {
            let names = store.playlist_list(BotId(bot)).await.map_err(store_err)?;
            serde_json::to_value(names).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::EnqueuePlaylist { bot, name } => {
            let tracks = store
                .enqueue_playlist(BotId(bot), &PlaylistName(name))
                .await
                .map_err(store_err)?;
            serde_json::to_value(tracks).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::LibraryAdd { bot, entry } => {
            let stored = store
                .library_add(BotId(bot), entry)
                .await
                .map_err(store_err)?;
            serde_json::to_value(stored).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::LibraryRemove { bot, id } => {
            let changed = store
                .library_remove(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::json!(changed)
        }
        StoreOp::LibraryLookup { bot, id } => {
            let entry = store
                .library_lookup(BotId(bot), LibraryEntryId(id))
                .await
                .map_err(store_err)?;
            serde_json::to_value(entry).unwrap_or(serde_json::Value::Null)
        }
        StoreOp::LibraryList { bot, tag } => {
            let entries = store
                .library_list(BotId(bot), tag.as_deref())
                .await
                .map_err(store_err)?;
            serde_json::to_value(entries).unwrap_or(serde_json::Value::Null)
        }
    };
    Ok(Json(value))
}

fn send_err(err: crate::supervisor::SendError) -> Response {
    status_err(StatusCode::NOT_FOUND, &err.to_string())
}

fn store_err(err: StoreError) -> Response {
    let status = match &err {
        StoreError::PlaylistNotFound(_)
        | StoreError::TrackNotFound(_)
        | StoreError::LibraryEntryNotFound(_) => StatusCode::NOT_FOUND,
        StoreError::PlaylistExists(_) => StatusCode::CONFLICT,
        StoreError::ReorderMismatch { .. } => StatusCode::BAD_REQUEST,
        StoreError::Snapshot(_) | StoreError::Backend(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let body = serde_json::to_vec(&store_err_to_wire(&err)).unwrap_or_default();
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| status_err(status, &err.to_string()))
}

fn status_err(status: StatusCode, msg: &str) -> Response {
    let body = serde_json::to_vec(&WireError {
        error: msg.to_string(),
    })
    .unwrap_or_default();
    Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(status)
                .body(axum::body::Body::from(msg.to_string()))
                .expect("static error response")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::BotCommand;
    use crate::config::BotConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn json<T: serde::de::DeserializeOwned>(resp: axum::http::Response<Body>) -> T {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).expect("json body")
    }

    #[tokio::test]
    async fn health_and_spawn_and_command_roundtrip() {
        let app = router(RuntimeState::new());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let health: HealthResponse = json(resp).await;
        assert_eq!(health.send_loop, "owned");
        assert_eq!(health.bots, 0);

        let cfg = BotConfig::new(
            "unit",
            std::env::temp_dir().join("music-runtime-unit.identity"),
        )
        .with_auto_connect(false);
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/bots")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SpawnRequest {
                            config: cfg,
                            id: None,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let spawned: SpawnResponse = json(resp).await;

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/v1/bots/{}/command", spawned.id.0))
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&SendRequest {
                            command: BotCommand::Disconnect,
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/v1/bots")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let list: ListResponse = json(resp).await;
        assert_eq!(list.bots.len(), 1);
        assert_eq!(list.bots[0].name, "unit");
    }
}
