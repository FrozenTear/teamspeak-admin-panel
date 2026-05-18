//! PURA-352 — resolve a yt-dlp URL to its direct media URL.
//!
//! The normal playback path streams `yt-dlp -f bestaudio -o - <url>` into
//! ffmpeg: yt-dlp does the (multi-second, see PURA-330) extraction *and*
//! the download in one process, and the resolved direct media URL is
//! never surfaced. A seek that re-spawned that pipeline would re-pay the
//! whole ~11 s yt-dlp resolution.
//!
//! [`resolve_direct_url`] runs `yt-dlp -g` once — the extraction without
//! the download — so the caller can retain the direct, ffmpeg-consumable
//! URL for the lifetime of the track. A later seek then re-spawns only
//! `ffmpeg -ss <offset> -i <direct-url>`, with no yt-dlp involvement.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// Resolve `url` to a direct, ffmpeg-consumable media URL via
/// `yt-dlp -f bestaudio -g`.
///
/// This runs the full extractor — including YouTube's nsig/signature
/// challenge — so it is as slow as the resolution stage of a normal
/// `!play` (PURA-330). Callers should run it **once**, off the playback
/// critical path, and retain the result for the track's seeks.
///
/// `cookie_file` mirrors the playback path: the resolved Netscape
/// `cookies.txt` path (or `None`), needed for age-gated / rate-limited
/// videos.
pub async fn resolve_direct_url(url: &str, cookie_file: Option<&Path>) -> std::io::Result<String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.arg("--quiet")
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("-f")
        .arg("bestaudio")
        .arg("-g");
    if let Some(p) = cookie_file {
        cmd.arg("--cookies").arg(p);
    }
    cmd.arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = cmd.output().await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(std::io::Error::other(format!(
            "yt-dlp -g exited {}: {}",
            out.status,
            stderr.lines().last().unwrap_or("").trim(),
        )));
    }

    // `-f bestaudio -g` prints one direct URL per line; with a single
    // selected format that is one line. Take the first non-empty line.
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .ok_or_else(|| std::io::Error::other("yt-dlp -g produced no URL"))
}
