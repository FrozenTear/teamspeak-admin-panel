//! Spec §6.3.2–§6.3.4 — AES-256-GCM seal/unseal with the
//! `enc:<iv-hex>:<tag-hex>:<ct-hex>` storage format.
//!
//! Tag size is 16 bytes (AES-GCM standard). IV / nonce is 12 random bytes per
//! encryption; never reused. The `enc:` prefix is an external-contract marker
//! that distinguishes ciphertext from legacy plaintext (§6.3.3).

use aes_gcm::aead::{Aead as AeadTrait, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;

/// Spec §6.3.2: external-contract storage prefix.
pub const STORAGE_PREFIX: &str = "enc:";
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed (bad key, corrupt data, or tampered ciphertext)")]
    Decrypt,
    #[error("malformed storage envelope")]
    BadEnvelope,
}

/// AES-256-GCM key + cipher instance. Construct once via [`Aead::from_key_bytes`]
/// or [`Aead::from_seed`]; use the same instance for many `seal`/`unseal` calls.
///
/// `Aead` deliberately does not derive `Debug`/`Display` — the key bytes are
/// not safe to print.
pub struct Aead {
    cipher: Aes256Gcm,
}

impl Aead {
    /// Build an [`Aead`] from a raw 32-byte key. Useful for tests with a
    /// fixed known key; production callers should prefer [`Aead::from_seed`].
    pub fn from_key_bytes(key: [u8; 32]) -> Self {
        let key = Key::<Aes256Gcm>::from_slice(&key);
        Self {
            cipher: Aes256Gcm::new(key),
        }
    }

    /// Derive the key from `seed` (typically `ENCRYPTION_KEY` or `JWT_SECRET`)
    /// via scrypt with the canonical salt, then build an [`Aead`].
    ///
    /// Slow on first call (scrypt by design); cache the [`Aead`] for the
    /// process lifetime — see [`crate::crypto::process_aead`].
    pub fn from_seed(seed: &str) -> Self {
        Self::from_key_bytes(super::kdf::derive_key(seed))
    }

    /// Encrypt `plaintext` and return the spec's `enc:<iv-hex>:<tag-hex>:<ct-hex>`
    /// envelope.
    ///
    /// A fresh 12-byte CSPRNG nonce is generated for every call. Re-encrypting
    /// the same plaintext yields a different ciphertext.
    pub fn seal(&self, plaintext: &str) -> Result<String, Error> {
        let mut iv = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut iv);
        let nonce = Nonce::from_slice(&iv);

        let mut sealed = self
            .cipher
            .encrypt(nonce, plaintext.as_bytes())
            .map_err(|_| Error::Encrypt)?;

        // aes-gcm returns `ciphertext || tag` concatenated. Spec wants the
        // tag and ciphertext as separate hex fields, so split them.
        if sealed.len() < TAG_LEN {
            return Err(Error::Encrypt);
        }
        let tag_start = sealed.len() - TAG_LEN;
        let tag = sealed.split_off(tag_start);
        let ct = sealed;

