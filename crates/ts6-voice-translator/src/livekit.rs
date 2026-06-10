// LiveKit access-token minting + the audio publisher bridge.
//
// Slice b shipped only the access-token minter and a stub bridge so the
// daemon scaffold compiled without a native libwebrtc build. Slice c
// (this revision) replaces the stub with `LiveKitBridge`, backed by the
// official `livekit` Rust SDK:
//
//   - `Room::connect(url, token, RoomOptions::default())` joins the
//     LiveKit room (the daemon's TS6 voice room mirror).
//   - A `NativeAudioSource` (48 kHz mono, 20 ms queue depth) feeds a
//     `LocalAudioTrack` published as `TrackSource::Microphone`. Browser
//     participants subscribe to it as a normal microphone track.
//   - `publish_opus_frame(from, opus)` decodes the inbound TS6 Opus
//     frame to PCM and pushes it to the audio source. The SDK re-encodes
//     into the LiveKit RTP/Opus stream over SRTP/DTLS. The intermediate
//     PCM hop costs ~80 µs/frame on the dev box and is the path the
//     LiveKit Rust SDK officially documents — there is no first-class
//     "publish pre-encoded Opus" API in `livekit` 0.7. A passthrough
//     path is a future optimization noted in the runbook.
//
// Access-token format: https://docs.livekit.io/home/get-started/authentication/
//   - HS256 JWT, signed with the LiveKit API secret.
//   - `iss` = the LiveKit API key.
//   - `sub` + `name` = the participant identity.
//   - Custom `video` claim carries the room grant.

use std::borrow::Cow;
use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use audiopus::{
    Application, Channels, MutSignals, SampleRate,
    coder::{Decoder as OpusDecoder, Encoder as OpusEncoder},
    packet::Packet,
};
use futures::StreamExt;
use jsonwebtoken::{EncodingKey, Header, encode};
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::{
    AudioSourceOptions, RtcAudioSource, native::NativeAudioSource,
};
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// 20 ms / 48 kHz / mono = 960 samples per Opus frame. Same constant the WS-4
// prototype settled on — TS6 emits exactly this framing on the §19.10 wire.
const FRAME_SAMPLES: usize = 960;
const SAMPLE_RATE_HZ: u32 = 48_000;

#[derive(Debug, Clone)]
pub struct LiveKitConfig {
    pub url: String,
    pub room: String,
    pub identity: String,
    pub api_key: String,
    pub api_secret: String,
    pub ttl: Duration,
}

#[derive(Serialize, Deserialize, Debug)]
struct VideoGrant {
    #[serde(rename = "roomJoin")]
    room_join: bool,
    room: String,
    #[serde(rename = "canPublish")]
    can_publish: bool,
    #[serde(rename = "canSubscribe")]
    can_subscribe: bool,
    #[serde(rename = "canPublishData")]
    can_publish_data: bool,
}

#[derive(Serialize, Deserialize, Debug)]
struct AccessTokenClaims {
    iss: String,
    sub: String,
    nbf: u64,
    exp: u64,
    name: String,
    video: VideoGrant,
}

