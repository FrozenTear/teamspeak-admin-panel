# `deploy/kube/` — Kubernetes-flavoured manifest for Podman

`podman kube play` reads this manifest and brings up the TS6 Manager
stack rootless on any Podman ≥ 4.4 host. The same YAML is portable to
a real Kubernetes cluster — but the supported runtime here is Podman.
Contabo production is this shape: a git checkout plus
`scripts/update.sh` — not Quadlet. For semantically-equivalent
systemd-managed deploys, see `deploy/quadlet/` (sibling workstream).

## Files

| File | Purpose |
|------|---------|
| `ts6-manager.yaml` | Pod + PVCs. Pod references a Secret named `ts6-manager-secrets`. |
| `secrets.example.yaml` | Template Secret. Copy → `secrets.yaml`, fill in real values, never commit. |

## Upgrade (existing host)

On a host that already has the pod and volumes (Contabo: a checkout
under a path like `/root/github/teamspeak-admin-panel`):

```bash
./scripts/update.sh v1.6.2
```

The script is cwd-agnostic. It `podman pull`s the fullstack, music,
and sidecar GHCR images for that tag (required — the manifest uses
`imagePullPolicy: IfNotPresent`), writes a temp manifest so all three
share the tag, `podman kube down`s the committed YAML **without**
`--force`, plays the temp file (pod-only if `podman secret exists
ts6-manager-secrets`, otherwise concatenates
`deploy/kube/secrets.yaml`), curls fullstack
`http://127.0.0.1:3001/health` **and** music
`http://127.0.0.1:3002/health`, then re-applies the Contabo soft CPU
pin (see [Contabo soft CPU pin](#contabo-soft-cpu-pin)).

Never `podman kube down --force` — that wipes `ts6-data` / `ts6-db` /
`ts6-music`. Confirm volumes survived with
`podman volume ls --filter name=^ts6-`.

Manual `sed` / concat / play steps are in [Appendix: manual kube
path](#appendix-manual-kube-path).

## Contabo soft CPU pin

`podman kube play` does not persist HostConfig `CpusetCpus` or process
nice. After both health checks succeed, `update.sh` runs
`scripts/apply-fullstack-soft-pin.sh`, which sources
`deploy/contabo/soft-pin.env`.

**Live apply-ready default:** fullstack `CpusetCpus=2-5` plus nice
`-5` on `ts6-manager-fullstack`. Do **not** shrink that pin on Contabo
until Robert picks packing **A** and sets `TS6_SOFT_PIN_SHRINK_ACK=1`.
Sidecar stays unpinned unless the file sets `TS6_SIDECAR_*`.

**Music unit (option 1, nproc=6).** `TS6_BOT_CPUSET` /
`TS6_BOT_SEND_CPUSET=0-1` is an **in-process** send-thread affinity
(`sched_setaffinity` on `voice-rt`), not HostConfig. Packing **C**
(music `podman update --cpuset-cpus=0-1`) is **rejected** — the
v1.6.15 Angerfist dig (und/C/stall **163/590/117**) showed
container-wide 0-1 traps ffmpeg on send cores. The apply script
refuses send-only music HostConfig.

`TS6_BOT_DECODE_CPUSET` is present on the music container but **empty**
until Robert picks **A** (`2-3` after fullstack→`4-5`) or **B**
(`2-5`, share Axum, fullstack stays `2-5`). `pin_decode_child` already
parks ffmpeg / yt-dlp / warm-resolver when that env is set. DECODE
must be set in this kube manifest (process env); `soft-pin.env` cannot
inject it into a running process.

Nice is host `renice` (`TS6_BOT_NICE`). `TS6_BOT_CHRT_SCHED` FIFO/RR
is opt-in, default off (no kube privileged / `CAP_SYS_NICE` default).
In-process `setpriority` as uid 10001 is EPERM. Disable pins by
emptying the vars, removing `soft-pin.env`, or pointing
`TS6_SOFT_PIN_ENV` at a host-local override (Floki / other hosts —
do not MOVE the bot runtime to Floki). A requested container cpuset
that `podman update` cannot apply fails the upgrade so Contabo does
not silently lose the pin.

## Bring up

```bash
# 1. Prepare your secrets (one-time).
cp deploy/kube/secrets.example.yaml deploy/kube/secrets.yaml
# Edit deploy/kube/secrets.yaml — set JWT_SECRET and (optionally) ENCRYPTION_KEY.

# 2. Pull or build the image (see "Image source" below).

# 3. Play the manifest. `podman kube play` accepts a single kube file
#    (multi-file args need Podman 5.0+), so concat the Secret + Pod
#    manifest first.
cat deploy/kube/secrets.yaml deploy/kube/ts6-manager.yaml > /tmp/ts6-manager.kube.yaml
podman kube play /tmp/ts6-manager.kube.yaml

# 4. Verify.
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

The committed manifest pins fullstack, music, and sidecar to the same
release tag (`…-fullstack:v1.6.2`, `…-music:v1.6.2`,
`…-sidecar:v1.6.2`). Bump all three on a release cut, or let
`scripts/update.sh TAG` override them. Images are published by
`.github/workflows/release.yml` — see `docs/ops/images.md`.

A blind `podman kube play` of the committed file without a prior
`podman pull` of those tags will keep stale layers (`IfNotPresent`)
or, if the host still has an older `:v1.0` pin in an old checkout,
downgrade. Always pull first — `update.sh` does this.

## Appendix: manual kube path

Prefer `./scripts/update.sh vX.Y.Z`. The steps below are the same
sequence without the helper (tag override, pull, down without
`--force`, play, health).

```bash
TAG=v1.6.2
podman pull "ghcr.io/frozentear/ts6-manager-fullstack:${TAG}"
podman pull "ghcr.io/frozentear/ts6-manager-sidecar:${TAG}"

sed -E \
  -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-fullstack:)[^[:space:]]+#\\1${TAG}#" \
  -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-sidecar:)[^[:space:]]+#\\1${TAG}#" \
  deploy/kube/ts6-manager.yaml > /tmp/ts6-manager.kube.override.yaml

# If the host already has podman secret ts6-manager-secrets:
podman kube down deploy/kube/ts6-manager.yaml   # never --force
podman kube play /tmp/ts6-manager.kube.override.yaml

# Otherwise concat secrets.yaml (copy from secrets.example.yaml first):
# cat deploy/kube/secrets.yaml /tmp/ts6-manager.kube.override.yaml \
#   > /tmp/ts6-manager.kube.yaml
# podman kube play /tmp/ts6-manager.kube.yaml

curl -fsS http://127.0.0.1:3001/health
./scripts/apply-fullstack-soft-pin.sh   # Contabo soft pin; no-op if unset
```

### Override to a local build (pre-publish smoke)

```bash
podman build -t localhost/ts6-manager-fullstack:dev -f Containerfile.fullstack .

# Override the image, concat with secrets, then play. `podman kube
# play` accepts a single kube file on Podman 4.4–4.x; multi-file is
# 5.0+.
sed 's|image: ghcr.io/.*ts6-manager-fullstack:.*|image: localhost/ts6-manager-fullstack:dev|; s|imagePullPolicy: IfNotPresent|imagePullPolicy: Never|' \
  deploy/kube/ts6-manager.yaml > /tmp/ts6-manager.kube.override.yaml
cat deploy/kube/secrets.yaml /tmp/ts6-manager.kube.override.yaml \
  > /tmp/ts6-manager.kube.yaml
podman kube play /tmp/ts6-manager.kube.yaml
```

`imagePullPolicy: Never` prevents Podman from trying to pull the
`localhost/...` image from a registry.

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
| 3002 | 3002 | Music unit loopback control (`MUSIC_RUNTIME_URL`) |
| 7080 | 7080 | MoQ sidecar HTTP control |
| 4443 | 4443 (UDP) | MoQ sidecar WebTransport |

The pod runs with `hostNetwork: true` (see "Network mode" below). All
listeners are on the host's network namespace directly — operators
fronting the manager with a reverse proxy (Caddy / nginx / Traefik)
should bind the proxy to the host and forward to `127.0.0.1:3001`.

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

- `./scripts/update.sh v1.6.2` (or a first-install concat + `kube play`) succeeds on a Podman ≥ 4.4 host with the published `v1.6.2` fullstack + sidecar images available.
- `curl http://localhost:3001/health` returns 200.
- `podman kube down deploy/kube/ts6-manager.yaml` cleans up the pod.
- Data on PVCs `ts6-data`, `ts6-db` and `ts6-music` survives `kube down` and is reachable on the next `kube play` — including a yt-dlp cookie uploaded via Settings.
