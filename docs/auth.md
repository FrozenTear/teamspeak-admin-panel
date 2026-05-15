# Auth TTLs + session-refresh failure modes

Operator-facing reference for the JWT access-token and refresh-token TTL
knobs and the SPA refresh-on-401 contract. Source of truth for the
mechanism: `crates/ts6-manager-server/src/auth/` (server) and
`crates/ts6-manager-server/src/client/session.rs` (SPA gate). Spec
chapters 6 + 7.

## TTL knobs

| Env var               | Default | Format          | Meaning                                                                    |
| --------------------- | ------- | --------------- | -------------------------------------------------------------------------- |
| `JWT_ACCESS_EXPIRY`   | `15m`   | `30s`/`15m`/`2h`/`7d` | Lifetime of access JWT minted at `POST /api/auth/login` and `POST /api/auth/refresh`. After this elapses every authed REST call returns `401 Invalid or expired token` until the SPA rotates. |
| `JWT_REFRESH_EXPIRY`  | `7d`    | same suffixes   | Lifetime of refresh-token rows in SurrealDB. After this the refresh row is rejected and the operator must re-authenticate. |
| `JWT_SECRET`          | —       | ≥32 random bytes | HS256 signing key. **Production refuses to start without a non-placeholder value.** Rotating this invalidates every access JWT in flight (refresh tokens survive — they're DB-rooted). |

Numeric-only values are seconds (`60` → 60s). Suffix parser lives in
`config::parse_duration`.

### Picking values

- `JWT_ACCESS_EXPIRY` is the worst-case staleness window for a revoked
  role / disabled user. Keep it short (≤ 15 min). Don't lower it below
  ~1 min — every expiry round-trips through the refresh endpoint, which
  is rate-limited along with `/login` (15 reqs / 15 min / IP).
- `JWT_REFRESH_EXPIRY` is the idle timeout for the whole session.
  Default 7 days balances "operator can come back the next day without
  re-login" against "stolen refresh token doesn't live forever".
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

## See also

- Refresh-token rotation: `crates/ts6-manager-server/src/auth/refresh.rs`
- SPA gate: `crates/ts6-manager-server/src/client/session.rs`
- AppShell redirect: `crates/ts6-manager-server/src/ui/layout/mod.rs`
- Spec: `study-documents/ts6-manager-spec.md` §6.4–§6.8
