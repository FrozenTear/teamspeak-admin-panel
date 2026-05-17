# Rootless Quadlet deploy — TS6 Manager

This directory holds the [Quadlet](https://docs.podman.io/en/latest/markdown/podman-systemd.unit.5.html)
unit files that bring the manager up as a rootless `systemd --user`
service. Quadlet is the recommended single-host shape per impl-plan §9.

For multi-host / Kubernetes-bound deploys, use the
`podman kube play` YAML emitted under `deploy/kube/` (sibling
[PURA-159](/PURA/issues/PURA-159), in progress). For day-to-day local
development, keep using `podman-compose.yml` at the repo root.

## What lands

| File | Purpose |
| --- | --- |
| `ts6-manager.pod`                          | The rootless pod (PublishPort 127.0.0.1:3001). |
| `ts6-manager-fullstack.container`          | The Dioxus fullstack server (axum + WASM UI). |
| `ts6-data.volume`                          | Named volume for the manager state root (`/var/lib/ts6-manager`) — persists `DATA_DIR` operator uploads such as the yt-dlp cookie file. |
| `ts6-db.volume`                            | Named volume for the embedded SurrealKV store. |
| `ts6-music.volume`                         | Named volume for the music library. |
| `ts6-manager-sidecar.container.example`    | Inactive template for the MoQ video sidecar (Phase 5). Rename when [PURA-160](/PURA/issues/PURA-160) publishes the image. |
| `ts6-manager.env.example`                  | Operator env-file template — copy and populate. |

## Requirements

- Podman ≥ **5.0** (Quadlet `.pod` unit support). Verify with
  `podman --version`. Most modern distros ship 5.x (Fedora 40+,
  Ubuntu 24.04+, RHEL 9.4+).
- systemd-user enabled for the operator account. Verify with
  `systemctl --user is-system-running`.
- `loginctl enable-linger $USER` if the manager must keep running
  across logouts (see [Persistence](#persistence) below).
- `curl` (only for the in-container health probe — already present
  in the image; no host requirement).

## Install (≤ 10 minutes from a clean host)

1. **Install Podman.** Distro package manager — `dnf install podman`,
   `apt install podman`, etc.

2. **Create the Quadlet directory** if it doesn't exist:

   ```sh
   mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd"
   ```

3. **Copy the unit files** into it:

   ```sh
   install -m 0644 \
       deploy/quadlet/ts6-manager.pod \
       deploy/quadlet/ts6-manager-fullstack.container \
       deploy/quadlet/ts6-data.volume \
       deploy/quadlet/ts6-db.volume \
       deploy/quadlet/ts6-music.volume \
       "${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/"
   ```

4. **Create the env file** with your secrets:

   ```sh
   install -m 0600 \
       deploy/quadlet/ts6-manager.env.example \
       "${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/ts6-manager.env"
   $EDITOR "${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd/ts6-manager.env"
   ```

   At minimum, set `JWT_SECRET` to ≥ 32 bytes (`openssl rand -base64 48`).
   See the comments inside the file for everything else.

5. **Generate systemd units from Quadlet and start the pod:**

   ```sh
   systemctl --user daemon-reload
   systemctl --user start ts6-manager-pod.service
   ```

   `daemon-reload` runs Quadlet which writes the generated
   `*.service` units under `$XDG_RUNTIME_DIR/systemd/generator/`. The
   pod service starts its member containers automatically.

6. **Verify** — the web UI should answer on
   <http://127.0.0.1:3001>:

   ```sh
   curl -fsS http://127.0.0.1:3001/health
   systemctl --user status ts6-manager-pod.service ts6-manager-fullstack.service
   ```

## Persistence

`systemd --user` instances stop when the operator's last login session
ends, unless **linger** is enabled. For an unattended deploy:

```sh
loginctl enable-linger $USER
```

This starts the user-instance at boot and keeps it running across
logouts. Disable with `loginctl disable-linger $USER`.

## Common operations

| Goal | Command |
| --- | --- |
| Start | `systemctl --user start ts6-manager-pod.service` |
| Stop | `systemctl --user stop ts6-manager-pod.service` |
| Restart | `systemctl --user restart ts6-manager-pod.service` |
| Enable on boot | `loginctl enable-linger $USER && systemctl --user enable ts6-manager-pod.service` |
| Reload after editing a unit file | `systemctl --user daemon-reload && systemctl --user restart ts6-manager-pod.service` |
| Tail logs | `journalctl --user -u ts6-manager-fullstack.service -f` |
| Pull a new image and roll | `podman auto-update --dry-run` then `podman auto-update` (Quadlet `AutoUpdate=registry` opts in) |
| Backup DB | `podman volume export ts6-db -o ts6-db-$(date +%F).tar` |
| Restore DB | `podman volume import ts6-db ts6-db-YYYY-MM-DD.tar` |
| Backup uploads (yt-dlp cookie) | `podman volume export ts6-data -o ts6-data-$(date +%F).tar` |

## Updating the units

Quadlet reads the source files on **every** `daemon-reload`. The
generated `*.service` units in `$XDG_RUNTIME_DIR/systemd/generator/`
are throwaway — never edit them directly. The supported override path
is drop-ins:

```sh
systemctl --user edit ts6-manager-fullstack.service
# (writes a drop-in under ~/.config/systemd/user/<name>.service.d/override.conf)
```

## Reverse-proxy fronting

The pod publishes on `127.0.0.1:3001` by default; bind your TLS
terminator (Caddy / nginx / Traefik) on the host and proxy_pass to
`http://127.0.0.1:3001`. To expose the pod on the LAN instead, edit
`ts6-manager.pod` and change `PublishPort=127.0.0.1:3001:3001` to
`PublishPort=3001:3001` (or bind a specific interface), then
`daemon-reload && restart`.

## Image source

The `ts6-manager-fullstack.container` unit currently points at
`ghcr.io/frozentear/ts6-manager-fullstack:latest`. The actual
signed-release image is owned by **WS-OPS-Images
([PURA-160](/PURA/issues/PURA-160))**; until that ships, two options:

- **Build locally:** `podman build -t localhost/ts6-manager-fullstack:dev
  -f Containerfile.fullstack .`, then override the `Image=` line via
  a systemd drop-in (`systemctl --user edit ts6-manager-fullstack.service`).
- **Wait for the signed release** and let the image path resolve via
  the registry once PURA-160 lands.

## Sidecar (Phase 5 video)

`ts6-manager-sidecar.container.example` is a parked Quadlet unit for
the MoQ video sidecar. Currently disabled (Quadlet ignores the
`.example` suffix). Rename to `.container` after WS-OPS-Images
publishes the sidecar image and update:

- `Image=` — point at the published sidecar image
- `ts6-manager.pod` — add `PublishPort=443:443/udp` (or whatever
  public UDP port lands the WebTransport listener)
- `ts6-manager.env` — set `SIDECAR_URL=http://127.0.0.1:9800` so the
  manager talks to it across the pod's shared loopback

## In-container health probe (disabled)

The Quadlet unit ships with `HealthCmd=` commented out. The current
`Containerfile.fullstack` runtime stage installs only `ca-certificates`
+ `libssl3` (the build minimisation), so an in-container
`curl http://127.0.0.1:3001/health` fails at exec and
`HealthOnFailure=kill` then crashloops the container. systemd still
restarts the service on process exit via `Restart=on-failure`, so the
loss is *automatic* recovery from a soft-hang state where the process
is running but unresponsive.

To enable the probe once
[PURA-160](/PURA/issues/PURA-160) ships a probe binary in the runtime
image, uncomment the five `Health*` lines in
`ts6-manager-fullstack.container` and `daemon-reload && restart`. The
external smoke (curl from the host or the proxy) always works as long
as the listener is up.

## Troubleshooting

- **`Job for ts6-manager-pod.service failed`** — `journalctl --user
  -u ts6-manager-pod.service` for the systemd-side error and
  `journalctl --user -u ts6-manager-fullstack.service` for the
  container-side. Quadlet syntax errors show up as `daemon-reload`
  warnings: `systemctl --user daemon-reload 2>&1 | grep -i quadlet`.
- **`JWT_SECRET must be set …`** in the journal — populate the env
  file at `~/.config/containers/systemd/ts6-manager.env`.
- **Image pull fails / no such image** — see [Image source](#image-source)
  above. Until WS-OPS-Images publishes, you need either a local build
  or a drop-in `Image=` override.
- **`Permission denied` opening the SurrealKV store** — you've
  switched to host bind-mount volumes without `:U` mode. Stay on the
  named volumes shipped here, or use `:U` to chown the bind-mount
  into the container's userns (PURA-67 background).

## Related tickets

- [PURA-158](/PURA/issues/PURA-158) — this workstream
- [PURA-159](/PURA/issues/PURA-159) — WS-OPS-Kube (sibling: `podman kube play` YAML)
- [PURA-160](/PURA/issues/PURA-160) — WS-OPS-Images (sibling: published OCI images)
- [PURA-155](/PURA/issues/PURA-155) — Phase 6 epic
- Impl-plan §9 — Podman-native deployment shapes
