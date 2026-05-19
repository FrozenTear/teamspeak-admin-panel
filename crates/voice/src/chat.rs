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
    /// `!pause` — pause the current track (the pipeline stays spawned).
    Pause,
    /// `!resume` / `!unpause` — resume a paused track.
    Resume,
    /// `!skip` / `!next` — drop the current track, advance the queue.
    Skip,
    /// `!prev` — replay the previous track if available.
    Prev,
    /// `!vol <0..100>` — set per-bot volume.
    Volume(u8),
    /// `!np` — reply with the current now-playing line.
    NowPlaying,
}

/// PURA-340 — what the bot actor must do to the audio pipeline after a
/// chat command mutated the queue.
///
/// The chat bridge does the queue mutation and the chat reply, but it
/// deliberately knows nothing about the live `current_audio` pipeline
/// handle — only the connected loop in `bot.rs` owns that. So `dispatch`
/// returns this plain value and `bot.rs` executes it. Keeping the
/// *decision* here and the *execution* there preserves the module's
/// "parser unit-testable without the bot actor" rule (see module docs).
///
/// Before PURA-340 there was no such hand-off at all: chat `!play` only
/// ever enqueued a track, so on an idle bot it was a no-op for audio —
/// the pipeline never spawned and the bot reported `playing` while
/// silent.
// `Eq` is intentionally absent — `SetVolume` carries an `f32`. Tests
// compare with `assert_eq!`, which needs only `PartialEq`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ChatAudioAction {
    /// Nothing to do — read-only command (`!np`), an audio-stub path
    /// (`!prev`), or a command that failed before it mutated the queue.
    None,
    /// `!pause` — park the live pipeline so playback halts without
    /// tearing it down. A no-op when nothing is playing.
    Pause,
    /// `!resume` — un-park a paused pipeline. A no-op when nothing is
    /// playing or the pipeline is already running.
    Resume,
    /// PURA-351 — `!vol <0..100>` set the output gain. The carried value
    /// is the linear multiplier (`0.0..=1.0`); the connected loop applies
    /// it to the bot's shared [`VolumeHandle`](music_bot_audio::VolumeHandle).
    SetVolume(f32),
    /// `!play` — a track was appended. Start the queue head iff the bot
    /// is idle; when a pipeline is already running the new track stays
    /// queued and plays when the current one finishes.
    StartIfIdle,
    /// `!radio` / `!skip` — the queue head changed and a new head exists.
    /// Tear down any running pipeline and start the new head now.
    RestartHead,
    /// `!stop`, or `!skip` past the last track — the queue is now empty.
    /// Tear down any running pipeline.
    StopPlayback,
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
                Ok(ParsedCommand::Radio {
                    arg: rest.to_string(),
                })
            }
        }
        "play" => {
            if rest.is_empty() {
                Err(ParseError::MissingArg { verb: "play" })
            } else {
                Ok(ParsedCommand::Play {
                    arg: rest.to_string(),
                })
            }
        }
        "stop" => Ok(ParsedCommand::Stop),
        "pause" => Ok(ParsedCommand::Pause),
        "resume" | "unpause" => Ok(ParsedCommand::Resume),
        "skip" | "next" => Ok(ParsedCommand::Skip),
        "prev" => Ok(ParsedCommand::Prev),
        "vol" => {
            if rest.is_empty() {
                return Err(ParseError::MissingArg { verb: "vol" });
            }
            let parsed: Option<u8> = rest
                .parse::<u32>()
                .ok()
                .and_then(|n| if n <= 100 { Some(n as u8) } else { None });
            match parsed {
                Some(v) => Ok(ParsedCommand::Volume(v)),
                None => Err(ParseError::BadVolume {
                    got: rest.to_string(),
                }),
            }
        }
        "np" => Ok(ParsedCommand::NowPlaying),
        _ => Err(ParseError::Unknown),
    }
}

