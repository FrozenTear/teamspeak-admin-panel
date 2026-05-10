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
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdout, Command};

use super::PcmSource;
use crate::types::{PipelineEvent, SAMPLE_RATE_HZ};

pub struct FfmpegSource {
    child: Option<Child>,
    stdout: BufReader<ChildStdout>,
    channels: u8,
    /// Pending warning events (e.g. ffmpeg startup hiccup).
    events: Vec<PipelineEvent>,
}

impl FfmpegSource {
    /// Spawn `ffmpeg -i <input>` with the output reformatted to `s16le` at
    /// 48 kHz with `channels` channels. `input` can be any string ffmpeg's
    /// `-i` accepts: a local path, an `http://` URL, an `icecast://` URL, etc.
    pub async fn from_input(input: &str, channels: u8) -> io::Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.kill_on_drop(true)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-i")
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
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stdout"))?;
        Ok(Self {
            child: Some(child),
            stdout: BufReader::with_capacity(64 * 1024, stdout),
            channels,
            events: Vec::new(),
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
            .stderr(Stdio::null());

        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("ffmpeg child has no stdout"))?;
        Ok((
            Self {
                child: Some(child),
                stdout: BufReader::with_capacity(64 * 1024, stdout),
                channels,
                events: Vec::new(),
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
        Ok(whole_samples)
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        std::mem::take(&mut self.events)
    }
}
