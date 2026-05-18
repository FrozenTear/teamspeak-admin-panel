//! Typed response shapes for the WebQuery command surface.
//!
//! The TS6 WebQuery API returns most numeric fields as JSON *strings*
//! (e.g. `"virtualserver_maxclients": "32"`). The shapes below opt every
//! numeric field into a string-or-number deserialiser so the SPA-facing
//! handlers can work in native Rust types.
//!
//! Phase 1 (PURA-23) shipped the read-only subset for the §7.19 dashboard.
//! Phase 2 (PURA-68) extends this module with the full ServerQuery surface
//! the FE needs for ops actions: clientinfo, clientdblist, channelinfo,
//! channelclientlist, hostinfo, logview, banlist, channelclientpermlist.

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

fn one_i64() -> i64 {
    1
}

/// `version` — instance scope (`/version`). Used as the cheap health probe
/// per spec §10.7. We only need to know the call succeeded; the body is
/// retained for diagnostics / future use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub build: String,
    pub platform: String,
}

/// `serverlist` row — instance scope (`/serverlist`). Spec §7.6 maps this to
/// `GET /servers/:configId/virtual-servers`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VirtualServerEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub virtualserver_id: i64,
    pub virtualserver_name: String,
    #[serde(default)]
    pub virtualserver_status: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualserver_clientsonline: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualserver_maxclients: i64,
}

/// `channellist` row. The basic projection has `cid` / `channel_name` /
/// `pid` / `channel_order`; flag-driven fields below default-init when the
/// corresponding `-topic` / `-flags` / `-voice` / `-limits` / `-icon` /
/// `-secondsempty` flag was not requested (spec §7.7 mandates these flags
/// at the REST layer; the typed surface tolerates either projection).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub cid: i64,
    #[serde(default)]
    pub channel_name: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub pid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_order: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_needed_subscribe_power: i64,
    // -topic
    #[serde(default)]
    pub channel_topic: String,
    // -flags
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_default: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_password: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_permanent: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_semi_permanent: i64,
    // -limits
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub total_clients: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub total_clients_family: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_maxclients: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_maxfamilyclients: i64,
    // -icon
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_icon_id: i64,
    // -secondsempty
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub seconds_empty: i64,
}

/// `clientlist` row. The §7.8 REST layer always asks for
/// `-uid -away -voice -times -groups -info -country` and conditionally
/// `-ip` for admin callers. Flag-driven fields default-init when the
/// upstream omits them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub clid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub cid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_database_id: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_type: i64,
    #[serde(default)]
    pub client_nickname: String,
    // -uid
    #[serde(default)]
    pub client_unique_identifier: String,
    // -away
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_away: i64,
    #[serde(default)]
    pub client_away_message: String,
    // -voice
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_flag_talking: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_input_muted: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_output_muted: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_input_hardware: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_output_hardware: i64,
    // Operator-set talker flag (PURA-299). Defaults to 1 (allowed); 0 means
    // talk permission revoked. Effective only in moderated channels.
    #[serde(default = "one_i64", deserialize_with = "stringy::deserialize_default")]
    pub client_is_talker: i64,
    // -times
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_idle_time: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_lastconnected: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_created: i64,
    // -groups
    #[serde(default)]
    pub client_servergroups: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_channel_group_id: i64,
    // -info
    #[serde(default)]
    pub client_version: String,
    #[serde(default)]
    pub client_platform: String,
    // -country
    #[serde(default)]
    pub client_country: String,
    // -ip (admin only)
    #[serde(default)]
    pub connection_client_ip: String,
}

/// `clientinfo` — full per-client metadata (`/<sid>/clientinfo?clid=<n>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub client_unique_identifier: String,
    #[serde(default)]
    pub client_nickname: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_database_id: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub cid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_type: i64,
    #[serde(default)]
    pub client_platform: String,
    #[serde(default)]
    pub client_version: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_idle_time: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_lastconnected: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_created: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_away: i64,
    #[serde(default)]
    pub client_away_message: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_input_muted: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_output_muted: i64,
    #[serde(default)]
    pub client_country: String,
    #[serde(default)]
    pub connection_client_ip: String,
    #[serde(default)]
    pub client_servergroups: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_channel_group_id: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_totalconnections: i64,
}

