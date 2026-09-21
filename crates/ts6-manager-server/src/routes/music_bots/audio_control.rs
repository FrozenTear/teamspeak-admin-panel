//! `/music-bots/{id}/(play|pause|resume|stop|skip-next|skip-prev|volume)`
//! — audio control surface (PURA-126 WS-6 follow-up).
//!
//! Each route lowers to a `BotCommand::Audio(...)` dispatch via
//! `BotSupervisor::send`. `play` additionally writes a `MusicRequest`
//! row to the request log so the FE's "recently requested" widget shows
//! direct-source plays alongside playlist enqueues + radio plays.

use std::path::{Component, Path as FsPath, PathBuf};

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chrono::Utc;
use music_bot::{AudioCommand, AudioSource as DomainAudioSource, BotCommand};
use ts6_manager_shared::music_bots as wire;
use ts6_ssrf::{Resolver, SsrfError, is_url_allowed};

use crate::app_state::AppState;
use crate::auth::extractors::RequireAuth;
use crate::routes::music_bots::{err_with_code, translate_send_error};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route("/api/music-bots/{id}/play", post(play))
        .route("/api/music-bots/{id}/pause", post(pause))
        .route("/api/music-bots/{id}/resume", post(resume))
        .route("/api/music-bots/{id}/stop", post(stop))
        .route("/api/music-bots/{id}/skip-next", post(skip_next))
        .route("/api/music-bots/{id}/skip-prev", post(skip_prev))
        .route("/api/music-bots/{id}/volume", post(volume))
        .route("/api/music-bots/{id}/seek", post(seek))
}

async fn play(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::PlayRequest>,
) -> Result<StatusCode, Response> {
    let bot = music_bot::BotId(id);
    // Gate before the supervisor sees the source. yt-dlp and ffmpeg do
    // their own I/O; a blocked URL or a path outside MUSIC_DIR must not
    // leave this handler.
    let domain_source =
        match gate_play_source(&req.source, state.ssrf_resolver.as_ref(), &state.music_dir).await {
            Ok(source) => source,
            Err(resp) => return Err(resp),
        };
    state
        .music_bots
        .supervisor
        .send(
            bot,
            BotCommand::Audio(AudioCommand::Play {
                source: domain_source,
            }),
        )
        .await
        .map_err(translate_send_error)?;
    // Side-effect: record a MusicRequest row mirroring the
    // `/radio-stations/{id}/play` handler. `track_id` is `None` because
    // the play bypasses the queue. Title falls back to the source string
    // when the caller didn't supply one (no body field for it).
    let title = source_label(&req.source);
    state
        .music_bots
        .requests
        .record(wire::MusicRequest {
            id: 0,
            bot: wire::BotId(id),
            track_id: None,
            source: req.source,
            title,
            requested_by: None,
            requested_at: Utc::now(),
        })
        .await;
    Ok(StatusCode::ACCEPTED)
}

async fn pause(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Pause).await
}

async fn resume(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Resume).await
}

async fn stop(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Stop).await
}

async fn skip_next(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::SkipNext).await
}

async fn skip_prev(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::SkipPrev).await
}

async fn volume(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::SetVolumeRequest>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::SetVolume(req.gain)).await
}

/// PURA-352 — scrub the current track to a position. Lowers to
/// `AudioCommand::Seek`; the bot re-spawns the decoder at the offset
/// reusing the already-resolved stream URL (no yt-dlp re-resolution).
async fn seek(
    State(state): State<AppState>,
    RequireAuth(_user): RequireAuth,
    Path(id): Path<u64>,
    Json(req): Json<wire::SeekRequest>,
) -> Result<StatusCode, Response> {
    dispatch_audio(state, id, AudioCommand::Seek { secs: req.secs }).await
}

async fn dispatch_audio(
    state: AppState,
    id: u64,
    cmd: AudioCommand,
) -> Result<StatusCode, Response> {
    state
        .music_bots
        .supervisor
        .send(music_bot::BotId(id), BotCommand::Audio(cmd))
        .await
        .map_err(translate_send_error)?;
    Ok(StatusCode::ACCEPTED)
}

/// SSRF-check a URL, or jail a library path under `music_dir`, before
/// `AudioCommand::Play` is dispatched.
///
/// Plaintext HTTP with no pinned address is refused (DNS failure or an
/// empty answer). yt-dlp resolves names itself and there is no
/// `resolve_to_addrs` hook on that path — handing it an unpinned `http`
/// URL is the webhook bug. HTTPS with no pin is allowed: TLS hostname
/// validation binds the name, same split as manager webhooks.
///
/// When plaintext HTTP *does* resolve to a public address, the original
/// URL is forwarded. yt-dlp cannot take reqwest's `resolve_to_addrs`
/// pin (that pin is what `flow/dispatch.rs` uses for webhooks). A name
/// that passed the blocklist can still rebind before yt-dlp connects;
/// closing that needs a pin proxy and is out of scope for this gate.
/// Private, loopback, and metadata targets are rejected here either way.
async fn gate_play_source(
    source: &wire::AudioSource,
    resolver: &dyn Resolver,
    music_dir: &FsPath,
) -> Result<DomainAudioSource, Response> {
    match source {
        wire::AudioSource::Url { url } => gate_play_url(url, resolver).await,
        wire::AudioSource::LibraryPath { path } => confine_library_path(music_dir, path)
            .map(DomainAudioSource::LibraryPath)
            .map_err(|message| err_with_code(StatusCode::BAD_REQUEST, message, "validation")),
    }
}

