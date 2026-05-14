// PURA-108 WS-4 / PURA-112 — "two clients can talk" prototype.
//
// Single binary that runs as one of two TS6 clients connecting to a
// self-hosted teamspeak6-server fixture (default 127.0.0.1:9987). Each
// instance simultaneously sends synthesized Opus voice frames at the
// standard 20 ms / 48 kHz / mono cadence and receives whatever the
// server forwards from the other client, decoding each remote sender's
// stream back to PCM and writing per-sender WAV files for offline
// audibility verification.
//
// Two-process recipe: `make voice-prototype`. Operator notes:
// `docs/voice-prototype.md`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use audiopus::{
    Application, Channels, MutSignals, SampleRate,
    coder::{Decoder as OpusDecoder, Encoder as OpusEncoder},
    packet::Packet,
};
use clap::Parser;
use futures::StreamExt;
use hound::{WavSpec, WavWriter};
use tokio::time::{Instant, interval};
use tracing::{debug, error, info, warn};
use tsclientlib::{Connection, DisconnectOptions, Identity, Reason, StreamItem};
use tsproto_packets::packets::{AudioData, CodecType, OutAudio};

const SAMPLE_RATE_HZ: u32 = 48_000;
const FRAME_MS: u32 = 20;
const FRAME_SAMPLES: usize = (SAMPLE_RATE_HZ / 1_000 * FRAME_MS) as usize; // 960
const MAX_OPUS_FRAME: usize = 4_000;

#[derive(Parser, Debug)]
#[command(
    name = "ts6-voice-prototype",
    about = "Bidirectional Opus voice prototype against a self-hosted TS6 fixture (PURA-108 WS-4)"
)]
struct Cli {
    /// Server host:port. Default = local podman-compose ts6-fixture (host net).
    #[arg(long, default_value = "127.0.0.1:9987")]
    server: String,

    /// In-channel display name. Use a distinct name per peer.
    #[arg(long)]
    name: String,

    /// Per-instance state directory; holds the cached identity. Two simultaneous
    /// instances MUST use different directories.
    #[arg(long)]
    identity_dir: PathBuf,

    /// WAV output stem. Per-sender files are written as `<stem>.from-<client_id>.wav`
    /// (e.g. `--out-wav /tmp/alice.wav` → `/tmp/alice.from-2.wav`).
    #[arg(long)]
    out_wav: PathBuf,

    /// Send + receive duration in seconds. Acceptance bar is ≥30 s.
    #[arg(long, default_value_t = 30)]
    duration_secs: u64,

    /// Tone frequency in Hz to send. Pass distinct values per peer (e.g. 440 / 660)
    /// so each side's WAV is identifiable. 0 = silence.
    #[arg(long, default_value_t = 440.0)]
    send_tone_hz: f32,

