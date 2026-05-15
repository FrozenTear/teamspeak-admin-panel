//! Flow-action command whitelist — `docs/flows/http-api.md` §3.1.
//!
//! Two surfaces consult this module:
//!
//! - [`crate::flow::routes::validate_definition`] — the create / patch-time
//!   gate. A flow whose definition names an unknown command is rejected
//!   with `400 validation` instead of failing only at run time.
//! - [`crate::flow::dispatch::ProductionDispatcher`] — re-checks at run
//!   time before lowering a command onto a typed backend call (a stored
//!   flow row can predate a whitelist change).
//!
//! ## `ts6Command`
//!
//! v1.1 lowers every whitelisted TS6 command onto a typed
//! [`crate::control::ControlBackend`] method. That trait does not expose
//! a raw ServerQuery passthrough, so the whitelist is exactly the set of
//! mutating commands the trait already models: `clientmove`,
//! `clientkick`, `clientmute`, `clientunmute`, `sendtextmessage`, and
//! `servergroupaddclient`.
//!
//! PURA-250 added the typed `sendtextmessage` / `servergroupaddclient`
//! `ControlBackend` methods, so the `welcome-on-join` example from the
//! HTTP API spec (`docs/flows/http-api.md` §3.1) and the auto-group
//! example from the architecture brief (`docs/flows/architecture.md` §4)
//! both validate and dispatch end to end.
//!
//! ## `musicBotCommand`
//!
//! Each whitelisted music-bot command lowers to a `music_bot::BotCommand`
//! dispatched at the bot actor via the supervisor.

use serde_json::{Map, Value as JsonValue};

/// One whitelisted command: its wire name plus the argument keys that
/// must be present in the action's `args` object.
///
/// Only key *presence* is checked at create / patch time — argument
/// *values* may be `${trigger.*}` templates that do not resolve until a
/// run fires, so type-checking is the dispatcher's job.
struct CommandSpec {
    name: &'static str,
    required_args: &'static [&'static str],
}

/// `ts6Command` whitelist. Each entry lowers to a typed `ControlBackend`
/// call in [`crate::flow::dispatch`].
const TS6_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "clientmove",
        required_args: &["clid", "cid"],
    },
    CommandSpec {
        name: "clientkick",
        required_args: &["clid"],
    },
    CommandSpec {
        name: "clientmute",
        required_args: &["clid"],
    },
    CommandSpec {
        name: "clientunmute",
        required_args: &["clid"],
    },
    CommandSpec {
        name: "sendtextmessage",
        required_args: &["targetmode", "target", "msg"],
    },
    CommandSpec {
        name: "servergroupaddclient",
        required_args: &["sgid", "cldbid"],
    },
];

/// `musicBotCommand` whitelist. Each entry lowers to a
/// `music_bot::BotCommand` in [`crate::flow::dispatch`].
const MUSIC_BOT_COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "connect",
        required_args: &[],
    },
    CommandSpec {
        name: "disconnect",
        required_args: &[],
    },
    CommandSpec {
        name: "joinChannel",
        required_args: &["channelId"],
    },
    CommandSpec {
        name: "leaveChannel",
        required_args: &[],
    },
    CommandSpec {
        name: "pause",
        required_args: &[],
    },
    CommandSpec {
        name: "resume",
        required_args: &[],
    },
    CommandSpec {
        name: "stop",
        required_args: &[],
    },
    CommandSpec {
        name: "skipNext",
        required_args: &[],
    },
    CommandSpec {
        name: "skipPrev",
        required_args: &[],
    },
    CommandSpec {
        name: "enqueue",
        required_args: &["title", "url"],
    },
    CommandSpec {
        name: "clearQueue",
        required_args: &[],
    },
];

/// Comma-joined command names, for the human half of a validation error.
fn names(specs: &[CommandSpec]) -> String {
    specs.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
}

/// Shared check: the command is whitelisted and every required argument
/// key is present in `args`. Returns a human-readable message on failure
/// (the caller wraps it into the relevant error envelope).
fn validate(
    kind: &str,
    specs: &[CommandSpec],
    command: &str,
    args: &Map<String, JsonValue>,
) -> Result<(), String> {
    let Some(spec) = specs.iter().find(|s| s.name == command) else {
        return Err(format!(
            "{kind} `{command}` is not whitelisted (allowed: {})",
            names(specs)
        ));
    };
    for key in spec.required_args {
        if !args.contains_key(*key) {
            return Err(format!("{kind} `{command}` requires the `{key}` argument"));
        }
    }
    Ok(())
}

