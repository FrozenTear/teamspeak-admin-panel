//! In-channel chat command bridge — PURA-122 WS-4.
//!
//! Two responsibilities, kept side-by-side so the parser is unit-testable
//! without dragging the bot actor in:
//!
//! 1. **Parser** ([`parse`]) — pure-functional; takes a raw chat line and
//!    returns a [`ParsedCommand`] or a [`ParseError`]. Tolerant of
//!    leading/trailing whitespace and case-insensitive on the verb. Lines
//!    that don't start with `!` (after trim) yield [`ParseError::Unknown`]
//!    so the bridge can drop them silently — chat noise must not echo back
//!    "unknown command" per the issue spec.
//!
//! 2. **Dispatcher** ([`dispatch`]) — async, takes a parsed command, the
//!    bot's connection + store + event sender, and:
//!    - Lowers the command into the existing `BotCommand` / store paths
//!      (so audio commands ride the same `AudioNotImplemented` stub the
//!      REST surface will get in WS-5; once WS-2 wires the audio task,
//!      this surface lights up automatically).
//!    - Sends a single short channel-chat reply so the operator sees the
//!      command was acknowledged.
//!
//! Permission model is intentionally permissive — anyone in the channel
//! can drive the bot. `docs/voice/chat-commands.md` flags the follow-up.
//!
//! Cleanroom rule: this module is derived from the `tsclientlib` upstream
//! `MessageTarget` / `events::Event::Message` API, the `BotCommand` /
//! `MusicBotStore` surface in this crate, and the issue spec. No
//! reference-impl reads.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::{debug, warn};
use tsclientlib::prelude::*;
use tsclientlib::{Connection, MessageTarget};

use crate::command::AudioSource;
use crate::config::BotId;
use crate::event::BotEvent;
use crate::store::{MusicBotStore, NewTrack};

