//! ICY (Shoutcast / Icecast) radio source.
//!
//! Protocol: client sends `Icy-MetaData: 1`. Server interleaves audio bytes
//! with metadata blocks every `icy-metaint` bytes. A metadata block is one
//! length byte L (actual size is `L * 16`), followed by `L*16` bytes of
//! metadata (`StreamTitle='…';` padded with NULs).
//!
//! We split the stream: audio bytes go to ffmpeg's stdin, metadata bytes are
//! parsed for `StreamTitle` and surfaced as `PipelineEvent::NowPlaying`.
//!
//! THE-898 reconnect: real Icecast/Shoutcast streams drop and recover
//! routinely (LB migrations, CDN edge re-routes, TCP idle timeouts). A naive
//! "break on first chunk error" would surface as `EndOfStream` to the bot,
//! which then auto-nexts or stops — breaking the `!radio` "leave-it-running"
//! promise. We wrap the body drain in an outer reconnect loop with a bounded
//! backoff ladder. `last_title` is preserved across reconnects so the
//! `NowPlaying` event only fires on real track changes.
//!
//! THE-984 reconnect splice: a reconnect splices the new HTTP body into the
//! *same* long-lived ffmpeg stdin — the old body truncates mid-frame and the
//! new one starts at an arbitrary byte offset. For self-sync codecs (mp3 /
//! ADTS AAC) we scan past the torn bytes to a verified frame boundary before
//! writing; for non-self-synchronizing containers (Ogg, FLAC) we write
//! through and surface a Warning — see [`ResyncMode`] for the rationale.

use std::io;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::PcmSource;
use super::ffmpeg::FfmpegSource;
use crate::icy;
use crate::types::PipelineEvent;

/// Backoff between reconnect attempts (ms). The total wall-clock sleep across
/// the ladder is ~15.5 s; combined with request latency the total reconnect
/// budget is ~30 s before we give up and surface terminal EOF.
const BACKOFF_LADDER_MS: [u64; 5] = [500, 1_000, 2_000, 4_000, 8_000];

pub struct IcyRadioSource {
    inner: FfmpegSource,
    fetcher: Option<JoinHandle<()>>,
    events: mpsc::Receiver<PipelineEvent>,
    pending_warnings: Vec<PipelineEvent>,
}

impl IcyRadioSource {
    pub async fn new(url: &str, channels: u8) -> io::Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| io::Error::other(format!("reqwest build: {e}")))?;
        let t0 = std::time::Instant::now();
        let (resp, metaint) = open_icy(&client, url).await?;
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // THE-972 — the ICY response names the codec up front, so hand
        // ffmpeg the matching demuxer instead of letting it probe the pipe.
        // The named demuxer shrinks ffmpeg's start-up read from ~18 KB to
        // ~3 KB (probe skipped, analysis window cut), which on a live
        // stream still ramping through TCP slow start is worth one or two
        // round trips of `!radio` → first-audio latency. Unknown / missing
        // Content-Type falls back to the probing path.
        let format_hint = ffmpeg_format_hint(&content_type);
        // THE-984 — the same Content-Type also picks the post-reconnect
        // resync strategy (frame-sync scan vs warn vs passthrough).
        let resync = resync_mode(&content_type);
        // The connect leg of the `!radio` → first-audio breakdown: DNS +
        // TCP/TLS + the ICY GET up to response headers. The remaining legs
        // are `ffmpeg_first_pcm` and `pipeline_first_frame`.
        tracing::info!(
            target: "music_bot_latency",
            stage = "icy_connect",
            elapsed_ms = t0.elapsed().as_millis() as u64,
            content_type = %content_type,
            format_hint = format_hint.unwrap_or("none"),
            metaint = ?metaint,
            "icy stream connected (response headers in)",
        );

        let (inner, ffmpeg_stdin) =
            FfmpegSource::from_stdin_with_hint(channels, format_hint).await?;
        let (event_tx, event_rx) = mpsc::channel::<PipelineEvent>(32);
        let url_owned = url.to_string();

        let mut pending_warnings = Vec::new();
        if metaint.is_none() {
            pending_warnings.push(PipelineEvent::Warning(format!(
                "icy stream {url} did not return icy-metaint; ICY metadata disabled"
            )));
        }

        let fetcher = tokio::spawn(run_fetcher(
            client,
            url_owned,
            FirstAttempt { resp, metaint },
            resync,
            ffmpeg_stdin,
            event_tx,
        ));

        Ok(Self {
            inner,
            fetcher: Some(fetcher),
            events: event_rx,
            pending_warnings,
        })
    }
}

impl Drop for IcyRadioSource {
    fn drop(&mut self) {
        if let Some(handle) = self.fetcher.take() {
            handle.abort();
        }
    }
}

#[async_trait]
impl PcmSource for IcyRadioSource {
    async fn read_samples(&mut self, buf: &mut [i16]) -> io::Result<usize> {
        self.inner.read_samples(buf).await
    }

    fn try_drain_events(&mut self) -> Vec<PipelineEvent> {
        let mut out = std::mem::take(&mut self.pending_warnings);
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev);
        }
        out.extend(self.inner.try_drain_events());
        out
    }
}

/// Issue the ICY GET and parse the `icy-metaint` header.
///
/// Errors here surface to the caller of `IcyRadioSource::new` — a bad URL,
/// 4xx, 5xx on the *initial* request all fail the `!radio` command rather
/// than silently entering a reconnect loop. Mid-stream and post-startup
/// failures are the reconnect loop's job.
pub(crate) async fn open_icy(
    client: &reqwest::Client,
    url: &str,
) -> io::Result<(reqwest::Response, Option<usize>)> {
    let resp = client
        .get(url)
        .header("Icy-MetaData", "1")
        .header("User-Agent", "music-bot-audio/0.0 (PURA-119)")
        .send()
        .await
        .map_err(|e| io::Error::other(format!("icy GET {url}: {e}")))?;
    let resp = resp
        .error_for_status()
        .map_err(|e| io::Error::other(format!("icy http status: {e}")))?;
    let metaint = parse_metaint(&resp);
    Ok((resp, metaint))
}

/// THE-972 — map an ICY response `Content-Type` to the ffmpeg demuxer that
/// reads it, for [`FfmpegSource::from_stdin_with_hint`].
///
/// Exact-match only, against the MIME types Icecast/Shoutcast actually
/// serve — a hint that names the *wrong* demuxer is worse than no hint
/// (ffmpeg would reject every frame instead of probing its way to the
/// truth), so anything unrecognised returns `None` and keeps the probing
/// path. The `mp3` demuxer covers all MPEG audio layers, matching the
/// `audio/mpeg` registration; `audio/aacp` is the de-facto Shoutcast type
/// for ADTS AAC; Ogg-contained codecs (vorbis, opus, flac-in-ogg) all ride
/// the `ogg` demuxer.
fn ffmpeg_format_hint(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" => Some("mp3"),
        "audio/aac" | "audio/aacp" | "audio/x-aac" => Some("aac"),
        "application/ogg" | "audio/ogg" | "audio/x-ogg" | "audio/opus" => Some("ogg"),
        "audio/flac" | "audio/x-flac" => Some("flac"),
        _ => None,
    }
}

