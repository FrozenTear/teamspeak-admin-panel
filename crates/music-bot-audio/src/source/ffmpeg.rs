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
    /// THE-983 (AR-2) — bytes left over from the previous read that did not
    /// fill a whole channel stride (2 bytes × channels). Holds 0..stride−1
    /// bytes; prepended to the next read so a short write landing on an odd
    /// byte boundary can never tear i16 alignment (full-scale static) or
    /// shift the interleave by one sample (permanent L/R swap on stereo).
    carry: Vec<u8>,
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
            carry: Vec::new(),
        })
    }

    /// Spawn ffmpeg reading from `pipe:0` instead of a URL/path. Returns the
    /// source plus the writable stdin end so the caller can pipe arbitrary
    /// bytes (e.g. yt-dlp output, ICY-stripped radio body) into ffmpeg.
    pub async fn from_stdin(channels: u8) -> io::Result<(Self, tokio::process::ChildStdin)> {
        Self::from_stdin_with_hint(channels, None).await
    }

    /// THE-972 — [`from_stdin`](Self::from_stdin) with an optional input
    /// *format hint*: an ffmpeg demuxer name (`"mp3"`, `"aac"`, `"ogg"`,
    /// `"flac"`) passed as `-f <fmt>` before `-i pipe:0`.
    ///
    /// What the hint buys is a much smaller **start-up read**: before the
    /// first PCM byte, ffmpeg consumes probe + stream-analysis input, and
    /// under the PURA-330 caps (256 KB probe / 1 s analysis) that is
    /// ~18 KB of a 128 kbps radio stream — measured against a live ICY
    /// station, where bytes trickle in at TCP-slow-start pace, that read
    /// alone holds first PCM back by one or two round trips. With the
    /// demuxer named there is nothing to probe and a 200 ms analysis
    /// window is plenty for codec parameters (the measured floor is ~3 KB
    /// — a handful of frames — for mp3/aac elementary streams), so the
    /// caps drop to 32 KB / 200 ms and first PCM rides the first burst.
    /// Callers that cannot trust their container knowledge (the yt-dlp
    /// path) pass `None` and keep the PURA-330 caps unchanged.
    pub async fn from_stdin_with_hint(
        channels: u8,
        format_hint: Option<&str>,
    ) -> io::Result<(Self, tokio::process::ChildStdin)> {
        let mut cmd = Command::new("ffmpeg");
        cmd.kill_on_drop(true)
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin");
        match format_hint {
            // THE-972 — named demuxer: no probe, short analysis (see above).
            Some(fmt) => {
                cmd.arg("-probesize")
                    .arg("32k")
                    .arg("-analyzeduration")
                    .arg("200000")
                    .arg("-f")
                    .arg(fmt);
            }
            // PURA-330 — cap container probing on the piped yt-dlp stream.
            // ffmpeg's defaults (5 MB probesize / 5 s analyzeduration) make
            // it buffer and inspect several seconds of input before it
            // emits the first PCM byte; for `yt-dlp -f bestaudio` output
            // (a single webm/opus or m4a/aac elementary stream) a fraction
            // of that is plenty to detect the format. This trims dead time
            // from `!play` → first-audio without affecting decode quality.
            None => {
                cmd.arg("-probesize")
                    .arg("256k")
                    .arg("-analyzeduration")
                    .arg("1000000");
            }
        }
        cmd.arg("-i")
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
                carry: Vec::new(),
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

/// THE-983 (AR-2) — read s16le bytes from `reader` into `buf`, returning
/// only whole *channel strides* (2 bytes × `channels`, i.e. one interleaved
/// sample per channel). Bytes past the last whole stride go into `carry`
/// and are prepended on the next call, so a short read landing on an odd
/// byte boundary (EINTR-style short pipe write) can never tear i16
/// alignment mid-stream, and a stride-odd boundary can never shift the
/// stereo interleave (permanent L/R swap). A sub-stride tail at EOF is
/// discarded — it can never be completed (ffmpeg died mid-sample).
///
/// Cancel-safe: `carry` is only rewritten after the reads complete, so a
/// future dropped mid-await leaves it holding the same bytes for a re-read.
async fn read_whole_strides<R>(
    reader: &mut R,
    carry: &mut Vec<u8>,
    channels: u8,
    buf: &mut [i16],
) -> io::Result<usize>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let stride = 2 * channels.max(1) as usize;
    let bytes_wanted = buf.len() * 2;
    // The pipeline always asks for whole frames (≥ 960 samples); a buffer
    // smaller than one stride could only return `Ok(0)`, which the caller
    // reads as EOF.
    debug_assert!(bytes_wanted >= stride + carry.len(), "buf too small");
    let mut byte_buf = vec![0u8; bytes_wanted];
    byte_buf[..carry.len()].copy_from_slice(carry);
    let mut filled = carry.len();
    let mut eof = false;
    while filled < bytes_wanted {
        match reader.read(&mut byte_buf[filled..]).await? {
            0 => {
                eof = true;
                break;
            }
            n => filled += n,
        }
        // Read at least one full stride before yielding back so callers
        // see whole interleaved samples. If we already have ≥ 1 stride and
        // the read hit a short window (typical for piped ffmpeg), fall
        // through and let the caller decide whether to ask for more.
        if filled >= stride {
            break;
        }
    }
    let whole_bytes = filled - filled % stride;
    carry.clear();
    if !eof {
        carry.extend_from_slice(&byte_buf[whole_bytes..filled]);
    }
    let whole_samples = whole_bytes / 2;
    for i in 0..whole_samples {
        buf[i] = i16::from_le_bytes([byte_buf[i * 2], byte_buf[i * 2 + 1]]);
    }
    Ok(whole_samples)
}

