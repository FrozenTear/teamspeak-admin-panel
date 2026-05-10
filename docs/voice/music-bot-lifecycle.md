# Music-bot lifecycle (PURA-118 WS-1)

This is the contract between the `music-bot` crate (`crates/voice/`) and
its callers (REST surface in WS-5, chat-bridge in WS-4, FE-PAGES UI in
WS-6, audio pipeline in WS-2). It documents the lifecycle state
machine, the `BotCommand` dispatch surface, and the `BotEvent`
broadcast surface — all three frozen for the rest of WS-2 onwards.

WS-1 ships only the **lifecycle skeleton**: a bot can connect to the
self-hosted TS6 server, hold state, accept commands, and clean up
cleanly. Audio is stubbed; queue/playlist/library and chat-command
bridges land in WS-2 / WS-3 / WS-4.

---

## States

| State | Meaning |
|-------|---------|
| `Disconnected` | Initial / terminal. No connection, no in-flight handshake. From here, `Connect` (or `auto_connect = true`) drives the bot online. |
| `Connecting` | Handshake in flight. Covers both the first connect and every auto-reconnect retry — backoff sleeps stay inside this state via a `Connecting → Connecting` self-loop, deliberately keeping the spec to five named states. |
| `Connected` | Handshake done, sitting in the server's default channel. External `BotCommand`s are accepted. |
| `InChannel` | Sitting in a specific channel reached via `JoinChannel`. Functionally indistinguishable from `Connected` from the dispatcher's point of view, but exposed separately so callers can light up "in-channel" UI without re-reading the connection book. |
| `Disconnecting` | Clean shutdown in progress. Driven by `Disconnect` or `Shutdown`; the actor sends a `clientdisconnect` and drains the event stream before flipping to `Disconnected`. |

`Connecting` and `Disconnecting` are transient — external commands
queue and dispatch when the bot returns to a quiescent state
(`Disconnected`, `Connected`, `InChannel`). The `BotState` enum's
`accepts_external_commands()` predicate is the source of truth.

## Transitions

```
                   ┌──────────────────────────────────────────────────┐
                   │                                                  │
                   │                       (network drop / kick)      │
                   │                                                  ▼
   Disconnected ─Connect─▶ Connecting ─handshake─▶ Connected ─JoinChannel─▶ InChannel
        ▲                       │  ▲                  │                    │ │ ▲
        │                       │  │                  │  LeaveChannel      │ │ │JoinChannel
        │                  retry│  │backoff           │◀───────────────────┘ │ │(self-loop)
        │                       │  │                  │                      └─┘
        │                       └──┘                  │
        │                                             │
        │            Disconnect / Shutdown            │
        │   ┌─────────────────────────────────────────┘
        │   ▼
        │   Disconnecting
        │   │
        └───┘
            (clean clientdisconnect drain → Disconnected)
```

The full set of legal transitions (also enforced in
`crates/voice/src/state.rs` and round-tripped by
`state::tests::illegal_skips_are_rejected`):

| From | To | Trigger |
|------|----|---------|
| `Disconnected` | `Connecting` | `Connect` command, or `auto_connect = true` on spawn. |
| `Connecting` | `Connected` | Handshake won (own client appears in book). |
| `Connecting` | `Connecting` | Backoff retry — handshake failure → exponential sleep → re-enter. |
| `Connecting` | `Disconnecting` | `Shutdown` / `Disconnect` arrived before handshake completed. |
| `Connected` | `InChannel` | `JoinChannel(id)` succeeded. |
| `InChannel` | `InChannel` | `JoinChannel(other_id)` — channel switch. |
| `InChannel` | `Connected` | `LeaveChannel`. |
| `Connected`, `InChannel` | `Connecting` | Network drop / server kick → auto-reconnect. |
| `Connected`, `InChannel` | `Disconnecting` | `Disconnect` or `Shutdown`. |
| `Disconnecting` | `Disconnected` | Clean disconnect drained. |

Anything else returns `IllegalTransition`. The actor logs and emits
`BotEvent::Error(BotError::Internal)` rather than crashing the
task — keeping the bot alive across operator misuse is more important
than enforcing the discipline at runtime.

## Auto-start and auto-reconnect

- `BotConfig::auto_connect = true` (the default for
  `BotConfig::new`) makes the actor enqueue an implicit `Connect` on
  spawn.
- An unexpected disconnect (handshake failure, mid-session UDP drop,
  server kick) re-enters `Connecting` after sleeping per
  `ExponentialBackoff` (default `1 s` → `2 s` → `4 s` → … → cap at
  `60 s`, no attempt cap).
- A successful handshake **resets** the backoff counter — five quick
  reconnects across a long-running session are not penalised.

## Commands (`BotCommand`)

The control plane. The supervisor maps a `BotId` to an
`mpsc::Sender<BotCommand>` and exposes `send` / `try_send` /
`subscribe`. WS-1 implements lifecycle commands; audio commands log
and emit `BotError::AudioNotImplemented` so the WS-4 chat bridge and
WS-5 REST surface can wire dispatch end-to-end against today's binary.

