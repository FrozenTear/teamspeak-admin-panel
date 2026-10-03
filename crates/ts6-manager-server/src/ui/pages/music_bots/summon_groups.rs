//! Group saved music-bot rows by the socket they dial.
//!
//! One summon cap and one summon home channel belong to that socket. A
//! server with nothing stored contributes `None`, which the page renders
//! as an empty field. A stored cap or home with no saved bots is still
//! shown, so the page can change or clear it.

use ts6_manager_shared::music_bots as wire;

#[derive(Debug, Clone, PartialEq)]
pub struct SummonServerGroup {
    pub server_addr: String,
    pub cap: Option<u32>,
    /// The channel summon clients wait in. `None` until one is picked.
    pub home: Option<u64>,
    pub bots: Vec<wire::MusicBotSummary>,
}

pub fn group_summon_settings(
    bots: &[wire::MusicBotSummary],
    caps: &[wire::SummonCap],
    homes: &[wire::SummonHome],
) -> Vec<SummonServerGroup> {
    let mut groups: Vec<SummonServerGroup> = Vec::new();
    let group_for = |groups: &mut Vec<SummonServerGroup>, addr: &str| -> Option<usize> {
        let key = wire::canon_server_addr(addr);
        if key.is_empty() {
            return None;
        }
        if let Some(index) = groups.iter().position(|group| group.server_addr == key) {
            return Some(index);
        }
        let cap = caps
            .iter()
            .find(|cap| wire::canon_server_addr(&cap.server_addr) == key)
            .map(|cap| cap.cap);
        let home = homes
            .iter()
            .find(|home| wire::canon_server_addr(&home.server_addr) == key)
            .and_then(|home| home.channel_id);
        groups.push(SummonServerGroup {
            server_addr: key,
            cap,
            home,
            bots: Vec::new(),
        });
        Some(groups.len() - 1)
    };
    for bot in bots {
        if let Some(index) = group_for(&mut groups, &bot.server_addr) {
            groups[index].bots.push(bot.clone());
        }
    }
    for cap in caps {
        group_for(&mut groups, &cap.server_addr);
    }
    for home in homes {
        group_for(&mut groups, &home.server_addr);
    }
    groups
}

/// The empty "No music bots yet" banner. A stored cap or home with no
/// bots is a field, so that banner waits until the summon settings have
/// loaded and there are none. While they are still loading the page does
/// not flash the banner.
pub fn show_empty_bot_list(bot_count: usize, stored_count: usize, caps_known: bool) -> bool {
    bot_count == 0 && caps_known && stored_count == 0
}

/// Text put back into the summon-cap field when a save fails.
pub fn summon_cap_field_after_failed_save(stored: Option<u32>) -> String {
    stored.map(|cap| cap.to_string()).unwrap_or_default()
}

/// Value put back into the summon-home picker when a save fails. The
/// empty value is "no home picked".
pub fn summon_home_field_after_failed_save(stored: Option<u64>) -> String {
    stored.map(|home| home.to_string()).unwrap_or_default()
}

/// What the picker's value means: `Ok(None)` clears the home.
pub fn parse_summon_home_field(value: &str) -> Result<Option<u64>, ()> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<u64>() {
        Ok(0) | Err(_) => Err(()),
        Ok(home) => Ok(Some(home)),
    }
}

/// `(value, label)` for each option of the home picker, "no home" first.
/// A stored home that is not on the loaded list stays selectable by its
/// id, so the picker never shows a different channel than the one saved.
pub fn summon_home_options(
    stored: Option<u64>,
    channels: &[wire::SummonHomeChannel],
) -> Vec<(String, String)> {
    let mut options = vec![(String::new(), "No home picked".to_string())];
    for channel in channels {
        let label = if channel.is_default {
            format!("{} (default channel)", channel.path)
        } else {
            channel.path.clone()
        };
        options.push((channel.channel_id.to_string(), label));
    }
    if let Some(home) = stored
        && !channels.iter().any(|channel| channel.channel_id == home)
    {
        options.push((home.to_string(), format!("Channel {home}")));
    }
    options
}

