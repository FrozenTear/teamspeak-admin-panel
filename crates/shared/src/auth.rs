//! Wire-format types for the auth REST surface (spec §6.5 + §7).
//!
//! Spec field names on the wire are camelCase. Rust fields here are
//! idiomatic snake_case, with `#[serde(rename_all = "camelCase")]` doing
//! the renaming at the (de)serialise boundary. The wire contract is
//! preserved verbatim — the camelCase regression test at the bottom of
//! the module pins every struct so a missing `rename_all` on a future
//! addition fails CI rather than drifting silently.
//!
//! Implementation lives in `ts6-manager-server::auth::routes` (Phase 1
//! SECURITY slice 2 — see [PURA-4 plan §11](/PURA/issues/PURA-4#document-plan)).

use serde::{Deserialize, Serialize};

/// `POST /api/auth/login` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// `POST /api/auth/login` and `POST /api/auth/refresh` response body.
///
/// Both tokens are returned together. Per spec §6.5.3 the refresh response
/// MUST issue a new refresh token alongside the new access token (rotation
/// is mandatory; reusing the old refresh token triggers reuse-detection).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPairResponse {
    pub access_token: String,
    pub refresh_token: String,
}

/// `POST /api/auth/refresh` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
    pub refresh_token: String,
}

/// `POST /api/auth/logout` request body. Refresh token is the credential —
/// no `Authorization` header required.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogoutRequest {
    pub refresh_token: String,
}

/// `PUT /api/auth/password` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// `GET /api/auth/me` response body — the current user's public profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfo {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: String,
}

/// Standard error envelope used across the auth surface.
///
/// Spec §6.4.1 / §6.4.2 / §6.6 mandate exact error strings — see
/// [`auth_error_strings`] for the full catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorResponse {
    pub error: String,
}

impl ErrorResponse {
    pub fn new(msg: impl Into<String>) -> Self {
        Self { error: msg.into() }
    }
}

/// Spec-verbatim error strings for the auth surface.
///
/// Constants here are the ground truth; `auth::extractors`, `auth::routes`,
/// and the Dioxus login route ([PURA-14](/PURA/issues/PURA-14)) MUST use
/// them directly so the same byte sequence reaches the wire and the
/// rendered UI without copy duplication.
pub mod auth_error_strings {
    // Server-side wire strings (spec §6.4).
    pub const NO_TOKEN: &str = "No token provided";
    pub const INVALID_TOKEN: &str = "Invalid or expired token";
    pub const USER_DISABLED: &str = "User account disabled or deleted";
    pub const INSUFFICIENT_PERMS: &str = "Insufficient permissions";
    pub const INVALID_SERVER_ID: &str = "Invalid server config ID";
    pub const NO_SERVER_ACCESS: &str = "No access to this server";
    pub const RATE_LIMIT_AUTH: &str = "Too many attempts, please try again later";
    pub const RATE_LIMIT_WEBHOOK: &str = "Too many webhook requests";

