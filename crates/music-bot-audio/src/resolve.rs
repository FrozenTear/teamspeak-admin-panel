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
//!
//! PURA-361 — the cold-path tail is hardened here: every outbound YouTube
//! HTTP request carries a [`SOCKET_TIMEOUT_SECS`] bound, and the whole
//! `yt-dlp -g` process runs under a [`PROCESS_TIMEOUT`] wall-clock budget
//! with one fresh-process retry, so a stalled watch-page fetch fails fast
//! instead of hanging the caller.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Per-request socket timeout passed to yt-dlp (`--socket-timeout`).
///
/// PURA-355 caught a watch-page HTTP request stalling ~41 s with no 429
/// and no log line — just a dead-slow socket. yt-dlp installs no socket
/// timeout by default, so a stalled read blocks until the OS gives up
/// (minutes). A healthy watch-page fetch is ~1.7 s of network (PURA-355);
/// 10 s is ~6× that — wide enough not to false-trip a merely-slow but
/// healthy connection, tight enough to turn the 41 s outlier into a fast
/// failure that a retry can paper over. Shared with the streaming
/// `yt-dlp` source (`source::url`) so both YouTube fetch paths agree.
pub(crate) const SOCKET_TIMEOUT_SECS: u32 = 10;

/// Wall-clock budget for one whole `yt-dlp -g` invocation.
///
/// A normal resolution is ~7 s (PURA-330 / PURA-355: ~4.4 s local nsig
/// solve + ~1.7 s network). 25 s is ~3.5× the steady-state cost — past it
/// the process is treated as wedged, killed, and retried once. This is the
/// backstop for a stall that slips under `--socket-timeout` (e.g. a socket
/// that trickles bytes just often enough to keep each read alive).
const PROCESS_TIMEOUT: Duration = Duration::from_secs(25);

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
///
/// PURA-361 — runs under a [`PROCESS_TIMEOUT`] budget with one automatic
/// retry on a timeout (whether the wall-clock budget was blown or yt-dlp
/// itself reported a socket timeout). The retry is a fresh process, hence
/// a fresh connection, which almost always clears a transient stall. A
/// non-timeout failure (private/unavailable video, unsupported URL) is
/// returned immediately — retrying it would only waste another ~7 s.
pub async fn resolve_direct_url(url: &str, cookie_file: Option<&Path>) -> std::io::Result<String> {
    match run_once(url, cookie_file).await {
        Ok(direct) => Ok(direct),
        Err(first) if first.kind() == std::io::ErrorKind::TimedOut => {
            tracing::warn!(
                error = %first,
                "PURA-361 yt-dlp -g resolve timed out — retrying once with a fresh process",
            );
            run_once(url, cookie_file).await.map_err(|second| {
                if second.kind() == std::io::ErrorKind::TimedOut {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        format!("yt-dlp -g timed out on two consecutive attempts: {second}"),
                    )
                } else {
                    second
                }
            })
        }
        Err(other) => Err(other),
    }
}

/// One `yt-dlp -g` attempt, bounded by [`PROCESS_TIMEOUT`].
///
/// A blown wall-clock budget and a yt-dlp exit whose stderr names a
/// transient network timeout both surface as [`std::io::ErrorKind::TimedOut`]
/// so the caller's retry sees a single, uniform "try again" signal.
async fn run_once(url: &str, cookie_file: Option<&Path>) -> std::io::Result<String> {
    let mut cmd = Command::new("yt-dlp");
    cmd.kill_on_drop(true)
        .arg("--quiet")
        .arg("--no-warnings")
        .arg("--no-playlist")
        // PURA-361 — bound every outbound HTTP request (watch page,
        // player JS, …) so a stalled socket fails fast.
        .arg("--socket-timeout")
        .arg(SOCKET_TIMEOUT_SECS.to_string())
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

    let child = cmd.spawn()?;
    let out = match tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_elapsed) => {
            // Budget blown. The `wait_with_output` future is dropped here,
            // which drops the `Child`; `kill_on_drop` SIGKILLs the wedged
            // process so it cannot linger past this attempt.
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "yt-dlp -g exceeded the {}s resolution budget",
                    PROCESS_TIMEOUT.as_secs(),
                ),
            ));
        }
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = format!(
            "yt-dlp -g exited {}: {}",
            out.status,
            stderr.lines().last().unwrap_or("").trim(),
        );
        // A socket timeout *inside* yt-dlp surfaces as a non-zero exit, not
        // as a blown wall-clock budget — classify it as `TimedOut` so the
        // one retry covers it too. Permanent failures stay non-retryable.
        return Err(if stderr_is_transient(&stderr) {
            std::io::Error::new(std::io::ErrorKind::TimedOut, msg)
        } else {
            std::io::Error::other(msg)
        });
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

/// Does `stderr` describe a transient network timeout — something a fresh
/// connection is likely to clear — rather than a permanent failure
/// (private/unavailable video, unsupported URL)?
fn stderr_is_transient(stderr: &str) -> bool {
    let lower = stderr.to_lowercase();
    const MARKERS: &[&str] = &[
        "timed out",
        "timeout",
        "read operation",
        "connection reset",
        "connection aborted",
        "connection refused",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::stderr_is_transient;

    #[test]
    fn socket_timeout_stderr_is_classified_transient() {
        // The shapes yt-dlp prints when `--socket-timeout` trips.
        assert!(stderr_is_transient(
            "ERROR: Unable to download webpage: The read operation timed out"
        ));
        assert!(stderr_is_transient("ERROR: <urlopen error timed out>"));
        assert!(stderr_is_transient(
            "ERROR: Unable to download API page: Connection reset by peer"
        ));
    }

    #[test]
    fn permanent_errors_are_not_transient() {
        // These must NOT retry — a fresh connection cannot fix them.
        assert!(!stderr_is_transient(
            "ERROR: [youtube] x: Private video. Sign in if you've been granted access"
        ));
        assert!(!stderr_is_transient("ERROR: [youtube] x: Video unavailable"));
        assert!(!stderr_is_transient(
            "ERROR: Unsupported URL: https://example.com/not-a-video"
        ));
    }
}