/// THE-984 (AR-4) — how the fetcher handles the byte-stream splice a
/// THE-898 reconnect creates: the old HTTP body truncates mid-frame and the
/// new one starts at an arbitrary offset, but both feed the *same*
/// long-lived ffmpeg stdin, so the demuxer sees torn bytes at the seam.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ResyncMode {
    /// Unknown Content-Type (ffmpeg probing path): write through unchanged.
    /// We can't scan for a frame boundary without knowing the codec; the
    /// dominant ICY codecs (mp3/aac) self-resync at the cost of one pop.
    Passthrough,
    /// Self-sync codec with a recognisable frame header (mp3 / ADTS AAC):
    /// after a reconnect, discard bytes until a verified frame boundary so
    /// ffmpeg never sees the torn seam — removes the per-reconnect pop.
    FrameSync(SyncKind),
    /// Non-self-synchronizing container (Ogg, FLAC) — THE-984 option (a):
    /// we deliberately do NOT attempt mid-stream repair. Ogg pages carry
    /// stream state (serials, granule positions, packet continuations) that
    /// a byte scanner can't rebuild, and a wrong guess turns "garbled audio"
    /// into "demuxer error kills the station". Write through and surface a
    /// Warning so the operator knows why audio degraded. The full fix is
    /// option (c) — respawn ffmpeg per connection — tracked only if Ogg
    /// stations show up in practice. Raw FLAC frames do carry sync codes,
    /// but mid-stream recovery by ffmpeg's `flac` demuxer isn't guaranteed,
    /// so FLAC rides the conservative bucket too.
    WarnNotSelfSync,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SyncKind {
    Mp3,
    Adts,
}

/// THE-984 — map the negotiated Content-Type to a post-reconnect resync
/// strategy. Mirrors the [`ffmpeg_format_hint`] table: same normalisation,
/// same conservative default (unknown → passthrough, never guess a codec).
fn resync_mode(content_type: &str) -> ResyncMode {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" => ResyncMode::FrameSync(SyncKind::Mp3),
        "audio/aac" | "audio/aacp" | "audio/x-aac" => ResyncMode::FrameSync(SyncKind::Adts),
        "application/ogg" | "audio/ogg" | "audio/x-ogg" | "audio/opus" | "audio/flac"
        | "audio/x-flac" => ResyncMode::WarnNotSelfSync,
        _ => ResyncMode::Passthrough,
    }
}

/// Cap on bytes examined per reconnect before the scanner gives up and falls
/// back to write-through. The torn prefix is at most one partial frame
/// (mp3 ≤ ~2.9 KB, ADTS ≤ 8 KB), so 32 KB without a verified sync means the
/// stream isn't the codec the Content-Type claimed — stop withholding audio.
const RESYNC_SCAN_MAX: usize = 32 * 1024;

/// Bytes of the *candidate* frame header needed to compute its length.
fn sync_header_len(kind: SyncKind) -> usize {
    match kind {
        SyncKind::Mp3 => 4,
        SyncKind::Adts => 6,
    }
}

/// Bytes of the *next* frame header needed to confirm the candidate.
const CONFIRM_LEN: usize = 3;

/// THE-984 — drops post-reconnect bytes until a verified frame boundary.
///
/// "Verified" means: a header that parses (sync bits + valid version /
/// layer / bitrate / samplerate fields) AND whose computed frame length
/// lands on a second header that agrees with it. A bare 11-bit sync check
/// false-positives every few KB of compressed audio; the two-header confirm
/// makes a false sync vanishingly unlikely. Both failure modes are benign:
/// a false positive merely degrades to the pre-THE-984 behaviour (decoder
/// self-resync pop), a missed sync costs one extra frame of discard.
struct ResyncScanner {
    kind: SyncKind,
    buf: Vec<u8>,
    /// Bytes already ruled out and dropped (excludes what's still in `buf`).
    discarded: usize,
}

enum ScanStep {
    /// Still hunting — nothing reaches the writer yet.
    Hold,
    /// Verified frame boundary: write `aligned`, pass through from here on.
    Synced { aligned: Vec<u8>, discarded: usize },
    /// Scan budget exhausted without a sync — flush what's buffered and
    /// fall back to write-through (the Content-Type probably lied).
    GaveUp { flush: Vec<u8>, scanned: usize },
}