/// Validate a `ts6Command` action at create / patch time.
pub fn validate_ts6_command(command: &str, args: &Map<String, JsonValue>) -> Result<(), String> {
    validate("ts6Command", TS6_COMMANDS, command, args)
}

/// Validate a `musicBotCommand` action at create / patch time.
pub fn validate_music_bot_command(
    command: &str,
    args: &Map<String, JsonValue>,
) -> Result<(), String> {
    validate("musicBotCommand", MUSIC_BOT_COMMANDS, command, args)
}

/// Whether `command` is a recognised `ts6Command`. The dispatcher uses
/// this as a run-time re-check before lowering onto a backend call.
pub fn is_ts6_command(command: &str) -> bool {
    TS6_COMMANDS.iter().any(|s| s.name == command)
}

/// Whether `command` is a recognised `musicBotCommand`.
pub fn is_music_bot_command(command: &str) -> bool {
    MUSIC_BOT_COMMANDS.iter().any(|s| s.name == command)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(v: JsonValue) -> Map<String, JsonValue> {
        v.as_object().cloned().unwrap_or_default()
    }

    #[test]
    fn ts6_unknown_command_is_rejected() {
        // `clientpoke` is a real ServerQuery command but is deliberately
        // not whitelisted — no typed `ControlBackend` method backs it.
        let err = validate_ts6_command("clientpoke", &args(json!({}))).unwrap_err();
        assert!(err.contains("not whitelisted"), "got: {err}");
        // The empty string is just another non-whitelisted command.
        assert!(validate_ts6_command("", &args(json!({}))).is_err());
    }

    #[test]
    fn ts6_sendtextmessage_requires_targetmode_target_and_msg() {
        // PURA-250 — `sendtextmessage` is now whitelisted; the create-time
        // gate checks key presence for all three required args.
        assert!(validate_ts6_command("sendtextmessage", &args(json!({}))).is_err());
        assert!(
            validate_ts6_command(
                "sendtextmessage",
                &args(json!({"targetmode": 2, "target": 5}))
            )
            .is_err()
        );
        validate_ts6_command(
            "sendtextmessage",
            &args(json!({
                "targetmode": 2,
                "target": "${trigger.channelId}",
                "msg": "Welcome ${trigger.clientNickname}.",
            })),
        )
        .expect("targetmode + target + msg present");
    }

    #[test]
    fn ts6_servergroupaddclient_requires_sgid_and_cldbid() {
        assert!(validate_ts6_command("servergroupaddclient", &args(json!({"sgid": 6}))).is_err());
        validate_ts6_command(
            "servergroupaddclient",
            &args(json!({"sgid": 6, "cldbid": 12})),
        )
        .expect("sgid + cldbid present");
    }

    #[test]
    fn ts6_clientmove_requires_clid_and_cid() {
        assert!(validate_ts6_command("clientmove", &args(json!({"clid": 1}))).is_err());
        validate_ts6_command("clientmove", &args(json!({"clid": 1, "cid": 5})))
            .expect("clid + cid present");
        // Templated values count as present — only the key is checked here.
        validate_ts6_command(
            "clientmove",
            &args(json!({"clid": "${trigger.clientId}", "cid": "${trigger.channelId}"})),
        )
        .expect("template placeholders satisfy the presence check");
    }

    #[test]
    fn music_bot_lifecycle_commands_need_no_args() {
        for cmd in [
            "connect",
            "disconnect",
            "leaveChannel",
            "pause",
            "clearQueue",
        ] {
            validate_music_bot_command(cmd, &args(json!({}))).unwrap_or_else(|e| {
                panic!("`{cmd}` should validate with no args: {e}");
            });
        }
    }

    #[test]
    fn music_bot_enqueue_requires_title_and_url() {
        assert!(validate_music_bot_command("enqueue", &args(json!({"title": "x"}))).is_err());
        validate_music_bot_command(
            "enqueue",
            &args(json!({"title": "Lo-fi", "url": "https://example.com/a.mp3"})),
        )
        .expect("title + url present");
    }

    #[test]
    fn join_channel_requires_channel_id() {
        assert!(validate_music_bot_command("joinChannel", &args(json!({}))).is_err());
        validate_music_bot_command("joinChannel", &args(json!({"channelId": 7}))).unwrap();
    }

    #[test]
    fn predicate_helpers_track_the_whitelist() {
        assert!(is_ts6_command("clientkick"));
        assert!(!is_ts6_command("clientpoke"));
        assert!(is_music_bot_command("skipNext"));
        assert!(!is_music_bot_command("teleport"));
    }
}