    /// Handshake timeout.
    #[arg(long, default_value_t = 30)]
    connect_timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ts6_voice_prototype=debug".into()),
        )
        .init();
    let cli = Cli::parse();

    info!(
        server = %cli.server,
        name = %cli.name,
        identity_dir = %cli.identity_dir.display(),
        out_wav = %cli.out_wav.display(),
        duration_secs = cli.duration_secs,
        tone_hz = cli.send_tone_hz,
        "ts6-voice-prototype starting",
    );

    tokio::fs::create_dir_all(&cli.identity_dir)
        .await
        .with_context(|| format!("create identity dir {}", cli.identity_dir.display()))?;
    if let Some(parent) = cli.out_wav.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("create wav parent dir {}", parent.display()))?;
    }

    let identity_path = cli.identity_dir.join("identity.json");
    let identity = load_or_create_identity(&identity_path).await?;
    info!(
        uid = %identity.key().to_pub().get_uid(),
        level = identity.level(),
        "identity ready"
    );

    let mut con = Connection::build(cli.server.as_str())
        .name(cli.name.clone())
        .identity(identity)
        .log_commands(false)
        .log_packets(false)
        .log_udp_packets(false)
        .connect()
        .context("Connection::build()")?;

    info!("connection object created — driving handshake");

    if !wait_for_connected(&mut con, Duration::from_secs(cli.connect_timeout_secs)).await? {
        bail!(
            "handshake did not complete within {}s — fixture up?",
            cli.connect_timeout_secs
        );
    }
    info!("handshake done — entering bidirectional Opus loop");

    let encoder = OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
        .map_err(|e| anyhow!("OpusEncoder::new failed: {e}"))?;
    let mut tone = ToneGen::new(cli.send_tone_hz, SAMPLE_RATE_HZ);
    let mut pcm_in = vec![0.0_f32; FRAME_SAMPLES];
    let mut opus_out = [0u8; MAX_OPUS_FRAME];

    let mut sinks: HashMap<u16, RecvSink> = HashMap::new();
    let wav_stem = cli.out_wav.clone();

    let mut tick = interval(Duration::from_millis(FRAME_MS as u64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = tokio::time::sleep(Duration::from_secs(cli.duration_secs));
    tokio::pin!(deadline);

    let mut frames_sent = 0u64;
    let started = Instant::now();
    let mut stop_sent = false;

    // Borrow-checker dance: the events stream borrows `&mut con` for as long
    // as it lives, which conflicts with `con.send_audio(...)` calls in the
    // tick / deadline arms. Pattern (matches `voice-tx/src/main.rs` from the
    // PURA-7 Day-2 spike): create a fresh events future inline as the arm
    // expression each iteration. `tokio::select!` drops the non-selected
    // arms' futures before running the chosen arm's body, releasing the
    // borrow so the body can mutate `con` again.
    'outer: loop {
        tokio::select! {
            biased;
            ev = async { con.events().next().await } => match ev {
                Some(Ok(item)) => {
                    handle_event(item, &wav_stem, &mut sinks)?;
                }
                Some(Err(e)) => {
                    error!(error = %e, "stream error during voice loop");
                    break 'outer;
                }
                None => {
                    warn!("event stream ended");
                    break 'outer;
                }
            },
            _ = &mut deadline => {
                if !stop_sent {
                    info!(
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        frames_sent,
                        "duration reached — sending voice-stop"
                    );
                    let stop_pkt = OutAudio::new(&AudioData::C2S {
                        id: 0,
                        codec: CodecType::OpusVoice,
                        data: &[],
                    });
                    con.send_audio(stop_pkt)?;
                    stop_sent = true;
                    // Drain extra time after the stop to capture late-forwarded frames.
                    deadline
                        .as_mut()
                        .reset(Instant::now() + Duration::from_millis(750));
                } else {
                    break 'outer;
                }
            }
            _ = tick.tick(), if !stop_sent => {
                tone.fill(&mut pcm_in);
                let opus_len = encoder
                    .encode_float(&pcm_in[..], &mut opus_out)
                    .map_err(|e| anyhow!("opus encode failed: {e}"))?;
                let pkt = OutAudio::new(&AudioData::C2S {
                    id: 0,
                    codec: CodecType::OpusVoice,
                    data: &opus_out[..opus_len],
                });
                con.send_audio(pkt)?;
                frames_sent += 1;
            }
        }
    }

    info!(frames_sent, "exiting — sending clean disconnect");
    if let Err(err) = con.disconnect(
        DisconnectOptions::new()
            .reason(Reason::Clientdisconnect)
            .message("ts6-voice-prototype shutdown"),
    ) {
        warn!(?err, "disconnect failed (non-fatal)");
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        let drain = con.events();
        tokio::pin!(drain);
        while drain.next().await.is_some() {}
    })
    .await;

    let mut summary: Vec<(u16, u64, PathBuf)> = sinks
        .into_iter()
        .map(|(id, sink)| {
            let path = sink.path.clone();
            let n = sink.frames;
            if let Err(e) = sink.writer.finalize() {
                warn!(?e, %id, "WAV finalize failed");
            }
            (id, n, path)
        })
        .collect();
    summary.sort_by_key(|(id, _, _)| *id);

    if summary.is_empty() {
        warn!(
            "no S2C audio frames received — see docs/voice-prototype.md \"no audio?\" \
             section (channel default-perms / common channel)"
        );
    } else {
        for (id, n, path) in &summary {
            info!(
                remote_client_id = id,
                frames = n,
                wav = %path.display(),
                "wrote WAV from remote sender"
            );
        }
    }

    let frames_recv: u64 = summary.iter().map(|(_, n, _)| *n).sum();
    info!(
        frames_sent,
        frames_recv, "ts6-voice-prototype exited cleanly"
    );

    Ok(())
}

struct RecvSink {
    decoder: OpusDecoder,
    writer: WavWriter<std::io::BufWriter<std::fs::File>>,
    path: PathBuf,
    frames: u64,
}

impl RecvSink {
    fn new(client_id: u16, stem: &Path) -> Result<Self> {
        let path = derive_wav_path(stem, client_id);
        let spec = WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE_HZ,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = WavWriter::create(&path, spec)
            .with_context(|| format!("create WAV {}", path.display()))?;
        let decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| anyhow!("OpusDecoder::new failed: {e}"))?;
        Ok(RecvSink {
            decoder,
            writer,
            path,
            frames: 0,
        })
    }
}

fn derive_wav_path(stem: &Path, client_id: u16) -> PathBuf {
    let parent = stem.parent().unwrap_or_else(|| Path::new("."));
    let stem_name = stem.file_stem().and_then(|s| s.to_str()).unwrap_or("voice");
    let ext = stem.extension().and_then(|s| s.to_str()).unwrap_or("wav");
    parent.join(format!("{stem_name}.from-{client_id}.{ext}"))
}

