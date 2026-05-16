# Admin audit log — v1.1 shape spec

- **Status:** draft, pending board ratification ([PURA-228](/PURA/issues/PURA-228)).
- **Companion docs:** [`architecture.md`](./architecture.md), [`http-api.md`](./http-api.md), [`ui-brief.md`](./ui-brief.md).
- **Precedent:** `crates/ts6-manager-server/migrations/0006_ssh_audit_log.surql` (PURA-79) and `crates/ts6-manager-server/src/sshbridge/audit.rs`.
- **Why this is its own doc:** the audit table is the surface a future compliance reviewer reads. The schema, retention policy, and write-path semantics deserve a sign-off slot of their own — separate from the broader architecture brief — so the board can ratify the audit shape on its own merits.

## 1. Storage shape

### 1.1 Table definition

Migration: `crates/ts6-manager-server/migrations/0010_admin_audit_log.surql`.

```surql
-- =====================================================================
-- TS6 Manager — Phase 7 admin audit log (PURA-228).
--
-- Append-only per-event row for admin-mutating actions on the user
-- surface and adjacent areas. Mirrors the ssh_audit_log shape from
-- 0006_ssh_audit_log.surql (PURA-79).
--
-- Cascade posture:
--   - actorUserId set-null on user delete (mirrors ssh_audit_log R7).
--   - targetId set-null on user delete when targetKind = 'user'.
-- Other targetKind values cascade per their own surface (none for v1.1).
--
-- Retention: row-cap + TTL, enforced by a Tokio janitor (see audit-shape.md
-- §3.3 and §3.4).
-- =====================================================================

DEFINE SEQUENCE admin_audit_log_id BATCH 1 START 1;

DEFINE TABLE admin_audit_log SCHEMAFULL;
DEFINE FIELD actorUserId      ON admin_audit_log TYPE option<int>;
DEFINE FIELD actorUsername    ON admin_audit_log TYPE string;     -- snapshot
DEFINE FIELD kind             ON admin_audit_log TYPE string;
DEFINE FIELD targetKind       ON admin_audit_log TYPE option<string>;
DEFINE FIELD targetId         ON admin_audit_log TYPE option<int>;
DEFINE FIELD targetLabel      ON admin_audit_log TYPE option<string>; -- snapshot
DEFINE FIELD payload          ON admin_audit_log TYPE option<object> FLEXIBLE; -- redacted detail
DEFINE FIELD outcome          ON admin_audit_log TYPE string;     -- 'success' | 'failure'
DEFINE FIELD errorMsg         ON admin_audit_log TYPE option<string>;
DEFINE FIELD requestIp        ON admin_audit_log TYPE option<string>;
DEFINE FIELD requestUserAgent ON admin_audit_log TYPE option<string>;
DEFINE FIELD occurredAt       ON admin_audit_log TYPE datetime
    VALUE $value OR time::now() READONLY;
DEFINE FIELD insertedAt       ON admin_audit_log TYPE datetime
    VALUE $value OR time::now() READONLY;

DEFINE INDEX admin_audit_log_occurred_idx ON admin_audit_log FIELDS occurredAt;
DEFINE INDEX admin_audit_log_actor_idx    ON admin_audit_log FIELDS actorUserId;
DEFINE INDEX admin_audit_log_kind_idx     ON admin_audit_log FIELDS kind;
DEFINE INDEX admin_audit_log_target_idx   ON admin_audit_log FIELDS targetKind, targetId;

-- Mirrors `user_set_null_ssh_audit` from 0006_ssh_audit_log.surql (R7).
DEFINE EVENT user_set_null_admin_audit ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    UPDATE admin_audit_log SET actorUserId = NONE
        WHERE actorUserId = $uid;
    UPDATE admin_audit_log SET targetId = NONE
        WHERE targetKind = 'user' AND targetId = $uid;
};

-- Retention default. Override semantics enforced by the parser in
-- crate::audit::retention (mirrors crate::sshbridge::retention):
--   empty / unset → 365 (default)
--   '0'           → unbounded, WARN at boot
--   '1'..='29'    → clamped to 30, WARN at boot
--   '>= 30'       → honored
CREATE app_setting:admin_audit_retention_days CONTENT {
    key: 'admin_audit_retention_days',
    value: '365'
};
```

### 1.2 Field rationale

