# Auth TTLs + session-refresh failure modes

Operator-facing reference for the JWT access-token and refresh-token TTL
knobs and the SPA refresh-on-401 contract. Source of truth for the
mechanism: `crates/ts6-manager-server/src/auth/` (server) and
`crates/ts6-manager-server/src/client/session.rs` (SPA gate). Spec
chapters 6 + 7.

## TTL knobs

| Env var               | Default | Format          | Meaning                                                                    |
| --------------------- | ------- | --------------- | -------------------------------------------------------------------------- |
| `JWT_ACCESS_EXPIRY`   | `4h`    | `30s`/`15m`/`2h`/`7d` | Lifetime of access JWT minted at `POST /api/auth/login` and `POST /api/auth/refresh`. After this elapses every authed REST call returns `401 Invalid or expired token` until the SPA rotates. |
| `JWT_REFRESH_EXPIRY`  | `30d`   | same suffixes   | Lifetime of refresh-token rows in SurrealDB. After this the refresh row is rejected and the operator must re-authenticate. |
| `JWT_SECRET`          | —       | ≥32 random bytes | HS256 signing key. **Production refuses to start without a non-placeholder value.** Rotating this invalidates every access JWT in flight (refresh tokens survive — they're DB-rooted). |

Numeric-only values are seconds (`60` → 60s). Suffix parser lives in
`config::parse_duration`.

### Picking values

- `JWT_ACCESS_EXPIRY` is the worst-case staleness window for a revoked
  role / disabled user. Default 4h is tuned for a single-operator panel
  where revocation latency is acceptable in exchange for far fewer
  silent-rotation events (each rotation is a chance for transient
  failure to surface as a user-visible bounce). Tighten to ≤ 15 min on
  multi-user deploys where revocation latency matters more. Don't
  lower below ~1 min — every expiry round-trips through the refresh
  endpoint, which is rate-limited along with `/login` (15 reqs / 15
  min / IP).
- `JWT_REFRESH_EXPIRY` is the idle timeout for the whole session.
  Default 30 days balances "operator can come back next month without
  re-login" against "stolen refresh token doesn't live forever".
  Tighten on multi-user deploys.
- `JWT_ACCESS_EXPIRY` MUST be shorter than `JWT_REFRESH_EXPIRY`. Boot
  doesn't enforce this yet; setting access > refresh produces a
  session that re-issues access tokens faster than the refresh row
  itself rotates, which is harmless but pointless.

## Lifecycle

1. `POST /api/auth/login` mints `(access_jwt, refresh_token)` and
   persists the refresh row in `refresh_token` with a fresh family id.
2. SPA stores both in `localStorage` under `ts6-manager.auth.session`.
   `localStorage` survives page reload; `sessionStorage` is NOT used.
3. Every authed `/api/*` call attaches `Authorization: Bearer <access>`
   via the [`RefreshGate`](../crates/ts6-manager-server/src/client/session.rs).
4. When the server returns `401 Invalid or expired token`, the gate
   single-flights one `POST /api/auth/refresh`. On success it rotates
   the access + refresh tokens in place (predecessor-preserved per
   §6.5.3) and replays the original request.
5. The WebSocket at `/api/ws?token=<jwt>` authenticates on connect
   only — it does NOT re-auth on the same socket after access expiry.
   A reconnect picks up the current access token from the session
   signal.

## Refresh failure contract (PURA-214)

The gate distinguishes recoverable vs. unrecoverable refresh failures:

- `POST /api/auth/refresh` → **401**: session is dead (token replayed,
  family revoked, owning user disabled). Gate wipes the in-memory and
  `localStorage` state; `AppShell` redirects to `/login`. This is the
  spec §6.5.4 reuse-detection path.
- `POST /api/auth/refresh` → **5xx**, **4xx other than 401**,
  **transport error**, or **JSON parse error**: session stays
  authenticated. The caller's original 401 surfaces as the raw
  refresh error (renderable as a "service unavailable" banner). The
  next `/api/*` call retries the refresh naturally.

Why this asymmetry: the deployed v1.0 image (PURA-181) restarts on
healthcheck failure. A refresh that races a restart blip returns a
transport error or a 502 through the reverse proxy. Before PURA-214
the gate invalidated on ANY refresh failure, bouncing the operator to
`/login` every few minutes of normal use. The fix scopes invalidation
to 401 only — anything else is "rotation did not happen", not
"session is dead".

## 401 sub-codes the gate treats (PURA-225)

The backend's `RequireAuth` extractor emits three distinct 401
envelopes (all share `Content-Type: application/json` and the spec
`{ "error": "<verbatim copy>" }` shape — strings defined in
`crates/shared/src/auth.rs` `auth_error_strings`):

| Sub-code body                       | Server reason                                                                                        | Gate treats as                                                          |
| ----------------------------------- | ---------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------- |
| `Invalid or expired token`          | JWT verify failed (expired / bad signature) OR the DB lookup for the user id errored.                | **Refresh-eligible.** One single-flight refresh; replay; invalidate only if the replay still 401s. |
| `User account disabled or deleted` | DB lookup returned `Some(user)` with `enabled = false`, or returned `None` (user row gone).         | **Session-killing.** Invalidate immediately — refresh cannot resurrect the row. |
| `No token provided`                 | The request reached the extractor without a `Authorization: Bearer <jwt>` header.                    | **Session-killing.** Invalidate immediately — the SPA's bearer is missing; refresh would also miss it. |

A 401 with any **other** body (empty, unknown sub-code, non-JSON) is
also treated as session-killing — the gate's
`is_invalid_or_expired_token()` test only passes for the exact spec
string. This is the PURA-225 contract: the gate refuses to leave the
session `Authenticated` after a 401 the server itself called "session
is dead" — that combination strands the operator on a "Session
expired" banner with no path back to `/login`.

A successful refresh followed by a **replayed call that also 401s** is
the same signal: invalidate. The server rotated the access token but
the upstream still rejects it, so the family is dead at the server.

### Operator-facing escape hatch

If the refresh path is wedged in a way that PURA-214's transient
handling cannot recover from (proxy 5xx loop, JSON corruption from a
buggy intermediary), the gate keeps the session alive on purpose. To
ensure the operator is never stuck, every authed surface that renders
a "Session expired" banner (today: `/servers`, and the chrome's
`ServerSelector` dropdown footer) also surfaces a primary **"Sign in
again"** button. It calls `session.replace(AuthState::Anonymous)` and
`nav.replace(LoginPage { next })` so a single click reaches `/login`
with the current path captured for return after re-auth.

### Test coverage

The contract is pinned by unit tests in `client/session.rs`:

- `non_invalid_token_data_401_invalidates_session` — `USER_DISABLED` /
  `NO_TOKEN` 401s clear the session.
- `non_401_data_errors_do_not_invalidate_session` — `Server`,
  `Transport`, `Client` errors keep the session alive.
- `replay_401_after_successful_refresh_invalidates_session` — a 401 on
  the replay after a successful refresh kills the session.
- `transient_refresh_failure_keeps_session_authenticated` /
  `non_401_4xx_refresh_failure_keeps_session_authenticated` — PURA-214
  regression (kept).
- `refresh_failure_invalidates_session_no_silent_retry` — 401 on
  refresh itself (kept).

UI-side coverage lives in `ui::pages::servers_index::tests`
(`unauthorized_error_state_renders_sign_in_again_cta`,
`non_unauthorized_error_state_omits_sign_in_again_cta`).

## Logging

The server logs at boot summarise the active config (see
`Config::log_hardening_summary`). Token TTLs are NOT logged at INFO
because they're operator-tunable; check the env if in doubt:

```sh
podman exec ts6-manager-fullstack env | grep ^JWT_
```

Refresh-token reuse detection emits `WARN refresh-token reuse detected;
revoking all sessions for user`. Any of those in the journal means
either an attacker replayed a captured token OR (more commonly) a tab
got resumed from `localStorage` after another tab already rotated past
it. Both paths are correctly handled — the operator just re-logs in.

### Boot-time refresh-token volume check (PURA-226)

At startup the server emits one of:

- `INFO  refresh-token volume snapshot at boot users=…
  refresh_tokens=… distinct_users_with_tokens=…` — normal.
- `WARN  no live refresh tokens at boot — if operators report repeat
  involuntary sign-outs, suspect ephemeral DB volume; see docs/auth.md
  §debug-knob` — every operator who was signed in before the restart
  will bounce to `/login` on their next request. Common cause: the
  panel container's data volume is not mounted (or got recreated).
  Mount a persistent volume at `/data` (see `deploy/`); the surrealkv
  store there carries the refresh-token table across restarts.

## Debug knob — root-causing involuntary logouts (PURA-226)

When operators report repeat involuntary sign-outs in the deployed
image, the candidate failure modes are:

1. `/api/*` returns a 401 whose body the gate misclassifies.
2. `POST /api/auth/refresh` fails non-401 and the *data* 401 strands
   the operator on the [PURA-225](/PURA/issues/PURA-225) escape-CTA
   banner.
3. Refresh succeeds but the rotated-pair `localStorage` write is
   racy / dropped.
4. Backend rejects the refresh row because the container lost its
   DB volume on restart.

The SPA and the backend can both emit structured, opt-in breadcrumbs
that pinpoint exactly which mode fired.

### Turning it on

**Browser (SPA).** Either:

- Append `?debug=auth` to any URL the operator hits (the knob is
  read once and cached for the page load), or
- In DevTools console: `localStorage.setItem('ts6-manager.debug',
  'auth')`, then reload.

Every event the operator copies out of DevTools console looks like:

```
[ts6-manager:auth] {"ts":1234.5,"tag":"gate.401.session_killing","data":{"body":"401 Unauthorized: User account disabled or deleted","invalidates_session":true}}
```

Tags emitted on the auth path:

| Tag | Meaning |
| --- | --- |
| `api.call.enter` / `api.call.exit` | Wrapper around every `/api/*` call. `exit.outcome` is one of `ok` / `unauthorized` / `bad_gateway` / `client` / `server` / `transport` / `deserialise`. |
| `api.raw_401` | A 401 came off the wire — `data.body` is the verbatim error string (`Invalid or expired token`, `User account disabled or deleted`, `No token provided`, …). **Mode 1** fingerprint when the body is unfamiliar. |
| `gate.enter` | `RefreshGate::run` received a request. `data.access` is the first 8 chars of the bearer attached. |
| `gate.anonymous_short_circuit` | The gate ran with `AuthState::Anonymous` — a request raced a logout / cold-start. |
| `gate.401.refresh_eligible` | The first call's 401 carried `Invalid or expired token`; the gate is about to single-flight `POST /api/auth/refresh`. |
| `gate.401.session_killing` | The first call's 401 was `User account disabled or deleted` / `No token provided` / unknown body — the session has been invalidated and the operator will bounce to `/login`. |
| `gate.non_401_propagate` | A non-401 error surfaced; session stays authenticated. |
| `gate.refresh.start` / `gate.refresh.ok` / `gate.refresh.fail` | The single-flight refresh attempt + duration. `fail.kept_session_alive` is `true` for PURA-214 transient errors (5xx / 4xx / transport / deserialise) and `false` for an actual 401 on `/api/auth/refresh`. **Mode 2** fingerprint when `kept_session_alive=true` immediately precedes an `api.raw_401` whose body is not `Invalid or expired token`. |
| `gate.replay_with_peer_rotation` | Another tab rotated while we waited on the lock — replaying with the freshly persisted access token. |
| `gate.replay_401_invalidate` | The post-refresh replay 401'd, so the family is dead at the server. Invalidated. |
| `refresh.post.send` / `refresh.post.ok` / `refresh.post.err` | The HTTP transport for `/api/auth/refresh`. `request_id` (e.g. `rfr-3`) pairs `send` with the outcome; `duration_ms` measures the round-trip. |
| `session.update_pair` | The gate handed a rotated pair off to the in-memory + localStorage session. `data.new_access` is the first 8 chars of the new access token. **Mode 3** fingerprint when this fires but the subsequent `storage.set` shows `err=true` or `bytes` of an empty blob. |
| `session.update_pair.dropped_on_anonymous` | A rotation was racy with a logout; the gate refused to resurrect a logged-out session. |
| `session.invalidate` | The session was invalidated. `data.reason` names the branch (`refresh_401`, `post_refresh_replay_401`, `first_err_session_killing`, `peer_rotation_replay_401`). |
| `session.replace` | A non-gate call (login, logout, escape-CTA) swapped the entire session. `from` / `to` are booleans indicating the authenticated-state transition. |
| `session.rehydrate` | The post-mount `use_effect` copied the persisted blob into the signal. `hydrated=false` after a fresh page-load means `localStorage` was empty (operator never signed in, or another tab cleared it). |
| `storage.get` / `storage.set` / `storage.remove` | Filtered to the `ts6-manager.auth.session` key. `err=true` on a `set` is the **Mode 3** smoking gun. |

**Backend.** Set `LOG_LEVEL=ts6_manager_server::auth=debug,info` (the
default is `info`, which keeps the lines off). The relevant entries:

- `auth 401 sub_code=… path=… user_id=…` — every 401 the `RequireAuth`
  extractor emits, with the path that produced it. **Mode 1** in the
  panel logs.
- `refresh ok user_id=… family=… duration_ms=…` — successful rotation.
- `refresh 401 reason=… duration_ms=…` — `POST /api/auth/refresh` got
  rejected. `reason` is `invalid_or_expired` (most likely PURA-225
  reuse-detection signal *or* **Mode 4** if the row was lost on
  restart), `user_row_missing_after_rotate`, or `user_disabled_after_rotate`.

### Mapping breadcrumbs back to failure modes

| Mode | Fingerprint in the console + panel logs |
| --- | --- |
| 1. 401 body misclassified | `api.raw_401` with an unfamiliar body, followed by `gate.401.session_killing` (not `gate.401.refresh_eligible`). |
| 2. Non-401 refresh fails, data 401 strands UI | `gate.refresh.fail` with `kept_session_alive=true`, immediately preceded by `api.raw_401` whose body is `Invalid or expired token`. |
| 3. Storage write dropped | `session.update_pair` with a new prefix, then `storage.set err=true` (or an `update_pair.dropped_on_anonymous` after the lock). |
| 4. Backend lost refresh row on restart | Panel `WARN no live refresh tokens at boot` at startup, OR backend `refresh 401 reason=invalid_or_expired` for every operator simultaneously after a restart. |

### Turning it off

- URL: drop the `?debug=auth` param. The cache is per-page-load so a
  navigation that omits the param disables the knob.
- Storage: `localStorage.removeItem('ts6-manager.debug')`, then
  reload.
- Backend: revert `LOG_LEVEL` to `info` (or unset it).

### Default behavior

With no knob set, the SPA emits **zero** console breadcrumbs and the
backend stays at `info`, so production noise is unchanged.

## See also

- Refresh-token rotation: `crates/ts6-manager-server/src/auth/refresh.rs`
- SPA gate: `crates/ts6-manager-server/src/client/session.rs`
- AppShell redirect: `crates/ts6-manager-server/src/ui/layout/mod.rs`
- Spec: `study-documents/ts6-manager-spec.md` §6.4–§6.8
