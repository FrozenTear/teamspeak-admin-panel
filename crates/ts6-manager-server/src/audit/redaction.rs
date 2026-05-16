//! Audit-payload credential hard-blocklist — `docs/admin/audit-shape.md`
//! §2.3 (PURA-236).
//!
//! `crate::audit::record` runs [`redact_payload`] on every event payload
//! before it reaches [`crate::repos::admin_audit_log::insert`]. Any JSON
//! key whose name matches a credential shape (`password`, `apiKey`,
//! `sshKey`, `authorization`, refresh/access token, …) has its value
//! rewritten to the [`REDACTED_SENTINEL`] string, and a `tracing::warn!`
//! fires so a caller that bypasses the type-seam discipline is visible
//! in the production log stream.
//!
//! This is the **hard-blocklist** half of §2.3. The **soft-truncate**
//! half — the 2 KiB payload cap and the errorMsg / userAgent caps — is
//! applied at the persistence boundary inside
//! `crate::repos::admin_audit_log::insert` (PURA-235), so this module
//! deliberately does not duplicate it.
//!
//! Posture mirrors `crate::sshbridge::audit` / `crate::repos::ssh_audit_log`:
//! the **right** place to keep credentials out of the audit row is
//! upstream — callers must never put a credential into the payload. This
//! module is the last-line belt that catches developer mistakes.

use serde_json::Value;

/// Sentinel value substituted in for any credential-shaped key.
pub const REDACTED_SENTINEL: &str = "<redacted>";

/// Case-insensitive substring matches that disqualify a JSON key. Mirrors
/// the audit-shape.md §2.3 hard-blocklist. Substring-matched (not
/// whole-word) so a future caller who renames a key (`passwordHash`,
/// `newPassword`) still gets caught.
const CREDENTIAL_KEY_SUBSTRINGS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "apikey",
    "api_key",
    "authorization",
    "sshkey",
    "ssh_key",
    "privatekey",
    "private_key",
    "refreshtoken",
    "refresh_token",
    "accesstoken",
    "access_token",
    "bearer",
];

/// Returns `true` if a key name matches one of the credential substrings.
/// Case-insensitive.
fn key_is_credential(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CREDENTIAL_KEY_SUBSTRINGS
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Apply the hard-blocklist to a JSON value in place. Walks objects and
/// arrays recursively; values for credential-shaped keys are replaced
/// with [`REDACTED_SENTINEL`] rather than removed so the audit row still
/// records that "a value WAS provided but was redacted".
///
/// A `tracing::warn!` under `target = "audit::redaction"` fires when a
/// credential key is found so an operator can grep the production stream
/// for callers leaking credentials into audit payloads.
pub fn redact_payload(value: &mut Value) {
    redact_payload_inner(value, /*depth=*/ 0);
}

fn redact_payload_inner(value: &mut Value, depth: usize) {
    // Cheap recursion guard so a pathological caller can't blow the stack.
    if depth > 32 {
        return;
    }
    match value {
        Value::Object(map) => {
            let bad_keys: Vec<String> = map
                .keys()
                .filter(|k| key_is_credential(k))
                .cloned()
                .collect();
            for k in &bad_keys {
                tracing::warn!(
                    target: "audit::redaction",
                    key = %k,
                    "audit payload contained credential-shaped key; soft-redacted. \
                     Callers MUST NOT put credentials into audit payloads \
                     (audit-shape.md §2.3 hard-blocklist)."
                );
                map.insert(k.clone(), Value::String(REDACTED_SENTINEL.to_string()));
            }
            for (_, v) in map.iter_mut() {
                redact_payload_inner(v, depth + 1);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                redact_payload_inner(v, depth + 1);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_credential_matches_blocklist_case_insensitive() {
        assert!(key_is_credential("password"));
        assert!(key_is_credential("PASSWORD"));
        assert!(key_is_credential("newPassword"));
        assert!(key_is_credential("apiKey"));
        assert!(key_is_credential("api_key"));
        assert!(key_is_credential("sshKey"));
        assert!(key_is_credential("Authorization"));
        assert!(key_is_credential("refreshToken"));

        assert!(!key_is_credential("role"));
        assert!(!key_is_credential("displayName"));
        assert!(!key_is_credential("enabled"));
        assert!(!key_is_credential("from"));
        assert!(!key_is_credential("to"));
        assert!(!key_is_credential("family"));
        assert!(!key_is_credential("rowsDeleted"));
        assert!(!key_is_credential("sessionsRevoked"));
        assert!(!key_is_credential("fields"));
    }

    /// PURA-236 acceptance — passing a hashmap with `password`/`apiKey`/
    /// `sshKey` keys produces a redacted payload (verified by reading back).
    #[test]
    fn redact_payload_blanks_credential_values() {
        let mut v = serde_json::json!({
            "username": "alice",
            "password": "hunter2",
            "apiKey": "k-aaa",
            "sshKey": "k-bbb",
            "innocent": "ok",
        });
        redact_payload(&mut v);
        assert_eq!(v["username"], serde_json::json!("alice"));
        assert_eq!(v["innocent"], serde_json::json!("ok"));
        for key in ["password", "apiKey", "sshKey"] {
            assert_eq!(
                v[key],
                serde_json::json!(REDACTED_SENTINEL),
                "{key} must be rewritten to the redacted sentinel"
            );
        }
        let raw = serde_json::to_string(&v).unwrap();
        assert!(!raw.contains("hunter2"));
        assert!(!raw.contains("k-aaa"));
        assert!(!raw.contains("k-bbb"));
    }

    #[test]
    fn redact_payload_walks_nested_objects() {
        let mut v = serde_json::json!({
            "outer": { "inner": { "password": "leak", "ok": "fine" } }
        });
        redact_payload(&mut v);
        assert_eq!(
            v["outer"]["inner"]["password"],
            serde_json::json!(REDACTED_SENTINEL)
        );
        assert_eq!(v["outer"]["inner"]["ok"], serde_json::json!("fine"));
    }

    #[test]
    fn redact_payload_walks_arrays() {
        let mut v = serde_json::json!({
            "items": [ {"password": "x"}, {"role": "moderator"} ]
        });
        redact_payload(&mut v);
        assert_eq!(
            v["items"][0]["password"],
            serde_json::json!(REDACTED_SENTINEL)
        );
        assert_eq!(v["items"][1]["role"], serde_json::json!("moderator"));
    }

    #[test]
    fn redact_payload_leaves_clean_payload_untouched() {
        let mut v = serde_json::json!({"from": "moderator", "to": "admin"});
        let before = v.clone();
        redact_payload(&mut v);
        assert_eq!(v, before);
    }
}
