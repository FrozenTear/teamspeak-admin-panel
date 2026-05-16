//! Wire-format types for the Phase 2 control surface — PURA-71.
//!
//! These shapes back `/api/servers/{configId}/vs/{sid}/...` REST endpoints
//! that expose the operator-facing control actions (clients, channels, bans,
//! info, logs). Names mirror the TS WebQuery JSON keys verbatim where they
//! cross the wire — the spec treats those keys as part of the external
//! contract (see deviations register entry D8 §"JSON wire-format keys").
//!
//! Numeric fields are emitted as native JSON numbers in responses (the
//! server-side `webquery::models` types have already coerced TS WebQuery's
//! string-numbers into Rust `i64`/`f64`). Request bodies use camelCase
//! aliases via `#[serde(rename_all = "camelCase")]` so the FE matches the
//! rest of the §7 surface.

use serde::{Deserialize, Serialize};

fn default_talker() -> i64 {
    1
}

/// `GET /api/servers/{configId}/vs/{sid}/clients` row. Mirrors the TS
/// WebQuery `clientlist -uid -away -voice -times -groups -info -country`
/// projection (spec §7.8). `connection_client_ip` is admin-only — the
/// route layer wipes it for non-admin callers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClientListItem {
    pub clid: i64,
    pub cid: i64,
    pub client_database_id: i64,
    pub client_type: i64,
    pub client_nickname: String,
    pub client_unique_identifier: String,
    pub client_away: i64,
    pub client_away_message: String,
    pub client_flag_talking: i64,
    pub client_input_muted: i64,
    pub client_output_muted: i64,
    pub client_input_hardware: i64,
    pub client_output_hardware: i64,
    /// Operator-set talker flag — the TS6 server-side mute primitive
    /// (PURA-292/PURA-299). `0` = talk permission revoked; `1` = allowed.
    /// Effective only in moderated channels (`channel_needed_talk_power > 0`).
    /// Defaults to `1` for all clients.
    #[serde(default = "default_talker")]
    pub client_is_talker: i64,
    pub client_idle_time: i64,
    pub client_lastconnected: i64,
    pub client_created: i64,
    pub client_servergroups: String,
    pub client_channel_group_id: i64,
    pub client_version: String,
    pub client_platform: String,
    pub client_country: String,
    /// Empty string for non-admin callers (spec §7.8 — admin-only field).
    #[serde(default)]
    pub connection_client_ip: String,
}

/// `GET /api/servers/{configId}/vs/{sid}/clients/{cldbid}` body.
/// Returns the full `clientdbinfo` row plus, when the cldbid is currently
/// online, a `liveClient` snapshot from `clientinfo`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientDetail {
    pub cldbid: i64,
    pub client_unique_identifier: String,
    pub client_nickname: String,
    pub client_created: i64,
    pub client_lastconnected: i64,
    pub client_totalconnections: i64,
    pub client_description: String,
    /// Wire field name preserves the TS key (`client_lastip`); admin-only
    /// — wiped to empty for non-admin callers.
    #[serde(default)]
    pub client_lastip: String,
    /// Present only when the database client is currently online. The
    /// inner shape is the §7.9 `clientinfo` projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_client: Option<LiveClient>,
}

/// Trimmed `clientinfo` shape — the operator-facing fields the FE actually
/// renders on the client-detail page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveClient {
    pub clid: i64,
    pub cid: i64,
    pub client_type: i64,
    pub client_nickname: String,
    pub client_platform: String,
    pub client_version: String,
    pub client_idle_time: i64,
    pub client_away: i64,
    pub client_away_message: String,
    pub client_input_muted: i64,
    pub client_output_muted: i64,
    pub client_country: String,
    pub client_servergroups: String,
    pub client_channel_group_id: i64,
    pub client_totalconnections: i64,
    /// Admin-only; wiped to empty for non-admin callers.
    #[serde(default)]
    pub connection_client_ip: String,
}