        Ok(format!(
            "{}{}:{}:{}",
            STORAGE_PREFIX,
            hex::encode(iv),
            hex::encode(tag),
            hex::encode(ct)
        ))
    }

    /// Decrypt a stored value.
    ///
    /// Per spec §6.3.3: if `stored` does NOT start with `enc:` it is treated
    /// as legacy plaintext and returned verbatim. This lets a deployment that
    /// migrated from a pre-encryption version keep working until each row is
    /// re-saved (re-saving re-encrypts at the repository layer).
    ///
    /// If `stored` starts with `enc:` it MUST split into exactly four parts
    /// (`enc`, iv-hex, tag-hex, ct-hex). Anything else is a malformed
    /// envelope and is rejected — we deliberately do not "best-effort" parse.
    pub fn unseal(&self, stored: &str) -> Result<String, Error> {
        if !stored.starts_with(STORAGE_PREFIX) {
            // Plaintext passthrough.
            return Ok(stored.to_string());
        }

        let parts: Vec<&str> = stored.split(':').collect();
        if parts.len() != 4 {
            return Err(Error::BadEnvelope);
        }
        // parts[0] is "enc"; we already matched it via the prefix check.
        let iv = hex::decode(parts[1]).map_err(|_| Error::BadEnvelope)?;
        let tag = hex::decode(parts[2]).map_err(|_| Error::BadEnvelope)?;
        let ct = hex::decode(parts[3]).map_err(|_| Error::BadEnvelope)?;

        if iv.len() != NONCE_LEN || tag.len() != TAG_LEN {
            return Err(Error::BadEnvelope);
        }

        let nonce = Nonce::from_slice(&iv);

        // aes-gcm's `decrypt` expects `ciphertext || tag`.
        let mut combined = ct;
        combined.extend_from_slice(&tag);

        let plaintext = self
            .cipher
            .decrypt(nonce, combined.as_ref())
            .map_err(|_| Error::Decrypt)?;

        String::from_utf8(plaintext).map_err(|_| Error::Decrypt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_aead() -> Aead {
        // Deterministic key for test reproducibility — never used in prod.
        Aead::from_key_bytes([7u8; 32])
    }

    #[test]
    fn roundtrip_preserves_plaintext() {
        let aead = fresh_aead();
        let sealed = aead.seal("hunter2!@#").unwrap();
        let unsealed = aead.unseal(&sealed).unwrap();
        assert_eq!(unsealed, "hunter2!@#");
    }

    #[test]
    fn seal_emits_external_contract_format() {
        let aead = fresh_aead();
        let sealed = aead.seal("hello world").unwrap();
        assert!(
            sealed.starts_with("enc:"),
            "expected enc: prefix, got {sealed}"
        );
        let parts: Vec<&str> = sealed.split(':').collect();
        assert_eq!(
            parts.len(),
            4,
            "expected 4 parts (enc:iv:tag:ct), got {parts:?}"
        );
        assert_eq!(parts[0], "enc");
        // IV: 12 bytes → 24 hex chars.
        assert_eq!(parts[1].len(), 24);
        // Tag: 16 bytes → 32 hex chars.
        assert_eq!(parts[2].len(), 32);
        // CT: len("hello world") = 11 → 22 hex chars.
        assert_eq!(parts[3].len(), 22);
    }

    #[test]
    fn distinct_seals_have_distinct_iv_and_ciphertext() {
        let aead = fresh_aead();
        let a = aead.seal("same plaintext").unwrap();
        let b = aead.seal("same plaintext").unwrap();
        assert_ne!(a, b, "fresh nonce per call must yield different envelopes");
    }

    #[test]
    fn plaintext_passthrough_for_legacy_values() {
        let aead = fresh_aead();
        assert_eq!(aead.unseal("rawkey").unwrap(), "rawkey");
        assert_eq!(aead.unseal("").unwrap(), "");
        assert_eq!(aead.unseal("apikey-12345").unwrap(), "apikey-12345");
    }

    #[test]
    fn rejects_envelope_with_wrong_part_count() {
        let aead = fresh_aead();
        // Three parts only (missing ct).
        assert!(matches!(
            aead.unseal("enc:0011:2233"),
            Err(Error::BadEnvelope)
        ));
        // Five parts (a stray colon).
        assert!(matches!(
            aead.unseal("enc:001122334455667788990011:00112233445566778899001122334455:aa:bb"),
            Err(Error::BadEnvelope)
        ));
    }

    #[test]
    fn rejects_envelope_with_wrong_iv_length() {
        let aead = fresh_aead();
        // 8-byte (16-hex) IV instead of 12-byte (24-hex).
        let bad = format!(
            "enc:{}:{}:{}",
            hex::encode([1u8; 8]),
            hex::encode([2u8; 16]),
            hex::encode([3u8; 4]),
        );
        assert!(matches!(aead.unseal(&bad), Err(Error::BadEnvelope)));
    }

    #[test]
    fn rejects_envelope_with_wrong_tag_length() {
        let aead = fresh_aead();
        let bad = format!(
            "enc:{}:{}:{}",
            hex::encode([1u8; 12]),
            hex::encode([2u8; 8]), // should be 16
            hex::encode([3u8; 4]),
        );
        assert!(matches!(aead.unseal(&bad), Err(Error::BadEnvelope)));
    }

    #[test]
    fn rejects_envelope_with_invalid_hex() {
        let aead = fresh_aead();
        let bad = "enc:zz:tag:ct";
        assert!(matches!(aead.unseal(bad), Err(Error::BadEnvelope)));
    }

    #[test]
    fn tampered_ciphertext_fails_authentication() {
        let aead = fresh_aead();
        let sealed = aead.seal("important").unwrap();
        // Flip a character in the ct hex.
        let parts: Vec<&str> = sealed.split(':').collect();
        let mut ct_bytes = hex::decode(parts[3]).unwrap();
        ct_bytes[0] ^= 0xFF;
        let tampered = format!("enc:{}:{}:{}", parts[1], parts[2], hex::encode(&ct_bytes));
        assert!(matches!(aead.unseal(&tampered), Err(Error::Decrypt)));
    }

    #[test]
    fn tampered_tag_fails_authentication() {
        let aead = fresh_aead();
        let sealed = aead.seal("important").unwrap();
        let parts: Vec<&str> = sealed.split(':').collect();
        let mut tag_bytes = hex::decode(parts[2]).unwrap();
        tag_bytes[0] ^= 0xFF;
        let tampered = format!("enc:{}:{}:{}", parts[1], hex::encode(&tag_bytes), parts[3]);
        assert!(matches!(aead.unseal(&tampered), Err(Error::Decrypt)));
    }

    #[test]
    fn wrong_key_fails_authentication() {
        let writer = Aead::from_key_bytes([7u8; 32]);
        let reader = Aead::from_key_bytes([8u8; 32]);
        let sealed = writer.seal("important").unwrap();
        assert!(matches!(reader.unseal(&sealed), Err(Error::Decrypt)));
    }
}
