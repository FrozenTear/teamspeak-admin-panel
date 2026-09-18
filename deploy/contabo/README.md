# Contabo Music+Voice split (draft)

**DRAFT — do not merge, do not tag, do not apply on Contabo** until
unanimous critical +1 from Voice / Music / API / Panel / Sidecar /
Release **and** FrozenTear. Floki MOVE is forbidden.

Contabo production is **rootful `podman kube play` + `scripts/update.sh`**,
not Quadlet. Never `podman kube down --force`.

## Topology

Same pod `ts6-manager` (`hostNetwork: true`):

| Container | Image | Role | Pin |
|-----------|-------|------|-----|
| `fullstack` | `ts6-manager-fullstack` | Panel / API / Surreal / Scuffed site | **Keep** `2-5` / nice `-5` until cutover |
| `music` | `ts6-manager-music` | Only decode → Opus → TS6 send loop | Send threads `0-1` in-process; container unpinned |
| `sidecar` | `ts6-manager-sidecar` | MoQ | Unpinned |

`ts6-music` PVC is mounted on fullstack **and** music — do not orphan it.
Music does not open Surreal (`ts6-db` stays on fullstack).

## nproc=6 honesty (Release snapshot)

Host has CPUs `0-5`. Fullstack occupies `2-5`. The only cores that do
not overlap fullstack are `0-1`.

A **whole-container** `TS6_BOT_CPUSET=0-1` would pin ffmpeg / yt-dlp
onto send cores and violates Music. Exclusive non-0-1 cores for media
workers are impossible without overlapping fullstack or shrinking that
pin (out of scope).

**Option (1) implemented:** `TS6_BOT_SEND_CPUSET` / `TS6_BOT_CPUSET=0-1`
pins `voice-rt` send threads only. ffmpeg / yt-dlp / the Python warm
resolver stay unpinned (`TS6_BOT_DECODE_CPUSET` unset) and may contend
on 0-1 with send (and with Scuffed / host noise). The hook is there so
a later third slice works after seats + FrozenTear approve shrinking
fullstack.

## Control plane

Panel still talks to fullstack HTTP/WS. Fullstack sets
`MUSIC_RUNTIME_URL=http://127.0.0.1:3002` and does **not** spawn a
second send loop. Chat parser stays on the bot (TS client). SSE /
NowPlaying / `music_bot_latency` + `logTail` are proxied from the
music unit so Report bug still attaches wire marks.

Music probes are **exec** `ts6-manager-music --healthcheck-url`, not
kube `httpGet` (Podman 5.6 → in-container curl; this image has none).
