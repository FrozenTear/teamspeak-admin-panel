//! `ffmpeg` subprocess source. Decodes anything ffmpeg can read into 48 kHz
//! s16le interleaved PCM on stdout.
//!
//! Why not `symphonia`: the music bot must accept whatever the operator throws
//! at it (mp3, aac, flac, ogg, opus, webm, hls, http radio …). Tracking
//! symphonia codec coverage one-by-one is weeks of work. ffmpeg is the standard
//! off-the-shelf piece, already required for the broader voice ecosystem, and
//! the CLI surface is stable enough to wrap. Decision rationale lives in
//! `docs/voice/audio-pipeline.md`.

use std::io;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

use super::PcmSource;
use crate::types::{PipelineEvent, SAMPLE_RATE_HZ};

pub struct FfmpegSource {
    child: Option<Child>,
    stdout: BufReader<ChildStdout>,
    channels: u8,
    /// Pending warning events (e.g. ffmpeg startup hiccup).
    events: Vec<PipelineEvent>,
    /// PURA-330 — process spawn time, used to log time-to-first-PCM so the
    /// `!play` → first-audio latency can be attributed per stage.
    spawned_at: std::time::Instant,
    /// PURA-330 — set once the first non-empty PCM read has been logged.
    first_pcm_logged: bool,
}

impl FfmpegSource {
    /// Spawn `ffmpeg -i <input>` with the output reformatted to `s16le` at
    /// 48 kHz with `channels` channels. `input` can be any string ffmpeg's
    /// `-i` accepts: a local path, an `http://` URL, an `icecast://` URL, etc.
    ///
    /// PURA-352 — `start_secs`, when `Some`, places `-ss <secs>` *before*
    /// `-i` so ffmpeg seeks the *input* (a fast HTTP range request on a
    /// remote URL, a container index seek on a local file) rather than
    /// decoding and discarding from the start.
    pub async fn from_input(
        input: &str,
        channels: u8,
        start_secs: Option<u64>,
    ) -> io::Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.kill_on_drop(true)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin");
        // PURA-352 — input-side seek. Must precede `-i` to apply to the
        // next input. Omitted (no `-ss`) for a normal start-at-zero play.
        if let Some(secs) = start_secs {
            cmd.arg("-ss").arg(secs.to_string());
        }
        // PURA-352 — a seek re-spawns ffmpeg directly on a resolved media
        // URL, so ffmpeg's HTTP client now serves the rest of the track.
        // Let it ride out transient drops instead of ending playback.
        // `-reconnect*` are http(s)-protocol options — only valid for an
        // http input, and ffmpeg errors if they are passed for a local file.
        if input.starts_with("http://") || input.starts_with("https://") {
            cmd.arg("-reconnect")
                .arg("1")
                .arg("-reconnect_streamed")
                .arg("1")
                .arg("-reconnect_delay_max")
                .arg("2");
        }
        cmd.arg("-i")
            .arg(input)
            .arg("-vn")
            .arg("-f")
            .arg("s16le")
            .arg("-acodec")
            .arg("pcm_s16le")
            .arg("-ar")
            .arg(SAMPLE_RATE_HZ.to_string())
            .arg("-ac")
            .arg(channels.to_string())
            .arg("pipe:1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stderr"))?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "ffmpeg", "{}", line);
            }
        });
        Ok(Self {
            child: Some(child),
            stdout: BufReader::with_capacity(64 * 1024, stdout),
            channels,
            events: Vec::new(),
            spawned_at: std::time::Instant::now(),
            first_pcm_logged: false,
        })
    }

    /// Spawn ffmpeg reading from `pipe:0` instead of a URL/path. Returns the
    /// source plus the writable stdin end so the caller can pipe arbitrary
    /// bytes (e.g. yt-dlp output, ICY-stripped radio body) into ffmpeg.
    pub async fn from_stdin(channels: u8) -> io::Result<(Self, tokio::process::ChildStdin)> {
        let mut cmd = Command::new("ffmpeg");
        cmd.kill_on_drop(true)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            // PURA-330 — cap container probing on the piped yt-dlp stream.
            // ffmpeg's defaults (5 MB probesize / 5 s analyzeduration) make
            // it buffer and inspect several seconds of input before it
            // emits the first PCM byte; for `yt-dlp -f bestaudio` output
            // (a single webm/opus or m4a/aac elementary stream) a fraction
            // of that is plenty to detect the format. This trims dead time
            // from `!play` → first-audio without affecting decode quality.
            .arg("-probesize")
            .arg("256k")
            .arg("-analyzeduration")
            .arg("1000000")
            .arg("-i")
            .arg("pipe:0")
            .arg("-vn")
            .arg("-f")
            .arg("s16le")
            .arg("-acodec")
            .arg("pcm_s16le")
            .arg("-ar")
            .arg(SAMPLE_RATE_HZ.to_string())
            .arg("-ac")
            .arg(channels.to_string())
            .arg("pipe:1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stderr"))?;
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "ffmpeg", "{}", line);
            }
        });
        Ok((
            Self {
                child: Some(child),
                stdout: BufReader::with_capacity(64 * 1024, stdout),
                channels,
                events: Vec::new(),
                spawned_at: std::time::Instant::now(),
                first_pcm_logged: false,
            },
            stdin,
        ))
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn push_event(&mut self, ev: PipelineEvent) {
        self.events.push(ev);
    }
}

impl Drop for FfmpegSource {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // `kill_on_drop(true)` already arms the runtime to reap on drop, but
            // we call `start_kill` explicitly so the SIGKILL races with the
            // current runtime tick instead of an arbitrary tokio reaper turn.
            // Failures are logged (the child may already have exited).
            if let Err(e) = child.start_kill() {
                tracing::debug!(?e, "ffmpeg child kill failed (likely already exited)");
            }
        }
    }
}

#[async_trait]
impl PcmSource for FfmpegSource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        // Read whole `i16` little-endian samples. We may receive a partial
        // sample at the very end of stdout — round down to the previous whole
        // sample boundary and discard the trailing odd byte (ffmpeg shouldn't
        // produce one, but treat it defensively).
        let bytes_wanted = buf.len() * 2;
        let mut byte_buf = vec![0u8; bytes_wanted];
        let mut filled = 0usize;
        while filled < bytes_wanted {
            match self.stdout.read(&mut byte_buf[filled..]).await? {
                0 => break, // EOF
                n => filled += n,
            }
            // Read at least one full sample before yielding back so callers
            // see whole frames. If we already have ≥ 1 sample and the read
            // hit a short window (typical for piped ffmpeg), fall through
            // and let the caller decide whether to ask for more.
            if filled >= 2 {
                break;
            }
        }
        let whole_samples = filled / 2;
        for i in 0..whole_samples {
            buf[i] = i16::from_le_bytes([byte_buf[i * 2], byte_buf[i * 2 + 1]]);
        }
        // PURA-330 — log time-to-first-PCM once. This is the boundary
        // between "ffmpeg has the input + finished probing" and "decoded
        // audio is flowing"; attributing the `!play` latency needs it.
        if whole_samples > 0 && !self.first_pcm_logged {
            self.first_pcm_logged = true;
            tracing::info!(
                target: "music_bot_latency",
                stage = "ffmpeg_first_pcm",
                elapsed_ms = self.spawned_at.elapsed().as_millis() as u64,
                "ffmpeg emitted first decoded PCM",
            );
        }
        Ok(whole_samples)
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        std::mem::take(&mut self.events)
    }
}