/// Lower a parsed command into store mutations + `BotEvent`s, returning
/// the operator-facing reply line and the [`ChatAudioAction`] the bot
/// actor must apply.
///
/// `Connection`-free by design: the bot actor pairs this with a wire-side
/// [`send_reply`] (single-loop path) or a `WireCmd::ChatReply` (PURA-396
/// split path). Keeping the lowering `Connection`-free also makes it
/// unit-testable without a live `Connection`.
pub(crate) async fn handle_command(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    cmd: ParsedCommand,
) -> (String, ChatAudioAction) {
    match cmd {
        ParsedCommand::Radio { arg } => handle_radio(bot_id, store, events, arg).await,
        ParsedCommand::Play { arg } => handle_play(bot_id, store, events, arg).await,
        ParsedCommand::Stop => handle_stop(bot_id, store, events).await,
        // PURA-353 — `!pause` / `!resume` now drive the live pipeline
        // (`apply_chat_audio_action` flips its pause `watch`), the same
        // path the REST `pause_bot` / `resume_bot` controls already use.
        // Before, chat `!pause` only emitted an `AudioNotImplemented`
        // stub, so it replied `pause` in chat but never halted playback.
        ParsedCommand::Pause => ("paused".to_string(), ChatAudioAction::Pause),
        ParsedCommand::Resume => ("resumed".to_string(), ChatAudioAction::Resume),
        ParsedCommand::Skip => handle_skip(bot_id, store, events).await,
        ParsedCommand::Prev => {
            // No queue history yet (PURA-121 ships forward-only). Lower
            // into the audio stub so WS-2 can wire it up later; today the
            // chat reply is honest about the limitation.
            emit_audio_stub_event(events, "SkipPrev");
            (
                "previous track not yet supported (queue history lands with WS-2)".to_string(),
                ChatAudioAction::None,
            )
        }
        ParsedCommand::Volume(v) => {
            // PURA-351 — `!vol 0..100` lowers to a linear-gain multiplier
            // `0.0..=1.0`. The connected loop applies it to the bot's
            // shared `VolumeHandle`, so it lands on the live track and
            // every later one.
            let gain = f32::from(v) / 100.0;
            (format!("volume {v}%"), ChatAudioAction::SetVolume(gain))
        }
        ParsedCommand::NowPlaying => (handle_np(bot_id, store).await, ChatAudioAction::None),
    }
}

/// The single short chat reply for a parse error, or `None` when the
/// error must stay silent. Only "user-visible" errors (`MissingArg`,
/// `BadVolume`) get a reply; `Empty` and `Unknown` are chat noise / typos
/// and are dropped per the issue spec.
///
/// `Connection`-free for the same reason as [`handle_command`] — the bot
/// actor decides how the line reaches the wire.
pub(crate) fn parse_error_reply(err: &ParseError) -> Option<String> {
    match err {
        ParseError::Empty | ParseError::Unknown => None,
        ParseError::MissingArg { .. } | ParseError::BadVolume { .. } => Some(err.to_string()),
    }
}

async fn handle_radio(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    arg: String,
) -> (String, ChatAudioAction) {
    let (track, label) = match resolve_source(bot_id, store, &arg).await {
        Ok(pair) => pair,
        Err(reply) => return (reply, ChatAudioAction::None),
    };
    // !radio replaces the queue: clear then enqueue.
    if let Err(err) = store.queue_clear(bot_id).await {
        warn!(?err, "queue_clear during !radio");
        return (format!("radio failed: {err}"), ChatAudioAction::None);
    }
    let _ = events.send(BotEvent::QueueChanged {
        len: 0,
        current: None,
    });
    let _ = events.send(BotEvent::QueueEmpty);
    match store.queue_enqueue(bot_id, track).await {
        Ok(stored) => {
            let _ = events.send(BotEvent::QueueChanged {
                len: 1,
                current: Some(stored.clone()),
            });
            let _ = events.send(BotEvent::NowPlaying(stored));
            // The queue head is now the radio track — the actor must tear
            // down whatever was playing and start it.
            (format!("radio: {}", label), ChatAudioAction::RestartHead)
        }
        Err(err) => {
            warn!(?err, "queue_enqueue during !radio");
            (format!("radio failed: {err}"), ChatAudioAction::None)
        }
    }
}

async fn handle_play(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
    arg: String,
) -> (String, ChatAudioAction) {
    let (track, label) = match resolve_source(bot_id, store, &arg).await {
        Ok(pair) => pair,
        Err(reply) => return (reply, ChatAudioAction::None),
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
            let reply = if was_empty {
                let _ = events.send(BotEvent::NowPlaying(stored));
                format!("playing: {}", label)
            } else {
                format!("queued: {} (#{})", label, queue.len())
            };
            // PURA-340 — the core fix: ask the actor to start the queue
            // head if the bot is idle. `StartIfIdle` is a no-op when a
            // pipeline is already running (the new track stays queued),
            // so it is correct to return it unconditionally — including
            // the `queued:` branch, where an idle bot with a pre-staged
            // queue still gets started from its head.
            (reply, ChatAudioAction::StartIfIdle)
        }
        Err(err) => {
            warn!(?err, "queue_enqueue during !play");
            (format!("play failed: {err}"), ChatAudioAction::None)
        }
    }
}

