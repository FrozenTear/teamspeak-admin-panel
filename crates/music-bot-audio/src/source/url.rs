//! `yt-dlp` → `ffmpeg` chained source. yt-dlp picks the best audio stream and
//! pipes raw container bytes into a tokio task that forwards them to ffmpeg's
//! stdin; ffmpeg decodes to s16le PCM.
//!
//! We bridge yt-dlp's stdout → ffmpeg's stdin in user-space (rather than via
//! `Stdio::from(OwnedFd)`) because tokio's `ChildStdin` does not expose an
//! `into_owned_fd` accessor on stable, and the byte volume is small enough
//! that the userspace copy is in the noise compared to the network fetch.

use std::io;
use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;

use super::PcmSource;
use super::ffmpeg::FfmpegSource;
use crate::types::PipelineEvent;

pub struct YtDlpSource {
    yt_dlp: Option<Child>,
    bridge: Option<JoinHandle<()>>,
    /// Stderr reader task. Returns the `ERROR:` lines yt-dlp printed so the
    /// pipeline can surface *why* a play failed (PURA-314). Taken (→ `None`)
    /// once `collect_diagnostics` has drained it.
    stderr_task: Option<JoinHandle<Vec<String>>>,
    /// Diagnostic events queued for the next `try_drain_events` call.
    diagnostics: Vec<PipelineEvent>,
    inner: FfmpegSource,
}

impl YtDlpSource {
    /// Spawn a yt-dlp → ffmpeg pipeline.
    ///
    /// `cookie_file` is the resolved Netscape `cookies.txt` path (or `None`).
    /// Callers resolve it from `app_setting:yt_cookie_path` (DB) with
    /// `YT_COOKIE_FILE` env as the boot-time fallback; the path is read at
    /// call-site so a UI cookie upload takes effect on the *next* track
    /// without a manager restart.
    pub async fn new(url: &str, channels: u8, cookie_file: Option<&Path>) -> io::Result<Self> {
        // PURA-330 — anchor for the per-stage latency log. yt-dlp URL
        // resolution (YouTube nsig/signature challenge via Deno on a
        // datacenter IP) is the dominant unknown in `!play` → first-audio;
        // logging time-to-first-bytes turns it into a measured number.
        let spawn_t0 = std::time::Instant::now();

        // Spawn ffmpeg first, take its stdin so we can pipe yt-dlp output in.
        let (inner, mut ffmpeg_stdin) = FfmpegSource::from_stdin(channels).await?;

        let mut cmd = Command::new("yt-dlp");
        cmd.kill_on_drop(true)
            .arg("--quiet")
            .arg("--no-warnings")
            .arg("--no-playlist")
            // PURA-361 — cold-path tail hardening. PURA-355 caught a
            // watch-page HTTP request stalling ~41 s silently (no 429, no
            // log line) on this `!play` resolution path. `--socket-timeout`
            // bounds every outbound request (watch page, player JS) to
            // ~6× the ~1.7 s healthy network cost (PURA-355), and
            // `--extractor-retries 1` lets yt-dlp reissue a timed-out
            // watch-page fetch once on a fresh request. Worst case the
            // resolution now fails in ~20 s with a clear error (surfaced
            // via the stderr `ERROR:` capture below) instead of hanging
            // the `!play` indefinitely.
            .arg("--socket-timeout")
            .arg(crate::resolve::SOCKET_TIMEOUT_SECS.to_string())
            .arg("--extractor-retries")
            .arg("1")
            .arg("-f")
            .arg("bestaudio")
            .arg("-o")
            .arg("-");

        // PURA-223 — cookie path plumbed from PipelineConfig (resolved from
        // `app_setting:yt_cookie_path` by the caller at play-time, with
        // `YT_COOKIE_FILE` env as the boot fallback). Needed for age-gated,
        // region-locked, and rate-limited videos.
        if let Some(p) = cookie_file {
            cmd.arg("--cookies").arg(p);
            tracing::debug!(target: "yt_dlp", cookie_file = %p.display(), "passing cookies file to yt-dlp");
        }

        cmd.arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut yt_dlp = cmd.spawn()?;
        tracing::info!(
            target: "music_bot_latency",
            stage = "yt_dlp_spawned",
            elapsed_ms = spawn_t0.elapsed().as_millis() as u64,
            "yt-dlp process spawned — resolving URL",
        );
        let mut yt_stdout = yt_dlp
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("yt-dlp child has no stdout"))?;

        // Log yt-dlp stderr so signature/cipher errors are visible to
        // operators, AND collect the `ERROR:` lines so the pipeline can
        // surface the cause to the UI — yt-dlp failures used to be log-only
        // and the bot just reported a generic "0 frames" (PURA-314).
        let yt_stderr = yt_dlp
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("yt-dlp child has no stderr"))?;
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(yt_stderr).lines();
            let mut errors = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "yt_dlp", "{}", line);
                if line.contains("ERROR:") {
                    errors.push(line);
                }
            }
            errors
        });

        // Bridge yt-dlp.stdout → ffmpeg.stdin in a background task. Closes
        // ffmpeg's stdin when yt-dlp finishes so ffmpeg sees clean EOF.
        let bridge = tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            let mut first_bytes_logged = false;
            loop {
                match yt_stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        // PURA-330 — first bytes out of yt-dlp marks the end
                        // of URL resolution + the start of the media fetch:
                        // the single largest latency stage in `!play`.
                        if !first_bytes_logged {
                            first_bytes_logged = true;
                            tracing::info!(
                                target: "music_bot_latency",
                                stage = "yt_dlp_first_bytes",
                                elapsed_ms = spawn_t0.elapsed().as_millis() as u64,
                                "yt-dlp produced first bytes — URL resolved, fetch started",
                            );
                        }
                        if ffmpeg_stdin.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let _ = ffmpeg_stdin.shutdown().await;
        });

        Ok(Self {
            yt_dlp: Some(yt_dlp),
            bridge: Some(bridge),
            stderr_task: Some(stderr_task),
            diagnostics: Vec::new(),
            inner,
        })
    }

    /// Drain the yt-dlp stderr task once the pipeline has hit EOF. Called
    /// from `read_samples` the first time the inner ffmpeg source reports
    /// EOF — at that point yt-dlp has already exited (its exit is what
    /// closed ffmpeg's stdin and ended ffmpeg), so awaiting the stderr
    /// reader is bounded. Each captured `ERROR:` line becomes a
    /// `PipelineEvent::Warning` carrying an operator-readable cause.
    async fn collect_diagnostics(&mut self) {
        let Some(task) = self.stderr_task.take() else {
            return;
        };
        if let Ok(errors) = task.await {
            for line in errors {
                self.diagnostics
                    .push(PipelineEvent::Warning(classify_yt_dlp_error(&line)));
            }
        }
    }
}

