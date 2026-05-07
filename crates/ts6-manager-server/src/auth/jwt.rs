//! Spec §6.4 — JWT access tokens (HS256, signed with `JWT_SECRET`).
//!
//! Claims shape is preserved verbatim per spec: `id`, `username`, `role`,
//! `iat`, `exp`. We standardise on the `id` claim everywhere — including the
//! WebSocket handshake — resolving the spec's Q-6.1 inconsistency between
//! `id` (REST) and `sub` (WS) in favour of `id`.
//!
//! **Security invariant:** the JWT's `role` claim is NEVER trusted for
//! authorisation decisions. The verifier returns the parsed claims, but
//! [`crate::auth::extractors::RequireAuth`] re-reads the user's role from the
//! database on every request (with an optional ≤ 5 s cache per spec §6.4.1).
//! Treat the `role` field on [`AccessClaims`] as routing metadata only.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccessClaims {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub iat: i64,
    pub exp: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid system clock (before UNIX epoch)")]
    SystemClock,
    #[error("token encoding failed")]
    Encode,
    #[error("invalid or expired token")]
    InvalidOrExpired,
}

/// Mint a fresh access token for a user.
///
/// `iat` is set to current wall clock; `exp = iat + lifetime`. The `lifetime`
/// argument typically comes from `Config::jwt_access_expiry` (spec default
/// `15m`). The role string is read from the user's database row at the moment
/// of minting — it is not cached server-side.
pub fn mint_access(
    user_id: i64,
    username: &str,
    role: &str,
    lifetime: Duration,
    secret: &[u8],
) -> Result<String, Error> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::SystemClock)?
        .as_secs() as i64;
    let claims = AccessClaims {
        id: user_id,
        username: username.to_string(),
        role: role.to_string(),
        iat: now,
        exp: now + lifetime.as_secs() as i64,
    };
    encode_access(&claims, secret)
}

/// Encode the supplied claims into an HS256-signed token. Lower-level than
/// [`mint_access`]; use this when you already have an `iat`/`exp` pair (e.g.
/// in tests).
pub fn encode_access(claims: &AccessClaims, secret: &[u8]) -> Result<String, Error> {
    let header = Header::new(Algorithm::HS256);
    let key = EncodingKey::from_secret(secret);
    encode(&header, claims, &key).map_err(|_| Error::Encode)
}

/// Verify an access token's HMAC and expiry, returning the parsed claims.
///
/// Returns [`Error::InvalidOrExpired`] for any of: bad signature, malformed
/// token, expired `exp`, or wrong algorithm. The route layer maps this to
/// HTTP 401 with body `{"error":"Invalid or expired token"}` (spec §6.4.1).
pub fn verify_access(token: &str, secret: &[u8]) -> Result<AccessClaims, Error> {
    let key = DecodingKey::from_secret(secret);
    let mut validation = Validation::new(Algorithm::HS256);
    // Spec does not require `aud` or `iss`; do not enforce them.
    validation.required_spec_claims = ["exp"].iter().map(|s| s.to_string()).collect();
    decode::<AccessClaims>(token, &key, &validation)
        .map(|t| t.claims)
        .map_err(|_| Error::InvalidOrExpired)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"test-secret-32-byte-minimum-len-please";

    #[test]
    fn mint_then_verify_roundtrip() {
        let token = mint_access(42, "alice", "admin", Duration::from_secs(900), SECRET).unwrap();
        let claims = verify_access(&token, SECRET).unwrap();
        assert_eq!(claims.id, 42);
        assert_eq!(claims.username, "alice");
        assert_eq!(claims.role, "admin");
        assert!(claims.exp > claims.iat);
        assert_eq!(claims.exp - claims.iat, 900);
    }

    #[test]
    fn wrong_secret_fails() {
        let token = mint_access(42, "alice", "admin", Duration::from_secs(900), SECRET).unwrap();
        let other_secret = b"different-secret-bytes-here";
        let err = verify_access(&token, other_secret).unwrap_err();
        matches!(err, Error::InvalidOrExpired);
    }

    #[test]
    fn expired_token_fails() {
        // Build claims directly with an exp in the past.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = AccessClaims {
            id: 1,
            username: "u".into(),
            role: "viewer".into(),
            iat: now - 7200,
            exp: now - 3600,
        };
        let token = encode_access(&claims, SECRET).unwrap();
        let err = verify_access(&token, SECRET).unwrap_err();
        matches!(err, Error::InvalidOrExpired);
    }

    #[test]
    fn tampered_payload_fails_signature_check() {
        let token = mint_access(1, "u", "viewer", Duration::from_secs(900), SECRET).unwrap();
        // Flip a byte in the signature segment (after the second '.').
        let mut bytes: Vec<u8> = token.into_bytes();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let tampered = String::from_utf8(bytes).unwrap();
        let err = verify_access(&tampered, SECRET).unwrap_err();
        matches!(err, Error::InvalidOrExpired);
    }

    #[test]
    fn malformed_token_fails() {
        assert!(matches!(
            verify_access("not-a-token", SECRET),
            Err(Error::InvalidOrExpired)
        ));
        assert!(matches!(
            verify_access("", SECRET),
            Err(Error::InvalidOrExpired)
        ));
    }

    // Note: AccessClaims deliberately exposes `role` as plain `String` and
    // provides NO authorisation helpers (`is_admin()`, `has_role()`, etc.).
    // Per spec §6.4.1 the JWT's role claim MUST NOT be trusted for
    // authorisation — the extractor re-reads the role from the database on
    // every request. Adding a helper here would invite the wrong call site.
}