fn handle_event(item: StreamItem, stem: &Path, sinks: &mut HashMap<u16, RecvSink>) -> Result<()> {
    match item {
        StreamItem::Audio(buf) => {
            let audio = buf.data().data();
            let (from, codec, opus): (u16, CodecType, &[u8]) = match audio {
                AudioData::S2C {
                    from, codec, data, ..
                } => (*from, *codec, *data),
                AudioData::S2CWhisper {
                    from, codec, data, ..
                } => (*from, *codec, *data),
                _ => return Ok(()),
            };
            if !matches!(codec, CodecType::OpusVoice | CodecType::OpusMusic) {
                debug!(?codec, "skipping non-Opus codec");
                return Ok(());
            }
            if opus.is_empty() {
                debug!(from, "voice-stop from peer");
                return Ok(());
            }
            let sink = match sinks.entry(from) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(v) => {
                    info!(from, "first audio frame from this client — opening WAV");
                    v.insert(RecvSink::new(from, stem)?)
                }
            };
            let mut pcm = vec![0i16; FRAME_SAMPLES];
            let opus_pkt = Packet::try_from(opus)
                .map_err(|e| anyhow!("audiopus Packet::try_from failed: {e}"))?;
            let signals = MutSignals::try_from(&mut pcm[..])
                .map_err(|e| anyhow!("audiopus MutSignals::try_from failed: {e}"))?;
            let n = sink
                .decoder
                .decode(Some(opus_pkt), signals, false)
                .map_err(|e| anyhow!("opus decode failed: {e}"))?;
            for sample in &pcm[..n] {
                sink.writer.write_sample(*sample)?;
            }
            sink.frames += 1;
        }
        StreamItem::AudioChange(change) => {
            debug!(?change, "audio-permission change");
        }
        StreamItem::DisconnectedTemporarily(reason) => {
            warn!(?reason, "disconnected temporarily — tsclientlib will retry");
        }
        StreamItem::IdentityLevelIncreasing(level) => {
            info!(level, "server requires higher identity level — upgrading");
        }
        StreamItem::IdentityLevelIncreased => {
            info!("identity upgraded — handshake will resume");
        }
        other => debug!(?other, "stream item"),
    }
    Ok(())
}

async fn wait_for_connected(con: &mut Connection, timeout: Duration) -> Result<bool> {
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let events = con.events();
    tokio::pin!(events);
    loop {
        tokio::select! {
            biased;
            _ = &mut deadline => return Ok(false),
            ev = events.next() => match ev {
                Some(Ok(StreamItem::BookEvents(_))) => return Ok(true),
                Some(Ok(StreamItem::IdentityLevelIncreasing(level))) => {
                    info!(level, "server requires higher identity — upgrading");
                }
                Some(Ok(StreamItem::IdentityLevelIncreased)) => {
                    info!("identity upgraded — handshake resumes");
                }
                Some(Ok(other)) => debug!(?other, "stream item during handshake"),
                Some(Err(err)) => return Err(anyhow!("stream error during handshake: {err}")),
                None => return Ok(false),
            }
        }
    }
}

async fn load_or_create_identity(path: &Path) -> Result<Identity> {
    if path.exists() {
        let raw = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read identity file {}", path.display()))?;
        let identity: Identity = serde_json::from_str(raw.trim())
            .with_context(|| format!("parse identity file {}", path.display()))?;
        info!(
            path = %path.display(),
            level = identity.level(),
            "loaded cached identity"
        );
        Ok(identity)
    } else {
        let identity = Identity::create();
        let raw = serde_json::to_string(&identity)?;
        tokio::fs::write(path, &raw)
            .await
            .with_context(|| format!("write identity file {}", path.display()))?;
        info!(
            path = %path.display(),
            level = identity.level(),
            "generated new identity"
        );
        Ok(identity)
    }
}

struct ToneGen {
    freq_hz: f32,
    sample_rate: f32,
    phase: f32,
}

impl ToneGen {
    fn new(freq_hz: f32, sample_rate: u32) -> Self {
        Self {
            freq_hz,
            sample_rate: sample_rate as f32,
            phase: 0.0,
        }
    }

    fn fill(&mut self, buf: &mut [f32]) {
        if self.freq_hz <= 0.0 {
            for s in buf.iter_mut() {
                *s = 0.0;
            }
            return;
        }
        let two_pi = 2.0 * std::f32::consts::PI;
        let step = two_pi * self.freq_hz / self.sample_rate;
        const AMP: f32 = 0.25;
        for s in buf.iter_mut() {
            *s = AMP * self.phase.sin();
            self.phase += step;
            if self.phase > two_pi {
                self.phase -= two_pi;
            }
        }
    }
}
