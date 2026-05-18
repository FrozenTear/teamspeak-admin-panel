# Music-bot in-channel chat commands

PURA-122 (WS-4 of [PURA-117 — Phase 4 epic][1]) ships a TS6 chat → BotCommand
bridge so operators in the bot's channel can drive playback without touching
the web UI.

[1]: ../../crates/voice/src/chat.rs

## Where it lives

- **Parser**: `crates/voice/src/chat.rs::parse` — pure-functional, fully
  unit-tested. Returns `ParsedCommand` or `ParseError`.
- **Dispatcher**: `crates/voice/src/chat.rs::dispatch` — async; lowers the
  parsed command into `BotCommand` / store mutations and posts a single
  short channel-chat reply.
- **Wire-in point**: `crates/voice/src/bot.rs::run_connected_loop` extracts
  `MessageTarget::Channel` events from each `StreamItem::BookEvents`,
  filters out the bot's own client id, and runs them through
  `chat::dispatch`.

## Command table

| Command            | Effect                                              | Today's behaviour                                                                                                |
| ------------------ | --------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| `!radio <name|url>`| Replace queue with a radio source; auto-play         | Clears queue, enqueues (URL or library lookup by title), emits `QueueChanged` + `NowPlaying`. Replies `radio: …` |
| `!play <url|search>` | Enqueue + start if idle                            | Enqueues (URL or library lookup); search → library lookup. Empty queue → `NowPlaying`. Replies `playing:` / `queued:` |
| `!stop`            | Stop playback, clear queue                          | Tears down the live pipeline + clears queue + `QueueEmpty`. Replies `stopped`                                    |
| `!pause`           | Pause current track                                 | Parks the live pipeline (sibling stops sending frames; pipeline stays spawned). Replies `paused`                 |
| `!resume` / `!unpause` | Resume a paused track                           | Un-parks the live pipeline. Replies `resumed`                                                                    |
| `!skip` / `!next`  | Drop current track, advance                         | Pops queue head, fires `QueueChanged` + `NowPlaying` / `QueueEmpty`. Replies `skipped → …`                        |
| `!prev`            | Replay previous track if available                  | No queue history yet (forward-only queue ships in PURA-121). Replies "previous track not yet supported"          |
| `!vol <0..100>`    | Per-bot volume                                      | Emits `AudioCommand::SetVolume(v/100)` stub. Replies `volume <v>`. (Real volume control lands in WS-2.)          |
| `!np`              | Reply with now-playing                              | Reads `MusicBotStore::queue_current`. Replies `now playing: <title>` or `queue is empty`                         |

### Source resolution

`!radio` and `!play` accept either:

- a URL — anything starting with `http://` or `https://` (case-insensitive)
  is enqueued as `AudioSource::Url(...)` verbatim;
- a name — looked up against the bot's library (`MusicBotStore::library_list`)
  by exact case-insensitive title match. No fuzzy / prefix search yet —
  full search lands with the WS-2 source pipeline. If no entry matches,
  the bot replies `no library entry titled `<arg>``.

## Parser tolerances

- Lines without a leading `!` are silently ignored (chat noise must not echo
  back "unknown command").
- Verbs are matched case-insensitively (`!Stop` ≡ `!stop` ≡ `!STOP`).
- Leading and trailing whitespace is trimmed. Inner whitespace inside an
  arg is preserved (`!play song with spaces` → arg = "song with spaces").
- Unknown verbs (`!foo`) are silent — same rationale.
- Empty after the prefix (`!`, `   !   `) is silent.
- `!vol` argument must be 0..=100 — anything else replies
  ``!vol argument must be 0..=100, got `<arg>` ``.
- Missing required args (`!play`, `!radio`, `!vol` with no value) reply
  ``!<verb>` requires an argument``.

## Permission model — TODO

WS-4 starts permissive: **anyone in the channel can drive the bot**.
There is no allow-list, no channel-group / server-group check, no per-UID
gating. This is fine for a single-operator self-host but gets ugly fast in
a public lobby.

Follow-up should add at least:

- An "operator UID list" the bot reads on boot (env or store-backed), and
  rejects commands from anyone else with a single short reply.
- An "operator channel-group" check using `tsclientlib`'s book — the bot
  walks `book.channel_groups[client.channel_group]` and accepts only the
  configured group ids.

Whichever route, the rejection path stays "single short reply" — chat
noise rule applies in both directions.

The bot already filters out its own client id from the event scan, so
self-loops on replies are not a concern.

## Reply policy

Every successfully-parsed command sends exactly one reply line via
`Connection::send_message(MessageTarget::Channel, …)`. Replies are
short — no multi-line summaries, no emoji, no leading `!` (so a sibling
bot in the channel can't loop on its own output).

User-visible parse errors (`MissingArg`, `BadVolume`) reply with the
error's `Display` impl. `Empty` and `Unknown` are silent by design.

## Self-host operator notes

- Bring up the TS6 fixture: `podman-compose --profile ts6-fixture up -d ts6-fixture`
  (still uses `--network=host` per `docs/ts6-fixture.md`).
- The bot must be in a channel for chat commands to reach it — it auto-joins
  the server's default channel on connect (see `BotEvent::Connected
  { default_channel }`), and `BotCommand::JoinChannel(...)` moves it.
- Only `MessageTarget::Channel` is wired today. Server-wide chat
  (`MessageTarget::Server`) and private chat (`MessageTarget::Client`) are
  intentionally not handled; that's an explicit follow-up alongside the
  permission model.

## Test coverage

- Unit: `cargo test -p music-bot --lib chat::` — every command in the
  table plus arg validation.
- Integration: `tests/chat_bridge_e2e.rs`, gated by the `lifecycle-e2e`
  feature + `TS6_VOICE_FIXTURE=1`. Spawns the music-bot plus a raw
  `tsclientlib::Connection` operator into the same channel, sends `!np` /
  `!play` / `!stop` as real TS6 chat, and asserts both the bot's
  `BotEvent` broadcast AND the operator's incoming reply line. Run:

  ```sh
  podman-compose --profile ts6-fixture up -d ts6-fixture
  TS6_VOICE_FIXTURE=1 cargo test -p music-bot \
      --features lifecycle-e2e -- music_bot::chat_bridge_e2e \
      --ignored --nocapture
  ```
