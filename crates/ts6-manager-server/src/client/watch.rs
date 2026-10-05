//! Typed client for `/api/watch/*`.
//!
//! Session, ticket, and playhead calls go through [`crate::client::api`]
//! so they share the panel access JWT and the single-flight refresh gate.
//! Paths are relative to [`crate::client::api::api_base`] (the page origin).
//! The watch ticket authorizes that handoff. Phase 1 does not present it
//! on the WebTransport session.

use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// Refresh a 120 s ticket this long before `ticketExpiresAt`.
pub const TICKET_REFRESH_LEAD: Duration = Duration::from_secs(30);

/// `POST /api/watch/sessions` body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateSessionBody<'a> {
    broadcast: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    replace: bool,
}

/// `POST /api/watch/tickets` body.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshTicketBody<'a> {
    session_id: &'a str,
}

/// Handoff returned by session create, ticket refresh, and the 201/200 body.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketGrant {
    pub session_id: String,
    pub ticket: String,
    pub ticket_expires_at: i64,
    pub relay_url: String,
    pub broadcast: String,
    pub cert_hash: Option<String>,
    pub alpn: String,
    pub video_track: String,
    pub audio_track: String,
}

/// `GET /api/watch/playhead/{broadcast}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayheadView {
    pub broadcast: String,
    pub position_ms: u64,
    pub paused: bool,
    #[serde(default)]
    pub updated_at: Option<i64>,
    #[serde(default)]
    pub updated_by: Option<i64>,
}

/// Certificate pin from a grant. `System` is a null `certHash`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayCert {
    Sha256Hex(String),
    System,
}

/// Failures the Watch page renders. `SessionExpired` is a 401 after the
/// refresh gate gives up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchFailure {
    SessionExpired(String),
    SessionAnonymous,
    SessionActive {
        session_id: String,
        broadcast: String,
    },
    SessionGone,
    RelayUnconfigured,
    InvalidBroadcast,
    BadCertHash,
    Other {
        status: u16,
        message: String,
    },
    Transport(String),
    Deserialise(String),
    UnsupportedTarget,
}

impl WatchFailure {
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, Self::SessionExpired(_))
    }

    pub fn title(&self) -> &'static str {
        match self {
            Self::SessionExpired(_) => "Session expired",
            Self::SessionAnonymous => "Session not ready",
            Self::SessionActive { .. } => "Watch session already active",
            Self::SessionGone => "Watch session ended",
            Self::RelayUnconfigured => "Relay not configured",
            Self::InvalidBroadcast => "Invalid broadcast name",
            Self::BadCertHash => "Bad relay certificate",
            Self::Other { .. } => "Could not watch",
            Self::Transport(_) => "Could not reach the panel",
            Self::Deserialise(_) => "Unexpected response",
            Self::UnsupportedTarget => "Watch unavailable",
        }
    }

    pub fn body(&self) -> String {
        match self {
            Self::SessionExpired(_) => "Sign in again to watch.".into(),
            Self::SessionAnonymous => "The session is still loading. Retry in a moment.".into(),
            Self::SessionActive { broadcast, .. } => {
                if broadcast.is_empty() {
                    "You already have a watch session. Take over to replace it.".into()
                } else {
                    format!(
                        "You already have a watch session on {broadcast}. Take over to replace it."
                    )
                }
            }
            Self::SessionGone => "The watch session is no longer active.".into(),
            Self::RelayUnconfigured => {
                "The server has no MoQ relay URL. Set MOQ_PUBLIC_URL and try again.".into()
            }
            Self::InvalidBroadcast => {
                "Use a name like lavfi-spike: letters, digits, and / . _ - segments.".into()
            }
            Self::BadCertHash => "The relay certificate hash is not 64 hex characters.".into(),
            Self::Other { status, message } => format!("{status}: {message}"),
            Self::Transport(message) | Self::Deserialise(message) => message.clone(),
            Self::UnsupportedTarget => "This view cannot call the watch API.".into(),
        }
    }
}