| Field              | Why                                                                                                                  |
| ------------------ | -------------------------------------------------------------------------------------------------------------------- |
| `actorUserId`      | FK back to `user.id` — `option<int>` so the row survives the actor's deletion (set-null event above). |
| `actorUsername`    | Snapshot of the actor's username **at the moment of the event**. Renaming the actor later does not rewrite history. |
| `kind`             | Discriminant for the event (see §2.1). String to allow forward-extensibility without a migration. |
| `targetKind`       | Optional; `null` for events without a single target (e.g., `auditConfigChanged`). |
| `targetId`         | FK back to the target table's id. `option<int>` for the same set-null reasoning as `actorUserId`. |
| `targetLabel`      | Snapshot of the target's human-readable label at event time (typically the target user's username). |
| `payload`          | Optional JSON object with event-kind-specific detail. Schema lives in §2.2 per event kind. |
| `outcome`          | `success` or `failure`. v1.1 only writes `success` rows — failure-path audit deferred to v1.2 (architecture §4.4 footgun). |
| `errorMsg`         | Only populated when `outcome = 'failure'`. Truncated to 4 KiB with sentinel (matches ssh_audit_log R2). |
| `requestIp`        | Source IP per the spec §6.8 reverse-proxy rules — leftmost `X-Forwarded-For` only one hop trusted. |
| `requestUserAgent` | Raw `User-Agent` header, truncated to 1 KiB. Useful for "was this UI or a script" forensics. |
| `occurredAt`       | Wall-clock at which the handler decided the action completed. UTC. READONLY. |
| `insertedAt`       | DB-write timestamp. Allows forensics to detect drift between handler and persistence. READONLY. |

### 1.3 Index rationale

- `admin_audit_log_occurred_idx` — every read of the audit log is `ORDER BY occurredAt DESC`. The first-class index makes deep pagination linear in page size, not table size.
- `admin_audit_log_actor_idx` — operator reads "what did Alice do?" and the `?actorUserId=` filter is the canonical entry point.
- `admin_audit_log_kind_idx` — operator reads "show me all `userDeleted` events".
- `admin_audit_log_target_idx` — operator reads "what happened to user 42?" via `?targetKind=user&targetId=42`.

No composite (kind + occurredAt) index in v1.1 — the operator UI's default sort works against `occurred_idx`, and filter-then-sort over a single-kind result set is cheap up to row-cap.

## 2. Event taxonomy

### 2.1 Event kinds (v1.1)

The full enum lives in `crates/ts6-manager-server/src/audit.rs::AuditKind` and the wire-side mirror in `crates/shared/src/admin.rs`. Strings are stable external-contract identifiers; renaming is a breaking change. **Adding** a new kind is non-breaking — the migration list does not need to update.

| Kind                  | Triggered by                                                              | `targetKind` | Notes |
| --------------------- | ------------------------------------------------------------------------- | ------------ | ----- |
| `userCreated`         | `POST /api/users` (success)                                                | `user`       | Payload includes the role and enabled state at creation. |
| `userPatched`         | `PATCH /api/users/{id}` (success, generic catch-all)                       | `user`       | Always emitted on PATCH; sibling rows below detail specific sub-changes. |
| `userDisabled`        | `PATCH /api/users/{id}` with `enabled: false → true→false transition`     | `user`       | Emitted alongside `userPatched`. Sessions revoked. |
| `userEnabled`         | `PATCH /api/users/{id}` with `enabled: true ← false transition`           | `user`       | Emitted alongside `userPatched`. No session change. |
| `userRoleChanged`     | `PATCH /api/users/{id}` with role transition                              | `user`       | Payload: `{ "from": "<role>", "to": "<role>" }`. |
| `userPasswordReset`   | `PATCH /api/users/{id}` with `password` present (admin-driven reset)      | `user`       | Sessions revoked. Payload does NOT contain the new password (see §2.3 redaction). |
| `userDeleted`         | `DELETE /api/users/{id}` (success)                                         | `user`       | Emitted BEFORE the user-row delete so `targetId` is still resolvable; the set-null event then clears the FK on the durable row. |
| `sessionRevoked`      | `DELETE /api/users/{id}/sessions/{sid}` (success)                          | `session`    | Payload: `{ "family": "<family>", "rowsDeleted": <n> }`. |
| `selfPasswordChanged` | `PUT /api/auth/password` (spec §6.2.3) — emitted by the auth surface     | `user`       | Actor and target are the same user. Differentiated from `userPasswordReset` so forensics can tell self-service apart from admin reset. |
| `setupCompleted`      | `POST /api/setup/init` (spec §7.2 first-admin bootstrap)                  | `user`       | Actor `actorUserId` is the newly-created admin; `actorUsername` is the bootstrap admin's chosen username. |

**No `*Failed` rows in v1.1.** Architecture §4.4 documents the trade-off: read-heavy audit reads stay clean (no failure-row noise), at the cost of forensics having to inspect server logs for the failure tail. v1.2 may add `*Failed` if a compliance review requires it.

