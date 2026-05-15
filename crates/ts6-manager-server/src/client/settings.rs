//! Typed client for `/api/settings/*` (PURA-224).
//!
//! Mirrors [`crate::routes::settings`] (PURA-223). Three calls cover the
//! YouTube cookie surface:
//!
//! - [`get_youtube_cookie_status`] → `GET … /youtube-cookies` →
//!   `{ uploaded, uploadedAt? }`
//! - [`upload_youtube_cookie_file`] → `PUT … /youtube-cookies` (multipart,
//!   field `file`) → 204
//! - [`delete_youtube_cookie_file`] → `DELETE … /youtube-cookies` → 204
//!
//! The upload helper builds a `FormData` directly on WASM and hands it to
//! `gloo-net` so the browser sets the `multipart/form-data; boundary=…`
//! content-type for us. The native build returns
//! [`ApiError::UnsupportedTarget`] — SSR + native unit tests never
//! exercise the upload path.

use std::sync::Arc;

use serde::Deserialize;

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// Wire shape returned by `GET /api/settings/youtube-cookies`. Mirrors
/// `routes::settings::CookieStatus` byte-for-byte.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CookieStatus {
    pub uploaded: bool,
    pub uploaded_at: Option<String>,
}

pub async fn get_youtube_cookie_status(gate: Arc<RefreshGate>) -> Result<CookieStatus, ApiError> {
    api::authorized_get_json::<CookieStatus>(
        &gate,
        &api::api_base(),
        "/api/settings/youtube-cookies",
    )
    .await
}

pub async fn delete_youtube_cookie_file(gate: Arc<RefreshGate>) -> Result<(), ApiError> {
    api::authorized_delete(&gate, &api::api_base(), "/api/settings/youtube-cookies").await
}

/// Upload `file` as a multipart `PUT` to `/api/settings/youtube-cookies`.
///
/// The browser populates the multipart boundary itself when the body is a
/// `FormData`, so this helper deliberately does NOT set a content-type
/// header — overriding it would emit a malformed boundary and the
/// backend's multipart extractor would reject the upload.
#[cfg(target_arch = "wasm32")]
pub async fn upload_youtube_cookie_file(
    gate: Arc<RefreshGate>,
    file: web_sys::File,
) -> Result<(), ApiError> {
    use crate::client::auth::AuthError;
    use ts6_manager_shared::auth::ErrorResponse;

    let base = api::api_base();
    let path = "/api/settings/youtube-cookies";

    let (status, body) = gate
        .run(|snap| {
            let url = format!("{}{}", base.trim_end_matches('/'), path);
            let access = snap.access.clone();
            let file = file.clone();
            async move {
                let form = web_sys::FormData::new()
                    .map_err(|e| AuthError::Transport(format!("FormData::new: {e:?}")))?;
                form.append_with_blob_and_filename("file", &file, &file.name())
                    .map_err(|e| AuthError::Transport(format!("FormData::append: {e:?}")))?;

                let resp = gloo_net::http::Request::put(&url)
                    .header("authorization", &format!("Bearer {access}"))
                    .body(form)
                    .map_err(|e| AuthError::Transport(e.to_string()))?
                    .send()
                    .await
                    .map_err(|e| AuthError::Transport(e.to_string()))?;

                let status = resp.status();
                let body = resp
                    .text()
                    .await
                    .map_err(|e| AuthError::Transport(e.to_string()))?;
                if status == 401 {
                    let msg = serde_json::from_str::<ErrorResponse>(&body)
                        .map(|e| e.error)
                        .unwrap_or_else(|_| body.clone());
                    return Err(AuthError::Unauthorized(msg));
                }
                Ok((status, body))
            }
        })
        .await
        .map_err(ApiError::from)?;

    api::classify_maybe_empty::<()>(status, &body)
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn upload_youtube_cookie_file(
    _gate: Arc<RefreshGate>,
    _file: (),
) -> Result<(), ApiError> {
    Err(ApiError::UnsupportedTarget)
}