impl ResyncScanner {
    fn new(kind: SyncKind) -> Self {
        Self {
            kind,
            buf: Vec::new(),
            discarded: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> ScanStep {
        self.buf.extend_from_slice(bytes);
        let mut i = 0;
        loop {
            // Jump to the next possible sync byte.
            match self.buf[i..].iter().position(|&b| b == 0xFF) {
                Some(off) => i += off,
                None => {
                    i = self.buf.len();
                    break;
                }
            }
            // Candidate header split across chunks — wait for more bytes.
            if self.buf.len() - i < sync_header_len(self.kind) {
                break;
            }
            let Some(frame_len) = parse_frame_len(self.kind, &self.buf[i..]) else {
                i += 1;
                continue;
            };
            // Need the start of the next frame's header to confirm.
            if self.buf.len() - i < frame_len + CONFIRM_LEN {
                break;
            }
            if headers_agree(self.kind, &self.buf[i..], &self.buf[i + frame_len..]) {
                let aligned = self.buf.split_off(i);
                return ScanStep::Synced {
                    aligned,
                    discarded: self.discarded + i,
                };
            }
            i += 1;
        }
        // Everything before `i` is ruled out; `buf[i..]` is a pending
        // candidate (or empty) that needs more bytes to judge.
        self.discarded += i;
        self.buf.drain(..i);
        if self.discarded + self.buf.len() > RESYNC_SCAN_MAX {
            return ScanStep::GaveUp {
                flush: std::mem::take(&mut self.buf),
                scanned: self.discarded,
            };
        }
        ScanStep::Hold
    }
}

fn parse_frame_len(kind: SyncKind, h: &[u8]) -> Option<usize> {
    match kind {
        SyncKind::Mp3 => mp3_frame_len(h),
        SyncKind::Adts => adts_frame_len(h),
    }
}

/// MPEG audio bitrate tables (kbps), indexed by the 4-bit bitrate field.
/// Index 0 ("free format" — frame length unknowable) and 15 (invalid) are 0
/// and rejected by the parser.
const MP3_KBPS_V1_L1: [u32; 16] = [
    0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448, 0,
];
const MP3_KBPS_V1_L2: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 0,
];
const MP3_KBPS_V1_L3: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const MP3_KBPS_V2_L1: [u32; 16] = [
    0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256, 0,
];
const MP3_KBPS_V2_L23: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// Parse an MPEG-audio frame header at the start of `h` (caller guarantees
/// 4 bytes); returns the whole-frame length in bytes, `None` if any field
/// is invalid.
fn mp3_frame_len(h: &[u8]) -> Option<usize> {
    if h[0] != 0xFF || (h[1] & 0xE0) != 0xE0 {
        return None;
    }
    let version = (h[1] >> 3) & 0x3; // 0=MPEG2.5  1=reserved  2=MPEG2  3=MPEG1
    let layer = (h[1] >> 1) & 0x3; // 0=reserved  1=III  2=II  3=I
    if version == 1 || layer == 0 {
        return None;
    }
    let bitrate_idx = (h[2] >> 4) as usize;
    let sr_idx = ((h[2] >> 2) & 0x3) as usize;
    if bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
        return None;
    }
    let v1 = version == 3;
    let kbps = match (v1, layer) {
        (true, 3) => MP3_KBPS_V1_L1[bitrate_idx],
        (true, 2) => MP3_KBPS_V1_L2[bitrate_idx],
        (true, _) => MP3_KBPS_V1_L3[bitrate_idx],
        (false, 3) => MP3_KBPS_V2_L1[bitrate_idx],
        (false, _) => MP3_KBPS_V2_L23[bitrate_idx],
    };
    let sr: u32 = match version {
        3 => [44_100, 48_000, 32_000][sr_idx],
        2 => [22_050, 24_000, 16_000][sr_idx],
        _ => [11_025, 12_000, 8_000][sr_idx],
    };
    let padding = ((h[2] >> 1) & 0x1) as u32;
    let bps = kbps * 1_000;
    // Samples per frame: Layer I = 384 (4-byte slots), Layer II = 1152,
    // Layer III = 1152 (MPEG1) / 576 (MPEG2/2.5).
    let len = match layer {
        3 => (12 * bps / sr + padding) * 4,
        2 => 144 * bps / sr + padding,
        _ if v1 => 144 * bps / sr + padding,
        _ => 72 * bps / sr + padding,
    };
    (len >= 24).then_some(len as usize)
}

/// Parse an ADTS frame header at the start of `h` (caller guarantees
/// 6 bytes); returns the whole-frame length (`aac_frame_length` includes
/// the header), `None` if any field is invalid.
fn adts_frame_len(h: &[u8]) -> Option<usize> {
    if h[0] != 0xFF || (h[1] & 0xF0) != 0xF0 {
        return None;
    }
    if (h[1] & 0x06) != 0 {
        return None; // layer bits must be 00 in ADTS
    }
    let sf_idx = (h[2] >> 2) & 0xF;
    if sf_idx > 12 {
        return None; // 13/14 reserved, 15 escape — never on a real stream
    }
    let len = (((h[3] & 0x03) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] as usize) >> 5);
    // Shorter than its own header → corrupt.
    (len >= 7).then_some(len)
}

/// Confirm that the bytes at `b` (caller guarantees [`CONFIRM_LEN`]) open
/// the next frame of the stream whose previous header is `a`: sync bits
/// present, fields constant within one stream agree (version / layer /
/// samplerate), and fields that legitimately vary (bitrate — VBR) are
/// merely valid.
fn headers_agree(kind: SyncKind, a: &[u8], b: &[u8]) -> bool {
    match kind {
        SyncKind::Mp3 => {
            b[0] == 0xFF
                && (b[1] & 0xE0) == 0xE0
                && (a[1] & 0x1E) == (b[1] & 0x1E) // version + layer
                && (a[2] & 0x0C) == (b[2] & 0x0C) // samplerate index
                && (b[2] >> 4) != 0xF
                && (b[2] >> 4) != 0 // bitrate valid (may differ — VBR)
        }
        SyncKind::Adts => {
            b[0] == 0xFF
                && (b[1] & 0xF6) == 0xF0 // sync + layer 00
                && (a[2] & 0x3C) == (b[2] & 0x3C) // sampling-frequency index
        }
    }
}

fn parse_metaint(resp: &reqwest::Response) -> Option<usize> {
    resp.headers()
        .get("icy-metaint")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<usize>().ok())
}

/// Reqwest errors we treat as transient (worth a reconnect attempt). Anything
/// else (decode/redirect/builder/status — i.e. protocol-level) is terminal.
fn is_connection_like(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout() || e.is_request() || e.is_body()
}

pub(crate) struct FirstAttempt {
    pub resp: reqwest::Response,
    pub metaint: Option<usize>,
}

