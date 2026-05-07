//! Spec §6.3 — credential at-rest encryption (`crypto::seal` / `crypto::unseal`).
//!
//! AES-256-GCM with a 12-byte CSPRNG nonce per call. The 32-byte key is
//! derived once at process start from `ENCRYPTION_KEY` (or `JWT_SECRET` per
//! the §5.1 fallback) using scrypt with the literal salt `ts6-webui-enc-v1`.
//!
//! Ciphertext is stored as `enc:<iv-hex>:<tag-hex>:<ct-hex>`. The `enc:`
//! prefix distinguishes encrypted values from legacy plaintext during
//! migration; values without the prefix are returned as-is by [`unseal`].
//!
//! Risk reviewed: see [`PURA-4` plan](/PURA/issues/PURA-4#document-plan) §4.

#![allow(dead_code)] // consumed by future workstreams (DATA, SECURITY routes)

mod aead;
mod kdf;

use std::sync::OnceLock;

#[allow(unused_imports)] // re-exported for future call sites
pub use aead::{Aead, Error as AeadError, STORAGE_PREFIX};
#[allow(unused_imports)] // re-exported for future call sites
pub use kdf::ENCRYPTION_SALT;

/// Process-wide cached AES key, derived from `ENCRYPTION_KEY`/`JWT_SECRET`.
///
/// Set once at boot via [`init`]. After init, [`seal`] and [`unseal`] resolve
/// it via [`process_aead`]. Tests that exercise the free functions must call
/// [`init`] first; tests that build their own [`Aead`] do not need this.
static PROCESS_AEAD: OnceLock<Aead> = OnceLock::new();

/// Initialise the process-wide AEAD from a seed. Idempotent for the same
/// seed (subsequent calls with any seed are silently ignored — the
/// `OnceLock` policy is "first writer wins"). Returns `Ok(())` whether the
/// init actually happened or was a no-op; this lets callers be relaxed about
/// double-init in test harnesses.
pub fn init(seed: &str) {
    let _ = PROCESS_AEAD.set(Aead::from_seed(seed));
}

/// Get the process-wide AEAD. Panics if [`init`] has not been called.
fn process_aead() -> &'static Aead {
    PROCESS_AEAD
        .get()
        .expect("crypto::init() must be called at boot before seal/unseal")
}

/// Spec §6.3 entry point — encrypt `plaintext` to `enc:<iv-hex>:<tag-hex>:<ct-hex>`.
pub fn seal(plaintext: &str) -> Result<String, AeadError> {
    process_aead().seal(plaintext)
}

/// Spec §6.3 entry point — decrypt a stored value, transparently returning
/// legacy plaintext (no `enc:` prefix) verbatim.
pub fn unseal(stored: &str) -> Result<String, AeadError> {
    process_aead().unseal(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_is_idempotent() {
        // First init wins; subsequent calls are no-ops.
        init("seed-a");
        init("seed-b");
        // We can't actually observe which seed won (both produce a valid
        // Aead) without leaking the key — but the Aead must be set.
        assert!(PROCESS_AEAD.get().is_some());
    }

    #[test]
    fn process_seal_unseal_roundtrip() {
        // Init may already have happened in another test thanks to test
        // ordering. This is fine — any seed produces a working key, and
        // because the OnceLock only allows one winner, both ends of the
        // roundtrip use the same key.
        init("test-seed");
        let sealed = seal("api-key-value").unwrap();
        assert!(sealed.starts_with(STORAGE_PREFIX));
        let unsealed = unseal(&sealed).unwrap();
        assert_eq!(unsealed, "api-key-value");
    }

    #[test]
    fn process_unseal_passes_through_legacy_plaintext() {
        init("test-seed");
        assert_eq!(unseal("rawkey").unwrap(), "rawkey");
    }
}
