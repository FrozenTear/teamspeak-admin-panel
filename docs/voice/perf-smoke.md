# Music-bot pipeline perf smoke

PURA-162 (WS-Perf) — sustained-load + latency smoke harness for the music-bot
audio pipeline (yt-dlp / FFmpeg → Opus 20 ms → wall-clock pacer → TS6 voice
frame). Lives in this repo so [WS-Gate](#) can rerun it before tagging v1.0.

Binary: [`crates/music-bot-audio/src/bin/perf_smoke.rs`](../../crates/music-bot-audio/src/bin/perf_smoke.rs).
Driver: [`scripts/perf-smoke.sh`](../../scripts/perf-smoke.sh).

## What it measures

Per-frame:

- **Pacer drift** — `recv_at − scheduled_at`. Summarised as p50 / p95 / p99 /
  max in milliseconds, with the first `WARMUP_FRAMES = 12` (~240 ms) skipped
  so cold-start spikes don't poison the steady-state percentile.
- **Cumulative drift** — the last frame's signed drift; should stay close to
  zero across a 30-minute run.

Every `--sample-interval-ms` (default 1 s):

- **CPU%** — delta of `utime + stime` from `/proc/self/stat` per wall-clock
  second, divided by `sysconf(_SC_CLK_TCK)`, scaled to a single-core
  percentage.
- **RSS** — `/proc/self/statm` field 2 × page size.
- **FD count** — entries under `/proc/self/fd`.

Leak deltas:

- **RSS growth %** — `(rss_end − rss_start) / rss_start × 100`.
- **FD growth** — `fd_end − fd_start`. Any positive value is a smoke signal
  that something is being held that should not be.

## How to run

```sh
# Fast 60 s gate — synthetic-tone source, no external deps.
scripts/perf-smoke.sh quick

# 30-minute sustained-load profile — what WS-Gate consumes.
scripts/perf-smoke.sh sustained

# Fold in ffmpeg subprocess cold-start (needs `ffmpeg` on PATH).
scripts/perf-smoke.sh ffmpeg crates/music-bot-audio/tests/fixtures/sine_440_1s_mono_48k.wav
```

Reports land under [`qa-evidence/perf-smoke/`](../../qa-evidence/perf-smoke/)
as `${mode}-${utc}.json`. The driver stamps `PERF_SMOKE_GIT_SHA` (short
12-char) into the report so diffs against a future commit are unambiguous.

You can also drive the binary directly, e.g. with custom budgets:

```sh
cargo run -q --release -p music-bot-audio --bin perf-smoke -- \
    --duration-seconds 120 \
    --source synthetic \
    --budget-drift-p99-ms 5 \
    --budget-drift-max-ms 10 \
    --budget-cpu-percent 25 \
    --budget-rss-growth-percent 5 \
    --budget-fd-growth 0 \
    --output /tmp/perf-smoke-custom.json
```

Exit code is `0` if every configured budget passed and `1` otherwise — so
the script can be wired directly into a CI / release-gate step.

## Synthetic by default, ffmpeg by opt-in

The `synthetic` source removes upstream variance:

- yt-dlp + ffmpeg add seconds of *buffering* before any 20 ms frame is
  emitted — that's a network + decoder property, not a pipeline-pacing
  property.
- The synthetic sine generator is in-process and non-blocking, so the
  pipeline encode loop is the only source of jitter the smoke sees.

The `ffmpeg` source is available for runs that explicitly want subprocess
cold-start folded into the budget. The pacer's first ~3 frames will spike;
the harness already excludes the first 12 frames from p99 / max
calculations, which is enough to absorb that.

## Budgets

Default budgets (override via `--budget-*` flags):

| Metric | Default budget | Source |
|---|---|---|
| Pacer drift p99 (steady-state) | ≤ 15 ms | 2–3× regression alarm over the 1–5 ms typical floor on a contended dev host |
| Pacer drift max (steady-state) | ≤ 50 ms | Large-spike alarm; clean release host should sit < 10 ms |
| CPU mean (single core) | ≤ 25 % | Music-bot pipeline on a modern x86 core, ample headroom for libopus + tokio |
| RSS growth over the run | ≤ 15 % | Tiny-process headroom for allocator settling; still flags a real leak on larger processes (15 % of 100 MB = 15 MB) |
| FD growth over the run | ≤ 0 | No FD should be left open by a steady-state worker |

These are not the absolute floors the pipeline hits — they are the **gate
thresholds** below which we ship. Baseline numbers from the current `main`
branch live alongside this doc in the PURA-162 issue thread.

## What this does NOT cover

- **Mouth-to-ear latency**. That's measured end-to-end in the WS-4 voice
  prototype (`crates/ts6-voice-prototype`) with PCM-in / PCM-out wall-clock
  capture on two clients. See [`docs/voice/0001-latency-budget.md`](./0001-latency-budget.md).
  This smoke measures the music-bot pipeline's *internal* contribution to
  that budget, which is the encode + pacer + handoff path.
- **Network jitter under loss**. WS-1's `tsclientlib` wire send sits below
  this harness's output; loss-resilience belongs to its receive jitter
  buffer + the TS6 server forward.
- **Multi-bot fan-out**. Single pipeline instance per run. Multi-instance
  contention is a separate workstream if the music-bot fleet ever needs
  benchmarking.

## Hand-off to WS-Gate

WS-Gate runs `scripts/perf-smoke.sh sustained` against the published
fullstack OCI image (once [WS-OPS-Images](#) lands) on a clean rootless
Podman host. Pass = ship-ready for v1.0. Fail = diagnose against the JSON
report's `samples` and `budgets` fields before tagging.

The harness lives in the main workspace, so the published `Containerfile.fullstack`
already carries the `perf-smoke` binary as soon as `cargo build --release`
runs inside the image build. WS-OPS-Images should expose it as a labelled
entrypoint (`OCI label: tsh.perf-smoke=true`) or simply run it from the
host against the image with `--network=host` for parity with the rest of
the [TS6 fixture](./../ts6-fixture.md).
