//! Group saved music-bot rows by the exact server address they dial.
//!
//! One summon cap belongs to that address. A server with no stored cap
//! contributes `None`, which the page renders as an empty field. A cap
//! whose address has no saved bot is left out.

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
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.server_addr == bot.server_addr)
        {
            group.bots.push(bot.clone());
            continue;
        }
        let cap = caps
            .iter()
            .find(|cap| cap.server_addr == bot.server_addr)
            .map(|cap| cap.cap);
        groups.push(SummonServerGroup {
            server_addr: bot.server_addr.clone(),
            cap,
            bots: vec![bot.clone()],
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
    fn a_cap_with_no_bot_is_omitted_and_another_server_is_not_borrowed() {
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
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].cap, None);
        assert_eq!(groups[0].bots.len(), 1);
    }

    #[test]
    fn no_bots_shows_no_group() {
        let caps = vec![wire::SummonCap {
            server_addr: "127.0.0.1:9987".into(),
            cap: 2,
        }];
        assert!(group_summon_caps(&[], &caps).is_empty());
    }
}
