# TS6 Manager — operator troubleshooting

Catalog of known failure modes with operator-actionable remediation. Pair
with [`runbook.md`](runbook.md) — the runbook covers steady-state
operations; this doc covers what to do when something is broken.

Each entry is shaped:

- **What the operator sees** — the literal error string or symptom.
- **Why it happens** — the underlying cause.
- **What to do** — the smallest action that fixes it.
- **Cross-link** — the source-of-truth doc, ADR, or ticket for deeper
  context.

If the symptom you are looking at is not in this doc, please file an issue
with the matching component's logs (see
[`runbook.md` § 2.1](runbook.md#21-where-logs-live-by-shape) for the
right command per shape).

---

## Quick index

- [Server does not answer on `/health`](#server-does-not-answer-on-health)
- [`JWT_SECRET must be set …` in the journal](#jwt_secret-must-be-set--in-the-journal)
- [Image pull fails / no such image](#image-pull-fails--no-such-image)
- [Cosign verification fails on pull](#cosign-verification-fails-on-pull)
- [`Permission denied` opening the SurrealKV store](#permission-denied-opening-the-surrealkv-store)
- [Sidecar gating wedge — pipelines stop publishing without an error](#sidecar-gating-wedge--pipelines-stop-publishing-without-an-error)
- [TS6 fixture wedges after ~5 WebQuery requests](#ts6-fixture-wedges-after-5-webquery-requests)
- [Refresh-token reuse-detection tripped](#refresh-token-reuse-detection-tripped)
- [SurrealDB error boundary surfaced to the API](#surrealdb-error-boundary-surfaced-to-the-api)
- [FFmpeg fetch refused — DNS rebinding pin tripped](#ffmpeg-fetch-refused--dns-rebinding-pin-tripped)
- [Dashboard tick republisher silent / backing off](#dashboard-tick-republisher-silent--backing-off)
- [Headless browser probes deadlock against the SPA](#headless-browser-probes-deadlock-against-the-spa)

---

## Server does not answer on `/health`

**What the operator sees.** `curl http://127.0.0.1:3001/health` returns
"Connection refused", "Empty reply from server", or hangs. The unit /
container shows as `failed` or `restarting`.

**Why it happens.** Several common causes, in rough order of likelihood:

1. `JWT_SECRET` is unset — see next entry.
2. The bound port (`3001` by default) is already in use on the host. The
   manager log shows `Address already in use (os error 98)`.
3. Another container is already binding the same port under
   `hostNetwork: true` (kube shape).
4. The runtime image is `ghcr.io/.../latest` and has been silently rolled
   to a broken build — see [Image pull fails](#image-pull-fails--no-such-image).

**What to do.** In order:

```sh
# 1. Confirm the unit / container is in the expected state.
systemctl --user status ts6-manager-pod.service        # Quadlet
podman ps                                              # kube / compose

# 2. Tail the journal / container log for the boot output.
journalctl --user -u ts6-manager-fullstack.service -n 100   # Quadlet
podman logs ts6-manager-fullstack                           # kube / compose

# 3. If the bind failed, check the host port.
ss -ltnp | grep ':3001'
```

**Cross-link.** Per-shape bring-up is in
[`deploy/quadlet/README.md`](../deploy/quadlet/README.md) and
[`deploy/kube/README.md`](../deploy/kube/README.md).

---

## `JWT_SECRET must be set …` in the journal

**What the operator sees.** The fullstack container exits within a second
of start with a log line of the shape:

```
ERROR ts6_manager_server::config: JWT_SECRET must be set to ≥32 bytes in production
```

**Why it happens.** The server refuses to boot in `NODE_ENV=production`
without a non-placeholder `JWT_SECRET` of at least 32 bytes of entropy.
This is a deliberate refusal — running with a weak or missing JWT secret
is a credential-forgery primitive.

**What to do.**

```sh
# Quadlet:
$EDITOR ~/.config/containers/systemd/ts6-manager.env
# Set: JWT_SECRET=$(openssl rand -base64 48)
systemctl --user daemon-reload
systemctl --user restart ts6-manager-pod.service

# Kube:
$EDITOR deploy/kube/secrets.yaml         # populate JWT_SECRET
podman kube down deploy/kube/ts6-manager.yaml
podman kube play deploy/kube/secrets.yaml deploy/kube/ts6-manager.yaml
```

**Cross-link.** Canonical env list with comments:
[`deploy/quadlet/ts6-manager.env.example`](../deploy/quadlet/ts6-manager.env.example).

---

## Image pull fails / no such image

**What the operator sees.** `Error: short-name "ghcr.io/frozentear/ts6-manager-fullstack:vX.Y.Z" did not resolve to an alias`,
or a `manifest unknown` error from `podman pull` / `podman kube play`.

**Why it happens.**

1. The release image has not been published yet (pre-tag, or a tag that
   was never pushed).
2. The package is set to private and your host is not authenticated to
   `ghcr.io`. Source-repo visibility and package visibility on GHCR are
   independent — see [`docs/ops/images.md` § 1](ops/images.md#source-repo-visibility-vs-package-visibility).
3. Network egress to `ghcr.io` is blocked.

**What to do.**

```sh
# Confirm the manifest exists.
podman manifest inspect ghcr.io/frozentear/ts6-manager-fullstack:v0.1.0-rc1

# If "manifest unknown", the tag was never pushed — pin the previous one
# or build locally:
podman build -t localhost/ts6-manager-fullstack:dev -f Containerfile.fullstack .

# For Quadlet, drop in an `Image=` override:
systemctl --user edit ts6-manager-fullstack.service
# [Container]
# Image=localhost/ts6-manager-fullstack:dev

# For kube, use the sed-pipe override documented in deploy/kube/README.md.
```

**Cross-link.** Local-build override path:
[`deploy/quadlet/README.md` § Image source](../deploy/quadlet/README.md#image-source)
and [`deploy/kube/README.md` § Override to a local build](../deploy/kube/README.md#override-to-a-local-build-pre-publish-smoke).

---

## Cosign verification fails on pull

**What the operator sees.**

```
Error: no matching signatures: invalid signature when validating ASN.1 encoded signature
```

or

```
Error: no matching certificate identity found
```

**Why it happens.**

1. The image was pulled before the signature was published — the cosign
   transparency log entry hasn't propagated yet (rare; usually < 60 s).
2. The pinned identity regex does not match the cert SAN of the build
   that produced the image (e.g. someone built and signed from a fork).
3. The OCI registry returned a different image manifest than the one
   originally signed — should not happen on GHCR but is the failure mode
   `cosign verify` is designed to catch.

**What to do.**

```sh
# Re-pull and re-verify after a short wait, with verbose output:
cosign verify ghcr.io/frozentear/ts6-manager-fullstack:v1.0.0 \
    --certificate-identity-regexp 'https://github.com/FrozenTear/teamspeak-admin-panel/.+' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    -o text
```

If verification still fails, **do not deploy that image**. The cosign
failure is the load-bearing line between "any image with the right tag"
and "the image FrozenTear actually built." Open an issue with the cosign
output attached.

**Cross-link.** Full verification recipe (image + sidecar binary):
[`docs/ops/images.md` § 3](ops/images.md#3-signing). The pinned identity
regex is also documented there.

---

## `Permission denied` opening the SurrealKV store

**What the operator sees.** The fullstack container exits seconds after
start with:

```
ERROR ts6_manager_server::db: failed to open SurrealKV store at /var/lib/ts6-manager/db: Permission denied (os error 13)
```

**Why it happens.** You switched from a named volume to a host bind-mount
without `:U` mode. Rootless Podman maps the in-container `uid:gid` to a
shifted host subuid (e.g. `10001` inside maps to `100000+10001` outside);
the bind-mount path is owned by your login uid on the host, which the
container userns cannot write to.

**What to do.**

Either stay on named volumes (the documented production layout) or use
the `:U` mount flag, which chowns the bind-mount target into the
container's userns on first start:

```yaml
# podman-compose.yml fragment
volumes:
  - ./data/db:/var/lib/ts6-manager/db:U
```

Quadlet: use `Volume=ts6-db.volume:/var/lib/ts6-manager/db` (named volume),
not `Volume=./data/db:/var/lib/ts6-manager/db`.

**Cross-link.** Background:
[`deploy/quadlet/README.md` § Troubleshooting](../deploy/quadlet/README.md#troubleshooting).
This is the [PURA-67](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
deviation — named volumes are the documented production layout for both
Quadlet and `podman kube play` precisely because they sidestep this.

---

## Sidecar gating wedge — pipelines stop publishing without an error

**What the operator sees.** The MoQ video sidecar accepts `POST /source`,
returns `200 OK`, and the operator sees frames briefly — then publication
stops. No fatal error in the sidecar log; the pipeline `info!` line says
`pipeline stopped` with a `source_id`. Subscribers see the relay quiesce.

**Why it happens.** Phase 5 surfaced this failure mode: the sidecar's
control-plane gate (preset matcher / origin allowlist) was rejecting a
mid-stream re-evaluation that the operator's source had picked up via an
ICY metadata refresh, but the rejection path logged at `debug!` and the
pipeline tore itself down silently as designed. The two common triggers
in production are:

1. The upstream source URL switched (HLS / ICY rotated) to a host the
   sidecar's `origin` allowlist no longer accepts.
2. The pipeline received an empty Opus frame as a stream-end marker and
   the gate did not re-issue a fresh source — operator action required.

**What to do.**

```sh
# 1. Bump the sidecar log to debug to see the gating decision.
podman exec ts6-manager-sidecar sh -c 'RUST_LOG=info,ts6_media_sidecar=debug'
# (or set LOG_LEVEL=debug in the env and restart the sidecar container)

# 2. Reissue the source.
curl -fsS -X POST http://127.0.0.1:7080/source -d '{"url":"https://…"}'
```

If the rejection persists, open an issue with the sidecar log between
`POST /source` and `pipeline stopped` — that gives the boundary owner
enough context to confirm whether the allowlist needs to widen or the
operator's URL needs to change.

**Cross-link.** [`crates/ts6-media-sidecar/src/control.rs`](../crates/ts6-media-sidecar/src/control.rs)
holds the gate; [`origin.rs`](../crates/ts6-media-sidecar/src/origin.rs)
is the allowlist surface.

---

## TS6 fixture wedges after ~5 WebQuery requests

**What the operator sees.** Fresh `teamspeak6-server:6.0.0-beta9` fixture
boots fine; the manager makes a handful of WebQuery calls (`200 OK`); then
every subsequent call returns "Empty reply from server" or
`curl` reports `000`. The dashboard goes blank. Restarting the fixture
"fixes" it for another ~5 requests.

**Why it happens.** Upstream TS6's WebQuery wedges after exactly 5
successful requests when reached through rootless podman's default
user-mode networking (passt port-forward). The dashboard tick worker fans
out 4 reads every 5 s, so the budget evaporates within ~30 s of operator
activity. Two plausible root causes — antiflood miscount under passt
translation, or a passt bug forwarding rapid sequential connections — but
neither is confirmed and neither is on the critical path because the
workaround is small and known to work.

**What to do.** Run the fixture with `--network=host`. The
`make ts6-up` wrapper and the `ts6-fixture` compose profile both bake
this in. If you copy-pasted a `podman run -p ...` command from somewhere
else, replace the `-p` flags with `--network=host`:

```sh
podman run -d --name ts6-fixture \
  --network=host \
  -e TSSERVER_LICENSE_ACCEPTED=accept \
  -e TSSERVER_QUERY_ADMIN_PASSWORD=qa-admin \
  -e TSSERVER_QUERY_SKIP_BRUTE_FORCE_CHECK=1 \
  -e TSSERVER_QUERY_HTTP_ENABLED=1 \
  -e TSSERVER_QUERY_SSH_ENABLED=1 \
  -v ts6-fixture-data:/var/tsserver \
  docker.io/teamspeaksystems/teamspeak6-server:latest
```

`TSSERVER_QUERY_HTTP_ENABLED=1` and `TSSERVER_QUERY_SSH_ENABLED=1` are
mandatory — both default to `0` in `tsserver --help`, and without them
the fixture only starts the legacy telnet ServerQuery.

**Cross-link.** Full background, repro, and the manager's own dashboard
fan-out behaviour: [`docs/ts6-fixture.md`](ts6-fixture.md). The kube
shape encodes the same constraint via `hostNetwork: true` —
[`deploy/kube/README.md` § Network mode](../deploy/kube/README.md#network-mode).

---

## Refresh-token reuse-detection tripped

**What the operator sees.** A user reports they were logged out
unexpectedly. The manager journal shows:

```
WARN  ts6_manager_server::auth::refresh: refresh-token reuse detected; revoking all sessions for user user_id=42 reason="replacedBy match"
```

possibly followed (only if the cleanup itself fails) by:

```
ERROR ts6_manager_server::auth::refresh: failed to revoke user sessions after reuse signal user_id=42 error=…
```

**Why it happens.** Spec §6.5 + §6.6 — the manager keeps a refresh-token
family per session. When a refresh token is rotated the old one is marked
`replacedBy=<new>`; presenting the *old* token after rotation is the
canonical reuse signal. The server then revokes the entire family for
that user. Common triggers:

1. **Legitimate but suspicious.** The user resumed a paused tab whose
   refresh token had already been rotated by another active tab.
2. **Token stolen and replayed.** The legitimate user's tab refreshes
   normally; the attacker's stolen token presents the predecessor.
3. **Clock skew on a load-balancer-fronted deploy.** Refresh tokens
   normally rotate on every access-token expiry — extreme clock skew can
   make a fresh token *look* like a replay.

The server treats all three the same way: revoke the family, force re-
authentication. This is correct.

**What to do.** Nothing in case (1) — the user re-logs-in and continues.
For (2), audit access logs for the specific `user_id` in the warn line.
For (3), check NTP on every host in the deploy.

There is no "clear" action in the manager itself — the family is gone,
and the next presentation of any token in it returns `401`. The user re-
authenticates and a fresh family is minted.

**Cross-link.** Implementation:
[`crates/ts6-manager-server/src/auth/refresh.rs`](../crates/ts6-manager-server/src/auth/refresh.rs).
Risk-register entry: R5 in
[`docs/phase6/readiness-audit.md` § 4](phase6/readiness-audit.md#4-risk-register-diff-impl-plan-6-vs-end-of-phase-5).

---

## SurrealDB error boundary surfaced to the API

**What the operator sees.** A client request returns one of the typed
boundary responses below. The exact body is a static string — the
underlying SurrealDB error text never crosses the HTTP boundary by
design.

| HTTP status | Body | Meaning (R8 / D8) |
| --- | --- | --- |
| `409 Conflict` | `Conflicting concurrent update; please retry.` | Two writes raced; the second hit a transaction conflict. Client should back off and retry. |
| `507 Insufficient Storage` | `Storage capacity exhausted on the server.` | Disk full / KV quota refused / out-of-memory growth. Operator action required. |
| `500 Internal Server Error` | `Internal server error` | Generic write failure (constraint violation, validation, schema rejection). Look at the server log for the matching `tracing::warn!` with the full source error chain. |

**Why it happens.** Impl-plan §6 R8 calls for three named boundaries —
write failure, transaction conflict, capacity pressure — to be mapped
onto the SurrealDB Rust client per the D8 deviation. The boundary
classifier ([`crates/ts6-manager-server/src/db/error.rs`](../crates/ts6-manager-server/src/db/error.rs))
turns SurrealDB error variants into one of the four `DbBoundary` cases
above, and the static-string responses make sure no internal SurrealDB
message text leaks to the wire.

**What to do.**

- `409` — a transient retry is the documented client behaviour. The
  surrounding repo function may already retry; no operator action needed
  unless rate is abnormally high (then check for write contention on a
  hot key).
- `507` — clear space on the volume backing `/var/lib/ts6-manager/db`,
  then restart. Backup beforehand
  ([`runbook.md` § 3.2](runbook.md#32-volume-backup-and-restore)).
- `500` — grep the journal for a `tracing::warn!` with `error =` that
  carries the full anyhow chain, then file an issue with that block.

**Cross-link.** Full taxonomy and the static-string body table:
[`crates/ts6-manager-server/src/db/error.rs`](../crates/ts6-manager-server/src/db/error.rs).
The R8 risk-register entry is in
[`docs/phase6/readiness-audit.md` § 4](phase6/readiness-audit.md#4-risk-register-diff-impl-plan-6-vs-end-of-phase-5).

---

## FFmpeg fetch refused — DNS rebinding pin tripped

**What the operator sees.** A `POST /source` to the sidecar succeeds, but
the upstream fetch fails with a `502 Bad Gateway` returned by the local
loopback proxy and the sidecar log shows one of:

```
WARN  ts6_media_sidecar::http_pin: PinProxy: refusing non-http upstream scheme=https host=cdn.example.org
WARN  ts6_media_sidecar::http_pin: PinProxy: upstream attempted redirect; refusing (v1 no re-validation) status=302 host=cdn.example.org location=…
WARN  ts6_media_sidecar::http_pin: PinProxy: upstream send failed err=… host=cdn.example.org ip=203.0.113.42
```

**Why it happens.** The sidecar interposes a Rust-side `reqwest`-backed
loopback proxy in front of FFmpeg's outbound HTTP fetch
([PURA-172](https://github.com/FrozenTear/teamspeak-admin-panel/issues),
the v1 close-out of the R6 DNS-rebinding window — the deeper v2
[PURA-150](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
surface remains tracked separately). The proxy:

1. Requires the upstream URL to be plaintext HTTP (HTTPS is unchanged —
   TLS validation already pins to the cert SAN).
2. Pins the outbound socket to the IP `ts6-ssrf` validated against the
   private-range blocklist, regardless of any DNS the host's resolver
   would return at connect time.
3. Refuses to follow redirects (every 3xx becomes a `502`) — re-running
   SSRF per hop is v2 work.
4. Preserves the original `Host:` header so virtual-hosted CDNs continue
   to serve the right vhost.

A "refusing non-http upstream" or "refusing redirect" log is **the system
working as designed**: the upstream tried something the v1 SSRF surface
will not let through. A "DNS rebinding attempt" specifically would show
up as an upstream `send failed` against the SSRF-pinned IP, not a private-
range IP — the rebind cannot land on a private host because the proxy
never resolved against the rebinder's response.

**What to do.**

- **Non-http upstream refused.** Use an HTTPS source URL — the proxy is
  HTTP-only by design. HTTPS sources go straight to FFmpeg (TLS cert SAN
  validation pins the connection).
- **Redirect refused.** Resolve the redirect chain off-line and supply
  the final URL to `POST /source`. v1 will not re-validate per hop;
  follow-the-redirect is on the v2 roadmap.
- **Upstream `send failed`.** Confirm the upstream is reachable from the
  host network. If it is, the SSRF-pinned IP may be stale — restart the
  pipeline; the SSRF resolve is per `POST /source`.

**Cross-link.** Module doc + tracing call sites:
[`crates/ts6-media-sidecar/src/http_pin.rs`](../crates/ts6-media-sidecar/src/http_pin.rs).
Background on why URL-host → IP-literal rewrite was reverted (the obvious
"fix" that breaks TLS SNI):
[PURA-149](https://github.com/FrozenTear/teamspeak-admin-panel/issues).
Risk-register entry: R6 in
[`docs/phase6/readiness-audit.md` § 4](phase6/readiness-audit.md#4-risk-register-diff-impl-plan-6-vs-end-of-phase-5).

---

## Dashboard tick republisher silent / backing off

**What the operator sees.** The dashboard UI has gone stale (the per-
server client/channel counts are not updating). The journal shows
intermittent `WARN` lines from `ws::dashboard_tick` referencing a
`server_id` and a backoff window growing from 5 s toward 60 s.

**Why it happens.** The republisher
([PURA-81](https://github.com/FrozenTear/teamspeak-admin-panel/issues))
spawns one task per enabled `server_connection` and emits a
`dashboard:tick` envelope on `server:{id}:clients` and
`server:{id}:channels` every 5 s. On any WebQuery transport failure it
backs off exponentially (5 s → 60 s) and resets to 5 s on the first
successful call. Steady-state behaviour: silent. Backoff steady-state
behaviour: one warn line per attempt, capped at one per 60 s.

The two common operator-visible symptoms map to:

1. **TS6 fixture wedge.** The tick is hitting the fixture that has
   wedged after 5 WebQuery calls — see
   [TS6 fixture wedges](#ts6-fixture-wedges-after-5-webquery-requests)
   above.
2. **Wrong API key / WebQuery port.** The server connection row in the
   manager has stale credentials. Edit it through the UI — the
   republisher reconciles on the next cadence.

**What to do.**

```sh
# 1. Identify the failing server_id from the warn line.
journalctl --user -u ts6-manager-fullstack.service | grep dashboard_tick | tail

# 2. Reach the upstream WebQuery directly with that server's credentials.
curl -fsS -H "x-api-key: $API_KEY" "http://$HOST:$PORT/1/version"
```

If the manual call works but the republisher does not, the credentials in
the connection row do not match the ones in your shell; re-enter them in
the UI. If the manual call fails the same way, fix the upstream and the
republisher will recover on its next reconcile cycle (≤ 60 s).

**Cross-link.** Implementation:
[`crates/ts6-manager-server/src/ws/dashboard_tick.rs`](../crates/ts6-manager-server/src/ws/dashboard_tick.rs).

---

## Headless browser probes deadlock against the SPA

**What the operator sees.** A puppeteer / playwright / chrome-devtools
script connects to the manager UI, the WASM bundle hydrates, then every
CDP call (`page.content()`, `page.screenshot()`, `page.evaluate(...)`)
times out 5–12 s later. No `pageerror`, no console output, the network
panel shows the bundle loaded.

**Why it happens.** Dioxus 0.7 SPA hangs CDP after hydration in headless
mode. Tracked under
[PURA-131](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
(low priority, not a v1.0 gate blocker). The WASM mount completes — the
asset URLs confirm the new bundle is live — but something in the
hydrated app holds the CDP DOM-serialisation primitive long enough that
every subsequent CDP call hits its budget.

**What to do.** This is a known-bad path; no operator workaround in v1.0.

- **For automated browser smoke runs**, exercise the HTTP API directly
  rather than driving the SPA — the API surface is the same one the SPA
  consumes.
- **For visual regression**, use the `dx serve` dev server in a real
  Chromium profile (Helium 148 is the supported viewer) rather than a
  headless probe.
- **If you must drive the SPA from CI**, exclude the headless path until
  PURA-131 lands.

**Cross-link.**
[PURA-131](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
carries the full repro and the QA evidence directory.

---

## Where this doc came from

This doc consolidates failure modes observed across Phase 2–5
([PURA-67](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
rootless-volume, [PURA-75](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
SPA hydration, [PURA-93](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
sidecar gating, [PURA-105](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
passt + TS6 fixture, [PURA-129](https://github.com/FrozenTear/teamspeak-admin-panel/issues) /
[PURA-131](https://github.com/FrozenTear/teamspeak-admin-panel/issues)
headless probes) plus the WS-Security closeout
([PURA-161](https://github.com/FrozenTear/teamspeak-admin-panel/issues),
covering R5 / R6 / R7 / R8 + D8). New entries fold in here as QA
([PURA-184](https://github.com/FrozenTear/teamspeak-admin-panel/issues))
and operators surface friction.
