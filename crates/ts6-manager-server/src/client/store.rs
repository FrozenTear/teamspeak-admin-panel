//! Session state machine + storage encoding.
//!
//! `AuthState` is a closed-set sum: either we have no session at all, or we
//! have one with both tokens and the user's profile. Refresh rotates the
//! tokens in place; logout / refresh failure invalidates and clears
//! storage.
//!
//! The storage representation is a single JSON blob under one key so a
//! reload is one read. A schema version prefix gives us a rip-cord for the
//! day the persisted shape changes — unknown versions are treated as
//! "no session", not parsed as the current shape.

use serde::{Deserialize, Serialize};
use ts6_manager_shared::auth::UserInfo;

use crate::client::storage::Storage;

/// `localStorage` key carrying the JSON-encoded [`PersistedSession`].
///
/// Per spec §28 it lives under a single namespaced key — making it easy to
/// invalidate every operator session by bumping the version field.
pub const SESSION_STORAGE_KEY: &str = "ts6-manager.auth.session";

/// Bumping this rejects every previously-persisted blob without touching
/// the bytes. v1 is the first cut; subsequent shapes should add fields and
/// keep `version: 1` until a breaking change forces a rev.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

/// Closed-set session state. Anything other than `Authenticated` is
/// equivalent to "no session" — we don't model "refreshing" or "expired"
/// because the interceptor handles those transitions atomically.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AuthState {
    #[default]
    Anonymous,
    Authenticated {
        access: String,
        refresh: String,
        user: UserInfo,
    },
}

impl AuthState {
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthState::Authenticated { .. })
    }

    pub fn access_token(&self) -> Option<&str> {
        match self {
            AuthState::Authenticated { access, .. } => Some(access.as_str()),
            AuthState::Anonymous => None,
        }
    }

    pub fn refresh_token(&self) -> Option<&str> {
        match self {
            AuthState::Authenticated { refresh, .. } => Some(refresh.as_str()),
            AuthState::Anonymous => None,
        }
    }

    pub fn user(&self) -> Option<&UserInfo> {
        match self {
            AuthState::Authenticated { user, .. } => Some(user),
            AuthState::Anonymous => None,
        }
    }
}

/// On-disk representation of [`AuthState::Authenticated`].
///
/// `version` is an integer schema tag — see [`SESSION_SCHEMA_VERSION`] for
/// the migration story. `UserInfo` is reused from the shared crate so the
/// disk layout matches the wire layout byte-for-byte.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSession {
    pub version: u32,
    pub access: String,
    pub refresh: String,
    pub user: UserInfo,
}

impl PersistedSession {
    fn from_state(state: &AuthState) -> Option<Self> {
        match state {
            AuthState::Anonymous => None,
            AuthState::Authenticated {
                access,
                refresh,
                user,
            } => Some(Self {
                version: SESSION_SCHEMA_VERSION,
                access: access.clone(),
                refresh: refresh.clone(),
                user: user.clone(),
            }),
        }
    }

    fn into_state(self) -> Option<AuthState> {
        if self.version != SESSION_SCHEMA_VERSION {
            return None;
        }
        Some(AuthState::Authenticated {
            access: self.access,
            refresh: self.refresh,
            user: self.user,
        })
    }
}

/// Read the persisted session out of `storage`. Missing key, malformed
/// JSON, and version-mismatch all collapse to [`AuthState::Anonymous`] —
/// the contract is "the user appears logged out" so the consumer doesn't
/// need to special-case storage failures.
pub fn load_state(storage: &dyn Storage) -> AuthState {
    let raw = match storage.get(SESSION_STORAGE_KEY) {
        Some(s) if !s.is_empty() => s,
        _ => return AuthState::Anonymous,
    };
    match serde_json::from_str::<PersistedSession>(&raw) {
        Ok(p) => p.into_state().unwrap_or(AuthState::Anonymous),
        Err(_) => AuthState::Anonymous,
    }
}

/// Persist a state, or clear storage if `Anonymous`.
pub fn save_state(storage: &dyn Storage, state: &AuthState) {
    match PersistedSession::from_state(state) {
        Some(p) => match serde_json::to_string(&p) {
            Ok(s) => storage.set(SESSION_STORAGE_KEY, &s),
            // Encoding `PersistedSession` should be infallible — we own the
            // shape — but log just in case. Storage stays untouched, which
            // means the next load_state() will see whatever was there
            // before. That's preferable to clobbering with bad data.
            Err(_) => tracing::warn!("failed to encode persisted session"),
        },
        None => storage.remove(SESSION_STORAGE_KEY),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::storage::MemoryStore;

    fn user(role: &str) -> UserInfo {
        UserInfo {
            id: 7,
            username: "alice".into(),
            display_name: "Alice".into(),
            role: role.into(),
        }
    }

    #[test]
    fn defaults_to_anonymous() {
        let s = AuthState::default();
        assert_eq!(s, AuthState::Anonymous);
        assert!(!s.is_authenticated());
        assert_eq!(s.access_token(), None);
        assert_eq!(s.refresh_token(), None);
        assert!(s.user().is_none());
    }

    #[test]
    fn authenticated_exposes_tokens_and_user() {
        let s = AuthState::Authenticated {
            access: "ax".into(),
            refresh: "rx".into(),
            user: user("admin"),
        };
        assert!(s.is_authenticated());
        assert_eq!(s.access_token(), Some("ax"));
        assert_eq!(s.refresh_token(), Some("rx"));
        assert_eq!(s.user().unwrap().role, "admin");
    }

    #[test]
    fn round_trip_through_storage_preserves_state() {
        let store = MemoryStore::new();
        let original = AuthState::Authenticated {
            access: "ax".into(),
            refresh: "rx".into(),
            user: user("viewer"),
        };
        save_state(&store, &original);
        assert!(store.get(SESSION_STORAGE_KEY).is_some());
        let recovered = load_state(&store);
        assert_eq!(recovered, original);
    }

    #[test]
    fn save_anonymous_clears_storage() {
        let store = MemoryStore::new();
        save_state(
            &store,
            &AuthState::Authenticated {
                access: "ax".into(),
                refresh: "rx".into(),
                user: user("viewer"),
            },
        );
        save_state(&store, &AuthState::Anonymous);
        assert!(store.get(SESSION_STORAGE_KEY).is_none());
    }

    #[test]
    fn malformed_blob_loads_as_anonymous_without_panic() {
        let store = MemoryStore::from_iter([(SESSION_STORAGE_KEY, "not-json")]);
        assert_eq!(load_state(&store), AuthState::Anonymous);
    }

    #[test]
    fn unknown_schema_version_loads_as_anonymous() {
        let blob = r#"{"version":999,"access":"a","refresh":"r","user":{"id":1,"username":"u","displayName":"u","role":"viewer"}}"#;
        let store = MemoryStore::from_iter([(SESSION_STORAGE_KEY, blob)]);
        assert_eq!(load_state(&store), AuthState::Anonymous);
    }

    #[test]
    fn empty_string_loads_as_anonymous() {
        // localStorage may persist an empty string after some browsers'
        // private-mode shenanigans. Treat it like missing.
        let store = MemoryStore::from_iter([(SESSION_STORAGE_KEY, "")]);
        assert_eq!(load_state(&store), AuthState::Anonymous);
    }
}