/// Turn a raw yt-dlp `ERROR:` stderr line into an operator-readable cause.
///
/// yt-dlp's wire messages are terse and link-heavy; the music-bot UI shows
/// this string verbatim in the per-bot "playback failed" banner, so known
/// failure modes get a plain-language rewrite that tells the operator what
/// to *do*. Unknown errors fall through with just the `ERROR: ` prefix
/// stripped.
pub fn classify_yt_dlp_error(raw: &str) -> String {
    let msg = raw.trim();
    let lower = msg.to_lowercase();

    // The single most common production failure: YouTube rate-limits the
    // datacenter IP and demands a signed-in session. The fix is operator
    // config, not a retry — say so. (PURA-314)
    if lower.contains("sign in to confirm")
        || lower.contains("confirm you’re not a bot")
        || lower.contains("confirm you're not a bot")
        || (lower.contains("--cookies") && lower.contains("authentication"))
    {
        return "YouTube is asking the bot to sign in to confirm it is not a bot. \
                No YouTube cookies are configured — upload a Netscape cookies.txt in \
                the manager settings (or set YT_COOKIE_FILE) so yt-dlp can authenticate."
            .to_string();
    }
    if lower.contains("private video") {
        return "This video is private and cannot be played.".to_string();
    }
    if lower.contains("video unavailable") || lower.contains("is not available") {
        return "This video is unavailable (removed, region-locked, or geo-blocked \
                for the server's location)."
            .to_string();
    }
    if lower.contains("age") && lower.contains("restrict") {
        return "This video is age-restricted — yt-dlp needs a signed-in YouTube \
                cookies.txt to play it."
            .to_string();
    }
    if lower.contains("members-only") || lower.contains("join this channel") {
        return "This video is members-only and needs a subscribed account's \
                cookies.txt to play."
            .to_string();
    }
    if lower.contains("unsupported url") {
        return "yt-dlp does not recognise this URL — check the link is a \
                supported site."
            .to_string();
    }

    // Unknown error — strip the noisy `ERROR: ` prefix and pass it through.
    msg.strip_prefix("ERROR:")
        .map(str::trim)
        .unwrap_or(msg)
        .to_string()
}

impl Drop for YtDlpSource {
    fn drop(&mut self) {
        if let Some(handle) = self.bridge.take() {
            handle.abort();
        }
        if let Some(handle) = self.stderr_task.take() {
            handle.abort();
        }
        if let Some(mut child) = self.yt_dlp.take()
            && let Err(e) = child.start_kill()
        {
            tracing::debug!(?e, "yt-dlp child kill failed (likely already exited)");
        }
    }
}

#[async_trait]
impl PcmSource for YtDlpSource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        let n = self.inner.read_samples(buf).await?;
        if n == 0 {
            // Inner ffmpeg hit EOF — yt-dlp has already exited. Collect its
            // stderr diagnostics so `try_drain_events` can surface the
            // cause before the pipeline emits `EndOfStream` (PURA-314).
            self.collect_diagnostics().await;
        }
        Ok(n)
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        let mut out = std::mem::take(&mut self.diagnostics);
        out.extend(self.inner.try_drain_events());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::classify_yt_dlp_error;

    #[test]
    fn cookie_gate_error_maps_to_actionable_message() {
        // The exact wording yt-dlp emits in production (PURA-314).
        let raw = "ERROR: [youtube] cvaIgq5j2Q8: Sign in to confirm you’re not a bot. \
                   Use --cookies-from-browser or --cookies for the authentication.";
        let msg = classify_yt_dlp_error(raw);
        assert!(msg.contains("cookies.txt"), "got: {msg}");
        assert!(!msg.contains("ERROR:"), "raw prefix leaked: {msg}");
    }

    #[test]
    fn private_and_unavailable_videos_are_recognised() {
        assert!(
            classify_yt_dlp_error(
                "ERROR: [youtube] x: Private video. Sign in if you've been granted access"
            )
            .contains("private")
        );
        assert!(
            classify_yt_dlp_error("ERROR: [youtube] x: Video unavailable")
                .to_lowercase()
                .contains("unavailable")
        );
    }

    #[test]
    fn unknown_error_passes_through_without_prefix() {
        let msg = classify_yt_dlp_error("ERROR: something nobody has classified yet");
        assert_eq!(msg, "something nobody has classified yet");
    }

    #[test]
    fn line_without_error_prefix_is_returned_verbatim() {
        let msg = classify_yt_dlp_error("WARNING: just a warning");
        assert_eq!(msg, "WARNING: just a warning");
    }
}
