//! Wire-format types for the auth REST surface (spec §6.5 + §7).
//!
//! JSON field names are part of the external contract — preserved verbatim
//! from the spec. The Dioxus client and the axum server both deserialise via
//! these types so the contract is enforced at compile time.
//!
//! Implementation lives in `ts6-manager-server::auth::routes` (Phase 1
//! SECURITY slice 2 — see [PURA-4 plan §11](/PURA/issues/PURA-4#document-plan)).

use serde::{Deserialize, Serialize};

/// `POST /api/auth/login` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
pub struct TokenPairResponse {
    pub accessToken: String,
    pub refreshToken: String,
}

/// `POST /api/auth/refresh` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub refreshToken: String,
}

/// `POST /api/auth/logout` request body. Refresh token is the credential —
/// no `Authorization` header required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutRequest {
    pub refreshToken: String,
}

/// `PUT /api/auth/password` request body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePasswordRequest {
    pub currentPassword: String,
    pub newPassword: String,
}

/// `GET /api/auth/me` response body — the current user's public profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: i64,
    pub username: String,
    pub displayName: String,
    pub role: String,
}

/// Standard error envelope used across the auth surface.
///
/// Spec §6.4.1 / §6.4.2 / §6.6 mandate exact error strings — see
/// [`auth_error_strings`] for the full catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
/// Constants here are the ground truth; `auth::extractors` and `auth::routes`
/// MUST use them directly so the wire shape is identical across call sites.
pub mod auth_error_strings {
    pub const NO_TOKEN: &str = "No token provided";
    pub const INVALID_TOKEN: &str = "Invalid or expired token";
    pub const USER_DISABLED: &str = "User account disabled or deleted";
    pub const INSUFFICIENT_PERMS: &str = "Insufficient permissions";
    pub const INVALID_SERVER_ID: &str = "Invalid server config ID";
    pub const NO_SERVER_ACCESS: &str = "No access to this server";
    pub const RATE_LIMIT_AUTH: &str = "Too many attempts, please try again later";
    pub const RATE_LIMIT_WEBHOOK: &str = "Too many webhook requests";
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
            accessToken: "jwt-here".into(),
            refreshToken: "hex-here".into(),
        };
        let json = serde_json::to_string(&pair).unwrap();
        assert!(json.contains(r#""accessToken":"jwt-here""#));
        assert!(json.contains(r#""refreshToken":"hex-here""#));
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
}