impl LiveKitConfig {
    pub fn mint_token(&self) -> Result<String> {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .context("system clock before UNIX epoch")?
            .as_secs();
        // Floor the TTL at one minute so a misconfigured `--duration-secs 0`
        // still produces a usable token for an operator dry-run.
        let ttl_secs = self.ttl.as_secs().max(60);
        let exp = now.saturating_add(ttl_secs);
        let claims = AccessTokenClaims {
            iss: self.api_key.clone(),
            sub: self.identity.clone(),
            nbf: now,
            exp,
            name: self.identity.clone(),
            video: VideoGrant {
                room_join: true,
                room: self.room.clone(),
                can_publish: true,
                can_subscribe: true,
                can_publish_data: false,
            },
        };
        let token = encode(
            &Header::default(),
            &claims,
            &EncodingKey::from_secret(self.api_secret.as_bytes()),
        )
        .context("encode LiveKit access token")?;
        Ok(token)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BridgeState {
    Connected,
    Disconnected,
}

/// Reverse-path Opus frame: a 20 ms / 48 kHz mono Opus payload encoded
/// from a remote LiveKit participant's audio. Main loop forwards each
/// of these into the TS6 voice room as a synthetic-client send so
/// native TS6 clients hear the browser participant.
pub type InboundOpusFrame = Vec<u8>;

pub struct LiveKitBridge {
    room: Room,
    audio_source: NativeAudioSource,
    decoders: HashMap<u16, OpusDecoder>,
    decode_buf: Vec<i16>,
    state: BridgeState,
    /// Receiver for browser-side Opus frames produced by per-track
    /// subscriber tasks spawned in `connect`. Drained by the main
    /// loop's `select!` and forwarded into TS6.
    inbound_opus_rx: mpsc::Receiver<InboundOpusFrame>,
}

impl LiveKitBridge {
    pub async fn connect(cfg: &LiveKitConfig, token: &str) -> Result<Self> {
        let (room, events) = Room::connect(&cfg.url, token, RoomOptions::default())
            .await
            .map_err(|e| anyhow!("LiveKit Room::connect failed: {e}"))?;
        info!(
            url = %cfg.url,
            room = %cfg.room,
            local_identity = %cfg.identity,
            "joined LiveKit room"
        );

        // queue_size_ms = 20 matches TS6's 20 ms framing one-to-one. The fast
        // path (queue_size_ms = 0) requires exactly 10 ms frames per
        // `NativeAudioSource::new` semantics, which would force us to split
        // each TS6 frame in half before publishing.
        let audio_source =
            NativeAudioSource::new(AudioSourceOptions::default(), SAMPLE_RATE_HZ, 1, 20);
        let track = LocalAudioTrack::create_audio_track(
            "ts6-bridge",
            RtcAudioSource::Native(audio_source.clone()),
        );
        let publish_opts = TrackPublishOptions {
            source: TrackSource::Microphone,
            ..Default::default()
        };
        let _publication = room
            .local_participant()
            .publish_track(LocalTrack::Audio(track), publish_opts)
            .await
            .map_err(|e| anyhow!("publish_track failed: {e}"))?;
        info!("LiveKit publisher track up — `ts6-bridge` Microphone source");

        // Slice d: per-track subscriber tasks send 20 ms Opus frames here,
        // drained by the main loop's `select!`. Bounded so a stalled TS6
        // sender backs pressure into the LiveKit subscribe path instead of
        // queueing unbounded across the bridge.
        let (inbound_opus_tx, inbound_opus_rx) = mpsc::channel::<InboundOpusFrame>(256);

        // Pump RoomEvents: log everything, and on every TrackSubscribed
        // for an audio track, spawn a per-track subscriber that produces
        // 20 ms Opus frames into `inbound_opus_tx`.
        spawn_event_pump(events, inbound_opus_tx);

        Ok(Self {
            room,
            audio_source,
            decoders: HashMap::new(),
            decode_buf: vec![0i16; FRAME_SAMPLES],
            state: BridgeState::Connected,
            inbound_opus_rx,
        })
    }

    /// Consume the next browser-side Opus frame produced by a subscribed
    /// remote audio track. Returns `None` when the bridge's room closes.
    pub async fn recv_inbound_opus(&mut self) -> Option<InboundOpusFrame> {
        self.inbound_opus_rx.recv().await
    }

    pub fn state(&self) -> BridgeState {
        self.state
    }

    /// Forward one TS6 Opus voice frame onto the LiveKit publisher track.
    /// `from` is the TS6 client id of the speaker — multiple speakers in
    /// the same TS6 channel are mixed by `NativeAudioSource`'s buffered
    /// queue (last-write-wins per 20 ms slot), which mirrors how a TS6
    /// client renders concurrent talkers locally before they hit the
    /// soundcard. A proper per-participant publisher track per remote
    /// speaker is a future optimisation.
    pub async fn publish_opus_frame(&mut self, from: u16, opus: &[u8]) -> Result<()> {
        if opus.is_empty() {
            // TS6 sends an empty frame as the "voice-stop" heartbeat. Don't
            // forward — let LiveKit's silence-detection do its thing.
            debug!(from, "voice-stop heartbeat from TS6 — not forwarding");
            return Ok(());
        }

        let decoder = match self.decoders.entry(from) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => {
                let d = OpusDecoder::new(SampleRate::Hz48000, Channels::Mono)
                    .map_err(|e| anyhow!("OpusDecoder::new for client {from}: {e}"))?;
                info!(
                    from,
                    "first Opus frame from this TS6 client — opening decoder"
                );
                v.insert(d)
            }
        };

        let pkt = Packet::try_from(opus).map_err(|e| anyhow!("audiopus Packet::try_from: {e}"))?;
        let signals = MutSignals::try_from(&mut self.decode_buf[..])
            .map_err(|e| anyhow!("audiopus MutSignals::try_from: {e}"))?;
        let n = decoder
            .decode(Some(pkt), signals, false)
            .map_err(|e| anyhow!("opus decode (client {from}): {e}"))?;

        let frame = AudioFrame {
            data: Cow::Borrowed(&self.decode_buf[..n]),
            sample_rate: SAMPLE_RATE_HZ,
            num_channels: 1,
            samples_per_channel: n as u32,
        };
        self.audio_source
            .capture_frame(&frame)
            .await
            .map_err(|e| anyhow!("NativeAudioSource::capture_frame: {e}"))?;
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        if self.state == BridgeState::Disconnected {
            return Ok(());
        }
        self.room
            .close()
            .await
            .map_err(|e| anyhow!("Room::close: {e}"))?;
        info!("LiveKit room closed");
        self.state = BridgeState::Disconnected;
        Ok(())
    }
}

fn spawn_event_pump(
    mut events: mpsc::UnboundedReceiver<RoomEvent>,
    inbound_opus_tx: mpsc::Sender<InboundOpusFrame>,
) {
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                RoomEvent::ParticipantConnected(p) => {
                    info!(identity = %p.identity(), "LiveKit participant connected");
                }
                RoomEvent::ParticipantDisconnected(p) => {
                    info!(identity = %p.identity(), "LiveKit participant disconnected");
                }
                RoomEvent::TrackSubscribed {
                    track, participant, ..
                } => {
                    let identity = participant.identity().to_string();
                    match track {
                        RemoteTrack::Audio(audio) => {
                            info!(
                                identity = %identity,
                                sid = %audio.sid(),
                                "LiveKit audio track subscribed — spawning reverse-path encoder"
                            );
                            spawn_audio_subscriber(audio, identity, inbound_opus_tx.clone());
                        }
                        RemoteTrack::Video(_) => {
                            debug!(identity = %identity, "ignoring subscribed video track");
                        }
                    }
                }
                RoomEvent::TrackUnsubscribed { participant, .. } => {
                    info!(identity = %participant.identity(), "LiveKit track unsubscribed");
                }
                RoomEvent::Disconnected { reason } => {
                    info!(?reason, "LiveKit room disconnected");
                    break;
                }
                other => debug!(?other, "LiveKit room event"),
            }
        }
    });
}

