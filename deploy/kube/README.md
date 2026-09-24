# `deploy/kube/` — Kubernetes-flavoured manifest for Podman

Start and restart this stack only with `./scripts/update.sh vX.Y.Z`.
The script rewrites a temp copy of the manifest and plays that. Do not
`podman kube play` the committed file: fullstack, music, and
`ts6-manager-sidecar` are `@UNRELEASED`, which podman rejects at image
pull. The same YAML shape is portable to a real Kubernetes cluster —
the supported runtime here is Podman on a host ≥ 4.4. Contabo
production is a git checkout plus `scripts/update.sh`, not Quadlet.
For semantically-equivalent systemd-managed deploys, see
`deploy/quadlet/` (sibling workstream).

## Files

| File | Purpose |
|------|---------|
| `ts6-manager.yaml` | Pod + PVCs. Pod references a Secret named `ts6-manager-secrets`. |
| `secrets.example.yaml` | Template Secret. Copy → `secrets.yaml`, fill in real values, never commit. |

## Upgrade (existing host)

On a host that already has the pod and volumes (Contabo: a checkout
under a path like `/root/github/teamspeak-admin-panel`):

```bash
./scripts/update.sh vX.Y.Z
```

The script is cwd-agnostic. It rewrites fullstack, music, and sidecar
(and any other `ts6-manager-*` image) onto that tag — whether the
checkout still has the committed `@UNRELEASED` placeholder or a legacy
`:vX.Y.Z` pin — `podman pull`s those GHCR images (required — the
manifest uses `imagePullPolicy: IfNotPresent`), `podman kube down`s
the committed YAML **without** `--force`, plays the temp file (pod-only
if `podman secret exists ts6-manager-secrets`, otherwise concatenates
`deploy/kube/secrets.yaml`), curls fullstack
`http://127.0.0.1:3001/health` **and** music
`http://127.0.0.1:3002/health`, then re-applies the Contabo soft CPU
pin (see [Contabo soft CPU pin](#contabo-soft-cpu-pin)).

Never `podman kube down --force` — that wipes `ts6-data` / `ts6-db` /
`ts6-music`. Confirm volumes survived with
`podman volume ls --filter name=^ts6-`. There is no hand-rolled
`sed` / `kube play` upgrade.

## Contabo soft CPU pin

`podman kube play` does not persist HostConfig `CpusetCpus` or process
nice. After both health checks succeed, `update.sh` runs
`scripts/apply-fullstack-soft-pin.sh`, which sources
`deploy/contabo/soft-pin.env`.

**Apply-ready default is packing B (Robert):** fullstack
`CpusetCpus=2-5` plus nice `-5` on `ts6-manager-fullstack`. No
fullstack shrink. `TS6_SOFT_PIN_SHRINK_ACK` stays unset. Sidecar
stays unpinned unless the file sets `TS6_SIDECAR_*`.

**Music unit (option 1, nproc=6).** `TS6_BOT_CPUSET` /
`TS6_BOT_SEND_CPUSET=0-1` is an **in-process** send-thread affinity
(`sched_setaffinity` on `voice-rt` wire-send threads only), not
HostConfig. kube `TS6_BOT_DECODE_CPUSET=2-5` pins `decode-rt`
(pipeline / fetch / bridge / resolve) and parks ffmpeg / yt-dlp /
warm-resolver via a pre_exec `sched_setaffinity` (leader
`pin_decode_child` is only a backup) on the fullstack slice (share
Axum — still off send `0-1`). Packing **C** (music `podman update --cpuset-cpus=0-1`)
is **rejected** — the v1.6.15 Angerfist dig (und/C/stall
**163/590/117**) showed container-wide 0-1 traps ffmpeg on send
cores. The apply script refuses any music HostConfig cpuset under
packing B (not only send-only `0-1`) and always refuses send-only
music HostConfig.

Packing **A** (fullstack→`4-5`, DECODE `2-3`, music HostConfig `0-3`)
stays a commented gated alt and needs `TS6_SOFT_PIN_SHRINK_ACK=1`.
DECODE must be set in this kube manifest (process env);
`soft-pin.env` cannot inject it into a running process.

Nice is host `renice` of the music leader **and** every `voice-rt` tid
(`TS6_BOT_NICE`). That walk is one-shot, after `/health`, inside
`update.sh` (Opus #66 L16). Tokio blocking-pool threads are also
named `voice-rt` and appear later (`spawn_blocking` /
`block_in_place`); they stay pinned to SEND `0-1` and inherit the
spawning thread's nice, so a parent the walk already reniced passes
`-5` on and a parent still at 0 does not. A music container restart
drops the nice until `apply-fullstack-soft-pin.sh` runs again.
`TS6_BOT_CHRT_SCHED` FIFO/RR
is opt-in, default off (no kube privileged / `CAP_SYS_NICE` default).
In-process `setpriority` as uid 10001 is EPERM. Do not add
`CAP_SYS_NICE`. Packing B stays. Do not MOVE the bot runtime to Floki.
Disable pins by
emptying the vars, removing `soft-pin.env`, or pointing
`TS6_SOFT_PIN_ENV` at a host-local override (Floki / other hosts —
do not MOVE the bot runtime to Floki). A requested container cpuset
that `podman update` cannot apply fails the upgrade so Contabo does
not silently lose the pin.

## Start / restart

First install and every later restart use the same command. One-time,
copy the secret template and fill it in. `update.sh` concatenates
`secrets.yaml` when the host does not already have `podman secret
ts6-manager-secrets`.

```bash
cp deploy/kube/secrets.example.yaml deploy/kube/secrets.yaml
# Edit deploy/kube/secrets.yaml — set JWT_SECRET and (optionally) ENCRYPTION_KEY.

./scripts/update.sh vX.Y.Z

curl http://localhost:3001/health
podman pod ps
podman logs ts6-manager-fullstack
```

## Bring down

```bash
podman kube down deploy/kube/ts6-manager.yaml
```

`kube down` stops and removes the pod + containers, but leaves the
PVC-backed named volumes (`ts6-data`, `ts6-db`, `ts6-music`) intact so
data survives. `--force` is the opt-in flag for wiping volumes — do not
pass it during normal redeploys.

> Note: podman's `kube down` output **always** prints a literal
> `Volumes removed:` header, even when no volumes were removed. Read
> the lines *after* that header — if there are none (the next line is
> shell or your next command), no volumes were removed. Confirm with:
>
> ```bash
> podman volume ls --filter name=^ts6-
> ```
>
> `ts6-data`, `ts6-db` and `ts6-music` should all still be listed after
> `kube down`. (Verified on Podman 5.8.2 rootless.)

To wipe data too:

```bash
podman volume rm ts6-data ts6-db ts6-music
```

## Image source

The committed manifest does not pin a release tag. Fullstack, music,
and sidecar are `ghcr.io/frozentear/ts6-manager-<name>@UNRELEASED` — not
an image tag, so podman rejects the reference at pull. `./scripts/update.sh vX.Y.Z`
is what substitutes a published tag (and what still substitutes a
legacy `:vX.Y.Z` if a live checkout has one). Images are published by
`.github/workflows/release.yml` — see `docs/ops/images.md`.

A blind `podman kube play` of the committed file fails at image pull
instead of starting whatever layers happen to be local. Do not play
it. `update.sh` pulls the requested tag first (`IfNotPresent`).

## Volumes

| PVC | Path inside container | Purpose |
|-----|-----------------------|---------|
| `ts6-data` | `/var/lib/ts6-manager` | State root — persists `DATA_DIR` operator uploads (yt-dlp cookie file) and music-bot TS identity files (PURA-357). `ts6-db` / `ts6-music` nest on top. |
| `ts6-db` | `/var/lib/ts6-manager/db` | SurrealKV embedded store (DATABASE_URL). Holds all configured bots, flows, rules, users, widgets — losing this volume loses that state across an upgrade. |
| `ts6-music` | `/var/lib/ts6-manager/music` | Music-bot library (MUSIC_DIR) |

PVCs map to Podman named volumes. Rootless Podman owns the chown
across the userns boundary — host bind-mounts under rootless break
SurrealKV with EACCES (PURA-67), so named-volume PVCs are the
documented production layout.

## Ports

| Container port | Host port | Notes |
|----------------|-----------|-------|
| 3001 | 3001 | HTTP, served by the Dioxus fullstack server |
| 3002 | loopback only | Music unit control (`--listen 127.0.0.1:3002`, `MUSIC_RUNTIME_URL`). Not a public listener; do not put it on Caddy. |
| 7080 | loopback only | MoQ sidecar HTTP control (`--http-listen 127.0.0.1:7080`). Not a public listener; do not put it on Caddy. |
| 4443 | 4443 (UDP) | MoQ sidecar WebTransport |

The pod runs with `hostNetwork: true` (see "Network mode" below). All
listeners are on the host's network namespace directly — operators
fronting the manager with a reverse proxy (Caddy / nginx / Traefik)
should bind the proxy to the host and forward to `127.0.0.1:3001`.

Contabo public HTTPS (draft, **not applied**): once
`panel.scuffedcrew.no` is live on the existing host Caddy, public
access is that hostname — not raw `:3001`. Music `:3002` and sidecar
`:7080` stay loopback-only (`MUSIC_RUNTIME_URL` remains
`http://127.0.0.1:3002`). Snippet + DNS gate:
[`deploy/contabo/Caddyfile.panel.snippet`](../contabo/Caddyfile.panel.snippet).
Soft pin / packing B / Floki MOVE NO are unchanged.

## Network mode

The pod runs with `hostNetwork: true`. This is **load-bearing**, not
a perf tweak.

The manager's WebQuery client reaches the TS6 fixture — and any
operator-added production TS6 server colocated on the same host —
over loopback. Without host networking the pod sits on the default
rootless pod-bridge and its egress goes through passt, which is the
same path that wedges TS6 6.0.0-beta9 WebQuery after ~5 requests
(see [`docs/ts6-fixture.md`](../../docs/ts6-fixture.md) "Why
`--network=host` is mandatory" and PURA-105). The dashboard tick
worker fans out 4 reads every 5 s, so the wedge fires within ~30 s
of operator activity.

`hostNetwork: true` drops passt from the call path. The external
surface area is unchanged from the previous bridged-+-`hostPort`
layout because the pod already advertised those ports as `hostPort`.
Operators with a TS6 server reachable on the LAN (not localhost) are
unaffected — that path was never on passt.

## Health checks

The manifest defines readiness (5s delay, 10s period) and liveness
(30s delay, 30s period) probes against `GET /health`. Fullstack
(`:3001`) uses kube `httpGet` — `podman kube play` turns that into
an in-container `curl` HealthCmd (and *overrides* any image
HEALTHCHECK), which is why `Containerfile.fullstack` installs curl.
Music (`:3002`) and sidecar (`:7080`) use an `exec` probe of the
binary `--healthcheck-url` because those images have neither curl
nor wget; an `httpGet` probe fails at exec (`curl: not found`) and
restart-loops ~every 105s. Do not switch music/sidecar to httpGet.
Podman applies the liveness probe as `HealthConfig` from v4.4
onward.

## Topology

```
Pod ts6-manager
├── container fullstack  (port 3001, uid 10001, non-root)
│    ├── PVC ts6-data  → /var/lib/ts6-manager       (state root / uploads)
│    ├── PVC ts6-db    → /var/lib/ts6-manager/db    (SurrealKV)
│    └── PVC ts6-music → /var/lib/ts6-manager/music
├── container music      (port 3002, uid 10001 — shared volume owner)
│    ├── PVC ts6-data  → /var/lib/ts6-manager       (identities + cookies; no Surreal open)
│    └── PVC ts6-music → /var/lib/ts6-manager/music (do not orphan this PVC)
└── container sidecar    (7080/tcp, 4443/udp, uid 10002)  # stays unpinned
```

This matches the Quadlet `ts6-manager.pod` topology in
`deploy/quadlet/` (sibling workstream) and the default services in
`podman-compose.yml` (dev).

## Definition of done check

- `./scripts/update.sh vX.Y.Z` is the only start/restart and succeeds on a Podman ≥ 4.4 host when that tag is published for fullstack, music, and sidecar.
- `podman kube play` of the committed manifest fails at image pull (`@UNRELEASED` on fullstack, music, and sidecar).
- `curl http://localhost:3001/health` returns 200.
- `podman kube down deploy/kube/ts6-manager.yaml` cleans up the pod.
- Data on PVCs `ts6-data`, `ts6-db` and `ts6-music` survives `kube down` and is reachable after the next `./scripts/update.sh` — including a yt-dlp cookie uploaded via Settings.
