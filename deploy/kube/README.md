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

The script is cwd-agnostic. It `podman pull`s both GHCR images for
that tag (required — the manifest uses `imagePullPolicy: IfNotPresent`),
writes a temp manifest so fullstack **and** sidecar share the tag,
`podman kube down`s the committed YAML **without** `--force`, plays
the temp file (pod-only if `podman secret exists ts6-manager-secrets`,
otherwise concatenates `deploy/kube/secrets.yaml`), and curls
`http://127.0.0.1:3001/health`.

Never `podman kube down --force` — that wipes `ts6-data` / `ts6-db` /
`ts6-music`. Confirm volumes survived with
`podman volume ls --filter name=^ts6-`.

Manual `sed` / concat / play steps are in [Appendix: manual kube
path](#appendix-manual-kube-path).

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

The committed manifest pins both images to the same release tag
(`ghcr.io/frozentear/ts6-manager-fullstack:v1.6.2` and
`ghcr.io/frozentear/ts6-manager-sidecar:v1.6.2`). Bump both on a
release cut, or let `scripts/update.sh TAG` override them. Images are
published by `.github/workflows/release.yml` — see
`docs/ops/images.md`.

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
(30s delay, 30s period) probes against `GET /health` on both
fullstack (`:3001`) and sidecar (`:7080`). These are kube `httpGet`
probes — Podman/kubelet issues the HTTP request itself, so the sidecar
image does not need curl/wget (Quadlet uses
`ts6-media-sidecar --healthcheck-url` for the same reason). Podman
respects probe semantics from v4.4 onward.

## Topology

```
Pod ts6-manager
├── container fullstack  (port 3001, uid 10001, non-root)
│    ├── PVC ts6-data  → /var/lib/ts6-manager       (state root / uploads)
│    ├── PVC ts6-db    → /var/lib/ts6-manager/db    (SurrealKV)
│    └── PVC ts6-music → /var/lib/ts6-manager/music
└── container sidecar    (7080/tcp, 4443/udp, uid 10002)
```

This matches the Quadlet `ts6-manager.pod` topology in
`deploy/quadlet/` (sibling workstream) and the default services in
`podman-compose.yml` (dev).

## Definition of done check

- `./scripts/update.sh v1.6.2` (or a first-install concat + `kube play`) succeeds on a Podman ≥ 4.4 host with the published `v1.6.2` fullstack + sidecar images available.
- `curl http://localhost:3001/health` returns 200.
- `podman kube down deploy/kube/ts6-manager.yaml` cleans up the pod.
- Data on PVCs `ts6-data`, `ts6-db` and `ts6-music` survives `kube down` and is reachable on the next `kube play` — including a yt-dlp cookie uploaded via Settings.
