//! Spec §6.2.1 — password hashing.
//!
//! New passwords are hashed with **Argon2id** (PHC string format
//! `$argon2id$...`). Legacy bcrypt hashes (`$2a$ | $2b$ | $2y$`) are accepted
//! by the verifier so a deployment migrating from a reference TypeScript
//! database keeps working until each user re-saves their password (which
//! re-hashes with Argon2id).
//!
//! This satisfies §6.2.1's "MAY substitute Argon2id provided existing bcrypt
//! hashes can still be verified during a transition period".

use argon2::password_hash::{PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng};
use argon2::{Argon2, PasswordHash as ArgonPhc};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("password hash format not recognised")]
    InvalidHashFormat,
    #[error("hash error: {0}")]
    Hash(String),
}

/// Hash a new password using Argon2id with the OWASP 2023 recommended
/// parameters (Argon2id default in this crate is m=19 MiB, t=2, p=1).
pub fn hash_new(password: &str) -> Result<String, Error> {
    let salt = SaltString::generate(&mut OsRng);
    let argon = Argon2::default();
    let phc = argon
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| Error::Hash(e.to_string()))?
        .to_string();
    Ok(phc)
}

/// Verify a candidate password against a stored hash.
///
/// Dispatches on the leading PHC-style prefix:
/// - `$argon2id$` → Argon2id verify (the format we write today).
/// - `$2a$ | $2b$ | $2y$` → bcrypt verify (legacy reference hashes).
/// - anything else → [`Error::InvalidHashFormat`].
///
/// Returns `Ok(true)` on match, `Ok(false)` on mismatch. Treats a malformed
/// stored hash as [`Error::Hash`] rather than `Ok(false)` — the spec's
/// "constant-time bcrypt compare" property assumes the stored hash is
/// well-formed; surfacing format errors prevents that assumption from
/// breaking silently.
pub fn verify(stored: &str, password: &str) -> Result<bool, Error> {
    if stored.starts_with("$argon2id$")
        || stored.starts_with("$argon2i$")
        || stored.starts_with("$argon2d$")
    {
        let phc = ArgonPhc::new(stored).map_err(|e| Error::Hash(e.to_string()))?;
        let argon = Argon2::default();
        match argon.verify_password(password.as_bytes(), &phc) {
            Ok(()) => Ok(true),
            Err(argon2::password_hash::Error::Password) => Ok(false),
            Err(e) => Err(Error::Hash(e.to_string())),
        }
    } else if stored.starts_with("$2a$") || stored.starts_with("$2b$") || stored.starts_with("$2y$")
    {
        bcrypt::verify(password, stored).map_err(|e| Error::Hash(e.to_string()))
    } else {
        Err(Error::InvalidHashFormat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argon2_roundtrip() {
        let hash = hash_new("Hunter2!ok").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify(&hash, "Hunter2!ok").unwrap());
        assert!(!verify(&hash, "wrong").unwrap());
    }

    #[test]
    fn each_hash_call_uses_a_fresh_salt() {
        let a = hash_new("samepw").unwrap();
        let b = hash_new("samepw").unwrap();
        assert_ne!(a, b, "different salts must produce different PHC strings");
    }

    #[test]
    fn bcrypt_legacy_hash_can_still_verify() {
        // Reference uses bcrypt cost 12. Generate a low-cost hash for test
        // speed; the format string is what dispatch keys on, not the cost.
        let legacy = bcrypt::hash("legacypass", 4).unwrap();
        assert!(legacy.starts_with("$2"));
        assert!(verify(&legacy, "legacypass").unwrap());
        assert!(!verify(&legacy, "wrong").unwrap());
    }

    #[test]
    fn unknown_hash_format_is_rejected() {
        let err = verify("$1$abc$xxx", "anything").unwrap_err();
        matches!(err, Error::InvalidHashFormat);
    }

    #[test]
    fn unrecognised_format_prefix_returns_invalid_hash_format() {
        // MD5-crypt-style and DES-crypt-style hashes are well outside our
        // supported set; the verifier MUST refuse them rather than treat
        // them as "wrong password" (which would silently mask a DB row that
        // came from an unsupported migration source).
        assert!(matches!(
            verify("$1$abc$xxx", "anything").unwrap_err(),
            Error::InvalidHashFormat
        ));
        assert!(matches!(
            verify("plain-text", "anything").unwrap_err(),
            Error::InvalidHashFormat
        ));
    }
}
