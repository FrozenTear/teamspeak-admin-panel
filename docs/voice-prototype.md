# "Two clients can talk" voice prototype

CTO probation deliverable for [PURA-108](/PURA/issues/PURA-108)
workstream **WS-4**, scoped on [PURA-112](/PURA/issues/PURA-112). Lives
on the TS6 native voice path picked in
[ADR-0005 rev 2](./adr/0005-voice-path-decision.md): two
`tsclientlib`-based Rust clients connect to the local
`teamspeaksystems/teamspeak6-server` fixture and exchange Opus voice
frames bidirectionally.

## What it demos

- Both clients complete the §19.5+ handshake against a self-hosted TS6
  server (no `beta.voice.teamspeak.com` dependency).
- Each client sends 20ms / 48kHz / mono Opus frames laid out per the
  §19.10 prefix settled in [PURA-7](/PURA/issues/PURA-7) Day 2:
  `voice_id u16 BE | codec_id u8 | opus_payload`.
- Each client decodes the other's S2C voice frames back to PCM and
  writes a WAV per remote sender so a human can verify audibility.
- Acceptance bar: ≥30s of stable bidirectional flow with no
  `ts3error` and no `tsproto` resends.

## One-shot run

```bash
make voice-prototype
```

What that does:

1. `cargo build --release -p ts6-voice-prototype`.
2. Brings up the upstream `teamspeaksystems/teamspeak6-server`
   container under `--profile ts6-fixture` (host networking — see
   [`docs/ts6-fixture.md`](./ts6-fixture.md) for the reason). If the
   container is already running, the step is a no-op.
3. Sleeps 5s for the fixture to settle.
4. Spawns two prototype processes in parallel: `alice` sending a
   440Hz tone, `bob` sending a 660Hz tone. Each runs for 30s by
   default.
5. Waits on both, lists the WAVs, reports a non-zero exit code if
   either side errored.

Outputs land under `target/voice-prototype/`:

```
target/voice-prototype/
├── alice/
│   └── identity.json           # cached level-8 identity (reused on rerun)
├── bob/
│   └── identity.json
├── alice.log                   # alice's tracing output
├── bob.log
├── alice.from-<bob_id>.wav     # bob's 660Hz tone, decoded on alice's side
└── bob.from-<alice_id>.wav     # alice's 440Hz tone, decoded on bob's side
```

The `*.from-<id>.wav` filenames embed the **remote** TS6 client id, so
once you've inspected one run you can map who sent what. WAVs are
16-bit PCM @ 48kHz mono — playable in any audio tool.

### Knobs

| Variable | Default | Purpose |
|---|---|---|
| `VOICE_PROTOTYPE_DURATION` | `30` | Send window in seconds (acceptance bar = 30). |
| `VOICE_PROTOTYPE_SERVER`   | `127.0.0.1:9987` | TS6 voice host:port. |
| `VOICE_PROTOTYPE_OUT_DIR`  | `target/voice-prototype` | Identity + WAV + log dir. |

```bash
VOICE_PROTOTYPE_DURATION=60 make voice-prototype
```

### Tear-down

```bash
make voice-prototype-fixture-down   # stop the TS6 server container
make voice-prototype-clean          # rm -rf target/voice-prototype
```

## Verifying the WAVs

The synthesized tones are easy to verify by ear or by FFT:

```bash
# Audibility: play and listen.
mpv target/voice-prototype/alice.from-*.wav

# Frequency check: spectrogram should show a clean line at 660Hz on
# alice's recording (= bob's tone) and 440Hz on bob's recording.
sox target/voice-prototype/alice.from-*.wav -n spectrogram -o /tmp/alice.png

# File-size sanity: ~30s @ 48kHz @ 16-bit mono ≈ 2.9 MB per WAV when
# the server forwards every frame.
ls -lh target/voice-prototype/*.wav
```

The acceptance bar ("no `ts3error`, no resends") is read from each
client's log:

```bash
grep -E "ts3error|resend|stream error" target/voice-prototype/{alice,bob}.log
# should produce zero lines
```

## Architecture

Each `ts6-voice-prototype` process is one of the two TS6 clients.
There is no shared state between them — the **server** is the
matchmaking + forwarding layer.