/// `clientdblist` row — paginated per `?start, ?duration` (§7.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientDbEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub cldbid: i64,
    #[serde(default)]
    pub client_unique_identifier: String,
    #[serde(default)]
    pub client_nickname: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_created: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_lastconnected: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_totalconnections: i64,
    #[serde(default)]
    pub client_description: String,
    #[serde(default)]
    pub client_lastip: String,
}

/// `channelinfo` — full per-channel metadata (`/<sid>/channelinfo?cid=<n>`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    #[serde(default)]
    pub channel_name: String,
    #[serde(default)]
    pub channel_topic: String,
    #[serde(default)]
    pub channel_description: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_codec: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_codec_quality: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_maxclients: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_maxfamilyclients: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_order: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub pid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_permanent: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_semi_permanent: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_default: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_flag_password: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_needed_talk_power: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub channel_needed_subscribe_power: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub seconds_empty: i64,
}

/// `serverinfo` — virtual-server scope. Pulls every field §7.19.1 needs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub virtualserver_name: String,
    #[serde(default)]
    pub virtualserver_platform: String,
    #[serde(default)]
    pub virtualserver_version: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualserver_maxclients: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualserver_uptime: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_float_default")]
    pub virtualserver_total_packetloss_total: f64,
    #[serde(default, deserialize_with = "stringy::deserialize_float_default")]
    pub virtualserver_total_ping: f64,
}

/// `serverrequestconnectioninfo` — virtual-server scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionInfo {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_bandwidth_received_last_second_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_bandwidth_sent_last_second_total: i64,
}

/// `hostinfo` — instance scope (`/hostinfo`). Headline counters the
/// §7.18 instance-info route returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub instance_uptime: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub host_timestamp_utc: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualservers_running_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualservers_total_clients_online: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualservers_total_channels_online: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub virtualservers_total_maxclients: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_bandwidth_sent_last_second_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_bandwidth_received_last_second_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_packets_sent_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_packets_received_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_bytes_sent_total: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub connection_bytes_received_total: i64,
}

/// `logview` row. The first row of a paginated response carries `last_pos`
/// and `file_size`; subsequent rows only carry the line text in `l`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    #[serde(default, deserialize_with = "stringy::deserialize_opt")]
    pub last_pos: Option<i64>,
    #[serde(default, deserialize_with = "stringy::deserialize_opt")]
    pub file_size: Option<i64>,
    #[serde(default)]
    pub l: String,
}

/// `banlist` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub banid: i64,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub uid: String,
    #[serde(default)]
    pub mytsid: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub created: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub duration: i64,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub invokername: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub invokercldbid: i64,
    #[serde(default)]
    pub invokeruid: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub enforcements: i64,
    #[serde(default)]
    pub lastnickname: String,
}

/// `banadd` response — TS returns the new ban id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanAddResponse {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub banid: i64,
}

/// `complainlist` row — one TS6 complaint. A complaint is a
/// `(tcldbid, fcldbid)` pair: the `t*` fields name the **target** (the
/// subject complained about), the `f*` fields name the **from** client
/// (the complainant). TS6 exposes no single complaint id on the wire —
/// the addressing key is the pair. Field names are the TS6 WebQuery wire
/// keys verbatim. The route layer translates upstream code 1281
/// (`database_empty_result`) to an empty list, mirroring `banlist`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplaintEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub tcldbid: i64,
    #[serde(default)]
    pub tname: String,
    #[serde(deserialize_with = "stringy::deserialize")]
    pub fcldbid: i64,
    #[serde(default)]
    pub fname: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub timestamp: i64,
}

/// `channelclientpermlist` row — single permission entry on a `(cid, cldbid)`
/// pair. The route layer translates upstream code 1281 (`database_empty_result`)
/// to an empty list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelClientPerm {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub permid: i64,
    #[serde(default)]
    pub permsid: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permvalue: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permnegated: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permskip: i64,
}

// =====================================================================
// PURA-373 — server-group / channel-group / permission / token / message
// command surface. Wire keys are preserved verbatim per spec §7.9–7.16
// and the PURA-370 research §2 command map.
// =====================================================================