#[async_trait]
impl PcmSource for FfmpegSource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        let whole_samples =
            read_whole_strides(&mut self.stdout, &mut self.carry, self.channels, buf).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// An `AsyncRead` that hands out its script at most `chunk` bytes per
    /// poll — a deterministic stand-in for a pipe delivering short writes
    /// on odd byte boundaries (THE-983 AR-2 repro).
    struct OddChunkReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }

    impl tokio::io::AsyncRead for OddChunkReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let remaining = self.data.len() - self.pos;
            let n = self.chunk.min(remaining).min(buf.remaining());
            buf.put_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Poll::Ready(Ok(()))
        }
    }

    /// s16le-encode a ramp of `count` i16 samples starting at `start`.
    fn ramp_bytes(start: i16, count: usize) -> (Vec<i16>, Vec<u8>) {
        let samples: Vec<i16> = (0..count as i16).map(|i| start + i).collect();
        let bytes = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        (samples, bytes)
    }

    /// Drain `reader` through `read_whole_strides` until EOF, asserting every
    /// intermediate return is stride-aligned.
    async fn drain(reader: &mut OddChunkReader, channels: u8) -> Vec<i16> {
        let mut carry = Vec::new();
        let mut out = Vec::new();
        let mut buf = [0i16; 64];
        loop {
            let n = read_whole_strides(reader, &mut carry, channels, &mut buf)
                .await
                .expect("read");
            if n == 0 {
                break;
            }
            assert_eq!(
                n % channels as usize,
                0,
                "read returned a torn channel stride",
            );
            out.extend_from_slice(&buf[..n]);
        }
        assert!(carry.is_empty(), "carry must be drained at EOF");
        out
    }

    /// Mono, 3-byte chunks: every read crosses an i16 boundary. The legacy
    /// `filled >= 2` early break dropped the odd trailing byte and statics
    /// the rest of the stream; the carry must reassemble the ramp exactly.
    #[tokio::test]
    async fn odd_chunks_keep_i16_alignment_mono() {
        let (samples, bytes) = ramp_bytes(-100, 40);
        let mut reader = OddChunkReader {
            data: bytes,
            pos: 0,
            chunk: 3,
        };
        assert_eq!(drain(&mut reader, 1).await, samples);
    }

    /// Stereo, 5-byte chunks: every read crosses a channel stride (4 bytes).
    /// Without stride tracking the interleave shifts by one sample — a
    /// permanent L/R swap. The carry must hold sub-stride tails back.
    #[tokio::test]
    async fn odd_chunks_keep_channel_stride_stereo() {
        let (samples, bytes) = ramp_bytes(1000, 48);
        let mut reader = OddChunkReader {
            data: bytes,
            pos: 0,
            chunk: 5,
        };
        assert_eq!(drain(&mut reader, 2).await, samples);
    }

    /// A torn trailing byte at EOF (ffmpeg killed mid-sample) is discarded;
    /// every whole sample before it survives.
    #[tokio::test]
    async fn torn_tail_at_eof_is_discarded() {
        let (samples, mut bytes) = ramp_bytes(7, 10);
        bytes.push(0xAB); // half an i16 that can never complete
        let mut reader = OddChunkReader {
            data: bytes,
            pos: 0,
            chunk: 7,
        };
        assert_eq!(drain(&mut reader, 1).await, samples);
    }
}
