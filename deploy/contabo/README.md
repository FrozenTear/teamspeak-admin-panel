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

| Packing | Fullstack HostConfig | SEND (in-process) | DECODE (pre_exec) | Music HostConfig | Status |
|---------|----------------------|-------------------|-----------------------------|------------------|--------|
| **B** | `2-5` / `-5` | `0-1` | `2-5` (share Axum) | **unset** | **apply-ready (Robert)** |
| **A** | `4-5` / `-5` (`3-5` if too tight) | `0-1` | `2-3` | prefer `0-3` | gated comment + `TS6_SOFT_PIN_SHRINK_ACK=1` |
| **C** | `2-5` | `0-1` | inside `0-1` | **`0-1`** | **REJECT** |

v1.6.15 option-1 + Angerfist dig **163/590/117** und/C/stall per min
(worse than Sep17 soft-pin-only HS PASS 133/248/60) is why C is
rejected: container-wide `0-1` traps ffmpeg on send cores. Hypothesis
(non-binding): parking DECODE on `2-5` avoids send-core contention
without a fullstack shrink.

`apply-fullstack-soft-pin.sh` refuses **any** music container HostConfig
cpuset under packing B (not only literal `0-1`) and always refuses
send-only music HostConfig. It also refuses fullstack `4-5` / `3-5`
unless `TS6_SOFT_PIN_SHRINK_ACK=1` (A is not the default; ACK stays
unset). A wider music cpuset such as `0-3` is packing A, and only
after that shrink ACK.

**Option (1) + B decode park:** `TS6_BOT_SEND_CPUSET` /
`TS6_BOT_CPUSET=0-1` pins `voice-rt` wire-send threads only. kube
`TS6_BOT_DECODE_CPUSET=2-5` pins `decode-rt` (pipeline / fetch /
bridge / resolve) and parks ffmpeg / yt-dlp / warm-resolver via
pre_exec `sched_setaffinity` before exec (`pin_decode_child` is a
leader backup). Nice is host `renice` on the music leader and on
`voice-rt` tids (per-thread; the leader alone is not enough). The
walk is one-shot after music `/health` (Opus #66 L16). Tokio's
blocking pool reuses the `voice-rt` comm and is created later; those
threads inherit the spawning thread's nice and stay on SEND `0-1`.
A music container restart drops the nice until
`apply-fullstack-soft-pin.sh` runs again. In-process `setpriority`
of `-5` is EPERM as uid 10001 — do not add `CAP_SYS_NICE`. `chrt`
FIFO/RR is opt-in env, default off. Music HostConfig stays unset.
Packing B stays. Floki MOVE NO.

## Control plane

Panel still talks to fullstack HTTP/WS. Fullstack sets
`MUSIC_RUNTIME_URL=http://127.0.0.1:3002` and does **not** spawn a
second send loop. Chat parser stays on the bot (TS client). SSE /
NowPlaying / `music_bot_latency` + `logTail` are proxied from the
music unit so Report bug still attaches wire marks.

`MUSIC_RUNTIME_TOKEN` is optional and is not set by the kube manifest.
When it is set, the music container and the fullstack container must
share the same environment value, after the same trim (empty or
whitespace-only is unset). A non-UTF-8 value makes fullstack refuse to
start and is not printed. Fullstack then sends `Authorization: Bearer`
on every runtime call (including rehydrate and the event-stream proxy).
Unset on this loopback deploy leaves the control API open and sends no
header. A runtime 401 is a browser 502
`{"error":"music_runtime_auth"}`, not a panel session expiry. Do not
log the token.

Music probes are **exec** `ts6-manager-music --healthcheck-url`, not
kube `httpGet` (Podman 5.6 → in-container curl; this image has none).

## Panel HTTPS (host Caddy + Let’s Encrypt)

**DRAFT — do not apply on Contabo** until unanimous seat critical +1s
**and** CoS/FrozenTear **and** Robert. Soft pin stays packing B
(fullstack `2-5` / `-5`). Floki MOVE NO. This section is Caddy / DNS
/ docs only — it does not change `soft-pin.env` or packing B.

Contabo already runs Caddy v2.11.4 at `/etc/caddy/Caddyfile`
(`scuffedcrew.no` → `:3100`, `news.scuffedcrew.no` → `:8888`,
`ow.scuffedcrew.no` → `:3000`). Panel uses the same host Caddy:

`panel.scuffedcrew.no` → `reverse_proxy 127.0.0.1:3001`

Fullstack is `0.0.0.0:3001` today (`hostNetwork`). Once live, public
access is the hostname, not raw `:3001`. Contabo kube fullstack env
(`deploy/kube/ts6-manager.yaml`) sets
`FRONTEND_URL=https://panel.scuffedcrew.no`, `TRUSTED_PROXY_HOPS=1`,
and `TRUSTED_PROXY_CIDRS=127.0.0.1/32` (host Caddy on loopback; an
empty CIDR list trusts no proxy headers; #50/#51).

Snippet: [`Caddyfile.panel.snippet`](Caddyfile.panel.snippet).

| Step | Owner | Gate |
|------|-------|------|
| A `panel.scuffedcrew.no` → `194.163.163.153` **and preferably** AAAA → `2a02:c207:2309:9279::1` (same as `ow` / `news`) | Robert | **Prerequisite** — LE cannot mint until the A record answers |
| Append snippet to `/etc/caddy/Caddyfile`; `systemctl reload caddy` | Release | After unanimous + Robert. Never replace the existing file. Never SSH-apply from a draft PR. |

**Stays internal (do not put on public Caddy):** music `:3002` and
sidecar `:7080`. `MUSIC_RUNTIME_URL` stays
`http://127.0.0.1:3002`.

**Ownership (not this PR):** API owns the HSTS gate (no HSTS on
cleartext `:3001`). Panel owns the `http://` absolute-URL audit.
Do not add HSTS in the Caddy site block.