| Command | Required state | Effect |
|---------|----------------|--------|
| `Connect` | `Disconnected` | Transitions → `Connecting` and starts the handshake. No-op if already online. |
| `Disconnect` | `Connected`, `InChannel` | Transitions → `Disconnecting`, sends a clean `clientdisconnect`, then → `Disconnected`. Auto-reconnect does **not** re-fire. |
| `JoinChannel(id)` | `Connected`, `InChannel` | Sends a `clientmove` for `(own_client, channel_id=id)`. The actual `StateChanged` / `JoinedChannel` event fires when the connection book confirms the move. |
| `LeaveChannel` | `InChannel` | WS-1 stub — emits `BotEvent::LeftChannel`. WS-3 will resolve "default channel" tracking and actually move. |
| `Shutdown` | any | Tears the bot down for good. The actor exits after the disconnect drains; the supervisor drops the handle. |
| `Audio(...)` | any | WS-1 stub — logs at debug and emits `BotEvent::Error(AudioNotImplemented)`. WS-2 wires this to the yt-dlp / FFmpeg / Opus pipeline. |

Commands hitting a non-accepting state get a
`BotEvent::Error(CommandRejected{ command, state })` and are **not**
queued for later — callers should resubscribe to the event stream
before driving the bot through a state change.

### `Audio(AudioCommand)` sub-surface

Stubbed for WS-1; locked here so WS-2 can plug in without touching
dispatch:

| Sub-command | WS-2 owner action |
|-------------|-------------------|
| `Play { source }` | Resolve `AudioSource::Url` via yt-dlp / `LibraryPath` via local file; pipe through FFmpeg → Opus encoder → 20 ms paced `OutAudio` frames. |
| `Stop` / `Pause` / `Resume` | Toggle the encoder + send-pacing task. |
| `SkipNext` / `SkipPrev` | Pop / re-queue the WS-3 queue, then `Play` the next track. |
| `SetVolume(f32)` | Apply gain pre-encoder. Unit decided in WS-2. |
| `NowPlaying(String)` | Set the ICY metadata header for the current track. |

## Events (`BotEvent`)

The bot → world broadcast. Subscribers (UI, REST, chat-bridge) attach
via `BotHandle::subscribe()` and get an independent
`broadcast::Receiver`. Default capacity is 64 events
(`BotConfig::event_buffer`); slow consumers get `Lagged(n)` and skip
events rather than backpressuring the actor.

| Event | When it fires |
|-------|---------------|
| `StateChanged { from, to }` | On every legal lifecycle transition. Source of truth for UI state. |
| `Connected { client_id, default_channel }` | After handshake completes AND the own client appears in the connection book. |
| `JoinedChannel { channel_id }` | Once with the default channel right after `Connected`, then once per server-confirmed channel move. |
| `LeftChannel` | After `LeaveChannel` (WS-1: stub). |
| `Disconnected { kind, reason }` | Once per disconnect. `kind = Clean / ShutdownRequested / Dropped` distinguishes operator-driven from auto-reconnect cases. |
| `Error(BotError)` | Recoverable error (illegal command, audio stub, transient connection error, max-retries reached). |

The actor emits `StateChanged` **before** the corresponding
"narrative" event (`Connected`, `Disconnected`, `JoinedChannel`) so
event-stream consumers can rely on observing the state transition
first.

## Wiring (WS-2 onwards)

WS-2 plugs audio into the bot by:

1. Adding a sibling task per bot that owns Opus encoding + 20 ms pacing.
2. Sharing the `Connection` handle through a small mutex / mpsc — the
   bot actor holds it today; WS-2 will switch to a shared
   `Arc<Mutex<Connection>>` so the audio task can `con.send_audio(...)`
   without going through the dispatch.
3. Replacing the `Audio(...)` stub branch in
   `crates/voice/src/bot.rs::run_connected_loop` with real dispatch into
   the audio task's mpsc.

The dispatch surface above (`BotCommand::Audio(AudioCommand::*)`) is
already shaped for that hand-off — WS-2 should not need to extend the
public enum unless a new audio operation surfaces.

## Tests

| Test | Where | What it proves |
|------|-------|----------------|
| `state::tests::*` | `crates/voice/src/state.rs` | Illegal transitions are rejected; happy-path covers `Disconnected → … → Disconnected`; auto-reconnect self-loop is legal. |
| `backoff::tests::*` | `crates/voice/src/backoff.rs` | Exponential growth + cap; `reset` after success; `max_attempts` returns `None`. |
| `command::tests::bot_command_serde_round_trip` | `crates/voice/src/command.rs` | `BotCommand` round-trips JSON without a separate wire-format type. |
| `music_bot::lifecycle_e2e` | `crates/voice/tests/lifecycle_e2e.rs` | Headless integration test against the `podman-compose --profile ts6-fixture` server: spawn → `Connected` → `JoinedChannel` → `JoinChannel(default)` → `Shutdown` → `Disconnected`. Feature-gated (`lifecycle-e2e`) **and** env-gated (`TS6_VOICE_FIXTURE=1`) **and** `#[ignore]` — `cargo test --workspace` never tries to run it without an explicit operator opt-in. |

Run the live test:

```sh
podman-compose --profile ts6-fixture up -d ts6-fixture
TS6_VOICE_FIXTURE=1 cargo test -p music-bot \
    --features lifecycle-e2e -- music_bot::lifecycle_e2e \
    --ignored --nocapture
```

The fixture profile uses `--network=host` (passt wedge mitigation —
see `docs/ts6-fixture.md`); license acceptance via
`TSSERVER_LICENSE_ACCEPTED=accept` belongs to the operator's runbook,
not committed config.
