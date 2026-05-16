# Admin management — v1.1 HTTP API spec

- **Status:** draft, pending board ratification ([PURA-228](/PURA/issues/PURA-228)).
- **Architecture brief:** [`architecture.md`](./architecture.md).
- **Spec refs:** Chapter 6 (Security Model), Chapter 7 §7.0 (routing conventions, error shapes), §7.4 (Users — superseded here per architecture §8 deviation).

## 1. Conventions

All routes are mounted under `/api`. The wire-shape conventions follow spec §7.0:

- Auth column: `Y+admin` = `RequireAdmin` extractor (admin role, DB-current).
- JSON request and response bodies; camelCase field names matching the `crates/shared/src/admin.rs` types.
- Error envelopes per spec §7.0.2 — `{ "error": "<message>", "details": "<optional>" }`.
- Integer URL params (`{id}`, `{sid}`) parsed strictly per spec §7.0.1; non-integer → `400 { "error": "Invalid <name>: must be a number" }`.

`crates/shared/src/admin.rs` is new; types listed inline below. Existing `crates/shared/src/auth.rs::UserInfo` is reused on response shapes where the row projection is identical.

## 2. Wire types

### 2.1 `UserSummary`

The list/detail response shape. **Never includes `passwordHash`** (spec §7.4 implicit; v1.1 makes it explicit).

```ts
interface UserSummary {
  id: number;
  username: string;
  displayName: string;
  role: 'admin' | 'moderator' | 'viewer';
  enabled: boolean;
  createdAt: string;     // ISO-8601 UTC
  updatedAt: string;     // ISO-8601 UTC
  lastLoginAt: string | null;
  activeSessionCount: number;  // count of non-replaced, non-expired refresh_token rows
}
```

`activeSessionCount` is computed at read time: `SELECT count() FROM refresh_token WHERE userId = $id AND replacedBy IS NONE AND expiresAt > time::now()`. Cheap with the existing `refresh_token_user_idx` index.

### 2.2 `UserCreate`

```ts
interface UserCreate {
  username: string;       // 1..=64 ASCII, [a-z0-9._-]+ (lowercase). Unique.
  password: string;       // validated against spec §6.2.2 complexity rules
  displayName: string;    // 1..=128 chars, freeform
  role?: 'admin' | 'moderator' | 'viewer';  // defaults to 'viewer'
}
```

`username` is normalised to lowercase before the DB lookup (matches the existing `auth/routes.rs::login` behaviour). Duplicate usernames return `409 { "error": "Username already exists" }`.

### 2.3 `UserPatch`

```ts
interface UserPatch {
  displayName?: string;
  role?: 'admin' | 'moderator' | 'viewer';
  enabled?: boolean;
  password?: string;  // if present, validated against spec §6.2.2 and bcrypt-rehashed
}
```

A request body that does not contain any of these four keys is rejected with `400 { "error": "No mutable fields supplied" }`. An empty `password` string is rejected with the spec §6.2.2 length-failure message — empty-string-as-no-change semantics are reserved for the existing server-config sensitive-field convention (`apiKey`, `sshPassword`), and applying the same trick here would silently strip an admin password reset.

### 2.4 `SessionSummary`

```ts
interface SessionSummary {
  id: number;            // refresh_token.id
  family: string;        // refresh_token.family
  createdAt: string;     // ISO-8601 UTC
  expiresAt: string;     // ISO-8601 UTC
  replacedBy: string | null;  // null = active, non-null = predecessor in chain
}
```

The `token` value itself is **never** returned — `family` is sufficient for the operator UI and exposing the token would let any audit-log reader hijack an active session.

### 2.5 `AuditEvent`

Full schema in [`audit-shape.md`](./audit-shape.md) §2. Wire excerpt:

```ts
interface AuditEvent {
  id: number;
  occurredAt: string;
  insertedAt: string;
  actorUserId: number | null;       // null = actor was deleted post-event
  actorUsername: string;            // snapshot
  kind: AuditKind;                  // see audit-shape.md §2.1
  targetKind: string | null;
  targetId: number | null;
  targetLabel: string | null;       // snapshot
  payload: object | null;
  outcome: 'success' | 'failure';
  errorMsg: string | null;
  requestIp: string | null;
  requestUserAgent: string | null;
}
```