```
┌────────────┐   send Opus 20ms    ┌──────────────────┐    forward S2C    ┌────────────┐
│   alice    │ ─────────────────▶  │ teamspeak6-server│ ──────────────▶  │    bob     │
│            │ ◀─────────────────  │  (default chan)  │ ◀──────────────  │            │
└────────────┘   recv S2C → WAV    └──────────────────┘   send Opus      └────────────┘
```

Inside one process:

- Single `tokio::main` task with one `tokio::select!` loop.
- Send arm: 20ms `interval` → `audiopus::Encoder::encode_float` → wrap
  as `OutAudio::C2S { id: 0, codec: OpusVoice, data }` →
  `Connection::send_audio`.
- Recv arm: `Connection::events()` → `StreamItem::Audio(InAudioBuf)` →
  destructure `AudioData::S2C { from, codec, data, .. }` →
  `audiopus::Decoder::decode` → `hound::WavWriter::write_sample`.
- Per-sender `Decoder` + `WavWriter` are lazy-initialised on first
  frame from each remote client id.
- Identity is generated on first run and cached at
  `<identity_dir>/identity.json`. Two clients in the same run MUST
  use distinct dirs (the Makefile target enforces this).

## "No audio?" — what to check

If `target/voice-prototype/*.wav` is missing or the `frames_recv` line
in either log shows `0`, work through this list before declaring the
prototype broken:

1. **Fixture up?** `podman ps | grep ts6-fixture`. If absent, run
   `make voice-prototype-fixture-up` and retry. The TSSERVER_LICENSE
   env var must be `accept` (the compose profile sets it).

2. **Both clients reached `Connected`?** Each log has a line
   `connection object created — driving handshake` followed by
   `handshake done — entering bidirectional Opus loop` within the 30s
   `connect_timeout_secs`. If alice connects but bob hangs, the
   fixture has booted but is rejecting bob — usually because both
   instances picked the same identity dir (and so the same identity).
   `make voice-prototype-clean` and rerun.

3. **`CanSendAudio(false)` event?** Grep the logs:
   `grep -i 'CanSendAudio\|audio-permission' target/voice-prototype/*.log`.
   The PURA-7 Day-2 spike noted that `beta.voice.teamspeak.com`
   immediately demotes new clients to `CanSendAudio(false)` because
   the public default channel restricts voice to authorised groups.
   The local fixture's default channel ships with looser permissions
   (`b_client_use_voice` granted to Guest by default in upstream
   defaults), but if a previous run reconfigured the fixture you may
   need to grant the test identities `b_client_use_voice` via the
   fixture's WebQuery API (recipe in
   [`docs/ts6-fixture.md`](./ts6-fixture.md)). Phase-4 cleanup will
   bake a permitted server-group provisioning step into the
   prototype itself; for the probation demo the fixture's stock
   defaults are sufficient.

4. **Both clients in the same channel?** Two anonymous clients land
   in the server's default channel by default. If the fixture was
   reconfigured to require a join-channel password or a moved-default
   channel, the clients will land in different channels and won't
   hear each other. Reset the fixture: `podman-compose --profile
   ts6-fixture down -v` then `make voice-prototype-fixture-up`.
   `-v` wipes the named volume so the fixture rebuilds defaults.

5. **Resends in trace?** `grep -i resend target/voice-prototype/*.log`.
   A handful of resends across a 30s run is acceptable for the
   prototype; sustained resends mean the OS dropped UDP frames
   (CPU-pinned dev box, or a noisy `iptables` ruleset on the loopback).

## Out of scope (for the prototype)

- **Browser client** — Phase 3.5 WS-7 (WebRTC bridge translator).
- **Production audio device handling** — synthesised tones are
  acceptable per [PURA-112](/PURA/issues/PURA-112) acceptance.
- **SFU / federation topology** — WS-3.
- **Latency budget** — WS-2 (parallel) feeds jitter-buffer / framing
  knobs into the production crate that follows this prototype.
- **Long-running stability** — covered by the WS-5 voice fixture
  extension on top of [PURA-106](/PURA/issues/PURA-106).

## Cleanroom

This crate is drafted from the upstream `tsclientlib` source tree
([`tsclientlib/examples/audio.rs`](https://github.com/ReSpeak/tsclientlib/blob/master/tsclientlib/examples/audio.rs)),
RFC 7587 (Opus), and the in-repo PURA-7 spike artefacts. The
forbidden `Agent-Fennec/ts6-manager` reference is **not** read.
