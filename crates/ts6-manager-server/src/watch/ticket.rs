//! Short-lived MoQ subscribe tickets.
//!
//! HS256 with the same secret as panel access tokens. The claim set and
//! the `typ` header differ, so a watch ticket fails
//! [`crate::auth::jwt::verify_access`] and an access token fails
//! [`verify`] here. Callers still authenticate session and playhead
//! routes with [`crate::auth::extractors::RequireAuth`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

/// JWT `typ` header. Access tokens keep the default `JWT`.
const TYP: &str = "moq-watch+jwt";

/// Required `purpose` claim. Access tokens do not carry it.
pub(crate) const PURPOSE: &str = "moq_subscribe";

/// Subscribe tickets are short. The watch session outlives them; the
/// panel refreshes via `POST /api/watch/tickets` while the session is held.
pub(crate) const TTL: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Claims {
    pub purpose: String,
    /// Panel user id. Named `uid` so the payload does not satisfy
    /// `AccessClaims.id`.
    pub uid: i64,
    pub sid: String,
    pub broadcast: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug)]
pub(crate) enum TicketError {
    Clock,
    Encode,
    InvalidOrExpired,
}

/// jsonwebtoken 10 panics on sign/verify when both `aws_lc_rs` and
/// `rust_crypto` are enabled in one build. Installing one provider once
/// per process selects the backend. A later call returns `Err` because
/// the default is already set; that is success.
fn ensure_jwt_crypto() {
    let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
}

pub(crate) fn mint(
    user_id: i64,
    session_id: &str,
    broadcast: &str,
    secret: &[u8],
) -> Result<(String, i64), TicketError> {
    let now = unix_now()?;
    let exp = now.saturating_add(TTL.as_secs() as i64);
    let claims = Claims {
        purpose: PURPOSE.to_string(),
        uid: user_id,
        sid: session_id.to_string(),
        broadcast: broadcast.to_string(),
        iat: now,
        exp,
    };
    encode_claims(&claims, secret).map(|token| (token, exp))
}

pub(crate) fn verify(token: &str, secret: &[u8]) -> Result<Claims, TicketError> {
    ensure_jwt_crypto();
    let key = DecodingKey::from_secret(secret);
    let mut validation = Validation::new(Algorithm::HS256);
    validation.required_spec_claims = ["exp"].into_iter().map(str::to_string).collect();
    // Short tickets should die at `exp`. Access-token verification keeps
    // jsonwebtoken's default leeway; this path does not.
    validation.leeway = 0;
    let data =
        decode::<Claims>(token, &key, &validation).map_err(|_| TicketError::InvalidOrExpired)?;
    if data.header.typ.as_deref() != Some(TYP) || data.claims.purpose != PURPOSE {
        return Err(TicketError::InvalidOrExpired);
    }
    if data.claims.sid.is_empty() || data.claims.broadcast.is_empty() {
        return Err(TicketError::InvalidOrExpired);
    }
    Ok(data.claims)
}

pub(crate) fn encode_claims(claims: &Claims, secret: &[u8]) -> Result<String, TicketError> {
    ensure_jwt_crypto();
    let mut header = Header::new(Algorithm::HS256);
    header.typ = Some(TYP.to_string());
    let key = EncodingKey::from_secret(secret);
    encode(&header, claims, &key).map_err(|_| TicketError::Encode)
}

fn unix_now() -> Result<i64, TicketError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| TicketError::Clock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::jwt::{self, AccessClaims};

    const SECRET: &[u8] = b"test-secret-32-byte-minimum-len-please";

    #[test]
    fn mint_then_verify_roundtrip() {
        let (token, exp) = mint(7, "sid-1", "lavfi-spike", SECRET).unwrap();
        let claims = verify(&token, SECRET).unwrap();
        assert_eq!(claims.uid, 7);
        assert_eq!(claims.sid, "sid-1");
        assert_eq!(claims.broadcast, "lavfi-spike");
        assert_eq!(claims.purpose, PURPOSE);
        assert_eq!(claims.exp, exp);
        assert_eq!(claims.exp - claims.iat, TTL.as_secs() as i64);
    }

    #[test]
    fn expired_ticket_fails() {
        let now = unix_now().unwrap();
        let claims = Claims {
            purpose: PURPOSE.into(),
            uid: 1,
            sid: "sid".into(),
            broadcast: "lavfi-spike".into(),
            iat: now - 30,
            exp: now - 1,
        };
        let token = encode_claims(&claims, SECRET).unwrap();
        assert!(matches!(
            verify(&token, SECRET),
            Err(TicketError::InvalidOrExpired)
        ));
    }

    #[test]
    fn access_token_is_not_a_watch_ticket() {
        let access =
            jwt::mint_access(7, "ada", "viewer", Duration::from_secs(900), SECRET).unwrap();
        assert!(matches!(
            verify(&access, SECRET),
            Err(TicketError::InvalidOrExpired)
        ));
    }

    #[test]
    fn watch_ticket_is_not_an_access_token() {
        let (token, _) = mint(7, "sid-1", "lavfi-spike", SECRET).unwrap();
        assert!(matches!(
            jwt::verify_access(&token, SECRET),
            Err(jwt::Error::InvalidOrExpired)
        ));
    }

    #[test]
    fn wrong_secret_and_wrong_purpose_fail() {
        let (token, _) = mint(1, "sid", "lavfi-spike", SECRET).unwrap();
        assert!(matches!(
            verify(&token, b"other-secret-bytes-here-please!!"),
            Err(TicketError::InvalidOrExpired)
        ));

        let now = unix_now().unwrap();
        let claims = Claims {
            purpose: "access".into(),
            uid: 1,
            sid: "sid".into(),
            broadcast: "lavfi-spike".into(),
            iat: now,
            exp: now + 60,
        };
        let forged = encode_claims(&claims, SECRET).unwrap();
        assert!(matches!(
            verify(&forged, SECRET),
            Err(TicketError::InvalidOrExpired)
        ));

        // An access-shaped payload signed as a watch header still fails
        // because `purpose` / `uid` / `sid` / `broadcast` are absent.
        let access = AccessClaims {
            id: 1,
            username: "ada".into(),
            role: "viewer".into(),
            iat: now,
            exp: now + 60,
        };
        let mut header = Header::new(Algorithm::HS256);
        header.typ = Some(TYP.to_string());
        ensure_jwt_crypto();
        let mixed = encode(&header, &access, &EncodingKey::from_secret(SECRET)).unwrap();
        assert!(matches!(
            verify(&mixed, SECRET),
            Err(TicketError::InvalidOrExpired)
        ));
    }
}
