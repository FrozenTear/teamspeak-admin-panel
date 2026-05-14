//! Typed REST client for `/api/video-sources` — PURA-145 WS-7.
//!
//! Mirrors the music-bot client layout: every call goes through the
//! shared [`crate::client::api`] helpers so the single-flight refresh
//! contract holds across the SPA. Wire shapes come straight from
//! [`ts6_manager_shared::video_sources`] — the FE never redefines a
//! JSON contract.

use std::sync::Arc;

use ts6_manager_shared::video_sources as wire;

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// `GET /api/video-sources` — the caller-visible row list.
pub async fn list_sources(
    gate: Arc<RefreshGate>,
) -> Result<Vec<wire::VideoSourceView>, ApiError> {
    api::authorized_get_json::<Vec<wire::VideoSourceView>>(
        &gate,
        &api::api_base(),
        "/api/video-sources",
    )
    .await
}

/// `POST /api/video-sources` — start a new pipeline.
pub async fn create_source(
    gate: Arc<RefreshGate>,
    body: &wire::CreateVideoSourceRequest,
) -> Result<wire::VideoSourceView, ApiError> {
    api::authorized_post_json::<_, wire::VideoSourceView>(
        &gate,
        &api::api_base(),
        "/api/video-sources",
        Some(body),
    )
    .await
}

/// `DELETE /api/video-sources/{id}` — stop the pipeline and drop the row.
pub async fn delete_source(gate: Arc<RefreshGate>, id: i64) -> Result<(), ApiError> {
    let path = format!("/api/video-sources/{id}");
    api::authorized_delete(&gate, &api::api_base(), &path).await
}