async fn handle_stop(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
) -> (String, ChatAudioAction) {
    // Clear the queue so the state is honest about "stopped"; the actor
    // tears the live pipeline down via the returned `StopPlayback`.
    let was_non_empty = store
        .queue_peek(bot_id)
        .await
        .map(|q| !q.is_empty())
        .unwrap_or(false);
    match store.queue_clear(bot_id).await {
        Ok(()) => {
            let _ = events.send(BotEvent::QueueChanged {
                len: 0,
                current: None,
            });
            if was_non_empty {
                let _ = events.send(BotEvent::QueueEmpty);
            }
            ("stopped".to_string(), ChatAudioAction::StopPlayback)
        }
        Err(err) => {
            warn!(?err, "queue_clear during !stop");
            (format!("stop failed: {err}"), ChatAudioAction::None)
        }
    }
}

async fn handle_skip(
    bot_id: BotId,
    store: &Arc<dyn MusicBotStore>,
    events: &broadcast::Sender<BotEvent>,
) -> (String, ChatAudioAction) {
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
                    // A new head exists — restart the pipeline onto it.
                    (format!("skipped → {}", title), ChatAudioAction::RestartHead)
                }
                None => {
                    let _ = events.send(BotEvent::QueueEmpty);
                    // Nothing left to play — but a pipeline may still be
                    // running the just-skipped track, so tear it down.
                    (
                        "skipped → queue empty".to_string(),
                        ChatAudioAction::StopPlayback,
                    )
                }
            }
        }
        Ok(None) => ("queue is empty".to_string(), ChatAudioAction::None),
        Err(err) => {
            warn!(?err, "queue_dequeue_head during !skip");
            (format!("skip failed: {err}"), ChatAudioAction::None)
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

/// Turn an arg into a `NewTrack`. Resolution order:
///  1. A bare `http(s)://` link passes straight through to yt-dlp.
///  2. A `yt:`/`youtube:` prefix is a YouTube search — the rest of the arg
///     is the query, resolved via yt-dlp's `ytsearch1:` (PURA-353).
///  3. Anything else is looked up against the bot's library by exact
///     title (case-insensitive).
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
    } else if let Some(query) = parse_yt_search(arg) {
        if query.is_empty() {
            return Err(
                "what should I search YouTube for? e.g. !play yt: artist - song".to_string(),
            );
        }
        // yt-dlp treats `ytsearch1:<query>` as a one-result YouTube search
        // and streams the top hit, so it flows through the same URL path
        // as a real link — no pipeline change needed.
        Ok((
            NewTrack {
                source: AudioSource::Url(format!("ytsearch1:{query}")),
                title: query.to_string(),
                duration_secs: None,
                requested_by: None,
            },
            format!("youtube search: {query}"),
        ))
    } else {
        match store.library_list(bot_id, None).await {
            Ok(entries) => {
                let lc = arg.to_ascii_lowercase();
                let hit = entries
                    .into_iter()
                    .find(|e| e.title.to_ascii_lowercase() == lc);
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

/// Detect a `yt:` / `youtube:` search prefix and return the trimmed query
/// after it. Returns `None` when the arg carries no such prefix (PURA-353).
fn parse_yt_search(arg: &str) -> Option<&str> {
    let lc = arg.to_ascii_lowercase();
    ["yt:", "youtube:"]
        .into_iter()
        .find(|p| lc.starts_with(p))
        .map(|p| arg[p.len()..].trim())
}

/// Best-effort send a chat reply to the bot's current channel. We don't
/// surface the failure to the caller — chat replies are advisory, and a
/// transient send error is logged at `warn!` only.
///
/// PURA-396 — `pub(crate)` so the split wire task can run it on the
/// `WireCmd::ChatReply` it receives from the control task.
pub(crate) fn send_reply(con: &mut Connection, line: &str) {
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
    let _ = events.send(BotEvent::Error(
        crate::event::BotError::AudioNotImplemented(label.to_string()),
    ));
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn happy_path_no_args() {
        assert_eq!(parse("!stop"), Ok(ParsedCommand::Stop));
        assert_eq!(parse("!pause"), Ok(ParsedCommand::Pause));
        assert_eq!(parse("!resume"), Ok(ParsedCommand::Resume));
        assert_eq!(parse("!unpause"), Ok(ParsedCommand::Resume));
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
        assert_eq!(
            parse("!radio"),
            Err(ParseError::MissingArg { verb: "radio" })
        );
        assert_eq!(parse("!play"), Err(ParseError::MissingArg { verb: "play" }));
        // Bare verb with trailing spaces is also a missing-arg.
        assert_eq!(
            parse("!radio   "),
            Err(ParseError::MissingArg { verb: "radio" })
        );
    }

    #[test]
    fn vol_range() {
        assert_eq!(parse("!vol 0"), Ok(ParsedCommand::Volume(0)));
        assert_eq!(parse("!vol 50"), Ok(ParsedCommand::Volume(50)));
        assert_eq!(parse("!vol 100"), Ok(ParsedCommand::Volume(100)));
        assert_eq!(
            parse("!vol 101"),
            Err(ParseError::BadVolume { got: "101".into() })
        );
        assert_eq!(
            parse("!vol abc"),
            Err(ParseError::BadVolume { got: "abc".into() })
        );
        assert_eq!(
            parse("!vol -5"),
            Err(ParseError::BadVolume { got: "-5".into() })
        );
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

    /// PURA-353 — the `yt:` / `youtube:` prefix is detected case-insensitively
    /// and the query is trimmed; anything else carries no prefix.
    #[test]
    fn yt_search_prefix_detection() {
        assert_eq!(parse_yt_search("yt: red leather"), Some("red leather"));
        assert_eq!(parse_yt_search("YT:red leather"), Some("red leather"));
        assert_eq!(parse_yt_search("youtube:  spaced  "), Some("spaced"));
        assert_eq!(parse_yt_search("yt:"), Some(""));
        assert_eq!(parse_yt_search("https://x/y.mp3"), None);
        assert_eq!(parse_yt_search("some library title"), None);
    }
}

/// PURA-340 — dispatch-level tests for the audio-action hand-off. These
/// exercise [`handle_command`] (the `Connection`-free half of `dispatch`)
/// so chat `!play` etc. can be verified without a live TS6 fixture.
#[cfg(test)]
mod dispatch_tests {
    use super::*;
    use crate::store::InMemoryMusicBotStore;

    fn store() -> Arc<dyn MusicBotStore> {
        Arc::new(InMemoryMusicBotStore::new())
    }

    async fn run(store: &Arc<dyn MusicBotStore>, cmd: ParsedCommand) -> (String, ChatAudioAction) {
        // Buffer is generous; the receiver is kept alive so `events.send`
        // inside the handlers never short-circuits on "no subscribers".
        let (events, _rx) = broadcast::channel(64);
        handle_command(BotId(1), store, &events, cmd).await
    }

    /// The regression: chat `!play` on an idle bot must ask the actor to
    /// start the pipeline. Before the fix `handle_play` only enqueued the
    /// track and returned a reply — `!play` was a no-op for audio, so the
    /// pipeline never spawned and the bot reported `playing` while silent.
    #[tokio::test]
    async fn play_on_idle_requests_pipeline_start() {
        let store = store();
        let (reply, action) = run(
            &store,
            ParsedCommand::Play {
                arg: "https://example.com/a.mp3".into(),
            },
        )
        .await;
        assert!(reply.starts_with("playing:"), "reply was {reply:?}");
        assert_eq!(action, ChatAudioAction::StartIfIdle);
    }

    /// A second `!play` while the queue is non-empty still returns
    /// `StartIfIdle` — it is a no-op when a pipeline is live, and keeps an
    /// *idle* bot with a pre-staged queue startable.
    #[tokio::test]
    async fn play_behind_a_queued_track_still_requests_start() {
        let store = store();
        let _ = run(
            &store,
            ParsedCommand::Play {
                arg: "https://example.com/a.mp3".into(),
            },
        )
        .await;
        let (reply, action) = run(
            &store,
            ParsedCommand::Play {
                arg: "https://example.com/b.mp3".into(),
            },
        )
        .await;
        assert!(reply.starts_with("queued:"), "reply was {reply:?}");
        assert_eq!(action, ChatAudioAction::StartIfIdle);
    }

    #[tokio::test]
    async fn radio_restarts_the_head() {
        let store = store();
        let (_reply, action) = run(
            &store,
            ParsedCommand::Radio {
                arg: "https://r.example/lofi.mp3".into(),
            },
        )
        .await;
        assert_eq!(action, ChatAudioAction::RestartHead);
    }

    #[tokio::test]
    async fn stop_tears_playback_down() {
        let store = store();
        let (_reply, action) = run(&store, ParsedCommand::Stop).await;
        assert_eq!(action, ChatAudioAction::StopPlayback);
    }

    #[tokio::test]
    async fn skip_restarts_when_a_track_follows_else_stops() {
        let store = store();
        for url in ["https://example.com/a.mp3", "https://example.com/b.mp3"] {
            let _ = run(&store, ParsedCommand::Play { arg: url.into() }).await;
        }
        // A track follows the head → restart onto it.
        let (_r, restart) = run(&store, ParsedCommand::Skip).await;
        assert_eq!(restart, ChatAudioAction::RestartHead);
        // Skipping the last track empties the queue → tear playback down.
        let (_r, stop) = run(&store, ParsedCommand::Skip).await;
        assert_eq!(stop, ChatAudioAction::StopPlayback);
        // Skipping an empty queue touches nothing.
        let (_r, none) = run(&store, ParsedCommand::Skip).await;
        assert_eq!(none, ChatAudioAction::None);
    }

    #[tokio::test]
    async fn read_only_and_stub_commands_are_audio_inert() {
        let store = store();
        for cmd in [ParsedCommand::NowPlaying, ParsedCommand::Prev] {
            let (_reply, action) = run(&store, cmd).await;
            assert_eq!(action, ChatAudioAction::None);
        }
    }

    /// PURA-353 — `!play yt: <query>` searches YouTube instead of needing
    /// a link. The query is wrapped as `ytsearch1:<query>`, which yt-dlp
    /// resolves to the top hit and streams through the normal URL path.
    #[tokio::test]
    async fn play_yt_search_enqueues_ytsearch_source() {
        let store = store();
        let (reply, action) = run(
            &store,
            ParsedCommand::Play {
                arg: "yt: red leather last call".into(),
            },
        )
        .await;
        assert!(reply.starts_with("playing:"), "reply was {reply:?}");
        assert!(
            reply.contains("youtube search: red leather last call"),
            "reply was {reply:?}",
        );
        assert_eq!(action, ChatAudioAction::StartIfIdle);
        let queue = store.queue_peek(BotId(1)).await.unwrap();
        assert_eq!(
            queue.first().map(|t| &t.source),
            Some(&AudioSource::Url("ytsearch1:red leather last call".into())),
        );
    }

    /// PURA-353 — a `yt:` prefix with no query is a user-visible error,
    /// not an empty search.
    #[tokio::test]
    async fn play_yt_search_without_query_is_rejected() {
        let store = store();
        let (reply, action) = run(&store, ParsedCommand::Play { arg: "yt:".into() }).await;
        assert!(reply.contains("search YouTube for"), "reply was {reply:?}");
        assert_eq!(action, ChatAudioAction::None);
    }

    /// PURA-353 — `!pause` / `!resume` must hand the connected loop a
    /// real audio action so it flips the live pipeline's pause `watch`.
    /// The regression was `ParsedCommand::Pause` returning
    /// `ChatAudioAction::None` (an `AudioNotImplemented` stub) — chat
    /// replied `pause` but playback never halted.
    #[tokio::test]
    async fn pause_and_resume_drive_the_pipeline() {
        let store = store();
        let (reply, action) = run(&store, ParsedCommand::Pause).await;
        assert_eq!(reply, "paused");
        assert_eq!(action, ChatAudioAction::Pause);
        let (reply, action) = run(&store, ParsedCommand::Resume).await;
        assert_eq!(reply, "resumed");
        assert_eq!(action, ChatAudioAction::Resume);
    }

    /// PURA-351 — `!vol N` lowers to a `SetVolume` action carrying the
    /// linear-gain multiplier `N / 100`, and the chat reply echoes the
    /// percent the operator typed.
    #[tokio::test]
    async fn volume_command_lowers_to_set_volume_action() {
        let store = store();
        let (reply, action) = run(&store, ParsedCommand::Volume(50)).await;
        assert_eq!(action, ChatAudioAction::SetVolume(0.5));
        assert_eq!(reply, "volume 50%");

        let (_r, muted) = run(&store, ParsedCommand::Volume(0)).await;
        assert_eq!(muted, ChatAudioAction::SetVolume(0.0));

        let (_r, full) = run(&store, ParsedCommand::Volume(100)).await;
        assert_eq!(full, ChatAudioAction::SetVolume(1.0));
    }
}
