//! Mic / speaker state for a live client row.
//!
//! `client_is_talker` is the moderated-channel talk grant. TeamSpeak leaves
//! it at 0 for clients who were never granted talk power, which is almost
//! every client outside a moderated channel. It is not a microphone or
//! speaker mute, and it must not drive a "muted" tag or hide "talking".
//!
//! Software mute is `client_input_muted` / `client_output_muted` (1 = muted).
//! Hardware availability is `client_input_hardware` / `client_output_hardware`
//! (1 = device available, 0 = disabled). Those hardware fields are plain
//! `i64`s on the list DTO and default to 0 when the payload omits them, so
//! both being 0 is treated as "not reported" rather than "both devices off".
//! A one-sided 0 (the other device is 1) is a real disable.

use dioxus::prelude::*;
use ts6_manager_shared::control::ClientListItem;

/// One compact flag label, matching the existing `client-flag` chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceTag {
    pub label: &'static str,
    pub title: &'static str,
}

/// Derived voice chips for one [`ClientListItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientVoiceState {
    pub mic_muted: bool,
    pub sound_muted: bool,
    pub mic_disabled: bool,
    pub sound_disabled: bool,
}

impl ClientVoiceState {
    pub fn from_client(client: &ClientListItem) -> Self {
        let input_hw = client.client_input_hardware != 0;
        let output_hw = client.client_output_hardware != 0;
        // Both zero is also the DTO default for a field the server omitted.
        let hardware_reported = input_hw || output_hw;
        Self {
            mic_muted: client.client_input_muted != 0,
            sound_muted: client.client_output_muted != 0,
            mic_disabled: hardware_reported && !input_hw,
            sound_disabled: hardware_reported && !output_hw,
        }
    }

    /// True when a mic/speaker mute or a reported hardware disable applies.
    /// `client_is_talker` is intentionally ignored.
    pub fn is_muted(self) -> bool {
        self.mic_muted || self.sound_muted || self.mic_disabled || self.sound_disabled
    }

    /// `client_flag_talking` is the server's "sending voice" bit.
    /// A missing talker grant does not suppress it. Speaker mute does not
    /// either: output mute only stops playback. Only a muted or disabled
    /// microphone hides the talking dot.
    pub fn shows_talking(self, client_flag_talking: i64) -> bool {
        client_flag_talking != 0 && !self.mic_muted && !self.mic_disabled
    }

    pub fn tags(self) -> Vec<VoiceTag> {
        let mut tags = Vec::new();
        if self.mic_muted {
            tags.push(VoiceTag {
                label: "mic muted",
                title: "Microphone muted",
            });
        }
        if self.sound_muted {
            tags.push(VoiceTag {
                label: "sound muted",
                title: "Speakers muted",
            });
        }
        if self.mic_disabled {
            tags.push(VoiceTag {
                label: "mic off",
                title: "Capture device disabled",
            });
        }
        if self.sound_disabled {
            tags.push(VoiceTag {
                label: "sound off",
                title: "Playback device disabled",
            });
        }
        tags
    }
}

#[derive(Props, Clone, PartialEq)]
pub struct VoiceFlagTagsProps {
    pub state: ClientVoiceState,
}

/// Compact mute chips. Renders nothing when the client is not muted.
#[component]
pub fn VoiceFlagTags(props: VoiceFlagTagsProps) -> Element {
    let tags = props.state.tags();
    rsx! {
        for tag in tags {
            span { key: "{tag.label}", class: "client-flag", title: "{tag.title}", "{tag.label}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(patch: impl FnOnce(&mut ClientListItem)) -> ClientListItem {
        let mut client = ClientListItem {
            client_input_hardware: 1,
            client_output_hardware: 1,
            client_is_talker: 1,
            ..ClientListItem::default()
        };
        patch(&mut client);
        client
    }

    fn labels(state: ClientVoiceState) -> Vec<&'static str> {
        state.tags().into_iter().map(|tag| tag.label).collect()
    }

    #[test]
    fn talker_flag_alone_is_not_muted() {
        let state = ClientVoiceState::from_client(&client(|c| {
            c.client_is_talker = 0;
            c.client_input_muted = 0;
            c.client_output_muted = 0;
        }));
        assert!(!state.is_muted());
        assert!(state.tags().is_empty());
        assert!(state.shows_talking(1));
    }

    #[test]
    fn omitted_hardware_flags_are_not_treated_as_disabled() {
        // Default i64 is 0. A partial row must not look hardware-muted.
        let state = ClientVoiceState::from_client(&ClientListItem {
            client_is_talker: 0,
            ..ClientListItem::default()
        });
        assert!(!state.is_muted());
        assert!(labels(state).is_empty());
        assert!(state.shows_talking(1));
    }

    #[test]
    fn input_muted_shows_mic_muted() {
        let state = ClientVoiceState::from_client(&client(|c| {
            c.client_is_talker = 0;
            c.client_input_muted = 1;
        }));
        assert!(state.is_muted());
        assert_eq!(labels(state), ["mic muted"]);
        assert!(!state.shows_talking(1));
    }

    #[test]
    fn output_muted_shows_sound_muted() {
        let state = ClientVoiceState::from_client(&client(|c| {
            c.client_output_muted = 1;
        }));
        assert!(state.is_muted());
        assert_eq!(labels(state), ["sound muted"]);
        assert!(state.shows_talking(1));
    }

    #[test]
    fn both_software_mutes_are_distinct_tags() {
        let state = ClientVoiceState::from_client(&client(|c| {
            c.client_input_muted = 1;
            c.client_output_muted = 1;
        }));
        assert_eq!(labels(state), ["mic muted", "sound muted"]);
    }

    #[test]
    fn one_sided_hardware_disable_is_tagged() {
        let mic = ClientVoiceState::from_client(&client(|c| {
            c.client_input_hardware = 0;
            c.client_output_hardware = 1;
        }));
        assert!(mic.is_muted());
        assert_eq!(labels(mic), ["mic off"]);
        assert!(!mic.shows_talking(1));

        let sound = ClientVoiceState::from_client(&client(|c| {
            c.client_input_hardware = 1;
            c.client_output_hardware = 0;
        }));
        assert_eq!(labels(sound), ["sound off"]);
        assert!(sound.shows_talking(1));
    }

    #[test]
    fn output_mute_still_shows_talking_input_mute_does_not() {
        let speakers = ClientVoiceState::from_client(&client(|c| {
            c.client_output_muted = 1;
            c.client_flag_talking = 1;
        }));
        assert!(speakers.is_muted());
        assert!(speakers.shows_talking(1));

        let mic = ClientVoiceState::from_client(&client(|c| {
            c.client_input_muted = 1;
            c.client_flag_talking = 1;
        }));
        assert!(mic.is_muted());
        assert!(!mic.shows_talking(1));
    }

    #[test]
    fn quiet_client_with_devices_shows_no_tag() {
        let state = ClientVoiceState::from_client(&client(|_| {}));
        assert!(!state.is_muted());
        assert!(!state.shows_talking(0));
    }
}
