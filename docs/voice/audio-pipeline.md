# Music-bot audio pipeline

PURA-119 (WS-2) — yt-dlp / FFmpeg / Opus 20 ms / ICY metadata.
Crate: [`crates/music-bot-audio`](../../crates/music-bot-audio).

## Block diagram

```
URL / file / radio
        │
        ▼
┌──────────────────┐    optional   ┌───────────────┐
│  source picker   │───────────────│  ICY splitter │
│  (AudioSource…)  │               │  (radio only) │
└──────────────────┘               └───────────────┘
        │ raw bytes / lavfi input         │ NowPlaying
        ▼                                 │
┌──────────────────┐                      │
│   ffmpeg child   │  s16le PCM 48kHz     │
│   pipe:0 → :1    │─────────────────┐    │
└──────────────────┘                 │    │
                                     ▼    │
                            ┌───────────────────┐
                            │ OpusFrameEncoder  │   audiopus
                            │ 20 ms / 48 kHz    │   (libopus 1.x)
                            └───────────────────┘
                                     │ Opus packet
                                     ▼
                            ┌───────────────────┐
                            │  WallClockPacer   │   scheduled_at = start
                            │   drift-free      │     + i × 20 ms
                            └───────────────────┘
                                     │
                  ┌──────────────────┴──────────────────┐
                  │                                     │
                  ▼ frames (mpsc<OpusFrame>)            ▼ events (broadcast<…>)
            WS-1 bot worker                       WS-1 / REST / FE-PAGES
            sends OutAudio::C2S                   NowPlaying / EndOfStream
            on tsclientlib                        / Warning
```

## Format decision: `ffmpeg` over `symphonia`

The WS-2 brief allowed either; we picked **`ffmpeg` subprocess** for the music-bot audio path. Rationale:

- The music bot has to accept whatever the operator throws at it: mp3, aac, flac, ogg, opus, webm/m4a (yt-dlp), HLS variants, ICY radio, and whatever next. `ffmpeg` covers all of those out of the box, including resampling to 48 kHz s16le.
- `symphonia` is a great pure-Rust effort but codec coverage is per-format opt-in (e.g. `symphonia-codec-aac`) and several modern codecs are still WIP. Tracking the matrix and shipping format-specific decoders is weeks of work that doesn't move the music-bot product forward.
- `ffmpeg` is already an off-the-shelf piece in the broader voice ecosystem (matches the "open standards before bespoke" hard constraint on [PURA-117](../../.. /PURA/issues/PURA-117)).
- Self-host posture: `ffmpeg` is in every distro's package manager and ships in the standard `Containerfile.fullstack` we already use.

The decision is local to WS-2 — if a future workstream wants a pure-Rust path for sandboxing reasons, swap the source impl. The `PcmSource` trait shields the rest of the pipeline.

## Public API (the seam WS-1 plugs into)

```rust
use music_bot_audio::{AudioPipeline, PipelineConfig, PipelineEvent};
use music_bot_audio::source::AudioSourceSpec;

let mut pipeline = AudioPipeline::spawn(
    AudioSourceSpec::IcyRadio { url: "http://stream.example/radio".into() },
    PipelineConfig::default(),  // mono, 64 kbps, OpusApplication::Audio
).await?;

let mut frames = pipeline.take_frames();          // mpsc<OpusFrame>
let mut events = pipeline.events();               // broadcast<PipelineEvent>

while let Some(frame) = frames.recv().await {
    tokio::time::sleep_until(frame.scheduled_at.into()).await;
    // wrap `frame.bytes` in an OutAudio::C2S { codec: OpusVoice, .. } and
    // hand to `tsclientlib::Connection::send_audio(...)`.
}
pipeline.shutdown().await;
```

`OpusFrame.scheduled_at` is the only thing WS-1 needs to honour for jitter-free wire transmission. The pacer is drift-free: `frame[i].scheduled_at == frame[0].scheduled_at + i × 20 ms`, exactly.

## Sources

| `AudioSourceSpec` variant | Use for                                        | Backend                                             |
| ------------------------- | ---------------------------------------------- | --------------------------------------------------- |
| `SyntheticTone`           | Tests, demo, sanity checks                     | In-process sine generator                           |
| `Ffmpeg { input }`        | Local files, simple HTTP, anything ffmpeg `-i` accepts | `ffmpeg` subprocess                                 |
| `YtDlp { url }`           | YouTube, SoundCloud, generic media URLs        | warm resolver → direct URL → `ffmpeg`; falls back to `yt-dlp` → `ffmpeg` subprocess pipeline |
| `IcyRadio { url }`        | Shoutcast / Icecast streams; raises `NowPlaying` | reqwest HTTP fetch → ICY splitter → `ffmpeg` stdin |