impl From<ApiError> for WatchFailure {
    fn from(err: ApiError) -> Self {
        match err {
            ApiError::Unauthorized(message) => Self::SessionExpired(message),
            ApiError::SessionAnonymous => Self::SessionAnonymous,
            ApiError::Client { status, message } | ApiError::Server { status, message } => {
                let body = serde_json::json!({ "error": message }).to_string();
                classify_error(status, &body)
            }
            ApiError::BadGateway { error, .. } => Self::Other {
                status: 502,
                message: error,
            },
            ApiError::Transport(message) => Self::Transport(message),
            ApiError::Deserialise(message) => Self::Deserialise(message),
            ApiError::UnsupportedTarget => Self::UnsupportedTarget,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    error: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    broadcast: Option<String>,
}

/// Decode a SHA-256 certificate fingerprint. Accepts 64 hex characters,
/// either case, with surrounding whitespace ignored.
pub fn decode_cert_hash(hex_in: &str) -> Option<[u8; 32]> {
    let hex = hex_in.trim();
    if hex.len() != 64 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_nibble(bytes[i * 2])?;
        let lo = hex_nibble(bytes[i * 2 + 1])?;
        out[i] = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Milliseconds until a ticket refresh should fire. `0` means immediately
/// (inside the lead window, or already past `expires_at_unix`).
pub fn ticket_refresh_delay_ms(now_unix: i64, expires_at_unix: i64) -> u64 {
    let lead = TICKET_REFRESH_LEAD.as_secs() as i64;
    let fire_at = expires_at_unix.saturating_sub(lead);
    if now_unix >= fire_at {
        0
    } else {
        (fire_at - now_unix) as u64 * 1_000
    }
}

/// `m:ss` until the ticket expires, or `expired`.
pub fn format_ticket_countdown(now_unix: i64, expires_at_unix: i64) -> String {
    if now_unix >= expires_at_unix {
        return "expired".to_string();
    }
    let remain = (expires_at_unix - now_unix) as u64;
    format!("{}:{:02}", remain / 60, remain % 60)
}

/// Shared playhead clock for the debug line. Hours appear only past 60 minutes.
pub fn format_playhead(position_ms: u64, paused: bool) -> String {
    let total = position_ms / 1_000;
    let hours = total / 3_600;
    let minutes = (total % 3_600) / 60;
    let seconds = total % 60;
    let clock = if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    };
    let motion = if paused { "paused" } else { "playing" };
    format!("{clock} · {motion}")
}

pub fn relay_cert(grant: &TicketGrant) -> Result<RelayCert, WatchFailure> {
    match grant.cert_hash.as_deref() {
        None => Ok(RelayCert::System),
        Some(hex) if decode_cert_hash(hex).is_some() => Ok(RelayCert::Sha256Hex(hex.to_string())),
        Some(_) => Err(WatchFailure::BadCertHash),
    }
}

/// Stable identity of the WebTransport session. Ticket expiry is omitted so
/// a refresh does not remount the player.
pub fn player_mount_key(grant: &TicketGrant, muted: bool) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{muted}",
        grant.relay_url,
        grant.broadcast,
        grant.alpn,
        grant.video_track,
        grant.audio_track,
        grant.cert_hash.as_deref().unwrap_or("")
    )
}

pub fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Classify a session-create or ticket-refresh HTTP response.
pub fn classify_grant(status: u16, body: &str) -> Result<TicketGrant, WatchFailure> {
    if (200..300).contains(&status) {
        return serde_json::from_str(body)
            .map_err(|err| WatchFailure::Deserialise(err.to_string()));
    }
    Err(classify_error(status, body))
}

pub fn classify_error(status: u16, body: &str) -> WatchFailure {
    if status == 401 {
        return WatchFailure::SessionExpired(error_text(body));
    }
    let parsed = serde_json::from_str::<ErrorBody>(body).ok();
    let code = parsed.as_ref().map(|err| err.error.as_str()).unwrap_or("");
    match (status, code) {
        (409, "watch_session_active") => WatchFailure::SessionActive {
            session_id: parsed
                .as_ref()
                .and_then(|err| err.session_id.clone())
                .unwrap_or_default(),
            broadcast: parsed
                .as_ref()
                .and_then(|err| err.broadcast.clone())
                .unwrap_or_default(),
        },
        (409, "watch_session_required" | "watch_session_mismatch") => WatchFailure::SessionGone,
        (503, "moq_relay_unconfigured") => WatchFailure::RelayUnconfigured,
        (400, "invalid_broadcast") => WatchFailure::InvalidBroadcast,
        (404, _) => WatchFailure::SessionGone,
        _ => WatchFailure::Other {
            status,
            message: if code.is_empty() {
                body.trim().to_string()
            } else {
                code.to_string()
            },
        },
    }
}