### 2.6 `Page<T>`

Shared envelope for paginated lists. Used by `GET /api/audit` and `GET /api/users/{id}/sessions`. Matches the shape the existing dashboard endpoint uses.

```ts
interface Page<T> {
  items: T[];
  total: number;          // exact row count for the filter set
  limit: number;          // echo of the request's limit (default + capped)
  offset: number;         // echo of the request's offset
}
```

## 3. Routes

### 3.1 Users — list / create

| Method | Path           | Auth     | Body            | Response  |
| ------ | -------------- | -------- | --------------- | --------- |
| GET    | `/api/users`   | Y+admin  | —               | `200 UserSummary[]` |
| POST   | `/api/users`   | Y+admin  | `UserCreate`    | `201 UserSummary` |

**`GET /api/users`** returns the full list (no pagination in v1.1 — the operator UI tolerates up to ~1000 users; v1.2 adds cursor pagination if a deployment exceeds that). Ordered by `id ASC`.

Query params (optional, all client-side filterable but server-side honoured for the audit table's larger result sets):

- `?role=admin|moderator|viewer` — filter by role.
- `?enabled=true|false` — filter by enabled state.
- `?q=<substring>` — case-insensitive substring on `username` or `displayName`.

**`POST /api/users`** creates a new user:

1. Validate `username` against the regex; lowercase it.
2. Validate `password` against spec §6.2.2 — return `400` with the spec verbatim failure string (per `crates/ts6-manager-server/src/auth/complexity.rs`).
3. Validate `role` is in the legal set or absent (default `viewer`).
4. Bcrypt-hash the password at cost 12 (already in `auth::password::hash`).
5. Insert via `repos::users::insert`. Duplicate-username errors translate to `409`.
6. Emit audit event `userCreated` (see `audit-shape.md` §2.1).
7. Return `201` with the new `UserSummary` (no password, `activeSessionCount: 0`).

### 3.2 Users — detail / patch / delete

| Method | Path                    | Auth     | Body         | Response  |
| ------ | ----------------------- | -------- | ------------ | --------- |
| GET    | `/api/users/{id}`       | Y+admin  | —            | `200 UserSummary` |
| PATCH  | `/api/users/{id}`       | Y+admin  | `UserPatch`  | `200 UserSummary` |
| DELETE | `/api/users/{id}`       | Y+admin  | —            | `204` |

**`PATCH /api/users/{id}`** enforces the self-action and last-enabled-admin protections (architecture §5.3):

1. Look up the target by id; `404 { "error": "User not found" }` if absent.
2. Reject self-disable, self-role-demote-away-from-admin, self-delete with `400 { "error": "<specific message>" }`. See architecture §5.3 for the exact list.
3. Reject patches that would leave zero enabled admins with `400 { "error": "Cannot remove the last enabled admin" }`. The check is a count query inside the same handler; documented TOCTOU under at-least-once execution license.
4. Apply the merged update:
   - `displayName`, `role`, `enabled` via `repos::users::update`.
   - `password` (if present) via `repos::users::set_password_hash` after complexity check.
5. **Session revocation** per architecture §5.4 table:
   - `enabled: false` set → `repos::refresh_tokens::delete_all_for_user(target.id)`.
   - `password` set → `repos::refresh_tokens::delete_all_for_user(target.id)`.
   - `role` change → no revocation.
6. Emit one or more audit events — one per logical change. Order: `userPatched`, `userDisabled` / `userEnabled` (if `enabled` changed), `userRoleChanged` (if role changed), `userPasswordReset` (if password set). Justification in `audit-shape.md` §2.2.
7. Return `200` with the refreshed `UserSummary`.

**`DELETE /api/users/{id}`** is the spec §7.4 verbatim shape:

1. Look up target; `404` if absent.
2. Refuse if `:id == requester.id` → `400 { "error": "Cannot delete yourself" }`.
3. Refuse if target is the last enabled admin → `400 { "error": "Cannot delete the last enabled admin" }`.
4. `repos::users::delete(target.id)` — the existing `user_cascade` event in `0001_baseline.surql` removes `refresh_token` and `server_user_grant` rows. The new `user_set_null_admin_audit` event in `0010_admin_audit_log.surql` null-points historic audit rows.
5. Emit `userDeleted` audit event **before** the delete (so the audit row's `targetId` is still resolvable for the event detail — the set-null cascade fires after the delete completes and clears it on the historic row).
6. Return `204`.

### 3.3 Sessions

| Method | Path                                          | Auth     | Body | Response |
| ------ | --------------------------------------------- | -------- | ---- | -------- |
| GET    | `/api/users/{id}/sessions`                    | Y+admin  | —    | `200 SessionSummary[]` |
| DELETE | `/api/users/{id}/sessions/{sid}`              | Y+admin  | —    | `204` |

**`GET /api/users/{id}/sessions`** lists all refresh-token rows for the target user — active and predecessor-preserved (per `auth::refresh` predecessor-preserved storage policy). Order by `createdAt DESC`. Operator UI filters active vs replaced client-side.

Returns `404` if the target user does not exist.

**`DELETE /api/users/{id}/sessions/{sid}`** revokes a single session by id:

1. Look up the `refresh_token` row by `{sid}`; `404` if absent.
2. Confirm the row belongs to user `{id}`; `404` if not (do not leak that the session exists under a different user).
3. **Delete the entire family**: `SELECT family FROM refresh_token WHERE id = $sid` → `DELETE refresh_token WHERE family = $family`. Killing one row leaves a partial chain that complicates reuse-detection; the whole-family revoke is correct under spec §6.5.
4. Emit `sessionRevoked` audit event.
5. Return `204`.

**Self-session-revoke is allowed** — an admin can kill their own active session via this route. The route does not refuse self-revoke because there is no operational footgun (the admin can just log in again).

### 3.4 Audit log

| Method | Path                | Auth     | Query params                                                                                        | Response             |
| ------ | ------------------- | -------- | --------------------------------------------------------------------------------------------------- | -------------------- |
| GET    | `/api/audit`        | Y+admin  | `actorUserId?`, `targetKind?`, `targetId?`, `kind?`, `outcome?`, `from?`, `to?`, `limit?`, `offset?` | `200 Page<AuditEvent>` |

**Query semantics:**

- `from` / `to` — ISO-8601 UTC; bounds on `occurredAt`. Both optional; if both present, `from <= to` enforced (400 otherwise).
- `kind` — exact match against the discriminant enum (`audit-shape.md` §2.1).
- `actorUserId` — exact match. `null` not supported via query string in v1.1 (operator UI does not need to filter for null-actor rows).
- `targetKind`, `targetId` — paired filter; supplying `targetId` without `targetKind` is rejected (400) so the index is usable.
- `outcome` — `success` or `failure`.
- `limit` — default 50, max 100. Values outside the range clamp to the bound silently (return value echoes the effective limit).
- `offset` — default 0. No upper bound; deep pagination is the operator's problem (v1.2 may add cursor pagination if needed).

Order: `occurredAt DESC, id DESC` for stable pagination.

The query is composed against the indexes documented in `audit-shape.md` §4.

### 3.5 Errors

| Status | When                                                                                | Body                                                              |
| ------ | ----------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| 400    | Validation failure (bad role string, password complexity, empty patch, etc.)        | `{ "error": "<specific>", "details": "<optional>" }`              |
| 401    | Missing / invalid / expired access token; user disabled mid-session (spec §6.4.1)   | `{ "error": "..." }` per spec §6.4 messages                        |
| 403    | Authenticated but not admin                                                          | `{ "error": "Insufficient permissions" }`                          |
| 404    | Target user / session / audit row not found                                          | `{ "error": "User not found" \| "Session not found" }`             |
| 409    | Username already exists                                                              | `{ "error": "Username already exists" }`                           |
| 422    | (reserved — not used in v1.1)                                                        | —                                                                 |
| 500    | Unhandled exception                                                                  | `{ "error": "Internal server error" }`                             |

Spec §7.0.2 is the source of truth; deviations from it should be noted in the deviation log file.

## 4. Worked examples

### 4.1 Create a moderator user

```http
POST /api/users HTTP/1.1
Content-Type: application/json
Authorization: Bearer <admin-jwt>

{
  "username": "moderator1",
  "password": "SecurePass123!",
  "displayName": "Moderator One",
  "role": "moderator"
}
```

```http
HTTP/1.1 201 Created
Content-Type: application/json

{
  "id": 2,
  "username": "moderator1",
  "displayName": "Moderator One",
  "role": "moderator",
  "enabled": true,
  "createdAt": "2026-05-19T13:05:14.512Z",
  "updatedAt": "2026-05-19T13:05:14.512Z",
  "lastLoginAt": null,
  "activeSessionCount": 0
}
```

### 4.2 Disable a user (revokes their sessions)

```http
PATCH /api/users/2 HTTP/1.1
Content-Type: application/json
Authorization: Bearer <admin-jwt>

{ "enabled": false }
```

```http
HTTP/1.1 200 OK

{ "id": 2, ..., "enabled": false, "activeSessionCount": 0 }
```

The handler issues `delete_all_for_user(2)` before returning. The disabled user's next REST request gets `401 { "error": "User account disabled or deleted" }` (spec §6.4.1 step 3). Two audit rows are emitted: `userPatched` and `userDisabled`.

### 4.3 Refuse last-enabled-admin removal

```http
PATCH /api/users/1 HTTP/1.1
Content-Type: application/json
Authorization: Bearer <admin-jwt-for-user-1>

{ "role": "viewer" }
```

```http
HTTP/1.1 400 Bad Request

{ "error": "Cannot remove the last enabled admin" }
```

No audit row emitted on the 400; the spec §7.0.2 application-error path is silent on the audit table. v1.1 could log failed mutation attempts (`userPatchFailed` event) but defers that to v1.2 — the read-heavy audit table benefits from being mutation-record-only.

### 4.4 Read the audit log

```http
GET /api/audit?kind=userDisabled&from=2026-05-19T00:00:00Z&limit=10 HTTP/1.1
Authorization: Bearer <admin-jwt>
```

```http
HTTP/1.1 200 OK
Content-Type: application/json

{
  "items": [
    {
      "id": 42,
      "occurredAt": "2026-05-19T13:08:01.221Z",
      "insertedAt": "2026-05-19T13:08:01.225Z",
      "actorUserId": 1,
      "actorUsername": "admin",
      "kind": "userDisabled",
      "targetKind": "user",
      "targetId": 2,
      "targetLabel": "moderator1",
      "payload": null,
      "outcome": "success",
      "errorMsg": null,
      "requestIp": "192.0.2.10",
      "requestUserAgent": "Mozilla/5.0 ..."
    }
  ],
  "total": 1,
  "limit": 10,
  "offset": 0
}
```

### 4.5 Revoke a session

```http
DELETE /api/users/2/sessions/17 HTTP/1.1
Authorization: Bearer <admin-jwt>
```

```http
HTTP/1.1 204 No Content
```

The handler deletes every refresh-token row in the family of session 17. The target user is signed out across all browsers/clients that share that login family. Audit event: `sessionRevoked` with `targetKind: 'session'`, `targetId: 17`, `payload: { "family": "<family-id>", "rowsDeleted": <n> }`.

## 5. Out of scope for v1.1 HTTP surface

- `GET /api/audit/{id}` single-row read — the list endpoint with `?id=` filter is sufficient; deferred.
- `POST /api/audit/export` — CSV / JSONL export. Operator can query DB directly in v1.1.
- `GET /api/sessions` global — all live sessions across all users. v1.2.
- `POST /api/users/{id}/password-reset` — separate route from PATCH-password. v1.1 keeps password inside `UserPatch` to keep the surface narrow.
- `POST /api/users/{id}/lock` (admin lockout flag) — `enabled: false` already covers the deactivate case; an explicit "locked" tri-state is a v1.2 product question.

## 6. References

- Spec Chapter 7 §7.0 (routing conventions, error shapes), §7.4 (Users — superseded per architecture §8).
- [`architecture.md`](./architecture.md) — the design context for these routes.
- [`audit-shape.md`](./audit-shape.md) — schema for the `AuditEvent` wire type and the `audit_log` table backing it.
- [`ui-brief.md`](./ui-brief.md) — UI surfaces that consume these endpoints.
- `crates/ts6-manager-server/src/auth/extractors.rs` — `RequireAdmin` definition.
- `crates/ts6-manager-server/src/auth/complexity.rs` — password-complexity error strings.
- `crates/ts6-manager-server/src/routes/servers.rs` — PATCH pattern precedent.
- `crates/shared/src/auth.rs` — `UserInfo` wire type (sibling of `UserSummary`).