/// `servergrouplist` row. `type` is `0` regular / `1` template / `2`
/// ServerQuery; `savedb` is `1` when membership persists to the TS
/// database (PURA-370 §1.1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerGroupEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub sgid: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub r#type: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub iconid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub savedb: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub sortid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub namemode: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n_modifyp: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n_member_addp: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n_member_removep: i64,
}

/// `channelgrouplist` row — mirrors [`ServerGroupEntry`] with `cgid`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelGroupEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub cgid: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub r#type: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub iconid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub savedb: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub sortid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub namemode: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n_modifyp: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n_member_addp: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n_member_removep: i64,
}

/// `servergroupadd` / `servergroupcopy` (`tsgid=0`) response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerGroupIdResponse {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub sgid: i64,
}

/// `channelgroupadd` response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelGroupIdResponse {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub cgid: i64,
}

/// `servergroupclientlist -names` row — one member of a server group.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerGroupClient {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub cldbid: i64,
    #[serde(default)]
    pub client_nickname: String,
    #[serde(default)]
    pub client_unique_identifier: String,
}

/// `channelgroupclientlist` row — a `(cid, cldbid, cgid)` assignment.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChannelGroupClient {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub cid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub cldbid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub cgid: i64,
}

/// `servergrouppermlist` / `channelgrouppermlist` row (requested with the
/// `-permsid` flag so `permsid` carries the stable string id). Channel
/// group permissions never set `permnegated` / `permskip` — they default
/// to `0` there.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupPermEntry {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permid: i64,
    #[serde(default)]
    pub permsid: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permvalue: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permnegated: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permskip: i64,
}

/// `permissionlist` row — one entry of the read-only permission catalog
/// (spec §7.11). The companion `i_needed_*` permissions carry an empty
/// `permdesc`; the UI hides them by default (PURA-370 §1.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermissionEntry {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permid: i64,
    #[serde(default)]
    pub permname: String,
    #[serde(default)]
    pub permdesc: String,
}

/// `permfind` row — one assignment of the searched permission. `t` is the
/// assignment kind (`0` server group, `1` client, `2` channel, `3` channel
/// group); `id1` / `id2` are the kind-specific ids; `p` is the numeric
/// `permid`. Every field defaults so a fixture-specific column omission
/// degrades gracefully rather than failing the whole decode.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermFindEntry {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub t: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub id1: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub id2: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub p: i64,
}

/// `permoverview` row. `t` tags the origin (`0` server group, `1` client,
/// `2` channel) and `id1` carries the origin `sgid` / `cid` — this answers
/// the §6.2 "why does this client have this permission" SOURCE column
/// without a second lookup. `v` / `n` / `s` are value / negated / skip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermOverviewEntry {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub t: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub id1: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub id2: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub p: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub v: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub n: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub s: i64,
}

/// `permidgetbyname` response — bridges a `permsid` string to the numeric
/// `permid` for the write paths that need it (spec §7.8 client perms).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PermIdEntry {
    #[serde(default)]
    pub permsid: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub permid: i64,
}

/// `privilegekeylist` row — one TS6 privilege key (token). `token_type` is
/// `0` for a server-group token, `1` for a channel-group token (PURA-370
/// §1.3).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivilegeKeyEntry {
    #[serde(default)]
    pub token: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub token_type: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub token_id1: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub token_id2: i64,
    #[serde(default)]
    pub token_description: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub token_created: i64,
    #[serde(default)]
    pub token_customset: String,
}

/// `privilegekeyadd` response — the freshly minted key string.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PrivilegeKeyAddResponse {
    #[serde(default)]
    pub token: String,
}

/// `messagelist` row — an offline-message inbox entry. `cluid` is the
/// recipient's unique id; `flag_read` is `1` once delivered/read.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub msgid: i64,
    #[serde(default)]
    pub cluid: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub timestamp: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub flag_read: i64,
}

/// `messageget` response — a single offline message including its body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageDetail {
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub msgid: i64,
    #[serde(default)]
    pub cluid: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub timestamp: i64,
}

/// String-or-number tolerance for TS WebQuery numeric fields.
mod stringy {
    use super::*;