/// Drain the ICY body into `writer`; on transient failures, re-issue the GET
/// with bounded backoff. Returns when the budget is exhausted, the consumer
/// closed (`writer` errored), a terminal HTTP error occurred, or the station
/// ended cleanly.
pub(crate) async fn run_fetcher<W>(
    client: reqwest::Client,
    url: String,
    first: FirstAttempt,
    resync: ResyncMode,
    mut writer: W,
    event_tx: mpsc::Sender<PipelineEvent>,
) where
    W: AsyncWrite + Unpin + Send,
{
    let mut last_title = String::new();
    let mut consecutive_failures: usize = 0;
    let mut attempt: usize = 0;
    let mut current: Option<FirstAttempt> = Some(first);

    loop {
        let (resp, metaint, spliced) = match current.take() {
            Some(c) => (c.resp, c.metaint, false),
            None => match reconnect_get(&client, &url, &event_tx).await {
                ReconnectOutcome::Got(resp) => {
                    let metaint = parse_metaint(&resp);
                    (resp, metaint, true)
                }
                ReconnectOutcome::Retry => {
                    consecutive_failures += 1;
                    if !sleep_backoff(consecutive_failures, &event_tx, &url, &mut attempt).await {
                        break;
                    }
                    continue;
                }
                ReconnectOutcome::Terminal => break,
            },
        };

        // THE-984 — this is the splice point: the previous body truncated
        // mid-frame and this one starts at an arbitrary offset, but both
        // feed the same long-lived ffmpeg stdin. Self-sync codecs get a
        // frame-boundary scan, non-self-sync containers (Ogg/FLAC) get a
        // Warning — see `ResyncMode`.
        let scanner = match (resync, spliced) {
            (ResyncMode::FrameSync(kind), true) => Some(ResyncScanner::new(kind)),
            (ResyncMode::WarnNotSelfSync, true) => {
                let _ = event_tx
                    .send(PipelineEvent::Warning(format!(
                        "icy stream {url}: reconnected mid-stream on a non-self-synchronizing \
                         container — audio may be garbled until the station is restarted"
                    )))
                    .await;
                None
            }
            _ => None,
        };

        let outcome = drain_body(
            resp,
            metaint,
            &url,
            &mut writer,
            &event_tx,
            &mut last_title,
            scanner,
        )
        .await;

        match outcome {
            BodyOutcome::ConsumerGone => return,
            BodyOutcome::EndedAfterAudio(reason) => {
                // ICY/Shoutcast streams are open-ended by design — they never
                // intend to end. Any end-of-body, whether reqwest reports it
                // as a clean EOF (Connection: close + socket close) or as a
                // chunked-encoding error, is unexpected from the operator's
                // POV. Reconnect and reset the counter; the audio bytes we
                // already pushed prove the URL is healthy.
                let _ = event_tx
                    .send(PipelineEvent::Warning(format!(
                        "icy stream {url} dropped: {reason} — reconnecting"
                    )))
                    .await;
                consecutive_failures = 0;
            }
            BodyOutcome::DropNoAudio(e) => {
                let _ = event_tx
                    .send(PipelineEvent::Warning(format!(
                        "icy stream {url} failed with no audio: {e} — reconnecting"
                    )))
                    .await;
                consecutive_failures += 1;
            }
            BodyOutcome::CleanEofNoAudio => {
                // 200 OK then immediate socket close before any audio. A
                // healthy ICY server would have sent at least the next
                // metaint's worth of audio bytes. Treat as terminal —
                // reconnecting would just hammer a misbehaving endpoint.
                let _ = event_tx
                    .send(PipelineEvent::Warning(format!(
                        "icy stream {url} returned empty body — not reconnecting"
                    )))
                    .await;
                break;
            }
        }

        if !sleep_backoff(consecutive_failures, &event_tx, &url, &mut attempt).await {
            break;
        }
    }

    let _ = event_tx
        .send(PipelineEvent::Warning(format!(
            "icy stream {url} reconnect budget exhausted after {attempt} attempts"
        )))
        .await;
    let _ = writer.shutdown().await;
}

enum BodyOutcome {
    /// Consumer (ffmpeg stdin) closed — caller should return immediately.
    ConsumerGone,
    /// Body ended (clean EOF or chunked-encoding error) after at least one
    /// audio byte was pushed. Retryable; the URL has proven healthy. The
    /// `String` is a short human-readable reason for the surfaced Warning.
    EndedAfterAudio(String),
    /// Body errored before any audio. Retryable but counts against the
    /// budget — a permanently-broken upstream should not be hammered.
    DropNoAudio(reqwest::Error),
    /// 200 OK then immediate clean EOF without any audio. Terminal — a
    /// healthy ICY endpoint always sends bytes.
    CleanEofNoAudio,
}

