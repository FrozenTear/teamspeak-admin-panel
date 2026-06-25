# WS-Gate probe suite

Per-verification HTTP/script probes for the Chapter 1 verification matrix.
`run-all.sh` globs every `*-probe.sh` in this directory and aggregates a
pass/fail matrix. Shared plumbing lives in `_probe-lib.sh` (the `-lib.sh`
suffix keeps it out of the `*-probe.sh` glob).

## Run modes

Every probe authored for [THE-1014](/THE/issues/THE-1014) (v2/v3/v5/v7)
supports three modes, selected from `BASE_URL` + `WS_GATE_DRY_RUN`:

| Mode | Trigger | What it does |
| --- | --- | --- |
| **dry-run** | `WS_GATE_DRY_RUN=1` | No boot, no network. Validates tooling + that every request payload builds, writes evidence + `verdict.json`, exits 0. The path used to prove the suite is turnkey from the headless heartbeat env. |
| **live** | `BASE_URL` given + `ADMIN_TOKEN` env | Runs against an already-running manager. The runner ([THE-1013](/THE/issues/THE-1013)) supplies a reachable manager **and a NON-PROD TeamSpeak target** via the `TS_*` / `REQUIRE_*` env. |
| **self-boot** | no `BASE_URL`, not dry-run | Boots `ts6-manager-server` against an in-memory SurrealDB (fresh deploy), mints a bootstrap admin, runs the env-independent assertions. Needs a Rust toolchain (or `SERVER_BIN`). |

The legacy `v6-probe`, `v6-graph-probe`, and `admin-probe` honour
`WS_GATE_DRY_RUN` too (tooling check + green), so the whole umbrella is
dry-run-green:

```bash
WS_GATE_DRY_RUN=1 scripts/ws-gate/run-all.sh        # headless, no manager
scripts/ws-gate/run-all.sh https://manager.example  # live (ADMIN_TOKEN exported)
```

Evidence lands in `qa-evidence/ws-gate/<probe>/<ISO8601-UTC>/` —
`step-N.{req,resp}.json` + `verdict.json` (secrets redacted in the stored
request bodies). Per the QA evidence convention.

## The matrix rows

| Probe | Row | Heartbeat-runnable (no TS) | Needs the runner / live TS |
| --- | --- | --- | --- |
| `v2-probe` | V2 add-TS-server | create row → 201, readback persists, `apiKey` never returned, `enabled` | `REQUIRE_HEALTHY=1`: dashboard probe-back proves reachability |
| `v3-probe` | V3 live dashboard | dashboard/clients/channels are auth-gated (401 unauth) + return a clean status (2xx or 502/503/504, never 500) | `REQUIRE_LIVE_TS=1`: 200 + snapshot shape (`onlineUsers`/`channelCount`, client/channel arrays) |
| `v5-probe` | V5 audio-capture | create bot → `POST /play` → 202 (command path + `MusicRequest` audit) + emits `capture-plan.json` | `--capture` + `CAPTURE_BACKEND`: RTP/Opus egress capture + click/gap/RMS analysis (audible stage) |
| `v7-probe` | V7 widget URL | unknown token → 404 (public, not 401); real variants public + clean + leak-free across data/svg/png | `REQUIRE_LIVE_TS=1`: 200 + content-type + non-empty per variant |
| `admin-probe` | admin-management | self-boots; user CRUD + audit replay + last-admin protection | — |
| `v6-probe` / `v6-graph-probe` | V6 flow trigger | — (needs a manager) | live manager + `ADMIN_TOKEN` |

## V4 honesty note (read before treating V4 as a mutating-TS row)

`admin-probe` exercises the **disable + audit** mutation path
(`PATCH /api/users/{id} enabled:false` → `userDisabled` audit row). It does
**not** perform a TeamSpeak *kick* of a connected client. If the matrix
intends V4 to be a true "mutating TS action takes effect on the server" row
(kick removes the user, ban appears in the ban list), that probe does **not
yet exist** — a real-kick path (`POST` the kick/move/ban control route
against a live NON-PROD client, then assert the client list reflects it +
the audit row appears) must be authored on the runner. Tracked as a gap for
[THE-1009](/THE/issues/THE-1009); do not score V4 as a mutating-TS row off
`admin-probe` alone.

## Audible-analysis stage (V5)

`v5-probe` always writes `capture-plan.json` describing what the runner must
supply: a bot joined to a NON-PROD channel (`BotState=in_channel`), an
RTP/Opus egress capture sink on the voice path, and the headless analyzer
(click / gap / RMS-continuity) specified in
[THE-1013](/THE/issues/THE-1013). Arm it with `--capture` (or `CAPTURE=1`)
and point `CAPTURE_BACKEND` at the analyzer once it lands; `STRICT_CAPTURE=1`
turns a missing backend into a hard fail.