/// The line under the home picker. `channels` is `None` until the list
/// has loaded.
pub fn summon_home_note(
    stored: Option<u64>,
    channels: Option<&[wire::SummonHomeChannel]>,
) -> Option<String> {
    let Some(home) = stored else {
        return Some(
            "No home channel is picked, so a summon client disconnects when it is done \
             instead of waiting in a public channel. Pick one, for example a bot room, and \
             summon clients wait there between summons."
                .into(),
        );
    };
    let channels = channels?;
    match channels.iter().find(|channel| channel.channel_id == home) {
        None => Some(format!(
            "Channel {home} is not on this server's channel list. A summon client that \
             cannot get into it disconnects."
        )),
        Some(channel) if channel.is_default => Some(
            "This is the server's default channel, so waiting summon clients sit where \
             everyone joins."
                .into(),
        ),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot(id: u64, server: &str) -> wire::MusicBotSummary {
        wire::MusicBotSummary {
            id: wire::BotId(id),
            name: format!("bot-{id}"),
            server_addr: server.into(),
            state: wire::BotState::Disconnected,
            now_playing: None,
            now_playing_elapsed_secs: None,
            last_error: None,
            orphaned: false,
        }
    }

    #[test]
    fn one_number_per_server_and_no_invented_default() {
        let bots = vec![
            bot(1, "127.0.0.1:9987"),
            bot(2, "127.0.0.1:9988"),
            bot(3, "127.0.0.1:9987"),
        ];
        let caps = vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 1,
        }];
        let groups = group_summon_settings(&bots, &caps, &[]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].server_addr, "127.0.0.1:9987");
        assert_eq!(groups[0].cap, Some(1));
        assert_eq!(groups[0].bots.len(), 2);
        assert_eq!(groups[1].server_addr, "127.0.0.1:9988");
        assert_eq!(groups[1].cap, None);
    }

    #[test]
    fn a_stored_cap_with_no_bot_stays_visible_and_is_not_borrowed() {
        let bots = vec![bot(1, "10.0.0.1:9987")];
        let caps = vec![
            wire::SummonCap {
                server_addr: "10.0.0.2:9987".into(),
                cap: 4,
            },
            wire::SummonCap {
                server_addr: "10.0.0.1:9988".into(),
                cap: 2,
            },
        ];
        let groups = group_summon_settings(&bots, &caps, &[]);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].server_addr, "10.0.0.1:9987");
        assert_eq!(groups[0].cap, None);
        assert_eq!(groups[0].bots.len(), 1);
        assert!(groups.iter().any(|group| {
            group.server_addr == "10.0.0.2:9987" && group.cap == Some(4) && group.bots.is_empty()
        }));
        assert!(groups.iter().any(|group| {
            group.server_addr == "10.0.0.1:9988" && group.cap == Some(2) && group.bots.is_empty()
        }));
    }

    #[test]
    fn spellings_of_one_socket_share_one_number() {
        let bots = vec![bot(1, "Voice.Example"), bot(2, "voice.example:9987")];
        let caps = vec![wire::SummonCap {
            server_addr: "voice.example:9987".into(),
            cap: 2,
        }];
        let groups = group_summon_settings(&bots, &caps, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].server_addr, "voice.example:9987");
        assert_eq!(groups[0].cap, Some(2));
        assert_eq!(groups[0].bots.len(), 2);
    }

    #[test]
    fn no_bots_and_no_caps_shows_no_group() {
        assert!(group_summon_settings(&[], &[], &[]).is_empty());
    }

    #[test]
    fn an_empty_bot_list_hides_the_banner_when_a_cap_is_stored() {
        assert!(!show_empty_bot_list(0, 1, true));
        assert!(show_empty_bot_list(0, 0, true));
        assert!(!show_empty_bot_list(0, 0, false));
        assert!(!show_empty_bot_list(2, 0, true));
    }

    #[test]
    fn a_failed_save_puts_the_stored_number_back() {
        assert_eq!(summon_cap_field_after_failed_save(Some(0)), "0");
        assert_eq!(summon_cap_field_after_failed_save(Some(4)), "4");
        assert_eq!(summon_cap_field_after_failed_save(None), "");
    }

    fn home(server: &str, channel: u64) -> wire::SummonHome {
        wire::SummonHome {
            server_addr: server.into(),
            channel_id: Some(channel),
        }
    }

    fn channel(id: u64, path: &str, is_default: bool) -> wire::SummonHomeChannel {
        wire::SummonHomeChannel {
            channel_id: id,
            path: path.into(),
            is_default,
        }
    }

    #[test]
    fn each_server_shows_its_own_home_and_none_is_invented() {
        let bots = vec![bot(1, "Voice.Example"), bot(2, "127.0.0.1:9988")];
        let caps = vec![wire::SummonCap {
            server_addr: "voice.example:9987".into(),
            cap: 1,
        }];
        let homes = vec![home("voice.example", 12), home("10.0.0.9:9987", 30)];
        let groups = group_summon_settings(&bots, &caps, &homes);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].server_addr, "voice.example:9987");
        assert_eq!(groups[0].cap, Some(1));
        assert_eq!(groups[0].home, Some(12));
        assert_eq!(groups[1].server_addr, "127.0.0.1:9988");
        assert_eq!(groups[1].home, None);
        // A home with no bot and no cap still shows, so it can be cleared.
        assert_eq!(groups[2].server_addr, "10.0.0.9:9987");
        assert_eq!(groups[2].cap, None);
        assert_eq!(groups[2].home, Some(30));
        assert!(groups[2].bots.is_empty());
    }

    #[test]
    fn the_home_picker_offers_no_home_first_and_keeps_the_saved_one() {
        let channels = vec![
            channel(1, "Lobby", true),
            channel(5, "Bots / Bot room", false),
        ];
        assert_eq!(
            summon_home_options(Some(5), &channels),
            vec![
                (String::new(), "No home picked".to_string()),
                ("1".to_string(), "Lobby (default channel)".to_string()),
                ("5".to_string(), "Bots / Bot room".to_string()),
            ]
        );
        // Saved, but not on the list (or the list did not load).
        let options = summon_home_options(Some(77), &[]);
        assert_eq!(options.len(), 2);
        assert_eq!(options[1], ("77".to_string(), "Channel 77".to_string()));
    }

    #[test]
    fn the_page_says_when_no_home_is_picked() {
        let channels = vec![
            channel(1, "Lobby", true),
            channel(5, "Bots / Bot room", false),
        ];
        let none = summon_home_note(None, Some(&channels)).expect("a note");
        assert!(none.contains("No home channel is picked"), "{none}");
        assert!(none.contains("disconnects"), "{none}");
        assert!(summon_home_note(None, None).is_some());
        assert_eq!(summon_home_note(Some(5), Some(&channels)), None);
        assert_eq!(summon_home_note(Some(5), None), None);
        let lobby = summon_home_note(Some(1), Some(&channels)).expect("a note");
        assert!(lobby.contains("default channel"), "{lobby}");
        let gone = summon_home_note(Some(9), Some(&channels)).expect("a note");
        assert!(gone.contains("Channel 9"), "{gone}");
    }

    #[test]
    fn the_home_field_reads_back_what_was_saved() {
        assert_eq!(summon_home_field_after_failed_save(None), "");
        assert_eq!(summon_home_field_after_failed_save(Some(12)), "12");
        assert_eq!(parse_summon_home_field(""), Ok(None));
        assert_eq!(parse_summon_home_field(" 12 "), Ok(Some(12)));
        assert_eq!(parse_summon_home_field("0"), Err(()));
        assert_eq!(parse_summon_home_field("lobby"), Err(()));
    }

    #[test]
    fn a_stored_home_alone_hides_the_empty_banner() {
        assert!(!show_empty_bot_list(0, 1, true));
    }

    #[test]
    fn a_stored_cap_with_no_bots_is_a_group() {
        let caps = vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 2,
        }];
        let groups = group_summon_settings(&[], &caps, &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].cap, Some(2));
        assert!(groups[0].bots.is_empty());
    }
}
