//! Typed response shapes for the Phase 1 WebQuery surface.
//!
//! The TS6 WebQuery API returns most numeric fields as JSON *strings*
//! (e.g. `"virtualserver_maxclients": "32"`). The shapes below opt every
//! numeric field into a string-or-number deserialiser so the SPA-facing
//! handler in [`crate::rest::dashboard`] can work in native Rust types.

use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

use serde::de::{self, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

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
/// `GET /servers/:configId/virtual-servers`. Phase 1 surfaces the minimal
/// fields the dashboard / virtual-server selector need.
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

/// `channellist` row. Phase 1 only counts entries; we keep the id and name
/// so the future channels page can lift this directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub cid: i64,
    #[serde(default)]
    pub channel_name: String,
}

/// `clientlist` row. The Phase 1 dashboard uses `client_type` to exclude
/// ServerQuery slots from `onlineUsers` (spec §7.19.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientEntry {
    #[serde(deserialize_with = "stringy::deserialize")]
    pub clid: i64,
    #[serde(default, deserialize_with = "stringy::deserialize_default")]
    pub client_type: i64,
    #[serde(default)]
    pub client_nickname: String,
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
    fn client_entry_defaults_missing_client_type() {
        let raw = serde_json::json!({"clid": "12"});
        let parsed: ClientEntry = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.clid, 12);
        assert_eq!(parsed.client_type, 0);
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
}