fn error_text(body: &str) -> String {
    serde_json::from_str::<ErrorBody>(body)
        .map(|err| err.error)
        .unwrap_or_else(|_| body.trim().to_string())
}

/// `POST /api/watch/sessions`.
pub async fn create_session(
    gate: &RefreshGate,
    broadcast: &str,
    replace: bool,
) -> Result<TicketGrant, WatchFailure> {
    let body = CreateSessionBody { broadcast, replace };
    let (status, raw) =
        api::authorized_post_raw(gate, &api::api_base(), "/api/watch/sessions", Some(&body))
            .await?;
    classify_grant(status, &raw)
}

/// `POST /api/watch/tickets`.
pub async fn refresh_ticket(
    gate: &RefreshGate,
    session_id: &str,
) -> Result<TicketGrant, WatchFailure> {
    let body = RefreshTicketBody { session_id };
    let (status, raw) =
        api::authorized_post_raw(gate, &api::api_base(), "/api/watch/tickets", Some(&body)).await?;
    classify_grant(status, &raw)
}

/// `DELETE /api/watch/sessions/{sessionId}`.
pub async fn delete_session(gate: &RefreshGate, session_id: &str) -> Result<(), WatchFailure> {
    let path = format!("/api/watch/sessions/{}", urlencoding::encode(session_id));
    api::authorized_delete(gate, &api::api_base(), &path)
        .await
        .map_err(WatchFailure::from)
}

