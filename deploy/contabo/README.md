# Contabo Music+Voice split (draft)

**DRAFT — do not merge, do not tag, do not apply on Contabo** until
unanimous critical +1 from Voice / Music / API / Panel / Sidecar /
Release **and** CoS/FrozenTear. Floki MOVE is forbidden. Live Contabo
soft pin stays fullstack `2-5` / `-5` until merge + tag + `update.sh`.

Contabo production is **rootful `podman kube play` + `scripts/update.sh`**,
not Quadlet. Never `podman kube down --force`.

## Topology

Same pod `ts6-manager` (`hostNetwork: true`):

| Container | Image | Role | Pin (packing B, Robert) |
|-----------|-------|------|-------------------------|
| `fullstack` | `ts6-manager-fullstack` | Panel / API / Surreal / Scuffed site | **stays `2-5` / nice `-5`** (no shrink) |
| `music` | `ts6-manager-music` | Only decode → Opus → TS6 send loop | SEND `0-1` in-process; HostConfig **unset**; DECODE **`2-5`** (share Axum) |
| `sidecar` | `ts6-manager-sidecar` | MoQ | Unpinned |

`ts6-music` PVC is mounted on fullstack **and** music — do not orphan it.
Music does not open Surreal (`ts6-db` stays on fullstack).
`MUSIC_RUNTIME_URL` / SSE / bug-report / `warm_resolver` (music-only
when the URL is set) are unchanged.

## nproc=6 honesty + packing A vs B vs C

Host has CPUs `0-5`. Fullstack occupies `2-5`. Exclusive DECODE off
send `0-1` would need a third slice (packing **A**, fullstack shrink)
— Robert's final pick is **B**: DECODE shares `2-5` with Axum instead
of shrinking fullstack. Ignore earlier A pings.

| Packing | Fullstack HostConfig | SEND (in-process) | DECODE (`pin_decode_child`) | Music HostConfig | Status |
|---------|----------------------|-------------------|-----------------------------|------------------|--------|
| **B** | `2-5` / `-5` | `0-1` | `2-5` (share Axum) | unset / `0-5` | **apply-ready (Robert)** |
| **A** | `4-5` / `-5` (`3-5` if too tight) | `0-1` | `2-3` | prefer `0-3` | gated comment + `TS6_SOFT_PIN_SHRINK_ACK=1` |
| **C** | `2-5` | `0-1` | inside `0-1` | **`0-1`** | **REJECT** |

v1.6.15 option-1 + Angerfist dig **163/590/117** und/C/stall per min
(worse than Sep17 soft-pin-only HS PASS 133/248/60) is why C is
rejected: container-wide `0-1` traps ffmpeg on send cores. Hypothesis
(non-binding): parking DECODE on `2-5` avoids send-core contention
without a fullstack shrink.

`apply-fullstack-soft-pin.sh` refuses send-only music HostConfig and
refuses fullstack `4-5` / `3-5` unless `TS6_SOFT_PIN_SHRINK_ACK=1`
(A is not the default; ACK stays unset).

**Option (1) + B decode park:** `TS6_BOT_SEND_CPUSET` /
`TS6_BOT_CPUSET=0-1` pins `voice-rt` only. kube
`TS6_BOT_DECODE_CPUSET=2-5` parks ffmpeg / yt-dlp / warm-resolver via
`pin_decode_child`. Nice is host `renice` on the music pid. `chrt`
FIFO/RR is opt-in env, default off.

## Control plane

Panel still talks to fullstack HTTP/WS. Fullstack sets
`MUSIC_RUNTIME_URL=http://127.0.0.1:3002` and does **not** spawn a
second send loop. Chat parser stays on the bot (TS client). SSE /
NowPlaying / `music_bot_latency` + `logTail` are proxied from the
music unit so Report bug still attaches wire marks.

Music probes are **exec** `ts6-manager-music --healthcheck-url`, not
kube `httpGet` (Podman 5.6 → in-container curl; this image has none).
