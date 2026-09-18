# Contabo Music+Voice split (draft)

**DRAFT — do not merge, do not tag, do not apply a packing change on
Contabo** until Robert picks A vs B, then unanimous critical +1 from
Voice / Music / API / Panel / Sidecar / Release **and** FrozenTear.
Floki MOVE is forbidden. Live soft pin stays fullstack `2-5` / `-5`.

Contabo production is **rootful `podman kube play` + `scripts/update.sh`**,
not Quadlet. Never `podman kube down --force`.

## Topology

Same pod `ts6-manager` (`hostNetwork: true`):

| Container | Image | Role | Pin (live apply-ready) |
|-----------|-------|------|------------------------|
| `fullstack` | `ts6-manager-fullstack` | Panel / API / Surreal / Scuffed site | **Live `2-5` / nice `-5`** until Robert picks A |
| `music` | `ts6-manager-music` | Only decode → Opus → TS6 send loop | SEND `0-1` in-process; HostConfig **unset**; DECODE **empty** |
| `sidecar` | `ts6-manager-sidecar` | MoQ | Unpinned |

`ts6-music` PVC is mounted on fullstack **and** music — do not orphan it.
Music does not open Surreal (`ts6-db` stays on fullstack).
`MUSIC_RUNTIME_URL` / SSE / bug-report / `warm_resolver` (music-only
when the URL is set) are unchanged.

## nproc=6 honesty + packing A vs B vs C

Host has CPUs `0-5`. **Live** fullstack occupies `2-5`. Exclusive
DECODE off send `0-1` needs a third slice — that is packing **A**
(shrink fullstack) and is **gated**, not the apply-ready default.

| Packing | Fullstack HostConfig | SEND (in-process) | DECODE (`pin_decode_child`) | Music HostConfig | Status |
|---------|----------------------|-------------------|-----------------------------|------------------|--------|
| **live** | `2-5` / `-5` | `0-1` | unset (ffmpeg may land on `0-1`) | unset | **apply-ready** |
| **A** | `4-5` / `-5` (`3-5` if too tight) | `0-1` | `2-3` | prefer `0-3` | gated — Robert + `TS6_SOFT_PIN_SHRINK_ACK=1` |
| **B** | stays `2-5` | `0-1` | `2-5` (share Axum) | unset / `0-5` | gated fallback |
| **C** | `2-5` | `0-1` | inside `0-1` | **`0-1`** | **REJECT** |

v1.6.15 option-1 + Angerfist dig **163/590/117** und/C/stall per min
(worse than Sep17 soft-pin-only HS PASS 133/248/60) is why C is
rejected: container-wide `0-1` traps ffmpeg on send cores. Hypothesis
(non-binding): that contention drives `C_loop_deferral`.

`apply-fullstack-soft-pin.sh` refuses send-only music HostConfig and
refuses fullstack `4-5` / `3-5` unless `TS6_SOFT_PIN_SHRINK_ACK=1`.
A shrink may regress Panel/API overnight — that risk is why live
stays `2-5` until Robert ticks cutover.

**Option (1) still live:** `TS6_BOT_SEND_CPUSET` / `TS6_BOT_CPUSET=0-1`
pins `voice-rt` only. `pin_decode_child` is wired for ffmpeg / yt-dlp /
warm-resolver; kube `TS6_BOT_DECODE_CPUSET` stays empty until A or B
is chosen (must set kube env, not only `soft-pin.env`). Nice is host
`renice` on the music pid. `chrt` FIFO/RR is opt-in env, default off.

## Control plane

Panel still talks to fullstack HTTP/WS. Fullstack sets
`MUSIC_RUNTIME_URL=http://127.0.0.1:3002` and does **not** spawn a
second send loop. Chat parser stays on the bot (TS client). SSE /
NowPlaying / `music_bot_latency` + `logTail` are proxied from the
music unit so Report bug still attaches wire marks.

Music probes are **exec** `ts6-manager-music --healthcheck-url`, not
kube `httpGet` (Podman 5.6 → in-container curl; this image has none).
