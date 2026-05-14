# `deploy/kube/` — Kubernetes-flavoured manifest for Podman

`podman kube play` reads this manifest and brings up the TS6 Manager
stack rootless on any Podman ≥ 4.4 host. The same YAML is portable to
a real Kubernetes cluster — but the supported runtime here is Podman.
For semantically-equivalent systemd-managed deploys, see
`deploy/quadlet/` (sibling workstream).

## Files

| File | Purpose |
|------|---------|
| `ts6-manager.yaml` | Pod + PVCs. Pod references a Secret named `ts6-manager-secrets`. |
| `secrets.example.yaml` | Template Secret. Copy → `secrets.yaml`, fill in real values, never commit. |

## Bring up

```bash
# 1. Prepare your secrets (one-time).
cp deploy/kube/secrets.example.yaml deploy/kube/secrets.yaml
# Edit deploy/kube/secrets.yaml — set JWT_SECRET and (optionally) ENCRYPTION_KEY.

# 2. Pull or build the image (see "Image source" below).

# 3. Play the manifest.
podman kube play deploy/kube/secrets.yaml deploy/kube/ts6-manager.yaml

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
PVC-backed named volumes (`ts6-db`, `ts6-music`) intact so data
survives. To wipe data too:

```bash
podman volume rm ts6-db ts6-music
```

## Image source

The committed manifest pins
`ghcr.io/frozentear/ts6-manager-fullstack:latest`. That image is
published by the `WS-OPS-Images` workstream — see `Containerfile.fullstack`.

### Override to a local build (pre-publish smoke)

```bash
podman build -t localhost/ts6-manager-fullstack:dev -f Containerfile.fullstack .

# One-liner override using sed → podman kube play piping:
sed 's|image: ghcr.io/.*ts6-manager-fullstack:.*|image: localhost/ts6-manager-fullstack:dev|; s|imagePullPolicy: IfNotPresent|imagePullPolicy: Never|' \
  deploy/kube/ts6-manager.yaml \
  | podman kube play deploy/kube/secrets.yaml -
```

`imagePullPolicy: Never` prevents Podman from trying to pull the
`localhost/...` image from a registry.

## Volumes

| PVC | Path inside container | Purpose |
|-----|-----------------------|---------|
| `ts6-db` | `/var/lib/ts6-manager/db` | SurrealKV embedded store (DATABASE_URL) |
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
(30s delay, 30s period) probes against `GET /health`. Podman
respects probe semantics from v4.4 onward.

## Topology

```
Pod ts6-manager
└── container fullstack  (port 3001, uid 10001, non-root)
     ├── PVC ts6-db    → /var/lib/ts6-manager/db    (SurrealKV)
     └── PVC ts6-music → /var/lib/ts6-manager/music
```

This matches the Quadlet `ts6-manager.pod` topology in
`deploy/quadlet/` (sibling workstream) and the default services in
`podman-compose.yml` (dev). Future workstreams may add a `sidecar`
container (MoQ media sidecar, port 9800) once `Containerfile.sidecar`
publishes — at that point, append a second container entry alongside
`fullstack` and a corresponding service-network glue if needed.

## Definition of done check

- `podman kube play deploy/kube/secrets.yaml deploy/kube/ts6-manager.yaml` succeeds on a clean rootless Podman ≥ 4.4 host with only the published image (`ghcr.io/frozentear/ts6-manager-fullstack:latest`) available.
- `curl http://localhost:3001/health` returns 200.
- `podman kube down deploy/kube/ts6-manager.yaml` cleans up the pod.
- Data on PVCs `ts6-db` and `ts6-music` survives `kube down` and is reachable on the next `kube play`.