/// One parsed chat command. The dispatcher decides what to do with it.
#[derive(Debug, Clone, PartialEq)]
pub enum ParsedCommand {
    /// `!radio <name|url>` — replace queue with the radio source and
    /// auto-play. `arg` is either a URL (`http(s)://…`) or the title of
    /// a library entry to look up.
    Radio { arg: String },
    /// `!play <url|search>` — enqueue the source and start if idle.
    /// `arg` is either a URL or a library lookup (search → library lookup
    /// for now; full search lands with WS-2's source pipeline).
    Play { arg: String },
    /// `!stop` — stop playback and drain the queue.
    Stop,
    /// `!pause` — pause the current track.
    Pause,
    /// `!skip` / `!next` — drop the current track, advance the queue.
    Skip,
    /// `!prev` — replay the previous track if available.
    Prev,
    /// `!vol <0..100>` — set per-bot volume.
    Volume(u8),
    /// `!np` — reply with the current now-playing line.
    NowPlaying,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ParseError {
    /// Line was just whitespace, just `!`, or otherwise empty after the
    /// command prefix. The bridge silently ignores these — empty input is
    /// not chat noise the operator wants echoed back.
    #[error("empty command")]
    Empty,
    /// Line did not start with `!`, or the verb is not one we recognise.
    /// The bridge silently ignores these too — non-commands and typos
    /// shouldn't spam the channel.
    #[error("unknown command")]
    Unknown,
    /// A required argument was missing (e.g. `!play` with no URL).
    #[error("`!{verb}` requires an argument")]
    MissingArg { verb: &'static str },
    /// `!vol` got a non-numeric or out-of-range argument.
    #[error("`!vol` argument must be 0..=100, got `{got}`")]
    BadVolume { got: String },
}

/// Parse a single line of channel chat into a [`ParsedCommand`].
///
/// Whitespace before `!` is tolerated (some clients prefix), as is trailing
/// whitespace. Verbs are matched case-insensitively.
pub fn parse(line: &str) -> Result<ParsedCommand, ParseError> {
    let trimmed = line.trim();
    let body = trimmed.strip_prefix('!').ok_or(ParseError::Unknown)?;
    if body.is_empty() {
        return Err(ParseError::Empty);
    }
    // Split off the first whitespace-delimited token (the verb); the rest
    // (also trimmed) is the single-blob argument. We don't tokenise the
    // arg further: `!play song name with spaces` keeps its spaces, which
    // matters for library-lookup-by-title.
    let (verb, rest) = match body.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (body, ""),
    };
    let verb_lc = verb.to_ascii_lowercase();
    match verb_lc.as_str() {
        "radio" => {
            if rest.is_empty() {
                Err(ParseError::MissingArg { verb: "radio" })
            } else {
                Ok(ParsedCommand::Radio { arg: rest.to_string() })
            }
        }
        "play" => {
            if rest.is_empty() {
                Err(ParseError::MissingArg { verb: "play" })
            } else {
                Ok(ParsedCommand::Play { arg: rest.to_string() })
            }
        }
        "stop" => Ok(ParsedCommand::Stop),
        "pause" => Ok(ParsedCommand::Pause),
        "skip" | "next" => Ok(ParsedCommand::Skip),
        "prev" => Ok(ParsedCommand::Prev),
        "vol" => {
            if rest.is_empty() {
                return Err(ParseError::MissingArg { verb: "vol" });
            }
            let parsed: Option<u8> = rest.parse::<u32>().ok().and_then(|n| {
                if n <= 100 {
                    Some(n as u8)
                } else {
                    None
                }
            });
            match parsed {
                Some(v) => Ok(ParsedCommand::Volume(v)),
                None => Err(ParseError::BadVolume { got: rest.to_string() }),
            }
        }
        "np" => Ok(ParsedCommand::NowPlaying),
        _ => Err(ParseError::Unknown),
    }
}

/// Dispatch a parsed command against the bot. Lowers into store mutations
/// (queue) and `BotEvent::Error(AudioNotImplemented)` (audio surface), and
/// returns a single short reply line for the channel.
///
/// `con` is borrowed mutably so the reply can ride the same connection
/// without a separate channel.
pub async fn dispatch(
    bot_id: BotId,
    con: &mut Connection,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    cmd: ParsedCommand,
) {
    let reply = match cmd {
        ParsedCommand::Radio { arg } => handle_radio(bot_id, store, events, arg).await,
        ParsedCommand::Play { arg } => handle_play(bot_id, store, events, arg).await,
        ParsedCommand::Stop => handle_stop(bot_id, store, events).await,
        ParsedCommand::Pause => {
            emit_audio_stub_event(events, "Pause");
            "pause".to_string()
        }
        ParsedCommand::Skip => handle_skip(bot_id, store, events).await,
        ParsedCommand::Prev => {
            // No queue history yet (PURA-121 ships forward-only). Lower
            // into the audio stub so WS-2 can wire it up later; today the
            // chat reply is honest about the limitation.
            emit_audio_stub_event(events, "SkipPrev");
            "previous track not yet supported (queue history lands with WS-2)".to_string()
        }
        ParsedCommand::Volume(v) => {
            // Audio pipeline isn't wired yet — emit the stub so REST/UI
            // subscribers see the dispatched intent. Reply optimistically
            // so the operator knows the value parsed.
            emit_audio_stub_event(events, &format!("SetVolume({})", v));
            format!("volume {}", v)
        }
        ParsedCommand::NowPlaying => handle_np(bot_id, store).await,
    };
    send_reply(con, &reply);
}

/// Emit a single short chat reply for a parse error. The bridge calls
/// this only for "user-visible" errors (`MissingArg`, `BadVolume`);
/// `Empty` and `Unknown` are silent per the issue spec.
pub fn reply_for_parse_error(con: &mut Connection, err: &ParseError) {
    match err {
        ParseError::Empty | ParseError::Unknown => {
            // Silent — these are chat noise / typos.
        }
        ParseError::MissingArg { .. } | ParseError::BadVolume { .. } => {
            send_reply(con, &err.to_string());
        }
    }
}

async fn handle_radio(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    arg: String,
) -> String {
    let (track, label) = match resolve_source(bot_id, store, &arg).await {
        Ok(pair) => pair,
        Err(reply) => return reply,
    };
    // !radio replaces the queue: clear then enqueue.
    if let Err(err) = store.queue_clear(bot_id).await {
        warn!(?err, "queue_clear during !radio");
        return format!("radio failed: {err}");
    }
    let _ = events.send(BotEvent::QueueChanged { len: 0, current: None });
    let _ = events.send(BotEvent::QueueEmpty);
    match store.queue_enqueue(bot_id, track).await {
        Ok(stored) => {
            let _ = events.send(BotEvent::QueueChanged {
                len: 1,
                current: Some(stored.clone()),
            });
            let _ = events.send(BotEvent::NowPlaying(stored));
            format!("radio: {}", label)
        }
        Err(err) => {
            warn!(?err, "queue_enqueue during !radio");
            format!("radio failed: {err}")
        }
    }
}

async fn handle_play(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    arg: String,
) -> String {
    let (track, label) = match resolve_source(bot_id, store, &arg).await {
        Ok(pair) => pair,
        Err(reply) => return reply,
    };
    let was_empty = store
        .queue_peek(bot_id)
        .await
        .map(|q| q.is_empty())
        .unwrap_or(false);
    match store.queue_enqueue(bot_id, track).await {
        Ok(stored) => {
            let queue = store.queue_peek(bot_id).await.unwrap_or_default();
            let _ = events.send(BotEvent::QueueChanged {
                len: queue.len(),
                current: queue.first().cloned(),
            });
            if was_empty {
                let _ = events.send(BotEvent::NowPlaying(stored));
                format!("playing: {}", label)
            } else {
                format!("queued: {} (#{})", label, queue.len())
            }
        }
        Err(err) => {
            warn!(?err, "queue_enqueue during !play");
            format!("play failed: {err}")
        }
    }
}

async fn handle_stop(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
) -> String {
    // Lower into the audio stub so REST/UI subscribers see the dispatched
    // intent; clear the queue so the state is honest about "stopped".
    emit_audio_stub_event(events, "Stop");
    let was_non_empty = store
        .queue_peek(bot_id)
        .await
        .map(|q| !q.is_empty())
        .unwrap_or(false);
    match store.queue_clear(bot_id).await {
        Ok(()) => {
            let _ = events.send(BotEvent::QueueChanged { len: 0, current: None });
            if was_non_empty {
                let _ = events.send(BotEvent::QueueEmpty);
            }
            "stopped".to_string()
        }
        Err(err) => {
            warn!(?err, "queue_clear during !stop");
            format!("stop failed: {err}")
        }
    }
}

async fn handle_skip(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
) -> String {
    match store.queue_dequeue_head(bot_id).await {
        Ok(Some(_popped)) => {
            let queue = store.queue_peek(bot_id).await.unwrap_or_default();
            let head = queue.first().cloned();
            let _ = events.send(BotEvent::QueueChanged {
                len: queue.len(),
                current: head.clone(),
            });
            match head {
                Some(track) => {
                    let title = track.title.clone();
                    let _ = events.send(BotEvent::NowPlaying(track));
                    format!("skipped → {}", title)
                }
                None => {
                    let _ = events.send(BotEvent::QueueEmpty);
                    "skipped → queue empty".to_string()
                }
            }
        }
        Ok(None) => "queue is empty".to_string(),
        Err(err) => {
            warn!(?err, "queue_dequeue_head during !skip");
            format!("skip failed: {err}")
        }
    }
}

async fn handle_np(bot_id: BotId, store: &Arc<dyn MusicBotStore>) -> String {
    match store.queue_current(bot_id).await {
        Ok(Some(track)) => format!("now playing: {}", track.title),
        Ok(None) => "queue is empty".to_string(),
        Err(err) => {
            warn!(?err, "queue_current during !np");
            format!("now playing: error: {err}")
        }
    }
}

/// Turn an arg into a `NewTrack` — URL passes through, anything else is
/// looked up against the bot's library by title (case-insensitive).
async fn resolve_source(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    arg: &str,
) -> Result<(NewTrack, String), String> {
    if is_url(arg) {
        Ok((
            NewTrack {
                source: AudioSource::Url(arg.to_string()),
                title: arg.to_string(),
                duration_secs: None,
                requested_by: None,
            },
            arg.to_string(),
        ))
    } else {
        match store.library_list(bot_id, None).await {
            Ok(entries) => {
                let lc = arg.to_ascii_lowercase();
                let hit = entries.into_iter().find(|e| e.title.to_ascii_lowercase() == lc);
                match hit {
                    Some(entry) => {
                        let source = match &entry.source {
                            AudioSource::Url(u) => AudioSource::Url(u.clone()),
                            AudioSource::LibraryPath(p) => {
                                AudioSource::LibraryPath(PathBuf::from(p))
                            }
                        };
                        Ok((
                            NewTrack {
                                source,
                                title: entry.title.clone(),
                                duration_secs: None,
                                requested_by: None,
                            },
                            entry.title,
                        ))
                    }
                    None => Err(format!("no library entry titled `{}`", arg)),
                }
            }
            Err(err) => {
                warn!(?err, "library_list during chat lookup");
                Err(format!("library lookup failed: {err}"))
            }
        }
    }
}

fn is_url(s: &str) -> bool {
    let lc = s.to_ascii_lowercase();
    lc.starts_with("http://") || lc.starts_with("https://")
}

/// Best-effort send a chat reply to the bot's current channel. We don't
/// surface the failure to the caller — chat replies are advisory, and a
/// transient send error is logged at `warn!` only.
fn send_reply(con: &mut Connection, line: &str) {
    let cmd = match con.get_state() {
        Ok(book) => book.send_message(MessageTarget::Channel, line),
        Err(err) => {
            warn!(?err, "no book available for chat reply");
            return;
        }
    };
    if let Err(err) = cmd.send(con) {
        warn!(?err, line, "chat reply send failed");
    } else {
        debug!(line, "chat reply sent");
    }
}

/// Emit `BotEvent::Error(AudioNotImplemented(label))` for an audio
/// command we have to stub today. Mirrors the existing `bot.rs`
/// `emit_audio_stub` so REST/UI subscribers see the same surface whether
/// the command was dispatched from chat or from the supervisor.
fn emit_audio_stub_event(events: &broadcast::Sender<BotEvent>, label: &str) {
    let _ = events.send(BotEvent::Error(crate::event::BotError::AudioNotImplemented(
        label.to_string(),
    )));
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn happy_path_no_args() {
        assert_eq!(parse("!stop"), Ok(ParsedCommand::Stop));
        assert_eq!(parse("!pause"), Ok(ParsedCommand::Pause));
        assert_eq!(parse("!skip"), Ok(ParsedCommand::Skip));
        assert_eq!(parse("!next"), Ok(ParsedCommand::Skip));
        assert_eq!(parse("!prev"), Ok(ParsedCommand::Prev));
        assert_eq!(parse("!np"), Ok(ParsedCommand::NowPlaying));
    }

    #[test]
    fn radio_play_take_arg() {
        assert_eq!(
            parse("!radio https://r.example/lofi.mp3"),
            Ok(ParsedCommand::Radio {
                arg: "https://r.example/lofi.mp3".into()
            })
        );
        assert_eq!(
            parse("!play song with spaces"),
            Ok(ParsedCommand::Play {
                arg: "song with spaces".into()
            })
        );
    }

    #[test]
    fn radio_play_missing_arg() {
        assert_eq!(parse("!radio"), Err(ParseError::MissingArg { verb: "radio" }));
        assert_eq!(parse("!play"), Err(ParseError::MissingArg { verb: "play" }));
        // Bare verb with trailing spaces is also a missing-arg.
        assert_eq!(parse("!radio   "), Err(ParseError::MissingArg { verb: "radio" }));
    }

    #[test]
    fn vol_range() {
        assert_eq!(parse("!vol 0"), Ok(ParsedCommand::Volume(0)));
        assert_eq!(parse("!vol 50"), Ok(ParsedCommand::Volume(50)));
        assert_eq!(parse("!vol 100"), Ok(ParsedCommand::Volume(100)));
        assert_eq!(parse("!vol 101"), Err(ParseError::BadVolume { got: "101".into() }));
        assert_eq!(parse("!vol abc"), Err(ParseError::BadVolume { got: "abc".into() }));
        assert_eq!(parse("!vol -5"), Err(ParseError::BadVolume { got: "-5".into() }));
        assert_eq!(parse("!vol"), Err(ParseError::MissingArg { verb: "vol" }));
    }

    #[test]
    fn whitespace_and_case_tolerance() {
        assert_eq!(parse("   !stop   "), Ok(ParsedCommand::Stop));
        assert_eq!(parse("!STOP"), Ok(ParsedCommand::Stop));
        assert_eq!(parse("!Stop"), Ok(ParsedCommand::Stop));
        assert_eq!(
            parse("!Play   foo   "),
            Ok(ParsedCommand::Play { arg: "foo".into() })
        );
    }

    #[test]
    fn non_command_lines_are_unknown() {
        assert_eq!(parse(""), Err(ParseError::Unknown));
        assert_eq!(parse("hello world"), Err(ParseError::Unknown));
        // No prefix, just the verb.
        assert_eq!(parse("stop"), Err(ParseError::Unknown));
    }

    #[test]
    fn empty_after_prefix_is_empty() {
        assert_eq!(parse("!"), Err(ParseError::Empty));
        assert_eq!(parse("   !   "), Err(ParseError::Empty));
    }

    #[test]
    fn unknown_verb_is_unknown() {
        assert_eq!(parse("!foo"), Err(ParseError::Unknown));
        assert_eq!(parse("!playlist add"), Err(ParseError::Unknown));
    }
}