/// Per-track reverse-path task. Drains the `NativeAudioStream` for one
/// remote audio track, accumulates 10 ms PCM blocks into 20 ms windows
/// (TS6's §19.10 framing), encodes each window to Opus with a fresh
/// `OpusEncoder`, and forwards over the bridge's inbound channel.
///
/// Each subscribed track gets its own encoder so multiple browser
/// participants don't fight for the same predictor state. Slice e
/// (browser demo) is the natural acceptance gate for this path.
fn spawn_audio_subscriber(
    audio: RemoteAudioTrack,
    identity: String,
    inbound_opus_tx: mpsc::Sender<InboundOpusFrame>,
) {
    tokio::spawn(async move {
        // The public `NativeAudioStream::new` takes (track, sample_rate,
        // num_channels). Backpressure is handled inside the SDK; if the
        // consumer falls behind, the SDK drops the oldest queued frames
        // (live-voice-friendly behaviour).
        let mut stream = NativeAudioStream::new(audio.rtc_track(), SAMPLE_RATE_HZ as i32, 1);

        let encoder = match OpusEncoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
        {
            Ok(e) => e,
            Err(err) => {
                warn!(?err, identity = %identity, "OpusEncoder::new failed; track abandoned");
                return;
            }
        };

        // 20 ms Opus frame fits comfortably under 1 KB at typical voice bitrates;
        // a 4 KB scratch buffer is a generous upper bound.
        let mut opus_out = vec![0u8; 4096];
        let mut frame_window: Vec<i16> = Vec::with_capacity(FRAME_SAMPLES * 2);
        let mut frames_published = 0u64;
        // THE-981 (AR-11) — count format-mismatch drops and surface them at
        // WARN. Each dropped frame is 100 % silent audio loss on the bridge;
        // the old debug!-only line meant an SDK update or remote-track
        // reconfiguration delivering 16 kHz / stereo would mute the bridge
        // with no operator-visible evidence. WARN on the first drop and
        // every 500 thereafter (~10 s of audio) so a sustained mismatch is
        // loud in logs without flooding them.
        let mut format_drops = 0u64;

        while let Some(frame) = stream.next().await {
            // The SDK sometimes delivers frames at sample rates other than
            // what we asked for (typical when libwebrtc resamples internally
            // before the sink). Skip such frames; we ask for 48 kHz mono.
            if frame.sample_rate != SAMPLE_RATE_HZ || frame.num_channels != 1 {
                format_drops += 1;
                if format_drops == 1 || format_drops.is_multiple_of(500) {
                    warn!(
                        identity = %identity,
                        sample_rate = frame.sample_rate,
                        num_channels = frame.num_channels,
                        format_drops,
                        "dropping LiveKit frame with unexpected format — \
                         bridge audio from this track is being lost"
                    );
                } else {
                    debug!(
                        sample_rate = frame.sample_rate,
                        num_channels = frame.num_channels,
                        "dropping LiveKit frame with unexpected format"
                    );
                }
                continue;
            }
            frame_window.extend_from_slice(&frame.data);

            while frame_window.len() >= FRAME_SAMPLES {
                let window: Vec<i16> = frame_window.drain(..FRAME_SAMPLES).collect();
                let opus_len = match encoder.encode(&window, &mut opus_out[..]) {
                    Ok(n) => n,
                    Err(err) => {
                        warn!(?err, "opus encode failed; dropping window");
                        continue;
                    }
                };
                let opus_frame: InboundOpusFrame = opus_out[..opus_len].to_vec();
                if inbound_opus_tx.send(opus_frame).await.is_err() {
                    debug!(
                        identity = %identity,
                        frames_published,
                        "inbound Opus channel closed — bridge consumer gone"
                    );
                    return;
                }
                frames_published += 1;
            }
        }

        info!(
            identity = %identity,
            frames_published,
            "LiveKit audio subscriber drained — track ended"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};

    fn dev_cfg(ttl_secs: u64) -> LiveKitConfig {
        LiveKitConfig {
            url: "ws://127.0.0.1:7880".into(),
            room: "test-room".into(),
            identity: "test-identity".into(),
            api_key: "devkey".into(),
            api_secret: "DEV_ONLY_CHANGE_ME_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    fn validation_for(cfg: &LiveKitConfig) -> Validation {
        let mut v = Validation::new(Algorithm::HS256);
        v.set_issuer(&[cfg.api_key.as_str()]);
        // LiveKit access tokens carry only `exp` from RFC 7519's required
        // set; remove the other defaults so decode does not reject the
        // token for missing claims jsonwebtoken pre-requires by default.
        v.required_spec_claims.clear();
        v.required_spec_claims.insert("exp".into());
        v
    }

    #[test]
    fn mint_token_roundtrips_through_jsonwebtoken() {
        let cfg = dev_cfg(120);
        let token = cfg.mint_token().expect("mint token");
        let decoded = decode::<AccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(cfg.api_secret.as_bytes()),
            &validation_for(&cfg),
        )
        .expect("decode roundtrip");
        assert_eq!(decoded.claims.iss, "devkey");
        assert_eq!(decoded.claims.sub, "test-identity");
        assert_eq!(decoded.claims.name, "test-identity");
        assert_eq!(decoded.claims.video.room, "test-room");
        assert!(decoded.claims.video.room_join);
        assert!(decoded.claims.video.can_publish);
        assert!(decoded.claims.video.can_subscribe);
        assert!(!decoded.claims.video.can_publish_data);
    }

    #[test]
    fn ttl_is_floored_to_one_minute() {
        // Very short TTLs must still produce a usable token; a signing
        // helper that emits an already-expired JWT is worse than useless.
        let cfg = dev_cfg(0);
        let token = cfg.mint_token().expect("mint");
        let decoded = decode::<AccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(cfg.api_secret.as_bytes()),
            &validation_for(&cfg),
        )
        .expect("decode");
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            decoded.claims.exp >= now + 60,
            "exp must be at least 60s in the future even when ttl=0; got exp={} now={}",
            decoded.claims.exp,
            now
        );
    }

    #[test]
    fn wrong_secret_fails_signature() {
        let cfg = dev_cfg(60);
        let token = cfg.mint_token().expect("mint");
        let other_secret = b"some-other-secret";
        let err = decode::<AccessTokenClaims>(
            &token,
            &DecodingKey::from_secret(other_secret),
            &validation_for(&cfg),
        )
        .expect_err("must reject wrong-secret signature");
        assert!(matches!(
            err.kind(),
            jsonwebtoken::errors::ErrorKind::InvalidSignature
        ));
    }
}
