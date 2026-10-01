# TS6 Manager

A self-hostable admin panel and voice-media ecosystem for
[TeamSpeak 6](https://teamspeak.com/) servers. Built by
[Teamspeak Heaven](https://github.com/FrozenTear/teamspeak-admin-panel).

A community operator should be able to bring up a working manager
in under ten minutes on a single rootless Podman host. That is the
bar this project holds itself to.

## Status

Pre-1.0. Phase 6 gate work — see
[docs/phase6/readiness-audit.md](docs/phase6/readiness-audit.md).
The fullstack admin panel runs and is daily-driven; the MoQ media
sidecar (Phase 5 video) is parked behind an opt-in deploy unit while
the publish pipeline lands.

The first signed release will be `v1.0.0`, cut by the Phase 6 gate
([PURA-155](https://github.com/FrozenTear/teamspeak-admin-panel/issues))
once the seven Chapter 1 verifications pass against a fresh rootless
Podman deploy.

## What it is

| Component | Crate | Image | Purpose |
| --- | --- | --- | --- |
| Fullstack admin panel | [`crates/ts6-manager-server`](crates/ts6-manager-server) | `ts6-manager-fullstack` | Dioxus 0.7 fullstack server (Axum API + WASM UI) — server management, accounts, music bot, audit. |
| Media sidecar | [`crates/ts6-media-sidecar`](crates/ts6-media-sidecar) | `ts6-manager-sidecar` | MoQ-over-WebTransport video/audio relay. Sibling workspace. |
| Music unit | [`crates/voice`](crates/voice) (`ts6-manager-music`) | `ts6-manager-music` | Music+Voice send loop (decode → Opus → TS6). The live process is on a different host from the panel. The local music container stays stopped. |
| Voice prototype | [`crates/ts6-voice-prototype`](crates/ts6-voice-prototype) | — | "Two clients can talk" reference: Opus over the TS6 wire protocol. |
| Voice translator | [`crates/ts6-voice-translator`](crates/ts6-voice-translator) | — | TS6 ↔ WebRTC voice bridge. |
| Music bot audio | [`crates/music-bot-audio`](crates/music-bot-audio) | — | Library helpers for the in-panel music bot. |
| Shared types | [`crates/shared`](crates/shared) | — | API DTOs used by both server and WASM UI. |
| SSRF guard | [`crates/ts6-ssrf`](crates/ts6-ssrf) | — | Outbound URL allowlist + IP pinning. |

The data plane is rooted in open standards: TeamSpeak 6's published
wire protocol, Opus, MoQ-over-QUIC, WebTransport, SRTP. No bespoke
voice stack.

## Where music runs

The panel and API run on one host. Music runs on another. The panel
still calls `http://127.0.0.1:3002` with no bearer. That address is a
reverse tunnel the music host opens, bound only to `127.0.0.1:3002`
on the panel host. Do not publish `:3002` or `:7080`, and do not
retarget the URL.

`TS6_LOCAL_MUSIC` decides whether `./scripts/update.sh` starts the
music container on the host where the script runs. The default is
`skip`, because music is already running on the other host. Starting
the local container takes `127.0.0.1:3002` and the TeamSpeak server
sees a second copy of the same client identity.
`TS6_LOCAL_MUSIC=play` starts that local container again, and only
after the remote music process is stopped. The script prints the mode
and the turn-back command.

The music process comes back from its restart policy. The client is
pushed only when the panel API starts, so the hop has to be listening
first. If the API boots while `127.0.0.1:3002` is not answering, the
saved row is never pushed and the music page is 404. That 404 is the
empty supervisor. A hop that dies after the client is already
connected is a 5xx from the API. The TeamSpeak client stays in the
channel, and the panel cannot drive it until the hop is back.

Identities and `yt-cookies.txt` live on the music host. An empty
cookie file fails the resolve and puts nothing on the wire.

Packing B (send CPUs 0-1, decode 2-5, nice -5) is the pin on the
panel host. The remote music process had no cpuset and no nice. Its
send thread was on both of that host's cores. The track with 0 late
frames is not that pin, and it is not an unpinned loop. Issue #93
stays open as that panel-host finding. Sidecar stays on the panel
host. Do not restart TeamSpeak on either host.

The present hosts are written once in
[`deploy/contabo/README.md`](deploy/contabo/README.md). Cut order:
that file and [`docs/runbook.md`](docs/runbook.md) § 3.4.

## Install

Three supported shapes, in increasing operational complexity. All
three target rootless Podman.

### 1. Quadlet — systemd-managed, single host (recommended)

Production-shape deploy on a single Linux host with `systemd --user`.
Step-by-step install in under ten minutes:

→ **[deploy/quadlet/README.md](deploy/quadlet/README.md)**

Requires Podman ≥ 5.0 (Quadlet `.pod` support).

### 2. `podman kube play` — Kubernetes-style manifest

Same artefacts as Quadlet but driven from a Kubernetes-flavoured YAML
that's portable to a real cluster.

→ **[deploy/kube/README.md](deploy/kube/README.md)**

Requires Podman ≥ 4.4.

### 3. `podman-compose` — local development

Single-command dev bring-up with hot rebuild.

```sh
export JWT_SECRET="$(openssl rand -base64 48)"
podman-compose up --build fullstack
# Web UI on http://localhost:3001
```

This builds `localhost/ts6-manager-fullstack:dev` from
[`Containerfile.fullstack`](Containerfile.fullstack) and stores state
in named volumes (`ts6-data`, `ts6-db`, `ts6-music`) — not host bind-mounts. See
the comment in [`podman-compose.yml`](podman-compose.yml) for the
rootless-userns rationale.

### Image source

Kube does not pin `:latest`. `deploy/kube/ts6-manager.yaml` uses
`@UNRELEASED` on fullstack, music, and sidecar, which fails reference
parsing. Start and restart that stack only with
`./scripts/update.sh vX.Y.Z` (it rewrites those images onto the tag
you pass). The default `TS6_LOCAL_MUSIC=skip` does not start the
local music container while music is running on another host. Quadlet
units are a separate shape; see
[`deploy/quadlet/README.md`](deploy/quadlet/README.md). Image build,
sign, and publish: [`docs/ops/images.md`](docs/ops/images.md).

## Configuration

Minimum required env vars:

| Variable | Purpose |
| --- | --- |
| `JWT_SECRET` | ≥ 32 bytes. `openssl rand -base64 48`. |
| `DATABASE_URL` | SurrealKV path. Default in containers: `surrealkv:///var/lib/ts6-manager/db`. |
| `MUSIC_DIR` | Music bot library directory. Default: `/var/lib/ts6-manager/music`. |
| `PORT` | HTTP listener. Default `3001`. |

Optional: `ENCRYPTION_KEY`, `LOG_LEVEL`, `LOG_PRETTY`, `FRONTEND_URL`,
`MUSIC_RUNTIME_URL`, `MUSIC_RUNTIME_TOKEN` (shared bearer for the music
control API; both processes, environment only, same trim, optional on
loopback; non-UTF-8 refuses fullstack startup; the token must travel
over WireGuard only).
See [`deploy/quadlet/ts6-manager.env.example`](deploy/quadlet/ts6-manager.env.example)
for the canonical list with comments.

## Development

The repo is a Cargo workspace plus a sibling workspace
(`crates/ts6-media-sidecar/`) — the sibling exists because
`moq-native` and `dioxus-server` enable mutually-exclusive
`parking_lot` features. See the comment at the top of
[`Cargo.toml`](Cargo.toml) for the full rationale.

```sh
# Build everything in the main workspace.
cargo build --workspace

# Sidecar (sibling workspace).
( cd crates/ts6-media-sidecar && cargo build )

# Test.
cargo test --workspace

# Lint.
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

### TS6 reference server fixture

A pinned upstream TeamSpeak 6 server image runs locally for
integration tests via `podman-compose`:

```sh
make ts6-up        # start fixture, print API key + verification env
make ts6-down      # stop
make ts6-apikey    # print just the API key
make ts6-logs      # tail
```

Operator notes: [`docs/ts6-fixture.md`](docs/ts6-fixture.md).

### Voice prototype

The "two clients can talk" reference exchanges Opus frames end-to-end
through the local TS6 fixture:

```sh
make voice-prototype
```

Operator notes: [`docs/voice-prototype.md`](docs/voice-prototype.md).

## Documentation

| Topic | Path |
| --- | --- |
| Operator runbook | [`docs/runbook.md`](docs/runbook.md) |
| Operator troubleshooting | [`docs/troubleshooting.md`](docs/troubleshooting.md) |
| Architecture decisions | [`docs/adr/`](docs/adr/) |
| Phase 6 readiness audit | [`docs/phase6/readiness-audit.md`](docs/phase6/readiness-audit.md) |
| Image build, sign, publish | [`docs/ops/images.md`](docs/ops/images.md) |
| TS6 fixture (CI + local) | [`docs/ts6-fixture.md`](docs/ts6-fixture.md) |
| Voice prototype | [`docs/voice-prototype.md`](docs/voice-prototype.md) |
| Voice translator | [`docs/voice-translator.md`](docs/voice-translator.md) |
| SSH host-key TOFU | [`docs/ssh-host-key-tofu.md`](docs/ssh-host-key-tofu.md) |

## Verifying a release

Release images are signed with cosign. To verify before deploying:

```sh
cosign verify ghcr.io/frozentear/ts6-manager-fullstack:v1.0.0 \
    --certificate-identity-regexp 'https://github.com/FrozenTear/teamspeak-admin-panel/.+' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Sidecar binaries ship as signed GitHub Release assets — verify with
`cosign verify-blob`. See [`docs/ops/images.md`](docs/ops/images.md) §3.

## License

MIT. See [LICENSE](LICENSE).
