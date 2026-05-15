//! `/api/settings/youtube-cookies` — PURA-223.
//!
//! Three endpoints that let an operator manage the yt-dlp Netscape cookies
//! file without restarting the manager process:
//!
//! - `PUT  /api/settings/youtube-cookies` — upload a new `cookies.txt`
//! - `DELETE /api/settings/youtube-cookies` — remove the current file
//! - `GET  /api/settings/youtube-cookies` — presence + upload timestamp
//!
//! All three require [`RequireAdmin`] — the cookies file typically contains
//! session tokens and should not be readable or replaceable by non-admins.
//!
//! The file is persisted to `<DATA_DIR>/yt-cookies.txt` atomically (write
//! to a temp path, then rename). The path + upload timestamp are also
//! recorded in `app_setting` rows so the UI can show the upload date
//! after a manager restart.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use axum::Router;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use chrono::Utc;
use serde::Serialize;

use crate::app_state::AppState;
use crate::auth::extractors::RequireAdmin;
use crate::repos::app_settings;

/// `app_setting` key for the persisted cookie file path.
const KEY_PATH: &str = "yt_cookie_path";
/// `app_setting` key for when the file was uploaded (ISO 8601).
const KEY_TS: &str = "yt_cookie_uploaded_at";

/// Maximum accepted cookie file size (64 KiB).
const MAX_SIZE_BYTES: usize = 64 * 1024;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/settings/youtube-cookies",
            get(get_cookies).put(put_cookies).delete(delete_cookies),
        )
}

/// Wire shape for `GET /api/settings/youtube-cookies`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CookieStatus {
    uploaded: bool,
    uploaded_at: Option<String>,
}

async fn get_cookies(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    let row = match app_settings::get(&state.db, KEY_TS).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "app_settings::get failed for yt_cookie_uploaded_at");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let (uploaded, uploaded_at) = match row {
        Some(r) => (true, Some(r.value)),
        None => (false, None),
    };
    Json(CookieStatus { uploaded, uploaded_at }).into_response()
}

async fn put_cookies(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Response {
    // Extract the `file` field from the multipart body.
    let mut file_bytes: Option<Vec<u8>> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) if field.name() == Some("file") => {
                match field.bytes().await {
                    Ok(b) => {
                        file_bytes = Some(b.to_vec());
                        break;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read multipart field bytes");
                        return (StatusCode::BAD_REQUEST, "failed to read upload").into_response();
                    }
                }
            }
            Ok(Some(_)) => continue, // skip unknown fields
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "multipart parsing error");
                return (StatusCode::BAD_REQUEST, "multipart parse error").into_response();
            }
        }
    }

    let bytes = match file_bytes {
        Some(b) => b,
        None => {
            return (StatusCode::BAD_REQUEST, "missing `file` field").into_response();
        }
    };

    // Validate size.
    if bytes.len() > MAX_SIZE_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            format!("file too large (max {} KiB)", MAX_SIZE_BYTES / 1024),
        )
            .into_response();
    }

    // Validate it looks like a text file: reject if >50% non-printable bytes.
    let non_printable = bytes
        .iter()
        .filter(|&&b| b < 0x09 || (b > 0x0D && b < 0x20) || b == 0x7F)
        .count();
    if bytes.len() > 0 && non_printable * 2 > bytes.len() {
        return (StatusCode::BAD_REQUEST, "file does not appear to be text").into_response();
    }

    // Validate the Netscape cookie file header.
    let text = match std::str::from_utf8(&bytes) {
        Ok(t) => t,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "file is not valid UTF-8").into_response();
        }
    };
    let first_content_line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    if !first_content_line.starts_with("# Netscape HTTP Cookie File")
        && !first_content_line.contains('\t')
    {
        return (
            StatusCode::BAD_REQUEST,
            "file does not look like a Netscape cookies.txt (missing header or tab-separated data)",
        )
            .into_response();
    }

    // Write atomically: temp file → rename.
    let cookie_path = state.data_dir.join("yt-cookies.txt");
    let tmp_path = state.data_dir.join("yt-cookies.txt.tmp");

    if let Err(e) = fs::create_dir_all(&state.data_dir) {
        tracing::error!(error = %e, data_dir = %state.data_dir.display(), "failed to create data dir");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)?;
        // chmod 0600 before writing data so the bytes never exist world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(&bytes)?;
        f.flush()?;
        drop(f);
        fs::rename(&tmp_path, &cookie_path)?;
        Ok(())
    })();

    if let Err(e) = write_result {
        tracing::error!(error = %e, path = %cookie_path.display(), "failed to write cookie file");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    let uploaded_at = Utc::now().to_rfc3339();
    let path_str = cookie_path.to_string_lossy().to_string();

    // Persist metadata.
    let db_result = async {
        app_settings::put(&state.db, KEY_PATH, &path_str).await?;
        app_settings::put(&state.db, KEY_TS, &uploaded_at).await
    }
    .await;

    if let Err(e) = db_result {
        tracing::error!(error = %e, "failed to persist yt_cookie app_settings");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Update the live runtime Arc so the next yt-dlp invocation picks it up.
    {
        let mut guard = state.yt_cookie.write().unwrap();
        *guard = Some(cookie_path);
    }

    tracing::info!(path = %path_str, "yt-dlp cookie file uploaded");
    StatusCode::NO_CONTENT.into_response()
}

async fn delete_cookies(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    // Read current path from db so we know what to delete from disk.
    let path_row = match app_settings::get(&state.db, KEY_PATH).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "app_settings::get failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    if let Some(row) = path_row {
        let p = PathBuf::from(&row.value);
        if p.exists() {
            if let Err(e) = fs::remove_file(&p) {
                tracing::warn!(error = %e, path = %p.display(), "failed to remove cookie file");
            }
        }
    }

    // Clear app_setting rows.
    let db_result = async {
        app_settings::delete(&state.db, KEY_PATH).await?;
        app_settings::delete(&state.db, KEY_TS).await
    }
    .await;

    if let Err(e) = db_result {
        tracing::error!(error = %e, "failed to clear yt_cookie app_settings");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    // Clear runtime Arc.
    {
        let mut guard = state.yt_cookie.write().unwrap();
        *guard = None;
    }

    tracing::info!("yt-dlp cookie file deleted");
    StatusCode::NO_CONTENT.into_response()
}