async fn gate_play_url(url: &str, resolver: &dyn Resolver) -> Result<DomainAudioSource, Response> {
    let target = match is_url_allowed(url, resolver).await {
        Ok(target) => target,
        Err(err) => {
            return Err(ssrf_rejected(&err));
        }
    };
    if target.url.scheme() == "http" && target.resolved_ip.is_none() {
        return Err(err_with_code(
            StatusCode::BAD_REQUEST,
            "plaintext HTTP URL has no pinned address",
            "ssrf_blocked",
        ));
    }
    Ok(DomainAudioSource::Url(url.to_string()))
}

fn ssrf_rejected(err: &SsrfError) -> Response {
    let body = wire::ErrorBody::new("URL rejected by SSRF policy")
        .with_code("ssrf_blocked")
        .with_details(err.to_string());
    (StatusCode::BAD_REQUEST, Json(body)).into_response()
}

/// Canonicalise `raw` and require the result to be a file inside
/// `music_dir`. Absolute paths, `..`, protocol-looking strings, missing
/// files, and symlinks that land outside the root are rejected.
pub(crate) fn confine_library_path(music_dir: &FsPath, raw: &str) -> Result<PathBuf, &'static str> {
    if raw.is_empty() || raw.contains('\0') {
        return Err("library path is empty or contains NUL");
    }
    if raw.contains(':') || raw.contains('\\') {
        return Err("library path must be a relative path under MUSIC_DIR");
    }
    let raw_path = FsPath::new(raw);
    if raw_path.is_absolute()
        || raw_path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("library path escapes MUSIC_DIR");
    }
    let root = music_dir
        .canonicalize()
        .map_err(|_| "MUSIC_DIR is not available")?;
    let canon = root
        .join(raw_path)
        .canonicalize()
        .map_err(|_| "library path is not a file under MUSIC_DIR")?;
    if !canon.starts_with(&root) || !canon.is_file() {
        return Err("library path escapes MUSIC_DIR");
    }
    Ok(canon)
}

/// Best-effort title for a request-log row when the caller didn't
/// supply one (the `play` endpoint takes only `{ source }` — no title
/// field). Mirrors what an operator would see in the FE's request list:
/// the URL or library path is enough to identify the source.
fn source_label(source: &wire::AudioSource) -> String {
    match source {
        wire::AudioSource::Url { url } => url.clone(),
        wire::AudioSource::LibraryPath { path } => path.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use ts6_ssrf::MockResolver;

    fn scratch() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ts6-music-jail-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn library_path_stays_inside_music_dir() {
        let root = scratch();
        fs::create_dir_all(root.join("a")).unwrap();
        fs::write(root.join("a/b.mp3"), b"x").unwrap();
        let got = confine_library_path(&root, "a/b.mp3").unwrap();
        assert_eq!(got, root.canonicalize().unwrap().join("a/b.mp3"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_path_rejects_dotdot_absolute_and_protocol() {
        let root = scratch();
        fs::write(root.join("ok.mp3"), b"x").unwrap();
        assert!(confine_library_path(&root, "../ok.mp3").is_err());
        assert!(confine_library_path(&root, "/etc/passwd").is_err());
        assert!(confine_library_path(&root, "http://127.0.0.1/a.mp3").is_err());
        assert!(confine_library_path(&root, "concat:ok.mp3").is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn library_path_rejects_symlink_escape() {
        let root = scratch();
        let outside = scratch();
        fs::write(outside.join("secret.mp3"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.mp3"), root.join("link.mp3")).unwrap();
        assert!(
            confine_library_path(&root, "link.mp3").is_err(),
            "symlink out of MUSIC_DIR must be rejected"
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[tokio::test]
    async fn play_url_rejects_loopback_and_unpinned_http() {
        let resolver = MockResolver::new();
        let err = gate_play_url("http://127.0.0.1/hook", &resolver)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        let err = gate_play_url("http://missing.example/a.mp3", &resolver)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        let body = http_body_util::BodyExt::collect(err.into_body())
            .await
            .unwrap()
            .to_bytes();
        let parsed: wire::ErrorBody = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed.code.as_deref(), Some("ssrf_blocked"));
        assert!(
            parsed.error.contains("no pinned") || parsed.error.contains("SSRF"),
            "got {parsed:?}"
        );
    }

    #[tokio::test]
    async fn play_url_allows_public_http_literal_and_unpinned_https() {
        let resolver = MockResolver::new();
        match gate_play_url("http://203.0.113.10/a.mp3", &resolver)
            .await
            .unwrap()
        {
            DomainAudioSource::Url(url) => assert!(url.contains("203.0.113.10")),
            other => panic!("expected url, got {other:?}"),
        }
        // HTTPS DNS miss is not fail-closed. Documented.
        assert!(
            gate_play_url("https://missing.example/a.mp3", &resolver)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn play_url_rejects_dns_answer_in_a_private_range() {
        let resolver =
            MockResolver::new().with("rebind.example", vec!["10.1.2.3".parse().unwrap()]);
        let err = gate_play_url("http://rebind.example/a.mp3", &resolver)
            .await
            .unwrap_err();
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
