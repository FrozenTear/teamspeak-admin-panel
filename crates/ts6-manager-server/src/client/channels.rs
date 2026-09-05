//! Typed REST client for channel write routes (PR #25 / spec §7.7).
//!
//! Wire bodies are camelCase and match `ts6_manager_shared::control`
//! (`ChannelCreateRequest`, `ChannelEditRequest`, `ChannelMoveRequest`,
//! `ChannelCreated`) once that crate lands those types. This module keeps
//! the same field names so Panel can ship against the drafted API without
//! waiting on the shared-crate merge.
//!
//! Routes (admin-only writes):
//! - `POST   /api/servers/{configId}/vs/{sid}/channels` → 201 `{cid}`
//! - `PUT    /api/servers/{configId}/vs/{sid}/channels/{cid}` → 204
//! - `DELETE /api/servers/{configId}/vs/{sid}/channels/{cid}?force=0|1` → 204
//! - `POST   /api/servers/{configId}/vs/{sid}/channels/{cid}/move` → 204

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::client::api::{self, ApiError};
use crate::client::session::RefreshGate;

/// `POST /api/servers/{configId}/vs/{sid}/channels` body.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelCreateRequest {
    pub channel_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_maxclients: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_maxfamilyclients: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_order: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_permanent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_semi_permanent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_temporary: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_default: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_needed_talk_power: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_icon_id: Option<i64>,
}

/// `POST …/channels` 201 body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelCreated {
    pub cid: i64,
}

/// `PUT /api/servers/{configId}/vs/{sid}/channels/{cid}` body.
/// Parent changes go through [`ChannelMoveRequest`], not this payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelEditRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_topic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_maxclients: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_maxfamilyclients: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_order: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_permanent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_semi_permanent: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_temporary: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_flag_default: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_needed_talk_power: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_icon_id: Option<i64>,
}

/// `POST …/channels/{cid}/move` body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMoveRequest {
    pub cpid: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i64>,
}

pub async fn create_channel(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    body: &ChannelCreateRequest,
) -> Result<ChannelCreated, ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/channels");
    api::authorized_post_json(&gate, &api::api_base(), &path, Some(body)).await
}

pub async fn edit_channel(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    cid: i64,
    body: &ChannelEditRequest,
) -> Result<(), ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/channels/{cid}");
    api::authorized_put_json(&gate, &api::api_base(), &path, body).await
}

pub async fn delete_channel(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    cid: i64,
    force: bool,
) -> Result<(), ApiError> {
    let force_flag = if force { 1 } else { 0 };
    let path = format!("/api/servers/{config_id}/vs/{sid}/channels/{cid}?force={force_flag}");
    api::authorized_delete(&gate, &api::api_base(), &path).await
}

pub async fn move_channel(
    gate: Arc<RefreshGate>,
    config_id: i64,
    sid: i64,
    cid: i64,
    body: &ChannelMoveRequest,
) -> Result<(), ApiError> {
    let path = format!("/api/servers/{config_id}/vs/{sid}/channels/{cid}/move");
    api::authorized_post_json::<_, ()>(&gate, &api::api_base(), &path, Some(body)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_request_emits_camel_case_and_skips_unset() {
        let body = ChannelCreateRequest {
            channel_name: "Music".into(),
            cpid: Some(1),
            channel_topic: Some("bots".into()),
            channel_flag_permanent: Some(1),
            channel_maxclients: Some(8),
            ..Default::default()
        };
        let encoded = serde_json::to_value(&body).unwrap();
        assert_eq!(encoded["channelName"], "Music");
        assert_eq!(encoded["cpid"], 1);
        assert_eq!(encoded["channelTopic"], "bots");
        assert_eq!(encoded["channelFlagPermanent"], 1);
        assert_eq!(encoded["channelMaxclients"], 8);
        assert!(encoded.get("channelPassword").is_none());
        assert!(encoded.get("channelDescription").is_none());
    }

    #[test]
    fn created_response_reads_camel_case_cid() {
        let parsed: ChannelCreated =
            serde_json::from_value(serde_json::json!({"cid": 42})).unwrap();
        assert_eq!(parsed.cid, 42);
    }

    #[test]
    fn move_request_omits_order_when_unset() {
        let body = ChannelMoveRequest {
            cpid: 0,
            order: None,
        };
        let encoded = serde_json::to_value(&body).unwrap();
        assert_eq!(encoded["cpid"], 0);
        assert!(encoded.get("order").is_none());
    }
}
