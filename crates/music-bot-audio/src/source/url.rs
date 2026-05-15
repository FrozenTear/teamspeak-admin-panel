//! `yt-dlp` → `ffmpeg` chained source. yt-dlp picks the best audio stream and
//! pipes raw container bytes into a tokio task that forwards them to ffmpeg's
//! stdin; ffmpeg decodes to s16le PCM.
//!
//! We bridge yt-dlp's stdout → ffmpeg's stdin in user-space (rather than via
//! `Stdio::from(OwnedFd)`) because tokio's `ChildStdin` does not expose an
//! `into_owned_fd` accessor on stable, and the byte volume is small enough
//! that the userspace copy is in the noise compared to the network fetch.

use std::io;
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
    inner: FfmpegSource,
}

impl YtDlpSource {
    pub async fn new(url: &str, channels: u8) -> io::Result<Self> {
        // Spawn ffmpeg first, take its stdin so we can pipe yt-dlp output in.
        let (inner, mut ffmpeg_stdin) = FfmpegSource::from_stdin(channels).await?;

        let mut cmd = Command::new("yt-dlp");
        cmd.kill_on_drop(true)
            .arg("--quiet")
            .arg("--no-warnings")
            .arg("--no-playlist")
            .arg("-f")
            .arg("bestaudio")
            .arg("-o")
            .arg("-")
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut yt_dlp = cmd.spawn()?;
        let mut yt_stdout = yt_dlp
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("yt-dlp child has no stdout"))?;

        // Log yt-dlp stderr so signature/cipher errors are visible to operators.
        let yt_stderr = yt_dlp
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("yt-dlp child has no stderr"))?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(yt_stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "yt_dlp", "{}", line);
            }
        });

        // Bridge yt-dlp.stdout → ffmpeg.stdin in a background task. Closes
        // ffmpeg's stdin when yt-dlp finishes so ffmpeg sees clean EOF.
        let bridge = tokio::spawn(async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match yt_stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
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
            inner,
        })
    }
}

impl Drop for YtDlpSource {
    fn drop(&mut self) {
        if let Some(handle) = self.bridge.take() {
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
        self.inner.read_samples(buf).await
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        self.inner.try_drain_events()
    }
}