async fn drain_body<W>(
    resp: reqwest::Response,
    metaint: Option<usize>,
    url: &str,
    writer: &mut W,
    event_tx: &mpsc::Sender<PipelineEvent>,
    last_title: &mut String,
    mut scanner: Option<ResyncScanner>,
) -> BodyOutcome
where
    W: AsyncWrite + Unpin,
{
    let mut splitter = icy::IcyStreamSplitter::new(metaint);
    let mut stream = resp.bytes_stream();
    let mut ever_pushed_audio = false;
    let mut chunk_error: Option<reqwest::Error> = None;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                splitter.feed(&bytes);
                while let Some(piece) = splitter.next_piece() {
                    match piece {
                        icy::IcyPiece::Audio(bs) => {
                            // "Audio arrived" — the URL is proven healthy
                            // (this drives the reconnect-budget reset) even
                            // while the THE-984 scanner is withholding the
                            // torn bytes from the writer.
                            ever_pushed_audio = true;
                            match scanner.as_mut().map(|s| s.feed(&bs)) {
                                None => {
                                    if writer.write_all(&bs).await.is_err() {
                                        return BodyOutcome::ConsumerGone;
                                    }
                                }
                                Some(ScanStep::Hold) => {}
                                Some(ScanStep::Synced { aligned, discarded }) => {
                                    tracing::debug!(
                                        url,
                                        discarded,
                                        "icy reconnect resync: frame boundary found, torn bytes dropped",
                                    );
                                    scanner = None;
                                    if writer.write_all(&aligned).await.is_err() {
                                        return BodyOutcome::ConsumerGone;
                                    }
                                }
                                Some(ScanStep::GaveUp { flush, scanned }) => {
                                    tracing::warn!(
                                        url,
                                        scanned,
                                        "icy reconnect resync: no frame sync within budget, writing through",
                                    );
                                    scanner = None;
                                    if writer.write_all(&flush).await.is_err() {
                                        return BodyOutcome::ConsumerGone;
                                    }
                                }
                            }
                        }
                        icy::IcyPiece::Metadata(bs) => {
                            if let Some(title) = icy::parse_stream_title(&bs)
                                && title != *last_title
                            {
                                *last_title = title.clone();
                                if event_tx
                                    .send(PipelineEvent::NowPlaying {
                                        title,
                                        source: url.to_string(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    return BodyOutcome::ConsumerGone;
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                chunk_error = Some(e);
                break;
            }
        }
    }

    match (ever_pushed_audio, chunk_error) {
        (true, Some(e)) => BodyOutcome::EndedAfterAudio(e.to_string()),
        (true, None) => BodyOutcome::EndedAfterAudio("connection closed".to_string()),
        (false, Some(e)) => BodyOutcome::DropNoAudio(e),
        (false, None) => BodyOutcome::CleanEofNoAudio,
    }
}

enum ReconnectOutcome {
    Got(reqwest::Response),
    Retry,
    Terminal,
}

async fn reconnect_get(
    client: &reqwest::Client,
    url: &str,
    event_tx: &mpsc::Sender<PipelineEvent>,
) -> ReconnectOutcome {
    match client
        .get(url)
        .header("Icy-MetaData", "1")
        .header("User-Agent", "music-bot-audio/0.0 (PURA-119)")
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => ReconnectOutcome::Got(r),
        Ok(r) if r.status().is_server_error() => {
            let _ = event_tx
                .send(PipelineEvent::Warning(format!(
                    "icy reconnect {url}: server status {} — retry",
                    r.status()
                )))
                .await;
            ReconnectOutcome::Retry
        }
        Ok(r) => {
            let _ = event_tx
                .send(PipelineEvent::Warning(format!(
                    "icy reconnect {url}: terminal status {} — giving up",
                    r.status()
                )))
                .await;
            ReconnectOutcome::Terminal
        }
        Err(e) if is_connection_like(&e) => {
            let _ = event_tx
                .send(PipelineEvent::Warning(format!(
                    "icy reconnect {url}: connection error: {e} — retry"
                )))
                .await;
            ReconnectOutcome::Retry
        }
        Err(e) => {
            let _ = event_tx
                .send(PipelineEvent::Warning(format!(
                    "icy reconnect {url}: protocol error: {e} — giving up"
                )))
                .await;
            ReconnectOutcome::Terminal
        }
    }
}

/// Sleep for the next backoff step. Returns `false` when the budget is
/// exhausted (caller should break out of the reconnect loop).
async fn sleep_backoff(
    consecutive_failures: usize,
    _event_tx: &mpsc::Sender<PipelineEvent>,
    _url: &str,
    attempt: &mut usize,
) -> bool {
    if consecutive_failures > BACKOFF_LADDER_MS.len() {
        return false;
    }
    *attempt += 1;
    // THE-983 (AR-5) — a healthy reconnect (`EndedAfterAudio` reset the
    // failure counter: audio flowed, the URL is proven good) reconnects
    // immediately. Sleeping ladder[0] here burned 500 ms of the thin
    // radio runway (ffmpeg's input buffer) on every routine stream drop.
    if consecutive_failures == 0 {
        return true;
    }
    let idx = consecutive_failures
        .saturating_sub(1)
        .min(BACKOFF_LADDER_MS.len() - 1);
    let wait = BACKOFF_LADDER_MS[idx];
    tokio::time::sleep(Duration::from_millis(wait)).await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    /// A scripted server response. Each entry is consumed by one accept.
    #[derive(Clone)]
    enum MockResponse {
        /// 200 OK with optional icy-metaint and a body, then close.
        Ok {
            metaint: Option<usize>,
            body: Vec<u8>,
        },
        /// 503 Service Unavailable.
        Status5xx,
        /// 410 Gone.
        Status410,
        /// 200 OK with no body, immediately close.
        InstantEof,
    }

    /// Spawn a TCP server scripted with the given responses. The Nth incoming
    /// connection gets the Nth response; further connections see immediate
    /// close.
    async fn spawn_server(script: Vec<MockResponse>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{}/", addr);
        let handle = tokio::spawn(async move {
            let mut iter = script.into_iter();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let Some(resp) = iter.next() else {
                    drop(sock);
                    continue;
                };
                // Consume request headers up to \r\n\r\n.
                let mut buf = vec![0u8; 4096];
                let mut total = 0;
                loop {
                    let n = match sock.read(&mut buf[total..]).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                    if total >= buf.len() {
                        break;
                    }
                }
                let _ = write_response(&mut sock, resp).await;
                let _ = sock.shutdown().await;
            }
        });
        (url, handle)
    }

    async fn write_response(
        sock: &mut tokio::net::TcpStream,
        resp: MockResponse,
    ) -> io::Result<()> {
        match resp {
            MockResponse::Ok { metaint, body } => {
                let header = match metaint {
                    Some(n) => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nicy-metaint: {n}\r\nConnection: close\r\n\r\n"
                    ),
                    None => {
                        "HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n"
                            .to_string()
                    }
                };
                sock.write_all(header.as_bytes()).await?;
                sock.write_all(&body).await?;
            }
            MockResponse::Status5xx => {
                sock.write_all(
                    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            }
            MockResponse::Status410 => {
                sock.write_all(
                    b"HTTP/1.1 410 Gone\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
            }
            MockResponse::InstantEof => {
                sock.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nConnection: close\r\n\r\n",
                )
                .await?;
            }
        }
        Ok(())
    }

    /// AsyncWrite that records every byte into a shared buffer.
    struct CaptureWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl AsyncWrite for CaptureWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            self.buf.lock().unwrap().extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// Build a body of `audio_bytes` filled with `audio_fill`, followed by one
    /// `StreamTitle='<title>';` metadata block, followed by another
    /// `audio_bytes` of `audio_fill_b`. icy-metaint is `audio_bytes`.
    fn body_with_title(
        audio_fill_a: u8,
        title: &str,
        audio_bytes: usize,
        audio_fill_b: u8,
    ) -> Vec<u8> {
        let mut body = vec![audio_fill_a; audio_bytes];
        let meta_payload = format!("StreamTitle='{title}';");
        let l = meta_payload.len().div_ceil(16);
        let meta_block_size = l * 16;
        body.push(l as u8);
        let mut padded = meta_payload.into_bytes();
        padded.resize(meta_block_size, 0);
        body.extend_from_slice(&padded);
        body.extend(std::iter::repeat_n(audio_fill_b, audio_bytes));
        body
    }

    /// Drain `event_rx` of any pending events without blocking.
    fn drain_events(rx: &mut mpsc::Receiver<PipelineEvent>) -> Vec<PipelineEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    /// Poll the writer's buffer until it reaches `min_len`, panic on timeout.
    /// Real-time wait — keep the timeout generous enough to absorb network
    /// jitter and the 500 ms backoff between attempts.
    async fn wait_for_bytes(
        buf: &Arc<Mutex<Vec<u8>>>,
        min_len: usize,
        timeout: Duration,
    ) -> Vec<u8> {
        let start = std::time::Instant::now();
        loop {
            {
                let g = buf.lock().unwrap();
                if g.len() >= min_len {
                    return g.clone();
                }
            }
            if start.elapsed() > timeout {
                let g = buf.lock().unwrap();
                panic!(
                    "wait_for_bytes timed out: wanted {min_len}, have {} ({:?})",
                    g.len(),
                    &g[..g.len().min(64)]
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Poll the event receiver until a matching event arrives or timeout.
    async fn wait_for_event<F>(
        rx: &mut mpsc::Receiver<PipelineEvent>,
        collected: &mut Vec<PipelineEvent>,
        timeout: Duration,
        mut pred: F,
    ) -> bool
    where
        F: FnMut(&PipelineEvent) -> bool,
    {
        let start = std::time::Instant::now();
        loop {
            while let Ok(ev) = rx.try_recv() {
                let matched = pred(&ev);
                collected.push(ev);
                if matched {
                    return true;
                }
            }
            if start.elapsed() > timeout {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn reconnects_after_mid_stream_drop_preserves_audio_and_title() {
        let audio_n = 200;
        let body_a = body_with_title(0xAB, "Same Title", audio_n, 0xCD);
        let body_b = body_with_title(0xEE, "Same Title", audio_n, 0xFF);
        let (url, _server) = spawn_server(vec![
            MockResponse::Ok {
                metaint: Some(audio_n),
                body: body_a,
            },
            MockResponse::Ok {
                metaint: Some(audio_n),
                body: body_b,
            },
        ])
        .await;

        let client = reqwest::Client::new();
        let (resp, metaint) = open_icy(&client, &url).await.unwrap();
        assert_eq!(metaint, Some(audio_n));
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter { buf: buf.clone() };
        let (tx, mut rx) = mpsc::channel(128);
        let fetcher = tokio::spawn(run_fetcher(
            client,
            url.clone(),
            FirstAttempt { resp, metaint },
            // Reconnect mechanics under test use fill bytes, not valid
            // frames — keep the THE-984 scanner out of the way.
            ResyncMode::Passthrough,
            writer,
            tx,
        ));

        // Both attempts together push 4 * audio_n bytes of audio. The 500 ms
        // backoff between them is real-time but small.
        let written = wait_for_bytes(&buf, 4 * audio_n, Duration::from_secs(5)).await;
        fetcher.abort();

        assert_eq!(
            &written[..audio_n],
            &vec![0xAB; audio_n][..],
            "attempt 1 first half"
        );
        assert_eq!(
            &written[audio_n..2 * audio_n],
            &vec![0xCD; audio_n][..],
            "attempt 1 second half"
        );
        assert_eq!(
            &written[2 * audio_n..3 * audio_n],
            &vec![0xEE; audio_n][..],
            "attempt 2 first half"
        );
        assert_eq!(
            &written[3 * audio_n..4 * audio_n],
            &vec![0xFF; audio_n][..],
            "attempt 2 second half"
        );

        let events = drain_events(&mut rx);
        let now_playing_count = events
            .iter()
            .filter(|e| matches!(e, PipelineEvent::NowPlaying { .. }))
            .count();
        assert_eq!(
            now_playing_count, 1,
            "expected exactly one NowPlaying across reconnects (got {events:?})"
        );
    }

    #[tokio::test]
    async fn retries_5xx_then_recovers() {
        // `open_icy` rejects 5xx at startup, so 5xx retry is only reachable
        // from inside the reconnect loop. Script: success, 5xx, success.
        let audio_n = 100;
        let (url, _server) = spawn_server(vec![
            MockResponse::Ok {
                metaint: None,
                body: vec![0xAA; audio_n],
            },
            MockResponse::Status5xx,
            MockResponse::Ok {
                metaint: None,
                body: vec![0xBB; audio_n],
            },
        ])
        .await;

        let client = reqwest::Client::new();
        let (resp, metaint) = open_icy(&client, &url).await.unwrap();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter { buf: buf.clone() };
        let (tx, mut rx) = mpsc::channel(128);
        let fetcher = tokio::spawn(run_fetcher(
            client,
            url.clone(),
            FirstAttempt { resp, metaint },
            // Reconnect mechanics under test use fill bytes, not valid
            // frames — keep the THE-984 scanner out of the way.
            ResyncMode::Passthrough,
            writer,
            tx,
        ));

        let written = wait_for_bytes(&buf, 2 * audio_n, Duration::from_secs(5)).await;
        let mut collected = Vec::new();
        let saw_5xx = wait_for_event(&mut rx, &mut collected, Duration::from_secs(2), |e| {
            matches!(e, PipelineEvent::Warning(s) if s.contains("503") || s.contains("server status"))
        })
        .await;
        fetcher.abort();

        assert_eq!(
            &written[..audio_n],
            &vec![0xAA; audio_n][..],
            "attempt 1 audio"
        );
        assert_eq!(
            &written[audio_n..2 * audio_n],
            &vec![0xBB; audio_n][..],
            "attempt 3 audio (after 5xx retry)"
        );
        assert!(saw_5xx, "expected 5xx retry warning, got: {collected:?}");
    }

    #[tokio::test]
    async fn terminal_410_does_not_retry() {
        let audio_n = 50;
        let (url, _server) = spawn_server(vec![
            MockResponse::Ok {
                metaint: None,
                body: vec![0xAB; audio_n],
            },
            MockResponse::Status410,
            // If we ever retry past terminal, this surfaces as another
            // audio block — the assert at the bottom catches that.
            MockResponse::Ok {
                metaint: None,
                body: vec![0xCD; audio_n],
            },
        ])
        .await;

        let client = reqwest::Client::new();
        let (resp, metaint) = open_icy(&client, &url).await.unwrap();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter { buf: buf.clone() };
        let (tx, mut rx) = mpsc::channel(128);
        let fetcher = tokio::spawn(run_fetcher(
            client,
            url.clone(),
            FirstAttempt { resp, metaint },
            // Reconnect mechanics under test use fill bytes, not valid
            // frames — keep the THE-984 scanner out of the way.
            ResyncMode::Passthrough,
            writer,
            tx,
        ));

        let mut collected = Vec::new();
        let saw_terminal = wait_for_event(&mut rx, &mut collected, Duration::from_secs(5), |e| {
            matches!(e, PipelineEvent::Warning(s) if s.contains("410") || s.contains("terminal status"))
        })
        .await;
        // Give the fetcher a moment to settle past the terminal — it should
        // emit the budget-exhausted warning and exit.
        let _ = tokio::time::timeout(Duration::from_secs(2), fetcher).await;

        let written = buf.lock().unwrap().clone();
        assert!(
            saw_terminal,
            "expected terminal-410 warning, got: {collected:?}"
        );
        assert_eq!(
            written,
            vec![0xAB; audio_n],
            "expected only first attempt's audio (no retry past 410)"
        );
    }

    #[tokio::test]
    async fn instant_eof_without_audio_is_terminal() {
        let (url, _server) =
            spawn_server(vec![MockResponse::InstantEof, MockResponse::InstantEof]).await;
        let client = reqwest::Client::new();
        let (resp, metaint) = open_icy(&client, &url).await.unwrap();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter { buf: buf.clone() };
        let (tx, mut rx) = mpsc::channel(128);
        let fetcher = tokio::spawn(run_fetcher(
            client,
            url.clone(),
            FirstAttempt { resp, metaint },
            // Reconnect mechanics under test use fill bytes, not valid
            // frames — keep the THE-984 scanner out of the way.
            ResyncMode::Passthrough,
            writer,
            tx,
        ));

        let mut collected = Vec::new();
        let saw_terminal = wait_for_event(&mut rx, &mut collected, Duration::from_secs(5), |e| {
            matches!(e, PipelineEvent::Warning(s) if s.contains("empty body") || s.contains("not reconnecting"))
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(2), fetcher).await;

        assert!(
            saw_terminal,
            "expected empty-body terminal warning, got: {collected:?}"
        );
        assert!(buf.lock().unwrap().is_empty(), "no audio expected");
    }

    /// Build one valid MPEG1 Layer III frame (128 kbps, 44.1 kHz, no
    /// padding — 417 bytes) filled with `fill`. `fill` must not be 0xFF so
    /// the only sync patterns in a test body are real headers.
    fn mp3_frame(fill: u8) -> Vec<u8> {
        assert_ne!(fill, 0xFF);
        let mut f = vec![fill; 417];
        f[0] = 0xFF;
        f[1] = 0xFB; // sync + MPEG1 + Layer III + no CRC
        f[2] = 0x90; // bitrate idx 9 (128 kbps), 44.1 kHz, no padding
        f[3] = 0x00;
        f
    }

    /// Build one valid ADTS AAC-LC frame (44.1 kHz) of `len` total bytes
    /// filled with `fill` (must not be 0xFF).
    fn adts_frame(fill: u8, len: usize) -> Vec<u8> {
        assert_ne!(fill, 0xFF);
        let mut f = vec![fill; len];
        f[0] = 0xFF;
        f[1] = 0xF1; // sync + MPEG-4 + layer 00 + no CRC
        f[2] = 0x50; // AAC LC, sampling-frequency index 4 (44.1 kHz)
        f[3] = ((len >> 11) & 0x3) as u8;
        f[4] = ((len >> 3) & 0xFF) as u8;
        f[5] = ((len & 0x7) << 5) as u8;
        f[6] = 0x00;
        f
    }

    #[test]
    fn frame_len_parsers_accept_canonical_reject_invalid() {
        assert_eq!(mp3_frame_len(&mp3_frame(0x11)), Some(417));
        assert_eq!(mp3_frame_len(&[0xFF, 0xFB, 0xF0, 0x00]), None); // bitrate idx 15
        assert_eq!(mp3_frame_len(&[0xFF, 0xFB, 0x0C, 0x00]), None); // free bitrate + sr idx 3
        assert_eq!(mp3_frame_len(&[0xFF, 0xEB, 0x90, 0x00]), None); // reserved version
        assert_eq!(mp3_frame_len(&[0xFF, 0xF9, 0x90, 0x00]), None); // reserved layer
        assert_eq!(mp3_frame_len(&[0x00, 0xFB, 0x90, 0x00]), None); // no sync

        assert_eq!(adts_frame_len(&adts_frame(0x11, 256)), Some(256));
        assert_eq!(adts_frame_len(&[0xFF, 0xF7, 0x50, 0x00, 0x20, 0x00]), None); // layer != 0
        assert_eq!(adts_frame_len(&[0xFF, 0xF1, 0x50, 0x00, 0x00, 0x20]), None); // len < 7
        assert_eq!(adts_frame_len(&[0xFF, 0xF1, 0xFC, 0x00, 0x20, 0x00]), None); // sf idx 15
    }

    /// THE-984 — scanner finds the first verified frame boundary even when
    /// the bytes arrive in awkward chunk sizes that split headers.
    #[test]
    fn resync_scanner_skips_torn_prefix_mp3() {
        let torn = vec![0x22u8; 137]; // mid-frame tail, no 0xFF
        let mut stream = torn.clone();
        stream.extend(mp3_frame(0x33));
        stream.extend(mp3_frame(0x44));

        let mut s = ResyncScanner::new(SyncKind::Mp3);
        let mut fed = 0;
        let mut synced = None;
        for chunk in stream.chunks(7) {
            fed += chunk.len();
            match s.feed(chunk) {
                ScanStep::Hold => {}
                ScanStep::Synced { aligned, discarded } => {
                    synced = Some((aligned, discarded));
                    break;
                }
                ScanStep::GaveUp { .. } => panic!("scanner gave up"),
            }
        }
        let (aligned, discarded) = synced.expect("never synced");
        assert_eq!(discarded, torn.len(), "exactly the torn prefix is dropped");
        assert_eq!(
            &aligned[..],
            &stream[torn.len()..fed],
            "aligned output starts at the verified frame header"
        );
    }

    /// THE-984 — a sync pattern whose computed frame length does NOT land
    /// on a second agreeing header is a false positive and must be skipped.
    #[test]
    fn resync_scanner_rejects_false_sync_without_second_header() {
        let mut prefix = vec![0u8; 300];
        let fake = adts_frame(0x00, 256);
        // Valid-looking ADTS header at offset 1, but offset 1+256 is junk.
        prefix[1..8].copy_from_slice(&fake[..7]);
        let mut stream = prefix.clone();
        stream.extend(adts_frame(0x55, 256));
        stream.extend(adts_frame(0x66, 256));

        let mut s = ResyncScanner::new(SyncKind::Adts);
        let mut synced = None;
        for chunk in stream.chunks(64) {
            if let ScanStep::Synced { aligned, discarded } = s.feed(chunk) {
                synced = Some((aligned, discarded));
                break;
            }
        }
        let (aligned, discarded) = synced.expect("never synced");
        assert_eq!(discarded, prefix.len(), "false sync at offset 1 skipped");
        assert_eq!(&aligned[..7], &adts_frame(0x55, 256)[..7]);
    }

    /// THE-984 — past the scan budget the scanner flushes and steps aside
    /// (write-through is the pre-THE-984 status quo, never worse).
    #[test]
    fn resync_scanner_gives_up_past_budget() {
        let mut s = ResyncScanner::new(SyncKind::Mp3);
        let junk = vec![0x20u8; RESYNC_SCAN_MAX + 1024];
        let mut gave_up = false;
        for chunk in junk.chunks(4096) {
            if let ScanStep::GaveUp { flush, scanned } = s.feed(chunk) {
                assert!(scanned + flush.len() > RESYNC_SCAN_MAX);
                gave_up = true;
                break;
            }
        }
        assert!(gave_up, "scanner must not withhold audio forever");
    }

    /// THE-984 — Content-Type → resync strategy. Same conservatism as the
    /// THE-972 hint table: unknown types pass through, never guess.
    #[test]
    fn content_type_maps_to_resync_mode() {
        assert_eq!(
            resync_mode("audio/mpeg"),
            ResyncMode::FrameSync(SyncKind::Mp3)
        );
        assert_eq!(
            resync_mode("audio/mpeg; charset=UTF-8"),
            ResyncMode::FrameSync(SyncKind::Mp3)
        );
        assert_eq!(
            resync_mode("audio/aacp"),
            ResyncMode::FrameSync(SyncKind::Adts)
        );
        assert_eq!(resync_mode("audio/ogg"), ResyncMode::WarnNotSelfSync);
        assert_eq!(resync_mode("application/ogg"), ResyncMode::WarnNotSelfSync);
        assert_eq!(resync_mode("audio/opus"), ResyncMode::WarnNotSelfSync);
        assert_eq!(resync_mode("audio/flac"), ResyncMode::WarnNotSelfSync);
        assert_eq!(resync_mode("text/html"), ResyncMode::Passthrough);
        assert_eq!(resync_mode(""), ResyncMode::Passthrough);
    }

    /// THE-984 acceptance — across a reconnect on a frame-sync codec, no
    /// torn bytes reach the writer: attempt 2's leading partial frame is
    /// discarded and writing resumes exactly at the first verified frame
    /// header.
    #[tokio::test]
    async fn reconnect_discards_torn_bytes_until_frame_sync() {
        // Attempt 1: two whole frames (a clean cut keeps the expected-output
        // arithmetic simple; the torn part is attempt 2's lead-in).
        let mut body_a = mp3_frame(0x21);
        body_a.extend(mp3_frame(0x22));
        // Attempt 2: rejoins mid-frame — 137 bytes of frame tail (torn),
        // then two whole frames.
        let torn = vec![0x23u8; 137];
        let f3 = mp3_frame(0x24);
        let f4 = mp3_frame(0x25);
        let mut body_b = torn.clone();
        body_b.extend_from_slice(&f3);
        body_b.extend_from_slice(&f4);

        let (url, _server) = spawn_server(vec![
            MockResponse::Ok {
                metaint: None,
                body: body_a.clone(),
            },
            MockResponse::Ok {
                metaint: None,
                body: body_b,
            },
        ])
        .await;

        let client = reqwest::Client::new();
        let (resp, metaint) = open_icy(&client, &url).await.unwrap();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter { buf: buf.clone() };
        let (tx, _rx) = mpsc::channel(128);
        let fetcher = tokio::spawn(run_fetcher(
            client,
            url.clone(),
            FirstAttempt { resp, metaint },
            ResyncMode::FrameSync(SyncKind::Mp3),
            writer,
            tx,
        ));

        let mut expected = body_a;
        expected.extend_from_slice(&f3);
        expected.extend_from_slice(&f4);
        let written = wait_for_bytes(&buf, expected.len(), Duration::from_secs(5)).await;
        fetcher.abort();

        assert_eq!(
            written, expected,
            "writer must see attempt 1 verbatim, then attempt 2 from the \
             verified frame header — no torn bytes"
        );
    }

    /// THE-984 option (a) — Ogg/FLAC reconnects pass bytes through but
    /// surface a Warning that the splice may garble audio.
    #[tokio::test]
    async fn reconnect_on_non_self_sync_warns_and_passes_through() {
        let audio_n = 100;
        let (url, _server) = spawn_server(vec![
            MockResponse::Ok {
                metaint: None,
                body: vec![0xAA; audio_n],
            },
            MockResponse::Ok {
                metaint: None,
                body: vec![0xBB; audio_n],
            },
        ])
        .await;

        let client = reqwest::Client::new();
        let (resp, metaint) = open_icy(&client, &url).await.unwrap();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter { buf: buf.clone() };
        let (tx, mut rx) = mpsc::channel(128);
        let fetcher = tokio::spawn(run_fetcher(
            client,
            url.clone(),
            FirstAttempt { resp, metaint },
            ResyncMode::WarnNotSelfSync,
            writer,
            tx,
        ));

        let written = wait_for_bytes(&buf, 2 * audio_n, Duration::from_secs(5)).await;
        let mut collected = Vec::new();
        let saw_warn = wait_for_event(
            &mut rx,
            &mut collected,
            Duration::from_secs(2),
            |e| matches!(e, PipelineEvent::Warning(s) if s.contains("non-self-synchronizing")),
        )
        .await;
        fetcher.abort();

        assert_eq!(&written[..audio_n], &vec![0xAA; audio_n][..]);
        assert_eq!(
            &written[audio_n..],
            &vec![0xBB; audio_n][..],
            "passthrough must not drop bytes"
        );
        assert!(
            saw_warn,
            "expected non-self-sync warning, got: {collected:?}"
        );
    }

    /// THE-972 — Content-Type → demuxer hint mapping. Parameters and
    /// unknown types must stay conservative: a wrong hint breaks decode
    /// outright, so anything unrecognised maps to `None` (probe).
    #[test]
    fn content_type_maps_to_demuxer_hint() {
        assert_eq!(ffmpeg_format_hint("audio/mpeg"), Some("mp3"));
        assert_eq!(ffmpeg_format_hint("audio/mpeg; charset=UTF-8"), Some("mp3"));
        assert_eq!(ffmpeg_format_hint("Audio/MPEG"), Some("mp3"));
        assert_eq!(ffmpeg_format_hint("audio/aacp"), Some("aac"));
        assert_eq!(ffmpeg_format_hint("application/ogg"), Some("ogg"));
        assert_eq!(ffmpeg_format_hint("audio/flac"), Some("flac"));
        // Unknown / absent / suspicious → probe, never guess.
        assert_eq!(ffmpeg_format_hint(""), None);
        assert_eq!(ffmpeg_format_hint("text/html"), None);
        assert_eq!(ffmpeg_format_hint("audio/x-scpls"), None);
        assert_eq!(ffmpeg_format_hint("video/mp4"), None);
    }

    /// THE-983 (AR-5) — a healthy reconnect (`consecutive_failures == 0`,
    /// i.e. `EndedAfterAudio` proved the URL good) must not sleep at all;
    /// a failure still pays the ladder. Paused tokio time makes the
    /// distinction deterministic: any `sleep` advances the mock clock.
    #[tokio::test(start_paused = true)]
    async fn healthy_reconnect_skips_backoff_sleep() {
        let (tx, _rx) = mpsc::channel(4);
        let mut attempt = 0usize;

        let t0 = tokio::time::Instant::now();
        assert!(sleep_backoff(0, &tx, "http://radio", &mut attempt).await);
        assert_eq!(
            t0.elapsed(),
            Duration::ZERO,
            "healthy reconnect slept the ladder instead of reconnecting now",
        );
        assert_eq!(attempt, 1, "the immediate reconnect still counts");

        let t1 = tokio::time::Instant::now();
        assert!(sleep_backoff(1, &tx, "http://radio", &mut attempt).await);
        assert_eq!(
            t1.elapsed(),
            Duration::from_millis(BACKOFF_LADDER_MS[0]),
            "first failure pays ladder[0]",
        );

        // Budget exhaustion contract unchanged.
        assert!(
            !sleep_backoff(
                BACKOFF_LADDER_MS.len() + 1,
                &tx,
                "http://radio",
                &mut attempt
            )
            .await
        );
    }

    // Smoke for the helper itself — keep simple so a test infrastructure
    // regression surfaces here.
    #[tokio::test]
    async fn server_serves_one_response() {
        let (url, _server) = spawn_server(vec![MockResponse::Ok {
            metaint: None,
            body: vec![0xAB; 16],
        }])
        .await;
        let body = reqwest::get(&url).await.unwrap().bytes().await.unwrap();
        assert_eq!(&body[..], &[0xAB; 16]);
    }
}