**Not in scope of v1.1:**

- Server-connection CRUD audit (separate audit surface; deferred to v1.2 or folded into `admin_audit_log` later — punted because PURA-209 fix scope already churns that route surface).
- WebQuery-mutating-command audit (kick, ban, etc.) — covered by `ssh_audit_log` for the SSH path; the direct WebQuery path is a v1.2 audit add.

### 2.2 Per-event payload schemas

Payloads are stored as SurrealDB `option<object>`; the Rust side serialises typed structs via `serde_json::to_value`. **Always-present envelope:** none — `payload` is optional. Each event kind defines its own shape.

#### `userCreated`

```json
{
  "role": "moderator",
  "enabled": true
}
```

`username` and `displayName` are NOT in the payload — they live in the snapshot fields (`targetLabel` for username; `displayName` is not snapshotted because v1.1 does not surface "what was their displayName at creation" in the UI).

#### `userPatched`

```json
{
  "fields": ["displayName", "role", "enabled"]   // list of patched keys
}
```

This is the catch-all. Specific transitions get their own row (below).

#### `userDisabled` / `userEnabled`

```json
null
```

The transition is implicit in the kind. No payload needed.

#### `userRoleChanged`

```json
{
  "from": "moderator",
  "to": "admin"
}
```

#### `userPasswordReset` / `selfPasswordChanged`

```json
{
  "sessionsRevoked": 3
}
```

The new password is NEVER in the payload. The pre-existing password hash is NEVER in the payload. See §2.3.

#### `userDeleted`

```json
{
  "role": "moderator",
  "enabled": false,
  "sessionsRevoked": 2
}
```

#### `sessionRevoked`

```json
{
  "family": "a1B2c3D4eF5gH6iJ7k",
  "rowsDeleted": 4
}
```

#### `setupCompleted`

```json
{
  "role": "admin",
  "via": "setup_init"
}
```

### 2.3 Redaction rules

**Hard-blocklist** — the writer module asserts these are never present in `payload`:

- Plaintext passwords (any form).
- Bcrypt / Argon2 password hashes.
- JWT access tokens.
- Refresh token strings (only `family` is allowed — the family id alone is not a credential).
- TS6 ServerQuery API keys, SSH passwords, SSH private key blobs.
- `Authorization` header values.

The hard-blocklist is enforced by a `debug_assert!` denylist of credential-token shapes in `crate::audit::redaction`, mirroring `crate::sshbridge::audit`'s R3. The denylist also includes coarse heuristic checks (any string longer than 32 chars looking like base64 / hex) — production builds skip the heuristic to avoid false-positives degrading the audit row, but debug builds catch developer mistakes early.

**Soft-redact** — any field present but exceeding the cap is replaced with `{"_truncated": true, "_byteCount": <n>}`. Caps:

- `payload` as a whole: 2 KiB serialised.
- `errorMsg`: 4 KiB.
- `requestUserAgent`: 1 KiB.

Truncation does not error the event write — the operator gets a partial row, with the sentinel surfacing in the UI so they know to look at server logs.

## 3. Retention model

### 3.1 Default

`admin_audit_retention_days = 365` (one year). Stored as a row in the `app_setting` table for operator override; the migration creates the row.

### 3.2 Override semantics

