//! Phase 1 SECURITY slice 5 (PURA-22) — REST routes for `/api/setup` and
//! `/api/servers`.
//!
//! The auth surface (`/api/auth/*`) lives under [`crate::auth::routes`]
//! because that module owns the cryptographic primitives the routes need
//! (JWT, refresh, password, complexity, extractors). This module is the
//! plain-HTTP layer for non-auth endpoints — handlers wire repos +
//! `crate::crypto` (seal/unseal) into wire-shape responses defined in
//! `ts6_manager_shared::{setup, servers}`.

pub mod servers;
pub mod setup;

use ts6_manager_shared::servers::ServerSummary;

use crate::repos::server_connections::ServerConnection;

/// Translate a DB row into the wire-shape [`ServerSummary`].
///
/// Spec §7.5 requires:
/// - `apiKey` MUST NOT appear in any response — preserved by construction
///   (`ServerSummary` has no `api_key` field).
/// - `hasSshCredentials: !!sshUsername` — mirrored verbatim here so callers
///   on the FE branch on a single boolean.
///
/// Shared between `routes::setup::init` and `routes::servers::*`.
pub(crate) fn server_summary_from_row(row: ServerConnection) -> ServerSummary {
    ServerSummary {
        id: row.id,
        name: row.name,
        host: row.host,
        webquery_port: row.webqueryPort,
        use_https: row.useHttps,
        ssh_port: row.sshPort,
        has_ssh_credentials: row.sshUsername.is_some(),
        ssh_username: row.sshUsername,
        query_bot_channel: row.queryBotChannel,
        query_bot_nickname: row.queryBotNickname,
        ssh_bot_nickname: row.sshBotNickname,
        enabled: row.enabled,
        created_at: row.createdAt,
        updated_at: row.updatedAt,
    }
}
