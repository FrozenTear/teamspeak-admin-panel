//! Compact talk-power hints for the Clients and Channels pages.
//!
//! A channel is moderated when `channel_needed_talk_power > 0`.
//! `channel_forced_silence != 0` silences everyone in that channel,
//! independent of talk power. `client_talk_power` is the client's
//! granted power. The talker flag (`client_is_talker`) stays on the
//! existing Grant / Revoke controls; these hints only explain the
//! numbers next to them.

use ts6_manager_shared::control::{ChannelTreeNode, ClientListItem};

/// One short chip. `tag_class` is for channel meta (`tag tag-*`).
/// Client rows render the same text as a `client-flag`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkHint {
    pub label: String,
    pub title: String,
    pub tag_class: &'static str,
}

/// Channel-row chips. Empty for an ordinary, unsilenced channel.
pub fn channel_talk_hints(needed_talk_power: i64, forced_silence: i64) -> Vec<TalkHint> {
    let mut hints = Vec::new();
    if needed_talk_power > 0 {
        hints.push(TalkHint {
            label: "Moderated".into(),
            title: format!(
                "Moderated channel. Speaking requires talk power {needed_talk_power}, or the talker flag."
            ),
            tag_class: "tag tag-info",
        });
    }
    if forced_silence != 0 {
        hints.push(TalkHint {
            label: "Silenced".into(),
            title: "This channel is silenced for everyone, independent of talk power.".into(),
            tag_class: "tag tag-warning",
        });
    }
    hints
}

/// Client chips for a channel the row already resolved.
///
/// `None` channel means the channel list has not loaded — skip the
/// hint rather than guessing the channel is unmoderated.
pub fn client_talk_hints(
    client_talk_power: i64,
    needed_talk_power: Option<i64>,
    forced_silence: Option<i64>,
) -> Vec<TalkHint> {
    let mut hints = Vec::new();
    if forced_silence.is_some_and(|value| value != 0) {
        hints.push(TalkHint {
            label: "silenced".into(),
            title: "This channel is silenced for everyone.".into(),
            tag_class: "tag tag-warning",
        });
    }
    if let Some(needed) = needed_talk_power.filter(|needed| *needed > 0) {
        hints.push(TalkHint {
            label: format!("{client_talk_power}/{needed}"),
            title: format!(
                "Talk power {client_talk_power}. This moderated channel needs {needed}."
            ),
            tag_class: "tag tag-info",
        });
    }
    hints
}

pub fn hints_for_client(
    client: &ClientListItem,
    channel: Option<&ChannelTreeNode>,
) -> Vec<TalkHint> {
    client_talk_hints(
        client.client_talk_power,
        channel.map(|channel| channel.channel_needed_talk_power),
        channel.map(|channel| channel.channel_forced_silence),
    )
}

/// Same predicate as the Clients page "talker" chip: the grant is the
/// state worth showing. `0` is not a microphone mute.
pub fn show_talker_chip(client_is_talker: i64) -> bool {
    client_is_talker != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_channel_has_no_hint() {
        assert!(channel_talk_hints(0, 0).is_empty());
        assert!(client_talk_hints(0, Some(0), Some(0)).is_empty());
        assert!(client_talk_hints(75, None, None).is_empty());
    }

    #[test]
    fn moderated_channel_shows_threshold_and_client_fraction() {
        let channel = channel_talk_hints(50, 0);
        assert_eq!(channel.len(), 1);
        assert_eq!(channel[0].label, "Moderated");
        assert!(channel[0].title.contains("50"));
        assert_eq!(channel[0].tag_class, "tag tag-info");

        let client = client_talk_hints(10, Some(50), Some(0));
        assert_eq!(client.len(), 1);
        assert_eq!(client[0].label, "10/50");
        assert!(client[0].title.contains("needs 50"));
    }

    #[test]
    fn forced_silence_is_its_own_hint() {
        let channel = channel_talk_hints(0, 1);
        assert_eq!(channel[0].label, "Silenced");
        assert_eq!(channel[0].tag_class, "tag tag-warning");

        let both = channel_talk_hints(20, 1);
        assert_eq!(
            both.iter()
                .map(|hint| hint.label.as_str())
                .collect::<Vec<_>>(),
            ["Moderated", "Silenced"]
        );
        let client = client_talk_hints(20, Some(20), Some(1));
        assert_eq!(
            client
                .iter()
                .map(|hint| hint.label.as_str())
                .collect::<Vec<_>>(),
            ["silenced", "20/20"]
        );
    }

    #[test]
    fn omitted_wire_fields_default_without_failing_the_list_decode() {
        let client: ClientListItem = serde_json::from_value(serde_json::json!({
            "clid": 1,
            "cid": 2,
            "client_database_id": 3,
            "client_type": 0,
            "client_nickname": "a",
            "client_unique_identifier": "uid",
            "client_away": 0,
            "client_away_message": "",
            "client_flag_talking": 0,
            "client_input_muted": 0,
            "client_output_muted": 0,
            "client_input_hardware": 1,
            "client_output_hardware": 1,
            "client_idle_time": 0,
            "client_lastconnected": 0,
            "client_created": 0,
            "client_servergroups": "",
            "client_channel_group_id": 0,
            "client_version": "",
            "client_platform": "",
            "client_country": ""
        }))
        .unwrap();
        assert_eq!(client.client_talk_power, 0);
        assert_eq!(client.client_is_talker, 1);
        assert!(hints_for_client(&client, None).is_empty());

        let channel: ChannelTreeNode = serde_json::from_value(serde_json::json!({
            "cid": 2,
            "pid": 0,
            "channel_name": "Lobby",
            "channel_order": 0,
            "channel_topic": "",
            "channel_flag_default": 0,
            "channel_flag_password": 0,
            "channel_flag_permanent": 1,
            "channel_flag_semi_permanent": 0,
            "channel_maxclients": 0,
            "channel_maxfamilyclients": 0,
            "total_clients": 0,
            "total_clients_family": 0,
            "channel_icon_id": 0,
            "seconds_empty": 0,
            "channel_needed_subscribe_power": 0
        }))
        .unwrap();
        assert_eq!(channel.channel_needed_talk_power, 0);
        assert_eq!(channel.channel_forced_silence, 0);
        assert!(
            channel_talk_hints(
                channel.channel_needed_talk_power,
                channel.channel_forced_silence
            )
            .is_empty()
        );
    }

    #[test]
    fn talker_chip_follows_the_grant_bit() {
        assert!(show_talker_chip(1));
        assert!(!show_talker_chip(0));
    }
}