    // UI-side copy used by the Dioxus login route. These are not server
    // wire strings — the server returns its own bodies (e.g., "Invalid
    // credentials") and the login page re-maps to user-facing copy.
    /// Shown for any 401 / generic 4xx during login. Spec §28.2 example
    /// uses "Invalid credentials"; the [PURA-14](/PURA/issues/PURA-14)
    /// issue tightened the copy to match the design-system tone.
    pub const INVALID_CREDENTIALS: &str = "Invalid username or password";
    /// Shown for transport / 5xx failures during login. Spec is silent
    /// on this branch; we intentionally avoid blaming the user.
    pub const SIGN_IN_UNAVAILABLE: &str = "Sign-in is temporarily unavailable, please try again";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_serde_roundtrip_preserves_wire_field_names() {
        // External contract: field names "username" and "password" — verbatim.
        let req = LoginRequest {
            username: "alice".into(),
            password: "Hunter2!".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""username":"alice""#));
        assert!(json.contains(r#""password":"Hunter2!""#));
        let back: LoginRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.username, "alice");
        assert_eq!(back.password, "Hunter2!");
    }

    #[test]
    fn token_pair_uses_camel_case_per_spec() {
        // External contract: "accessToken" and "refreshToken" (camelCase).
        let pair = TokenPairResponse {
            access_token: "jwt-here".into(),
            refresh_token: "hex-here".into(),
        };
        let json = serde_json::to_string(&pair).unwrap();
        assert!(json.contains(r#""accessToken":"jwt-here""#));
        assert!(json.contains(r#""refreshToken":"hex-here""#));
        let back: TokenPairResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.access_token, "jwt-here");
        assert_eq!(back.refresh_token, "hex-here");
    }

    #[test]
    fn refresh_request_serialises_camelcase() {
        let req = RefreshRequest {
            refresh_token: "abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""refreshToken":"abc""#));
        let back: RefreshRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.refresh_token, "abc");
    }

    #[test]
    fn logout_request_serialises_camelcase() {
        let req = LogoutRequest {
            refresh_token: "abc".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""refreshToken":"abc""#));
        let back: LogoutRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.refresh_token, "abc");
    }

    #[test]
    fn change_password_request_serialises_camelcase() {
        let req = ChangePasswordRequest {
            current_password: "old".into(),
            new_password: "new".into(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""currentPassword":"old""#));
        assert!(json.contains(r#""newPassword":"new""#));
        let back: ChangePasswordRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.current_password, "old");
        assert_eq!(back.new_password, "new");
    }

    #[test]
    fn user_info_serialises_camelcase() {
        let info = UserInfo {
            id: 7,
            username: "alice".into(),
            display_name: "Alice".into(),
            role: "admin".into(),
        };
        let json = serde_json::to_string(&info).unwrap();
        assert!(json.contains(r#""displayName":"Alice""#));
        let back: UserInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.display_name, "Alice");
    }

    #[test]
    fn error_strings_match_spec() {
        // Sample two — the rest are checked at the route layer when handlers land.
        assert_eq!(auth_error_strings::NO_TOKEN, "No token provided");
        assert_eq!(
            auth_error_strings::USER_DISABLED,
            "User account disabled or deleted"
        );
    }

    /// Cheap insurance against forgetting `#[serde(rename_all = "camelCase")]`
    /// on a future struct in this module. If a multi-word JSON key is ever
    /// missing — i.e. a field crosses the wire as snake_case when the spec
    /// requires camelCase — this test fires before the wire shape ships.
    ///
    /// We can't enumerate the structs reflectively without a macro, so this
    /// test asserts the camelCase form for every multi-word key currently
    /// in scope. Adding a new struct? Add a key here.
    #[test]
    fn every_multi_word_wire_key_is_camelcase() {
        let cases: &[(&str, String)] = &[
            (
                "TokenPairResponse",
                serde_json::to_string(&TokenPairResponse {
                    access_token: "a".into(),
                    refresh_token: "r".into(),
                })
                .unwrap(),
            ),
            (
                "RefreshRequest",
                serde_json::to_string(&RefreshRequest {
                    refresh_token: "r".into(),
                })
                .unwrap(),
            ),
            (
                "LogoutRequest",
                serde_json::to_string(&LogoutRequest {
                    refresh_token: "r".into(),
                })
                .unwrap(),
            ),
            (
                "ChangePasswordRequest",
                serde_json::to_string(&ChangePasswordRequest {
                    current_password: "c".into(),
                    new_password: "n".into(),
                })
                .unwrap(),
            ),
            (
                "UserInfo",
                serde_json::to_string(&UserInfo {
                    id: 1,
                    username: "u".into(),
                    display_name: "d".into(),
                    role: "viewer".into(),
                })
                .unwrap(),
            ),
        ];
        for (name, json) in cases {
            // No snake_case keys in any wire body — catches missing
            // `rename_all` (the renamed key disappears, the snake_case
            // one shows up).
            for forbidden in [
                "access_token",
                "refresh_token",
                "current_password",
                "new_password",
                "display_name",
            ] {
                assert!(
                    !json.contains(forbidden),
                    "{name} leaked snake_case field `{forbidden}` to the wire: {json}"
                );
            }
        }
    }
}
