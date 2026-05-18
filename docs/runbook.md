# TS6 Manager — operator runbook

The single landing page for operators who have already followed the top-level
[README](../README.md) install path and now need to run the manager day-to-day.

This runbook does **not** repeat the per-deploy-shape install steps — those
live in the shape READMEs and are linked from each section below. It covers
first-boot checks, observability, routine operations, capacity guidance, and
release verification across all three supported shapes:

| Shape | Source of truth |
| --- | --- |
| Quadlet (`systemd --user`) | [`deploy/quadlet/README.md`](../deploy/quadlet/README.md) |
| `podman kube play` | [`deploy/kube/README.md`](../deploy/kube/README.md) |
| `podman-compose` (development) | [`README.md` Install §3](../README.md#3-podman-compose--local-development) |

Failure modes with operator-actionable remediation live in
[`troubleshooting.md`](troubleshooting.md). When a section below points at
"the operator sees X", that string is documented in the troubleshooting doc.

---

## 1. First-boot checklist

Run through this before exposing the manager to anyone other than yourself.

### 1.1 Required environment

| Variable | Purpose | How to generate |
| --- | --- | --- |
| `JWT_SECRET` | Signs access + refresh tokens. Server refuses to boot in production with a placeholder or shorter than 32 bytes. | `openssl rand -base64 48` |

The single hard requirement. The server boots without anything else.

### 1.2 Recommended environment

| Variable | Purpose | How to generate |
| --- | --- | --- |
| `ENCRYPTION_KEY` | AES-256-GCM key for at-rest encryption of TS server-connection credentials and SSH host-key fingerprints. If unset, derived from `JWT_SECRET` — workable, but rotates together. | `openssl rand -base64 32` |
| `FRONTEND_URL` | Public origin the browser hits (CORS + cookie domain). Default `http://localhost:3000`. | Operator-supplied. |
| `TRUSTED_PROXY_HOPS` | Number of trusted reverse-proxy hops in front of the listener. `0` = ignore `X-Forwarded-For`; `1` = exactly one trusted proxy. | Set to match your TLS terminator. |

The full canonical env list, with comments, is
[`deploy/quadlet/ts6-manager.env.example`](../deploy/quadlet/ts6-manager.env.example).
Kube operators populate the same keys via the
[`Secret`](../deploy/kube/secrets.example.yaml) referenced by the pod.

### 1.3 Ports to expose

The fullstack image listens on a single TCP port; the optional MoQ video
sidecar adds two more.

| Port | Component | Protocol | Required? | Notes |
| --- | --- | --- | --- | --- |
| `3001` | Fullstack admin panel | TCP | Yes | Web UI + API. Bind a reverse proxy in front for TLS. |
| `7080` | MoQ sidecar HTTP control | TCP | Only if running the sidecar | Loopback-only inside the pod by default. |
| `4443` | MoQ sidecar WebTransport | UDP | Only if exposing public video | Browsers reject WebTransport on cleartext origins; terminate TLS. |

For Quadlet, `127.0.0.1:3001:3001` is the default and is overridden by
editing `ts6-manager.pod`. For `kube`, the pod runs with `hostNetwork: true`
and the container ports become host ports directly — see
[`deploy/kube/README.md` § Network mode](../deploy/kube/README.md#network-mode)
for why this is load-bearing, not a perf tweak.

### 1.4 First-login verification

After bring-up:

```sh
curl -fsS http://127.0.0.1:3001/health
```

Should return `200 OK`. If it does not, jump to
[`troubleshooting.md` § "Server does not answer on /health"](troubleshooting.md#server-does-not-answer-on-health).

The first browser visit lands on the setup wizard
(`crates/ts6-manager-server/src/ui/pages/setup.rs`), which mints the first
admin account. From there you add a TeamSpeak server connection (Chapter 1
verification V2) — host, WebQuery port, API key, optional SSH credentials.
If you do not have a real TS6 server handy, the local fixture path is
documented in [`docs/ts6-fixture.md`](ts6-fixture.md).

---

## 2. Healthcheck and observability

### 2.1 Where logs live, by shape

| Shape | Read with |
| --- | --- |
| Quadlet | `journalctl --user -u ts6-manager-fullstack.service -f` |
| Kube | `podman logs -f ts6-manager-fullstack` (or `podman pod logs -f ts6-manager`) |
| Compose | `podman-compose logs -f fullstack` |

Sidecar / music-bot / voice-translator logs follow the same pattern with
the matching unit / container name (`ts6-manager-sidecar.service`,
`ts6-fixture`, `voice-translator`, etc.).

### 2.2 What each component logs

The manager and its peripherals all use the [`tracing`](https://docs.rs/tracing/)
crate. Format is JSON in production (`LOG_PRETTY=` unset) and pretty in dev
(`LOG_PRETTY=1`). `LOG_LEVEL` accepts the standard `trace,debug,info,warn,error`.

| Component | Notable log surfaces |
| --- | --- |
| **Manager (fullstack)** | HTTP access logs, WebQuery client errors, SSH-bridge connection state, refresh-token reuse warnings (R5 — see [troubleshooting](troubleshooting.md#refresh-token-reuse-detection-tripped)), database boundary errors (R8 / D8). |
| **Sidecar (MoQ video)** | `PinProxy listening` on startup, per-pipeline `info!` start/stop, SSRF rejections, redirect refusals, upstream-fetch failures with the SSRF-pinned IP. |
| **Music bot** | yt-dlp / FFmpeg subprocess lifecycle, ICY metadata events, Opus pacer drift on overload, chat-command echoes (`!radio`, `!play`, `!stop`, `!skip`, `!vol`, `!np`). |
| **Voice translator** | TS6 handshake state, LiveKit `Room::connect` result, per-direction frame counters (`audio_frames_published`, `reverse_frames_received`). |
| **Dashboard tick republisher (PURA-81)** | Per-server tick on `server:{id}:clients` + `server:{id}:channels`, exponential backoff on transport failure (5 s → 60 s, reset on success). One `dashboard:tick` envelope every 5 s per enabled server connection. |

### 2.3 Steady-state log volume

For a single TS6 server connection sitting idle (no operator activity, no
talkers, no video subscribers):

- **Manager**: ~1 line / 60 s at `LOG_LEVEL=info`. The dashboard tick
  republisher does not log per tick — only on transport-failure transitions
  and reconcile-cycle changes.
- **Sidecar**: silent until a `POST /source` lands.
- **Music bot**: silent until a chat command arrives.

If your `info`-level log volume exceeds a few lines per minute on an idle
deploy, something is in a backoff loop. Grep for `error|warn` and cross-
reference [`troubleshooting.md`](troubleshooting.md).

### 2.4 In-container health probe

The Quadlet unit ships with `HealthCmd=` commented out. The fullstack
runtime image does not include a `curl`-style probe binary, so an
in-container probe wedges the container under `HealthOnFailure=kill`.
systemd still recovers from soft hangs via `Restart=on-failure`. Re-enable
the probe lines once a probe binary lands in the runtime image — see
[`deploy/quadlet/README.md` § In-container health probe](../deploy/quadlet/README.md#in-container-health-probe-disabled).

The `kube` manifest defines readiness (5 s delay, 10 s period) and liveness
(30 s delay, 30 s period) probes against `GET /health`. Podman ≥ 4.4
respects probe semantics from the manifest directly.

For all three shapes the external smoke is the same: `curl -fsS http://127.0.0.1:3001/health`.

---

## 3. Routine operations

### 3.1 Start / stop / restart

| Shape | Start | Stop | Restart |
| --- | --- | --- | --- |
| Quadlet | `systemctl --user start ts6-manager-pod.service` | `systemctl --user stop ts6-manager-pod.service` | `systemctl --user restart ts6-manager-pod.service` |
| Kube | `cat deploy/kube/secrets.yaml deploy/kube/ts6-manager.yaml > /tmp/ts6-manager.kube.yaml && podman kube play /tmp/ts6-manager.kube.yaml` | `podman kube down deploy/kube/ts6-manager.yaml` | `podman kube down …` then `podman kube play …` (re-concat the kube file too if you re-edited a source manifest) |
| Compose | `podman-compose up -d fullstack` | `podman-compose down` | `podman-compose restart fullstack` |

Kube `kube down` removes the pod and containers but leaves the named
volumes (`ts6-data`, `ts6-db`, `ts6-music`) intact, so data survives a
restart. `ts6-data` backs the manager state root and is what keeps a
yt-dlp cookie uploaded via Settings from being wiped on redeploy
([PURA-314](https://github.com/FrozenTear/teamspeak-admin-panel/issues)).

### 3.2 Volume backup and restore

Both production shapes use named volumes — bind-mounts under rootless
podman break SurrealKV with `EACCES` ([PURA-67](https://github.com/FrozenTear/teamspeak-admin-panel/issues),
see [troubleshooting](troubleshooting.md#permission-denied-opening-the-surrealkv-store)).

```sh
# Backup. Date-stamped tarball in the working directory.
podman volume export ts6-data  -o ts6-data-$(date +%F).tar
podman volume export ts6-db    -o ts6-db-$(date +%F).tar
podman volume export ts6-music -o ts6-music-$(date +%F).tar

# Restore (volume must exist; destroy it first if you want a clean state).
podman volume import ts6-data  ts6-data-YYYY-MM-DD.tar
podman volume import ts6-db    ts6-db-YYYY-MM-DD.tar
podman volume import ts6-music ts6-music-YYYY-MM-DD.tar
```

Stop the manager before restoring `ts6-db` — SurrealKV does not tolerate
the underlying files being swapped under a live process.

### 3.3 Log rotation

`journalctl` (Quadlet) and `podman logs` (kube / compose) inherit the
host's journald / container-engine log rotation. The defaults are sane on
Fedora/RHEL/Ubuntu — 10 % of `/var` budget, monthly vacuum.

To bound `journalctl --user` size explicitly:

```sh
journalctl --user --vacuum-size=500M
journalctl --user --vacuum-time=30d
```

For long-form retention, ship logs to your existing aggregator. The JSON
format the server emits in production was chosen for direct ingestion by
Vector / Promtail / Fluent Bit. There is no built-in shipper — keep the
log path off the manager itself.

### 3.4 Image upgrade

> **Persistence requirement (PURA-357).** An image upgrade *recreates the
> container*. Everything an operator configures — TeamSpeak server
> connections, music bots, automation flows and their automod rules,
> users, widgets — lives in the SurrealDB store on the **`ts6-db`**
> volume, and music-bot TS identity files live under `DATA_DIR` on the
> **`ts6-data`** volume. Both volumes MUST survive the upgrade or that
> state is lost. Concretely:
>
> - Never pass `--force` to `podman kube down` during a redeploy — it
>   wipes the named volumes. Confirm with
>   `podman volume ls --filter name=^ts6-` after `kube down` (see
>   [`deploy/kube/README.md`](../deploy/kube/README.md#bring-down)).
> - The manifest must keep `DATABASE_URL` pointed at a path on the
>   `ts6-db` volume and `DATA_DIR` at a path on the `ts6-data` volume.
>   The committed [`deploy/kube/ts6-manager.yaml`](../deploy/kube/ts6-manager.yaml)
>   does this; a hand-edited manifest must not drop those env vars.
> - Music bots are re-spawned at boot from the `music_bot_runtime` DB
>   table (the supervisor itself is in-memory), so a bot only returns
>   after an upgrade if the `ts6-db` volume persisted. An
>   `autoConnect=false` bot is restored idle.
>
> If bots/flows/rules vanish after an upgrade, the `ts6-db` volume was
> lost — restore it from a § 3.2 backup.

The Quadlet and Kube manifests pin `ghcr.io/frozentear/ts6-manager-fullstack:v0.1.0-rc1`
(release candidate; floats to `:v1.0.0` on the next signed cut). Two
documented upgrade paths:

**Pin-by-tag (recommended):**

1. Verify the new image's signature with cosign — see § 5 below and
   [`docs/ops/images.md` § 3](ops/images.md#3-signing) for the canonical
   recipe.
2. Edit the unit / manifest to pin the new tag.
3. `daemon-reload && restart` (Quadlet) or `kube down && kube play` (kube).

**Auto-update (Quadlet only, opt-in):** the unit ships with
`AutoUpdate=registry`. Then:

```sh
podman auto-update --dry-run    # show what would upgrade
podman auto-update              # apply
```

Only enable auto-update against immutable `vX.Y.Z` tags — pointing it at a
floating `latest` tag will roll silently on every push and breaks the
"every running instance has a known signature" property.

### 3.5 Re-issuing secrets

Rotating `JWT_SECRET` invalidates every active session — the next
authenticated request returns `401` and the operator re-logs-in. Refresh
tokens are also invalidated; sessions held by other users in the same
deploy are kicked.

`ENCRYPTION_KEY` rotation is **not** automatic — encrypted-at-rest values
(stored TS server credentials, SSH fingerprints) are encrypted under the
old key. If you must rotate, plan a brief maintenance window:

1. Stop the manager.
2. Decrypt the at-rest table with the old key (no tooling ships for this
   today — open an issue if you need it; this is a v1.x carry-over).
3. Set the new key.
4. Re-encrypt and restart.

If `ENCRYPTION_KEY` was unset (so it was derived from `JWT_SECRET`),
rotating `JWT_SECRET` will also break decryption of stored secrets. Set
`ENCRYPTION_KEY` explicitly before the first JWT rotation if you have
production server-credentials stored.

---

## 4. Capacity reference

Numbers below come from the WS-Perf sustained-load smoke
([PURA-162](https://github.com/FrozenTear/teamspeak-admin-panel/issues),
report at [`docs/voice/perf-smoke.md`](voice/perf-smoke.md)). The harness
runs the music-bot pipeline (yt-dlp / FFmpeg → Opus 20 ms → wall-clock
pacer → TS6 voice frame) for 10 minutes (`quick`) or 30 minutes
(`sustained`) and writes a JSON report under `qa-evidence/perf-smoke/`.

### 4.1 Music-bot pipeline — measured baseline

600 s sustained-load on a contended dev workstation, `git 51ecb58ad2a0`,
synthetic-tone source, mono, 64 kbps Opus, 30,001 frames received:

| Metric | Value | Notes |
| --- | --- | --- |
| Pacer drift p50 | 1.15 ms | Steady-state floor. |
| Pacer drift p95 | 1.60 ms | |
| Pacer drift p99 | 1.68 ms | First 12 frames excluded as warm-up. |
| Pacer drift max | 151.27 ms | Single OS scheduling pause on contended host; idle host hits 1.82 ms. |
| Cumulative drift | 0.84 ms | Pipeline does not creep across the run. |
| CPU mean | 0.76 % of one core | |
| CPU peak | 2.00 % of one core | |
| RSS start (post-warmup) | 6.03 MB | |
| RSS end | 5.97 MB | |
| RSS growth | −0.91 % | Allocator returned pages to OS — no leak. |
| FD start / end | 10 / 10 | No FD leak. |

### 4.2 v1.0 budgets (default, gate-enforced)

The pipeline ships with these defaults; `scripts/perf-smoke.sh` exits
non-zero on any breach and the WS-Gate run rejects regressions.

| Metric | Budget | Rationale |
| --- | --- | --- |
| Pacer drift p99 (post-warmup) | ≤ 15 ms | ~9× idle floor — catches real regressions, doesn't false-fire on contended hosts. |
| Pacer drift max (post-warmup) | ≤ 50 ms | Large-spike alarm. Clean release host expected < 10 ms. |
| CPU mean (single core) | ≤ 25 % | Leaves headroom for FFmpeg. |
| RSS growth over the run | ≤ 15 % | Small process headroom; still catches real leaks on larger hosts. |
| FD growth over the run | ≤ 0 | Any FD leak is suspect. |

### 4.3 Re-running the smoke on your host

```sh
scripts/perf-smoke.sh quick       # 60 s synthetic
scripts/perf-smoke.sh sustained   # 1800 s synthetic — what WS-Gate runs
scripts/perf-smoke.sh ffmpeg crates/music-bot-audio/tests/fixtures/sine_440_1s_mono_48k.wav
```

The binary is in the main workspace, so it ships inside
`Containerfile.fullstack`. Drive the budgets with `--budget-*` flags if you
have a bigger box and want to tighten.

### 4.4 What is not yet measured

- **TS6 manager HTTP throughput** under widget polling load.
- **WS hub fan-out ceiling** (see [PURA-81](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
  background — the dashboard tick worker is fixed at 4 reads / 5 s per
  server connection).
- **Concurrent music-bot streams.** The pipeline scales per bot; the
  per-bot baseline is the table in § 4.1. We have not run a 10-bot stress.
- **Voice-translator (LiveKit) sustained load.**

These are tracked under the v1.x perf workstream. If you need a number
before then, please run the smoke yourself and post the report on a new
issue — the harness is shipped for exactly this reason.

---

## 5. Verifying a release

Image signatures are cosign keyless OIDC. The verification recipe is in
the top-level [README § "Verifying a release"](../README.md#verifying-a-release);
the full sign / publish procedure (and the per-arch sidecar binary
verification) lives at [`docs/ops/images.md` § 3](ops/images.md#3-signing).

WS-Gate's deploy validation runs `cosign verify` *before* `podman pull`
and refuses to bring up an image whose signature does not match the pinned
identity. We recommend the same posture for any production deploy: pin the
identity regex in your deploy automation and reject unverified images at
the gate, not at the operator.

---

## 6. Where to go next

- **Something is broken** → [`troubleshooting.md`](troubleshooting.md).
- **Image / sign / publish details** → [`docs/ops/images.md`](ops/images.md).
- **TS6 fixture setup** → [`docs/ts6-fixture.md`](ts6-fixture.md).
- **Voice-prototype reference** → [`docs/voice-prototype.md`](voice-prototype.md).
- **WebRTC bridge (translator)** → [`docs/voice-translator.md`](voice-translator.md).
- **Architecture decisions** → [`docs/adr/`](adr/).
- **Phase 6 readiness audit (history)** → [`docs/phase6/readiness-audit.md`](phase6/readiness-audit.md).
