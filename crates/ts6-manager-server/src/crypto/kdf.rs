//! Spec §6.3.1 — derive the 32-byte AES-256-GCM key from a string seed.
//!
//! The salt `ts6-webui-enc-v1` is an external-contract value: changing it
//! invalidates every existing ciphertext. The derived key is held in process
//! memory for the lifetime of the process and is never written to disk or
//! logs.

/// External-contract: literal salt used by `scrypt`. MUST NOT change.
pub const ENCRYPTION_SALT: &[u8] = b"ts6-webui-enc-v1";

/// Derive a 32-byte AES-256-GCM key from `seed` using scrypt with the
/// canonical [`ENCRYPTION_SALT`].
///
/// `seed` is `ENCRYPTION_KEY` if set, otherwise `JWT_SECRET` (the env loader
/// already resolves the fallback per spec §5.1; this function takes the
/// resolved string).
pub fn derive_key(seed: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    // scrypt::Params::recommended() = (log_n=17, r=8, p=1, dklen=32) per
    // OWASP 2023 guidance. Slow once at boot; the result is cached.
    let params = scrypt::Params::recommended();
    scrypt::scrypt(seed.as_bytes(), ENCRYPTION_SALT, &params, &mut out)
        .expect("scrypt with default parameters cannot fail on a 32-byte output");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salt_is_the_external_contract_literal() {
        assert_eq!(ENCRYPTION_SALT, b"ts6-webui-enc-v1");
    }

    #[test]
    fn derive_is_deterministic_for_same_seed() {
        let a = derive_key("hunter2");
        let b = derive_key("hunter2");
        assert_eq!(a, b);
    }

    #[test]
    fn derive_differs_for_different_seeds() {
        let a = derive_key("hunter2");
        let b = derive_key("hunter3");
        assert_ne!(a, b);
    }
}
