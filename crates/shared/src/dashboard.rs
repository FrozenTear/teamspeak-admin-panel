//! Wire-format type for `GET /api/servers/:configId/vs/:sid/dashboard`
//! (spec §7.19.1).
//!
//! The wire keys are camelCase per Chapter 7. Numeric fields stay numeric on
//! the wire; the WebQuery upstream returns most numbers as strings, but the
//! axum handler parses them into Rust integers before the response is shaped
//! so the SPA receives a typed JSON document.

use serde::{Deserialize, Serialize};

/// Spec §7.19.1 — flattened snapshot the dashboard view consumes.
///
/// `onlineUsers` excludes ServerQuery slots (`client_type == 1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardData {
    pub server_name: String,
    pub platform: String,
    pub version: String,
    pub online_users: u32,
    pub max_clients: u32,
    pub uptime: u64,
    pub channel_count: u32,
    pub bandwidth: BandwidthSnapshot,
    pub packetloss: f64,
    pub ping: f64,
}

/// Bytes/sec (last second total) reported by `serverrequestconnectioninfo`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BandwidthSnapshot {
    pub incoming: u64,
    pub outgoing: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin the wire shape — DioxusLead's PURA-5 dashboard parser depends on
    /// these keys verbatim.
    #[test]
    fn dashboard_data_serialises_with_camel_case_keys() {
        let dd = DashboardData {
            server_name: "TeamSpeak".into(),
            platform: "Linux".into(),
            version: "3.13.7".into(),
            online_users: 4,
            max_clients: 32,
            uptime: 12_345,
            channel_count: 9,
            bandwidth: BandwidthSnapshot {
                incoming: 100,
                outgoing: 200,
            },
            packetloss: 0.0,
            ping: 12.5,
        };
        let json: serde_json::Value = serde_json::to_value(&dd).unwrap();
        assert!(json.get("serverName").is_some());
        assert!(json.get("onlineUsers").is_some());
        assert!(json.get("maxClients").is_some());
        assert!(json.get("channelCount").is_some());
        assert_eq!(
            json.get("bandwidth").unwrap().get("incoming").unwrap(),
            &serde_json::json!(100)
        );
    }
}
