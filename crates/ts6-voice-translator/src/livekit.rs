// LiveKit access-token minting + stub publish/subscribe bridge.
//
// Slice b ships only the access-token minter and a stub bridge so the
// daemon scaffold compiles and runs end-to-end against the
// `voice-translator` compose profile without a native libwebrtc build.
// Slice c replaces `StubLiveKitBridge` with a real implementation
// backed by the official `livekit` Rust SDK.
//
// Access-token format: https://docs.livekit.io/home/get-started/authentication/
//   - HS256 JWT, signed with the LiveKit API secret.
//   - `iss` = the LiveKit API key.
//   - `sub` + `name` = the participant identity.
//   - Custom `video` claim carries the room grant.

use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use jsonwebtoken::{EncodingKey, Header, encode};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

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

/// Stub LiveKit bridge — slice b only. Logs the would-be connect, publish,
/// and subscribe operations so an operator can see the seam where the
/// real Rust SDK plugs in. Slice c replaces this struct with one backed
/// by the `livekit` crate (RoomClient + AudioTrackPublication).
pub struct StubLiveKitBridge {
    state: BridgeState,
    room: String,
    identity: String,
    url: String,
}

impl StubLiveKitBridge {
    pub async fn connect(cfg: &LiveKitConfig, token: &str) -> Result<Self> {
        info!(
            url = %cfg.url,
            room = %cfg.room,
            identity = %cfg.identity,
            token_len = token.len(),
            "stub LiveKit bridge connect (real SDK lands in slice c)"
        );
        Ok(Self {
            state: BridgeState::Connected,
            room: cfg.room.clone(),
            identity: cfg.identity.clone(),
            url: cfg.url.clone(),
        })
    }

    pub fn state(&self) -> BridgeState {
        self.state
    }

    /// Slice c will call this with each inbound TS6 Opus frame so it
    /// gets republished onto a LiveKit publisher track. Slice b just
    /// counts and logs at debug.
    #[allow(dead_code)]
    pub fn publish_opus_frame(&self, frame_bytes: usize) {
        debug!(
            frame_bytes,
            room = %self.room,
            identity = %self.identity,
            "stub publish_opus_frame (slice c connects this to the SDK)"
        );
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        info!(
            url = %self.url,
            room = %self.room,
            "stub LiveKit bridge disconnect"
        );
        self.state = BridgeState::Disconnected;
        Ok(())
    }
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
