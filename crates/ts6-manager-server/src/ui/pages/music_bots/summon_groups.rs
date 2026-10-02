//! Group saved music-bot rows by the socket they dial.
//!
//! One summon cap belongs to that socket. A server with no stored cap
//! contributes `None`, which the page renders as an empty field. A stored
//! cap with no saved bots is still shown, so the page can set it to 0.

use ts6_manager_shared::music_bots as wire;

#[derive(Debug, Clone, PartialEq)]
pub struct SummonServerGroup {
    pub server_addr: String,
    pub cap: Option<u32>,
    pub bots: Vec<wire::MusicBotSummary>,
}

pub fn group_summon_caps(
    bots: &[wire::MusicBotSummary],
    caps: &[wire::SummonCap],
) -> Vec<SummonServerGroup> {
    let mut groups: Vec<SummonServerGroup> = Vec::new();
    for bot in bots {
        let key = wire::canon_server_addr(&bot.server_addr);
        if let Some(group) = groups.iter_mut().find(|group| group.server_addr == key) {
            group.bots.push(bot.clone());
            continue;
        }
        let cap = caps
            .iter()
            .find(|cap| wire::canon_server_addr(&cap.server_addr) == key)
            .map(|cap| cap.cap);
        groups.push(SummonServerGroup {
            server_addr: key,
            cap,
            bots: vec![bot.clone()],
        });
    }
    for cap in caps {
        let key = wire::canon_server_addr(&cap.server_addr);
        if key.is_empty() || groups.iter().any(|group| group.server_addr == key) {
            continue;
        }
        groups.push(SummonServerGroup {
            server_addr: key,
            cap: Some(cap.cap),
            bots: Vec::new(),
        });
    }
    groups
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
        let groups = group_summon_caps(&bots, &caps);
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
        let groups = group_summon_caps(&bots, &caps);
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
        let groups = group_summon_caps(&bots, &caps);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].server_addr, "voice.example:9987");
        assert_eq!(groups[0].cap, Some(2));
        assert_eq!(groups[0].bots.len(), 2);
    }

    #[test]
    fn no_bots_and_no_caps_shows_no_group() {
        assert!(group_summon_caps(&[], &[]).is_empty());
    }

    #[test]
    fn a_stored_cap_with_no_bots_is_a_group() {
        let caps = vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 2,
        }];
        let groups = group_summon_caps(&[], &caps);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].cap, Some(2));
        assert!(groups[0].bots.is_empty());
    }
}
