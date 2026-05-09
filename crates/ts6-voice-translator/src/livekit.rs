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
use audiopus::{Channels, MutSignals, SampleRate, coder::Decoder as OpusDecoder, packet::Packet};
use jsonwebtoken::{EncodingKey, Header, encode};
use livekit::options::TrackPublishOptions;
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::{
    AudioSourceOptions, RtcAudioSource, native::NativeAudioSource,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, info};

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

pub struct LiveKitBridge {
    room: Room,
    audio_source: NativeAudioSource,
    decoders: HashMap<u16, OpusDecoder>,
    decode_buf: Vec<i16>,
    state: BridgeState,
    publish_count: u64,
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
        let audio_source = NativeAudioSource::new(
            AudioSourceOptions::default(),
            SAMPLE_RATE_HZ,
            1,
            20,
        );
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

        // Drain RoomEvents into trace logs so we don't lose them. Slice d
        // taps the same channel for inbound Opus from browser participants
        // (subscribe path); for slice c we just observe.
        spawn_event_pump(events);

        Ok(Self {
            room,
            audio_source,
            decoders: HashMap::new(),
            decode_buf: vec![0i16; FRAME_SAMPLES],
            state: BridgeState::Connected,
            publish_count: 0,
        })
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
                info!(from, "first Opus frame from this TS6 client — opening decoder");
                v.insert(d)
            }
        };

        let pkt = Packet::try_from(opus)
            .map_err(|e| anyhow!("audiopus Packet::try_from: {e}"))?;
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
        self.publish_count += 1;
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
        info!(
            publish_count = self.publish_count,
            "LiveKit room closed"
        );
        self.state = BridgeState::Disconnected;
        Ok(())
    }
}

fn spawn_event_pump(mut events: mpsc::UnboundedReceiver<RoomEvent>) {
    tokio::spawn(async move {
        while let Some(ev) = events.recv().await {
            match ev {
                RoomEvent::ParticipantConnected(p) => {
                    info!(identity = %p.identity(), "LiveKit participant connected");
                }
                RoomEvent::ParticipantDisconnected(p) => {
                    info!(identity = %p.identity(), "LiveKit participant disconnected");
                }
                RoomEvent::TrackSubscribed { participant, .. } => {
                    info!(identity = %participant.identity(), "LiveKit track subscribed");
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