ICY metadata is only surfaced for the `IcyRadio` variant. yt-dlp / file / generic ffmpeg inputs do not see ICY.

## Persistent yt-dlp resolver (PURA-359)

`YtDlp { url }` no longer spawns a fresh `yt-dlp` subprocess on every `!play`.
[PURA-355](/PURA/issues/PURA-355) measured ~2 s of every resolution as pure
yt-dlp *process startup* — importing the extractor registry — re-paid per
track. `crates/music-bot-audio/src/resolver.rs` instead runs a long-lived
Python process (`yt_resolver.py`, embedded via `include_str!`) that imports
`yt_dlp` once at boot and resolves tracks over a unix-domain socket; the warm
process returns the direct `bestaudio` URL and `ffmpeg` consumes it directly.
Measured on contabo-dev: ~6.5 s cold subprocess vs ~3.8 s warm — **−~2.7 s**.

- The manager warms the resolver at boot (`music_bot::warm_resolver()` in
  `main.rs`), so the `import yt_dlp` cost is paid before the first `!play`.
- A background supervisor restarts the process on exit; after repeated fast
  crashes it gives up and leaves the subprocess fallback in effect.
- **Every failure path falls back to the `yt-dlp` subprocess** — service down,
  mid-restart, malformed reply, or a genuine resolution failure. A broken
  resolver can slow `!play` but cannot break it.
- `YT_RESOLVER_DISABLE=1` pins playback to the subprocess path.
- The container imports `yt_dlp` straight out of the same `yt-dlp` zipapp the
  subprocess fallback runs (`YT_DLP_ZIPAPP`), so the two never drift versions;
  a manager restart after an image upgrade re-imports the new zipapp for free.

## Subprocess hygiene

- `ffmpeg` and `yt-dlp` children are spawned with `kill_on_drop(true)` and explicitly `start_kill()`'d in their respective `Drop` impls.
- The WS-1 supervisor controls the pipeline via the `AudioPipeline` handle. Dropping the pipeline aborts the worker task and reaps every child.
- The ICY fetcher task is `JoinHandle::abort()`'d on `IcyRadioSource::drop`.

The `tests/fixture_pacing.rs::ffmpeg_subprocess_cleanup_on_cancel` test verifies via `pgrep` that no orphan ffmpeg lingers after a cancelled pipeline. Run it whenever the source plumbing is touched.

## Acceptance / smoke tests

### 1. Unit tests (always-on)

```sh
cargo test -p music-bot-audio
```

Covers: ICY splitter / parser, encoder boundary cases, pacer drift, synthetic source semantics, pipeline cancel-on-consumer-drop.

### 2. Pacing acceptance (PURA-119 acceptance bar — `--ignored`, needs ffmpeg)

```sh
cargo test -p music-bot-audio --test fixture_pacing -- --ignored --nocapture --test-threads=1
```

Feeds the committed `tests/fixtures/sine_440_1s_mono_48k.wav` through the full ffmpeg → audiopus → pacer chain and asserts:

- 50 Opus frames arrive (1 s ÷ 20 ms = 50).
- Per-frame jitter ≤ ±2 ms after a 3-frame ffmpeg cold-start window.
- Cumulative drift ≤ ±5 ms.
- Cancelling the pipeline mid-stream leaves no orphan ffmpeg subprocess (verified via `pgrep`).

Locally the test typically lands at `max per-frame drift 1 ms, total drift -1 ms`.

### 3. ICY radio smoke recipe (manual, not CI)

```sh
# Replace with any reachable Shoutcast/Icecast endpoint.
RUST_LOG=info cargo run -q -p music-bot-audio \
    --bin music-bot-audio-demo -- \
    --max-frames 200 --pace \
    icy --url "http://stream.example/radio"
```

Expected output:

- `INFO music_bot_audio_demo: starting pipeline …`
- One or more `INFO … NowPlaying title="<artist> - <track>" source="<url>"` lines, one per `StreamTitle` change.
- Periodic `INFO … stats … frame=50/100/… max_jitter_ms=0/1` updates.
- `INFO … exit frames=200 max_jitter_ms=…` on completion.

