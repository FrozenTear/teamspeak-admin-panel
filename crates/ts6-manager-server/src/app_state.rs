//! Shared application state for axum handlers.
//!
//! Phase 1 SECURITY (slice 3): carries the SurrealDB handle, the JWT secret,
//! and the configured access/refresh lifetimes so [`auth::routes`] can mint
//! tokens without re-reading env vars on every request. Future workstreams
//! (REST, WS, FLOW) extend this struct in their own slices.

use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::db::Database;
use crate::webquery::WebQueryPool;

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    /// HS256 signing secret for access tokens. `Arc<Vec<u8>>` keeps cloning
    /// cheap (axum hands the state to every handler by value).
    pub jwt_secret: Arc<Vec<u8>>,
    pub jwt_access_expiry: Duration,
    pub jwt_refresh_expiry: Duration,
    /// PURA-23: pool of WebQuery clients keyed by `server_connection.id`.
    /// Phase 1 fills lazily on first dashboard hit; Phase 2 will pre-populate
    /// on boot and run the §10.7 30s health probe.
    pub webquery: WebQueryPool,
}

impl AppState {
    pub fn from_config(cfg: &Config, db: Arc<Database>) -> Self {
        Self {
            db,
            jwt_secret: Arc::new(cfg.jwt_secret.as_bytes().to_vec()),
            jwt_access_expiry: cfg.jwt_access_expiry,
            jwt_refresh_expiry: cfg.jwt_refresh_expiry,
            webquery: WebQueryPool::new(cfg.ts_allow_self_signed),
        }
    }
}