    /// Required field: error if missing or unparseable.
    pub fn deserialize<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        T: FromStr + serde::Deserialize<'de>,
        T::Err: fmt::Display,
        D: Deserializer<'de>,
    {
        d.deserialize_any(StringyVisitor::<T>(PhantomData))
    }

    /// Optional field: missing or unparseable → `T::default()`.
    pub fn deserialize_default<'de, T, D>(d: D) -> Result<T, D::Error>
    where
        T: FromStr + Default + serde::Deserialize<'de>,
        T::Err: fmt::Display,
        D: Deserializer<'de>,
    {
        Ok(d.deserialize_any(StringyVisitor::<T>(PhantomData))
            .unwrap_or_default())
    }

    /// Optional field that maps to `Option<T>` — distinguishes "absent" from
    /// "present but malformed" only weakly (both yield `None`); used for
    /// log-pagination metadata fields that only appear on the first row.
    pub fn deserialize_opt<'de, T, D>(d: D) -> Result<Option<T>, D::Error>
    where
        T: FromStr + serde::Deserialize<'de>,
        T::Err: fmt::Display,
        D: Deserializer<'de>,
    {
        Ok(d.deserialize_any(StringyVisitor::<T>(PhantomData)).ok())
    }

    /// Floats need their own visitor because the integer one rejects
    /// `visit_f64` for integer targets.
    pub fn deserialize_float_default<'de, D>(d: D) -> Result<f64, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(d.deserialize_any(StringyFloatVisitor).unwrap_or_default())
    }

    struct StringyVisitor<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for StringyVisitor<T>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a number or numeric string")
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<T, E> {
            v.to_string().parse().map_err(de::Error::custom)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<T, E> {
            v.to_string().parse().map_err(de::Error::custom)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<T, E> {
            v.parse().map_err(de::Error::custom)
        }
        fn visit_string<E: de::Error>(self, v: String) -> Result<T, E> {
            v.parse().map_err(de::Error::custom)
        }
    }

    struct StringyFloatVisitor;

    impl<'de> Visitor<'de> for StringyFloatVisitor {
        type Value = f64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a number or numeric string")
        }

        fn visit_f64<E: de::Error>(self, v: f64) -> Result<f64, E> {
            Ok(v)
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<f64, E> {
            Ok(v as f64)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<f64, E> {
            v.parse().map_err(de::Error::custom)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_entry_parses_stringy_id() {
        let raw = serde_json::json!({"cid": "5", "channel_name": "Lobby"});
        let parsed: ChannelEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.cid, 5);
        assert_eq!(parsed.channel_name, "Lobby");
    }

    #[test]
    fn channel_entry_parses_numeric_id() {
        let raw = serde_json::json!({"cid": 7, "channel_name": "Lobby"});
        let parsed: ChannelEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.cid, 7);
    }

    #[test]
    fn channel_entry_parses_full_flag_projection() {
        let raw = serde_json::json!({
            "cid": "12",
            "pid": "1",
            "channel_order": "3",
            "channel_name": "Lobby",
            "channel_topic": "Welcome",
            "channel_flag_default": "0",
            "channel_flag_password": "1",
            "channel_flag_permanent": "1",
            "channel_flag_semi_permanent": "0",
            "total_clients": "4",
            "total_clients_family": "10",
            "channel_maxclients": "32",
            "channel_maxfamilyclients": "-1",
            "channel_icon_id": "0",
            "seconds_empty": "0",
        });
        let parsed: ChannelEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.cid, 12);
        assert_eq!(parsed.pid, 1);
        assert_eq!(parsed.channel_topic, "Welcome");
        assert_eq!(parsed.channel_flag_password, 1);
        assert_eq!(parsed.total_clients, 4);
        assert_eq!(parsed.channel_maxfamilyclients, -1);
    }

    #[test]
    fn client_entry_defaults_missing_client_type() {
        let raw = serde_json::json!({"clid": "12"});
        let parsed: ClientEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.clid, 12);
        assert_eq!(parsed.client_type, 0);
    }

    #[test]
    fn client_entry_parses_full_flag_projection() {
        let raw = serde_json::json!({
            "clid": "10",
            "cid": "1",
            "client_database_id": "1000",
            "client_type": "0",
            "client_nickname": "Alice",
            "client_unique_identifier": "abc123=",
            "client_away": "0",
            "client_idle_time": "5000",
            "client_country": "DE",
            "connection_client_ip": "203.0.113.10",
            "client_servergroups": "8,9",
            "client_input_muted": "1",
            "client_output_muted": "0",
            "client_version": "3.5.6",
            "client_platform": "Linux",
        });
        let parsed: ClientEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.clid, 10);
        assert_eq!(parsed.client_unique_identifier, "abc123=");
        assert_eq!(parsed.client_country, "DE");
        assert_eq!(parsed.connection_client_ip, "203.0.113.10");
        assert_eq!(parsed.client_servergroups, "8,9");
        assert_eq!(parsed.client_input_muted, 1);
        assert_eq!(parsed.client_idle_time, 5000);
    }

    #[test]
    fn client_info_parses_stringy_payload() {
        let raw = serde_json::json!({
            "client_unique_identifier": "uid=",
            "client_nickname": "Alice",
            "client_database_id": "1000",
            "cid": "5",
            "client_type": "0",
            "client_idle_time": "10000",
            "client_lastconnected": "1700000000",
            "client_input_muted": "0",
            "client_output_muted": "1",
            "client_country": "FR",
        });
        let parsed: ClientInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.client_database_id, 1000);
        assert_eq!(parsed.cid, 5);
        assert_eq!(parsed.client_output_muted, 1);
    }

    #[test]
    fn client_db_entry_parses() {
        let raw = serde_json::json!({
            "cldbid": "42",
            "client_unique_identifier": "uid==",
            "client_nickname": "Bob",
            "client_created": "1690000000",
            "client_lastconnected": "1700000000",
            "client_totalconnections": "37",
            "client_description": "regular",
            "client_lastip": "10.0.0.1",
        });
        let parsed: ClientDbEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.cldbid, 42);
        assert_eq!(parsed.client_totalconnections, 37);
        assert_eq!(parsed.client_lastip, "10.0.0.1");
    }

    #[test]
    fn channel_info_parses() {
        let raw = serde_json::json!({
            "channel_name": "Default Channel",
            "channel_topic": "topic",
            "channel_codec": "4",
            "channel_codec_quality": "10",
            "channel_maxclients": "-1",
            "channel_maxfamilyclients": "-1",
            "channel_order": "0",
            "pid": "0",
            "channel_flag_permanent": "1",
            "channel_needed_talk_power": "0",
        });
        let parsed: ChannelInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.channel_name, "Default Channel");
        assert_eq!(parsed.channel_codec, 4);
        assert_eq!(parsed.channel_maxclients, -1);
        assert_eq!(parsed.channel_flag_permanent, 1);
    }

    #[test]
    fn host_info_parses() {
        let raw = serde_json::json!({
            "instance_uptime": "12345",
            "host_timestamp_utc": "1700000000",
            "virtualservers_running_total": "2",
            "virtualservers_total_clients_online": "10",
            "virtualservers_total_channels_online": "20",
            "virtualservers_total_maxclients": "64",
            "connection_bandwidth_sent_last_second_total": "1024",
            "connection_bandwidth_received_last_second_total": "2048",
        });
        let parsed: HostInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.instance_uptime, 12_345);
        assert_eq!(parsed.virtualservers_running_total, 2);
        assert_eq!(parsed.connection_bandwidth_received_last_second_total, 2048);
    }

    #[test]
    fn log_entry_parses_first_row_and_subsequent_rows() {
        let first = serde_json::json!({
            "last_pos": "1024",
            "file_size": "4096",
            "l": "2024-01-01 INFO ServerLib started",
        });
        let parsed: LogEntry = serde_json::from_value(first).unwrap();
        assert_eq!(parsed.last_pos, Some(1024));
        assert_eq!(parsed.file_size, Some(4096));
        assert!(parsed.l.starts_with("2024"));

        let next = serde_json::json!({"l": "another line"});
        let parsed: LogEntry = serde_json::from_value(next).unwrap();
        assert_eq!(parsed.last_pos, None);
        assert_eq!(parsed.file_size, None);
        assert_eq!(parsed.l, "another line");
    }

    #[test]
    fn ban_entry_parses() {
        let raw = serde_json::json!({
            "banid": "7",
            "ip": "10.0.0.5",
            "uid": "abc=",
            "mytsid": "",
            "name": "",
            "created": "1700000000",
            "duration": "0",
            "reason": "Spamming",
            "invokername": "operator",
            "invokercldbid": "1",
            "invokeruid": "op-uid=",
            "enforcements": "1",
            "lastnickname": "Spammer",
        });
        let parsed: BanEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.banid, 7);
        assert_eq!(parsed.duration, 0);
        assert_eq!(parsed.reason, "Spamming");
        assert_eq!(parsed.invokername, "operator");
    }

    #[test]
    fn ban_add_response_parses() {
        let raw = serde_json::json!({"banid": "11"});
        let parsed: BanAddResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.banid, 11);
    }

    #[test]
    fn complaint_entry_parses() {
        // TS6 delivers every field as a JSON string; the stringy
        // visitors coerce `tcldbid` / `fcldbid` / `timestamp` to `i64`.
        let raw = serde_json::json!({
            "tcldbid": "5",
            "tname": "Troublemaker",
            "fcldbid": "3",
            "fname": "Reporter",
            "message": "spamming the channel",
            "timestamp": "1700000000",
        });
        let parsed: ComplaintEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.tcldbid, 5);
        assert_eq!(parsed.fcldbid, 3);
        assert_eq!(parsed.timestamp, 1_700_000_000);
        assert_eq!(parsed.tname, "Troublemaker");
        assert_eq!(parsed.message, "spamming the channel");
    }

    #[test]
    fn channel_client_perm_parses() {
        let raw = serde_json::json!({
            "permid": "12345",
            "permsid": "i_channel_needed_modify_power",
            "permvalue": "75",
            "permnegated": "0",
            "permskip": "0",
        });
        let parsed: ChannelClientPerm = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.permid, 12_345);
        assert_eq!(parsed.permvalue, 75);
        assert_eq!(parsed.permsid, "i_channel_needed_modify_power");
    }

    #[test]
    fn server_info_parses_full_payload() {
        let raw = serde_json::json!({
            "virtualserver_name": "TS",
            "virtualserver_platform": "Linux",
            "virtualserver_version": "3.13.7",
            "virtualserver_maxclients": "32",
            "virtualserver_uptime": "12345",
            "virtualserver_total_packetloss_total": "0.0042",
            "virtualserver_total_ping": "12.5"
        });
        let info: ServerInfo = serde_json::from_value(raw).unwrap();
        assert_eq!(info.virtualserver_maxclients, 32);
        assert_eq!(info.virtualserver_uptime, 12_345);
        assert!((info.virtualserver_total_packetloss_total - 0.0042).abs() < 1e-9);
        assert!((info.virtualserver_total_ping - 12.5).abs() < 1e-9);
    }

    // ---- PURA-373 group / permission / token / message models ----

    #[test]
    fn server_group_entry_parses_stringy_payload() {
        let raw = serde_json::json!({
            "sgid": "6",
            "name": "Server Admin",
            "type": "1",
            "iconid": "300",
            "savedb": "1",
            "sortid": "0",
            "namemode": "0",
            "n_modifyp": "100",
            "n_member_addp": "100",
            "n_member_removep": "100",
        });
        let g: ServerGroupEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(g.sgid, 6);
        assert_eq!(g.name, "Server Admin");
        assert_eq!(g.r#type, 1);
        assert_eq!(g.savedb, 1);
        assert_eq!(g.n_member_addp, 100);
    }

    #[test]
    fn channel_group_entry_parses() {
        let raw = serde_json::json!({"cgid": "2", "name": "Operator", "type": "0"});
        let g: ChannelGroupEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(g.cgid, 2);
        assert_eq!(g.name, "Operator");
    }

    #[test]
    fn group_id_responses_parse() {
        let s: ServerGroupIdResponse =
            serde_json::from_value(serde_json::json!({"sgid": "13"})).unwrap();
        assert_eq!(s.sgid, 13);
        let c: ChannelGroupIdResponse =
            serde_json::from_value(serde_json::json!({"cgid": "14"})).unwrap();
        assert_eq!(c.cgid, 14);
    }

    #[test]
    fn server_group_client_parses() {
        let raw = serde_json::json!({
            "cldbid": "1000",
            "client_nickname": "Alice",
            "client_unique_identifier": "uid=",
        });
        let m: ServerGroupClient = serde_json::from_value(raw).unwrap();
        assert_eq!(m.cldbid, 1000);
        assert_eq!(m.client_nickname, "Alice");
    }

    #[test]
    fn channel_group_client_parses() {
        let raw = serde_json::json!({"cid": "5", "cldbid": "1000", "cgid": "2"});
        let m: ChannelGroupClient = serde_json::from_value(raw).unwrap();
        assert_eq!(m.cid, 5);
        assert_eq!(m.cldbid, 1000);
        assert_eq!(m.cgid, 2);
    }

    #[test]
    fn group_perm_entry_parses_with_permsid() {
        let raw = serde_json::json!({
            "permsid": "b_virtualserver_info_view",
            "permvalue": "1",
            "permnegated": "0",
            "permskip": "0",
        });
        let p: GroupPermEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(p.permsid, "b_virtualserver_info_view");
        assert_eq!(p.permvalue, 1);
        assert_eq!(p.permid, 0); // omitted under -permsid
    }

    #[test]
    fn permission_entry_parses() {
        let raw = serde_json::json!({
            "permid": "8470",
            "permname": "b_serverinstance_help_view",
            "permdesc": "Retrieve information about ServerQuery commands",
        });
        let p: PermissionEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(p.permid, 8470);
        assert_eq!(p.permname, "b_serverinstance_help_view");
        assert!(p.permdesc.starts_with("Retrieve"));
    }

    #[test]
    fn perm_find_and_overview_parse() {
        let f: PermFindEntry = serde_json::from_value(
            serde_json::json!({"t": "0", "id1": "6", "id2": "0", "p": "8470"}),
        )
        .unwrap();
        assert_eq!(f.t, 0);
        assert_eq!(f.id1, 6);
        assert_eq!(f.p, 8470);

        let o: PermOverviewEntry = serde_json::from_value(serde_json::json!({
            "t": "0", "id1": "6", "id2": "0", "p": "8470", "v": "1", "n": "0", "s": "0",
        }))
        .unwrap();
        assert_eq!(o.t, 0);
        assert_eq!(o.id1, 6);
        assert_eq!(o.v, 1);
    }

    #[test]
    fn perm_id_entry_parses() {
        let raw =
            serde_json::json!({"permsid": "i_channel_needed_modify_power", "permid": "12345"});
        let p: PermIdEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(p.permsid, "i_channel_needed_modify_power");
        assert_eq!(p.permid, 12_345);
    }

    #[test]
    fn privilege_key_entry_parses() {
        let raw = serde_json::json!({
            "token": "abcDEF123",
            "token_type": "0",
            "token_id1": "6",
            "token_id2": "0",
            "token_description": "default serveradmin",
            "token_created": "1700000000",
            "token_customset": "",
        });
        let t: PrivilegeKeyEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(t.token, "abcDEF123");
        assert_eq!(t.token_type, 0);
        assert_eq!(t.token_id1, 6);
        assert_eq!(t.token_description, "default serveradmin");
    }

    #[test]
    fn privilege_key_add_response_parses() {
        let r: PrivilegeKeyAddResponse =
            serde_json::from_value(serde_json::json!({"token": "newKEY="})).unwrap();
        assert_eq!(r.token, "newKEY=");
    }

    #[test]
    fn message_entry_and_detail_parse() {
        let e: MessageEntry = serde_json::from_value(serde_json::json!({
            "msgid": "7", "cluid": "uid=", "subject": "hi", "timestamp": "1700000000", "flag_read": "0",
        }))
        .unwrap();
        assert_eq!(e.msgid, 7);
        assert_eq!(e.subject, "hi");
        assert_eq!(e.flag_read, 0);

        let d: MessageDetail = serde_json::from_value(serde_json::json!({
            "msgid": "7", "cluid": "uid=", "subject": "hi", "message": "body text",
            "timestamp": "1700000000",
        }))
        .unwrap();
        assert_eq!(d.msgid, 7);
        assert_eq!(d.message, "body text");
    }
}