Mirrors `ssh_audit_log` exactly (PURA-79's R1). Parser in `crate::audit::retention`:

| Input             | Effective retention | Boot-time log level                         |
| ----------------- | ------------------- | -------------------------------------------- |
| empty / unset     | 365 d (default)     | `info` ("admin audit retention: default 365 d") |
| `"0"`             | unbounded           | `warn` ("admin audit retention: UNBOUNDED — operator opt-in") |
| `"1"`..=`"29"`    | clamped to 30 d     | `warn` ("admin audit retention: clamped from <n> to 30 d") |
| `">= 30"`         | honored             | `info` |
| non-integer       | 365 d, error logged | `warn` ("admin audit retention: invalid value '<v>', falling back to 365 d") |

The 30-day floor protects against a misclick that silently destroys forensic evidence. SecurityEngineer signed off on this floor for the SSH path; the admin path reuses the same posture.

### 3.3 Row-cap (defence-in-depth)

In addition to the TTL, the writer enforces a global **row cap of 100 000**. When the table reaches the cap, the next insert triggers a synchronous cull of the oldest 1 000 rows (`DELETE FROM admin_audit_log WHERE id IN (SELECT id FROM admin_audit_log ORDER BY occurredAt ASC LIMIT 1000)`).

Why both: a misconfigured TTL of "unbounded" combined with a runaway admin-action loop would otherwise grow the table without bound. The 100 000 cap is conservative for an audit surface (a busy multi-admin community deployment writes < 1000 admin-mutating actions a year; 100 000 ≈ a century at that rate). v1.2 adds a configurable cap if the wedge attracts deployments that legitimately exceed it.

### 3.4 Janitor task

A `tokio::time::interval(Duration::from_secs(3600))` loop runs in the manager's background-task supervisor. On each tick:

1. Read the effective retention value (cached at boot; re-read every 1 h to pick up settings changes without restart).
2. `DELETE FROM admin_audit_log WHERE occurredAt < time::now() - <retention_days>` (skip when retention is unbounded).
3. If the row count is still over the row-cap, cull the oldest 1 000.
4. Log at `info` with rows-deleted count.

The janitor is shared by `ssh_audit_log` and `admin_audit_log` — same code path, different table names. v1.1 keeps them as two distinct janitor tasks for blast-radius isolation; v1.2 may unify if the operational overhead bites.

## 4. Write path

### 4.1 Single entrypoint

```rust
// crates/ts6-manager-server/src/audit.rs

pub struct Event {
    pub actor: AuthUser,
    pub kind: AuditKind,
    pub target: Option<Target>,
    pub payload: Option<serde_json::Value>,
    pub outcome: Outcome,
    pub error_msg: Option<String>,
    pub request_meta: RequestMeta,  // ip, user-agent — populated by extractor
}

pub async fn record(db: &Database, event: Event) {
    // 1. Apply hard-blocklist + truncation to payload, errorMsg, requestUserAgent.
    // 2. Build the row via repos::admin_audit_log::insert(...).
    // 3. tracing::warn! on failure; never panic, never propagate to the caller.
    // 4. If row count crossed the 100 000 cap, schedule a cull via the janitor's
    //    immediate-mode trigger (a `Notify` consumed inside the supervisor loop).
}
```

The caller never receives an error from `record`. This is deliberate: an audit-write failure is an operational bug, not a user-facing one. Spec §7.0.2's unhandled-exception path covers the user response if the user-mutation itself failed; the audit write is best-effort post-commit.

### 4.2 Caller pattern

Inside an admin-mutating route handler:

```rust
// 1. RequireAdmin extractor populates `actor: AuthUser`.
// 2. RequestMeta extractor (new — sibling of RequireAuth) populates ip + UA.
let actor = require_admin.0;
let target = users::find_by_id(&state.db, target_id).await?
    .ok_or(NotFound)?;

// 3. Validate and mutate. Audit row is only written on success.
let updated = users::update(&state.db, target_id, patch).await?;

// 4. Post-commit audit.
audit::record(
    &state.db,
    audit::Event {
        actor,
        kind: AuditKind::UserPatched,
        target: Some(audit::Target::user(updated.id, updated.username.clone())),
        payload: Some(json!({ "fields": patched_field_names })),
        outcome: audit::Outcome::Success,
        error_msg: None,
        request_meta,
    },
).await;

// 5. Emit sibling rows for sub-changes (role/enabled/password).
```

The handler emits **one row per logical change**. Architecture §5.4 and §2.1 list the emitted-kind rules; the audit module exposes a higher-level `record_user_patch_chain(...)` helper to keep the handler short.

### 4.3 RequestMeta extractor

New extractor in `crates/ts6-manager-server/src/auth/extractors.rs` (sibling of `RequireAuth`). Populates:

- `ip` — leftmost `X-Forwarded-For` entry if reverse-proxy mode is enabled, else `peer_addr` (matches spec §6.8 rate-limit posture).
- `user_agent` — raw `User-Agent` header value, truncated to 1 KiB at the extractor boundary.

The extractor is infallible — missing headers degrade to `None`. The audit row stores `None` as `option<string>` `NONE`.

## 5. Read path

### 5.1 Operator query patterns

The five canonical operator questions and the indexes they use:

| Question                                  | Filter                                                                  | Index used                                                |
| ----------------------------------------- | ----------------------------------------------------------------------- | --------------------------------------------------------- |
| "What happened in the last 24 hours?"     | `from = now - 1d`                                                       | `admin_audit_log_occurred_idx` (range scan on `occurredAt`) |
| "What did Alice do?"                      | `actorUserId = <alice>`                                                 | `admin_audit_log_actor_idx`                                |
| "Who deleted user `bob`?"                 | `kind = userDeleted, targetKind = user, targetId = <bob>`              | `admin_audit_log_target_idx` (composite)                   |
| "Show me all role changes."                | `kind = userRoleChanged`                                                | `admin_audit_log_kind_idx`                                 |
| "What did Alice do to user `bob`?"        | `actorUserId = <alice>, targetKind = user, targetId = <bob>`           | `admin_audit_log_actor_idx` then in-memory filter on target |

Read latency budget (single-deployment SurrealDB, ~10 000 rows): **< 50 ms per page of 50 rows**. The operator UI surfaces a "Query took N ms" debug line in dev builds; production builds suppress it.

### 5.2 Reuse with the existing SSH audit reader

`ssh_audit_log` already has a read endpoint (PURA-79 — see `crates/ts6-manager-server/src/routes/ssh_audit.rs` if it exists, otherwise file as a follow-up). v1.1 does NOT unify the two audit readers — they live in separate routes (`/api/audit` for admin, `/api/ssh-audit` for SSH). A unified "all audit streams" view is a v1.2 product surface.

### 5.3 Pagination

Offset-based (per `http-api.md` §3.4). Cursor-based would be cleaner but v1.1 sticks with offset to match the existing `crates/shared/src/dashboard.rs` `Page<T>` envelope. v1.2 may migrate audit-log paging to cursor if deep-page latency degrades.

## 6. How to verify

A new gate row in `scripts/ws-gate/admin-probe.sh` (specified in [`http-api.md`](./http-api.md) and the implementation child) covers the end-to-end admin path. Specifically for the audit shape:

1. **Audit-row-on-create** — `POST /api/users` succeeds → `GET /api/audit?kind=userCreated&targetId=<new-id>` returns exactly one row.
2. **Audit-row-on-disable** — `PATCH /api/users/{id}` with `enabled: false` → audit log contains BOTH `userPatched` AND `userDisabled` rows, in that order.
3. **Audit-row-on-delete** — `DELETE /api/users/{id}` → `GET /api/audit?kind=userDeleted` returns one row with `targetId` matching, **then** after the user-row delete completes the set-null event clears `targetId` on the durable row (this is a race the test tolerates: at-most-1-second polling).
4. **Redaction** — `PATCH /api/users/{id}` with `password: <newpass>` → audit row's `payload` does NOT contain the plaintext password, the bcrypt hash, or any field longer than 32 chars looking like a credential.
5. **Retention boot** — start the manager with `admin_audit_retention_days = '7'` → boot logs the `warn` line clamping to 30; manager keeps running.
6. **Set-null on user-delete** — create row referencing user-A, delete user-A, observe `actorUserId = NONE` on the historic row.

The gate script writes these as plain `curl` + `jq` lines, mirroring the FLOW v6 gate pattern.

## 7. Footguns

1. **Audit rows survive the actor — but `actorUsername` snapshot is the only forensic identifier.** If the manager is in a state where two users have shared a username over time (created → deleted → recreated), the snapshot is ambiguous. v1.1 mitigates by carrying `actorUserId` AS WELL AS the snapshot; forensics correlates by id where possible.
2. **Best-effort audit writes can drop on crash.** Documented in architecture §10. The user-facing mutation already committed; the operator sees the action effected but not recorded. Mitigation: server logs the intent at `info` before the user-mutation, so a forensic reconstruction is possible from logs even without the audit row.
3. **Retention is operator-controlled.** A malicious operator with admin role can set retention to `"0"` (unbounded — defence-in-depth row-cap still applies) or arrange other contortions. v1.1 explicitly is NOT a defence against a hostile admin — the boundary is "an admin can audit themselves out". Hostile-admin scenarios are out of v1.1 scope.
4. **Payload shape evolution.** Adding a new `kind` is free; changing the payload shape of an existing kind is **not** — it breaks forensic queries against historic data. v1.1 commits to the §2.2 payload shapes as a public contract. Sub-fields can be ADDED; existing keys cannot be renamed or removed without a deviation note.

## 8. References

- `crates/ts6-manager-server/migrations/0006_ssh_audit_log.surql` — direct precedent for table shape, set-null event, and retention.
- `crates/ts6-manager-server/src/sshbridge/audit.rs` — direct precedent for the writer-helper pattern, hard-blocklist, and truncation sentinels.
- [PURA-79](/PURA/issues/PURA-79) — SSH audit-log security review (R1–R7), much of which transfers to this design.
- Spec §6 — security model context.
- Spec §6.8 — rate-limit and reverse-proxy IP-extraction semantics that the `requestIp` field shares.
- [`architecture.md`](./architecture.md) §4.2, §5.4 — table shape rationale and the session-revocation events that feed the audit log.
- [`http-api.md`](./http-api.md) §2.5, §3.4 — wire types and query semantics on the read path.