If the server returns no `icy-metaint` header you'll see `pipeline warning … icy stream … did not return icy-metaint; ICY metadata disabled` and the audio still plays — `NowPlaying` events are simply not emitted.

### 4. yt-dlp smoke recipe (manual)

```sh
RUST_LOG=info cargo run -q -p music-bot-audio \
    --bin music-bot-audio-demo -- \
    --max-frames 200 --pace \
    yt-dlp --url "<media URL>"
```

Reasonable inputs: short YouTube videos, SoundCloud tracks, direct .mp3 / .m4a URLs. yt-dlp does not raise ICY metadata even for radio URLs — use the `icy` subcommand for those.

## Operator dependencies

The pipeline shells out to:

- `ffmpeg` (any 4.x+; tested locally against 8.x). Required for everything except the `SyntheticTone` source.
- `yt-dlp` (recent — must support `-f bestaudio -o -`). Required only for the `YtDlp` source. YouTube changes its player cipher often; a yt-dlp more than a few months old will silently stop resolving formats. The `Containerfile.fullstack` pin (`YTDLP_VERSION`) must be bumped when that happens.
- A JavaScript runtime — **Deno** — on `PATH`. Modern YouTube requires solving a player signature / `n` challenge in JS (yt-dlp's EJS challenge solver). Without a JS runtime, signature solving fails, every real audio/video format is filtered out, and yt-dlp returns only storyboard images — the bot then reports the track as "unavailable" even when cookie auth succeeded. The `Containerfile.fullstack` installs Deno (`DENO_VERSION`); host installs must provide it too. Required only for the `YtDlp` source.
- `libopus` 1.x — pulled in by `audiopus` at compile time.

These are not provisioned by the workspace; the operator-side `Containerfile.fullstack` and the host package manager handle them. Compose-side runbooks live alongside `docs/ts6-fixture.md`.

### YouTube cookies (PURA-216)

YouTube increasingly requires a logged-in session for age-gated content, region-locked content, and "Sign in to confirm you're not a bot" rate-limited videos. yt-dlp accepts a Netscape `cookies.txt` file via `--cookies <path>`. The manager picks it up from the `YT_COOKIE_FILE` env var:

1. Generate the cookies file from a browser logged into youtube.com. Recommended: the [cookies.txt](https://addons.mozilla.org/firefox/addon/cookies-txt/) Firefox add-on. Export to `cookies.txt`.
2. Make the file readable by the manager process (the fullstack image runs as uid `10001` / `ts6:ts6`). Place it under `<DATA_DIR>` so it persists across image rebuilds — e.g. `/var/lib/ts6-manager/yt-cookies.txt`.
3. Set `YT_COOKIE_FILE=/var/lib/ts6-manager/yt-cookies.txt` in the manager's environment (Quadlet env file or kube `env:` entry).
4. Restart the manager. Boot summary logs `yt_cookie_file_set=true`. Each yt-dlp invocation passes `--cookies <path>`; absence of the env var means no flag is added.

Cookies expire (typical YouTube cookies: weeks to months). A future UI ticket adds an upload + replace surface inside the operator panel; for now, replace the file in place and restart the manager.

## What this crate does not do

- TS6 wire transmission — that's WS-1's job. The pipeline only produces `OpusFrame { bytes, scheduled_at, … }`.
- Queue / playlist / library — owned by [PURA-121](../../../PURA/issues/PURA-121) (WS-3). The pipeline plays one source at a time and exits at EOF; chaining sources is a higher-level concern.
- `!play` / `!stop` chat commands — owned by WS-4.
- REST endpoints — owned by WS-5.
- FE-PAGES Music Bots UI — owned by WS-6.

## Related

- [PURA-117](../../../PURA/issues/PURA-117) — Phase 4 epic.
- [PURA-118](../../../PURA/issues/PURA-118) — WS-1 (bot lifecycle, music-bot crate). This crate is the audio producer for WS-1's bot worker.
- [PURA-110](../../../PURA/issues/PURA-110) — first audiopus + Opus 20 ms framing landing in this codebase (TS6 voice fixture audio-E2E).
- [PURA-112](../../../PURA/issues/PURA-112) — the live two-clients-talking prototype that established the audiopus + 20 ms encoder/decoder shape we mirror here.