/// `GET /api/servers/{configId}/vs/{sid}/channels` row. The route returns
/// a flat list ordered by upstream `channel_order`; FE assembles the tree
/// using `pid` (channels with `pid == 0` are top-level).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChannelTreeNode {
    pub cid: i64,
    pub pid: i64,
    pub channel_name: String,
    pub channel_order: i64,
    pub channel_topic: String,
    pub channel_flag_default: i64,
    pub channel_flag_password: i64,
    pub channel_flag_permanent: i64,
    pub channel_flag_semi_permanent: i64,
    pub channel_maxclients: i64,
    pub channel_maxfamilyclients: i64,
    pub total_clients: i64,
    pub total_clients_family: i64,
    pub channel_icon_id: i64,
    pub seconds_empty: i64,
    pub channel_needed_subscribe_power: i64,
}

/// `POST /api/servers/{configId}/vs/{sid}/clients/{clid}/kick` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KickRequest {
    /// Either `"channel"` (kick from channel only, reasonid=4) or
    /// `"server"` (kick from virtual-server, reasonid=5). TS spec §14.1.
    pub kind: KickKind,
    /// Optional reason text shown to the kicked client + visible in TS logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KickKind {
    Channel,
    Server,
}

impl KickKind {
    /// TS reason id per §14.1. `4` → kick from channel; `5` → kick from
    /// server.
    pub fn reason_id(self) -> i64 {
        match self {
            KickKind::Channel => 4,
            KickKind::Server => 5,
        }
    }
}

/// `POST /api/servers/{configId}/vs/{sid}/clients/{clid}/move` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoveRequest {
    /// Destination channel id.
    pub cid: i64,
    /// Optional channel password if the destination is password-protected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_password: Option<String>,
}

/// `POST /api/servers/{configId}/vs/{sid}/clients/{clid}/mute` body.
/// Pass `null` for either field to leave it unchanged (the route
/// short-circuits when both are `null`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MuteRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_muted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_muted: Option<bool>,
}

/// `POST /api/servers/{configId}/vs/{sid}/bans` body. At least one of
/// `ip` / `uid` / `myTsId` / `name` MUST be supplied — the route
/// rejects an all-empty body with `400`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BanCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub my_ts_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Ban duration in seconds. `Some(0)` is permanent per spec §7.12;
    /// `None` lets upstream defaults apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,
}

/// `POST /api/servers/{configId}/vs/{sid}/bans` 201 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BanCreated {
    pub banid: i64,
}

/// `GET /api/servers/{configId}/vs/{sid}/bans` row.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BanListItem {
    pub banid: i64,
    pub ip: String,
    pub uid: String,
    pub mytsid: String,
    pub name: String,
    pub created: i64,
    pub duration: i64,
    pub reason: String,
    pub invokername: String,
    pub invokercldbid: i64,
    pub invokeruid: String,
    pub enforcements: i64,
    pub lastnickname: String,
}

/// `GET /api/servers/{configId}/vs/{sid}/info` body — `serverinfo`
/// passthrough.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerInfoResponse {
    pub virtualserver_name: String,
    pub virtualserver_platform: String,
    pub virtualserver_version: String,
    pub virtualserver_maxclients: i64,
    pub virtualserver_uptime: i64,
    pub virtualserver_total_packetloss_total: f64,
    pub virtualserver_total_ping: f64,
}

/// `GET /api/servers/{configId}/vs/{sid}/logs` query.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogTailQuery {
    /// Cursor — pass the previous response's `last_pos` to page forward.
    /// Omit on initial fetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<i64>,
    /// Max lines to return. Capped to `MAX_LOG_LINES` server-side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<u32>,
    /// Minimum severity to include. Passed through as a substring filter
    /// applied to the line text before it ships to the client (the TS
    /// `logview` upstream does not filter; we filter on egress).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

/// `GET /api/servers/{configId}/vs/{sid}/logs` body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogTailResponse {
    /// `last_pos` from the upstream. Echo back as `?after=` to page.
    /// `None` when the server returns an empty page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pos: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<i64>,
    pub lines: Vec<LogLine>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogLine {
    /// Raw upstream log line. The TS `logview` payload prepends an ISO
    /// timestamp + severity token; FE parses that into `level` / `at` /
    /// `body` for rendering. The server keeps the raw text so anyone
    /// running curl gets the spec-shaped response.
    pub text: String,
}