/// `GET /api/watch/playhead/{broadcast}`.
pub async fn get_playhead(
    gate: &RefreshGate,
    broadcast: &str,
) -> Result<PlayheadView, WatchFailure> {
    let path = format!("/api/watch/playhead/{broadcast}");
    api::authorized_get_json(gate, &api::api_base(), &path)
        .await
        .map_err(WatchFailure::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn grant_json(cert: &str) -> String {
        format!(
            r#"{{
                "sessionId": "abc",
                "ticket": "tok",
                "ticketExpiresAt": 1700000120,
                "relayUrl": "https://relay.example/anon",
                "broadcast": "lavfi-spike",
                "certHash": {cert},
                "alpn": "moq-lite-04",
                "videoTrack": "video",
                "audioTrack": "audio"
            }}"#
        )
    }

    #[test]
    fn cert_hash_decodes_64_hex_chars() {
        let bytes = decode_cert_hash(HASH).expect("lowercase");
        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x01);
        assert_eq!(bytes[1], 0x23);
        assert_eq!(bytes[31], 0xef);
        let upper = decode_cert_hash(&HASH.to_ascii_uppercase()).expect("uppercase");
        assert_eq!(bytes, upper);
        assert_eq!(decode_cert_hash(&format!("  {HASH}\n")), Some(bytes));
    }

    #[test]
    fn cert_hash_rejects_bad_input() {
        assert!(decode_cert_hash("").is_none());
        assert!(decode_cert_hash(&HASH[..63]).is_none());
        assert!(decode_cert_hash(&format!("{HASH}aa")).is_none());
        assert!(decode_cert_hash(&format!("{}g{}", &HASH[..31], &HASH[32..])).is_none());
        assert!(decode_cert_hash("zz").is_none());
    }

    #[test]
    fn refresh_fires_thirty_seconds_before_expiry() {
        assert_eq!(ticket_refresh_delay_ms(1_000, 1_120), 90_000);
        assert_eq!(ticket_refresh_delay_ms(1_090, 1_120), 0);
        assert_eq!(ticket_refresh_delay_ms(1_110, 1_120), 0);
        assert_eq!(ticket_refresh_delay_ms(1_200, 1_120), 0);
        assert_eq!(ticket_refresh_delay_ms(1_000, 1_010), 0);
    }

    #[test]
    fn countdown_and_playhead_format() {
        assert_eq!(format_ticket_countdown(1_000, 1_090), "1:30");
        assert_eq!(format_ticket_countdown(1_000, 1_005), "0:05");
        assert_eq!(format_ticket_countdown(1_000, 1_000), "expired");
        assert_eq!(format_ticket_countdown(1_010, 1_000), "expired");
        assert_eq!(format_playhead(0, true), "0:00 · paused");
        assert_eq!(format_playhead(65_000, false), "1:05 · playing");
        assert_eq!(format_playhead(3_661_000, false), "1:01:01 · playing");
    }

    #[test]
    fn create_body_uses_camel_case_and_omits_replace_when_false() {
        let body = CreateSessionBody {
            broadcast: "lavfi-spike",
            replace: false,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"broadcast": "lavfi-spike"})
        );
        let body = CreateSessionBody {
            broadcast: "lavfi-spike",
            replace: true,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"broadcast": "lavfi-spike", "replace": true})
        );
    }

    #[test]
    fn grant_created_and_null_cert_hash() {
        let grant = classify_grant(201, &grant_json(&format!("\"{HASH}\""))).unwrap();
        assert_eq!(grant.session_id, "abc");
        assert_eq!(grant.ticket_expires_at, 1_700_000_120);
        assert_eq!(grant.alpn, "moq-lite-04");
        assert_eq!(grant.video_track, "video");
        assert_eq!(grant.audio_track, "audio");
        assert!(matches!(relay_cert(&grant), Ok(RelayCert::Sha256Hex(_))));

        let grant = classify_grant(200, &grant_json("null")).unwrap();
        assert_eq!(relay_cert(&grant), Ok(RelayCert::System));
        assert!(!player_mount_key(&grant, true).contains("expired"));
        assert!(player_mount_key(&grant, true).ends_with("true"));
        assert!(player_mount_key(&grant, false).ends_with("false"));
    }

    #[test]
    fn bad_cert_hash_is_rejected() {
        let grant = classify_grant(201, &grant_json("\"abcd\"")).unwrap();
        assert_eq!(relay_cert(&grant), Err(WatchFailure::BadCertHash));
    }

    #[test]
    fn error_statuses_match_the_watch_contract() {
        let active = classify_error(
            409,
            r#"{"error":"watch_session_active","sessionId":"old","broadcast":"other"}"#,
        );
        assert_eq!(
            active,
            WatchFailure::SessionActive {
                session_id: "old".into(),
                broadcast: "other".into(),
            }
        );
        assert_eq!(active.title(), "Watch session already active");
        assert!(active.body().contains("other"));

        let relay = classify_error(503, r#"{"error":"moq_relay_unconfigured"}"#);
        assert_eq!(relay, WatchFailure::RelayUnconfigured);
        assert_eq!(relay.title(), "Relay not configured");
        assert!(relay.body().contains("MOQ_PUBLIC_URL"));

        let expired = classify_error(401, r#"{"error":"Invalid or expired token"}"#);
        assert!(matches!(expired, WatchFailure::SessionExpired(_)));
        assert!(expired.needs_sign_in());
        assert_eq!(expired.title(), "Session expired");
        assert_eq!(expired.body(), "Sign in again to watch.");

        assert_eq!(
            classify_error(409, r#"{"error":"watch_session_required"}"#),
            WatchFailure::SessionGone
        );
        assert_eq!(
            classify_error(400, r#"{"error":"invalid_broadcast"}"#),
            WatchFailure::InvalidBroadcast
        );
        assert_eq!(
            classify_error(404, r#"{"error":"not_found"}"#),
            WatchFailure::SessionGone
        );
    }

    #[test]
    fn api_error_401_is_session_expired() {
        let err = WatchFailure::from(ApiError::Unauthorized("nope".into()));
        assert!(err.needs_sign_in());
        let relay = WatchFailure::from(ApiError::Server {
            status: 503,
            message: "moq_relay_unconfigured".into(),
        });
        assert_eq!(relay, WatchFailure::RelayUnconfigured);
    }
}
