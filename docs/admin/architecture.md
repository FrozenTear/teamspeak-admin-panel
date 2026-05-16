# Admin management — v1.1 architecture brief

- **Status:** draft, pending board ratification ([PURA-228](/PURA/issues/PURA-228)).
- **Spec refs:** Chapter 6 (Security Model), Chapter 7 §7.1–§7.4 (REST API surface).
- **Parent epic:** [PURA-227](/PURA/issues/PURA-227) (Phase 7).
- **Sibling brief:** [`docs/flows/architecture.md`](../flows/architecture.md).

## 1. Purpose

v1.0 ships a **single-admin** model: the setup wizard creates one bootstrap admin row and there is no REST or UI surface for adding, listing, editing, disabling, or auditing additional admins. This is hostile for any deployment beyond a single-operator install.

v1.1 closes the gap with the smallest, gate-able cut:

1. Expose the **existing** three-role model (`admin` / `moderator` / `viewer`) via `/api/users` REST.
2. Land an admin-only UI for user CRUD, session management, and an audit log viewer.
3. Persist an `admin_audit_log` table that records who-did-what-when for the events compliance / forensics care about.

The engine for the role model already exists in `crates/ts6-manager-server/src/auth/extractors.rs`. v1.1 builds the REST + UI on top — no new permission machinery, no SSO, no per-server-group ACLs.

This document is the **architecture brief**. HTTP wire, UI brief, and audit-log shape live in sibling files:

- [`http-api.md`](./http-api.md)
- [`ui-brief.md`](./ui-brief.md)
- [`audit-shape.md`](./audit-shape.md)

## 2. v1.1 scope vs deferred

### 2.1 In scope (v1.1)

- **Existing roles exposed.** `/api/users` CRUD over `admin`, `moderator`, `viewer` per [spec §6.1](../../study-documents/ts6-manager-spec.md). No new tiers.
- **Persistence.** Reuses the existing `user` table (no migration to `user`). Adds `admin_audit_log` table via new migration `0010_admin_audit_log.surql`.
- **Session lifecycle.** Disabling, deleting, or password-resetting a user revokes that user's refresh-token family set immediately (re-uses the §6.5 family-revocation primitive already in `auth/refresh.rs`).
- **Authorization.** `RequireAdmin` extractor (existing) gates `/api/users/*` and `/api/audit/*`. No new extractor.
- **UI surface.** Three admin pages — Users, Sessions (per-user pane), Audit. Header-gated by role.
- **Audit model.** Append-only row per admin-action event; redaction of secrets at the write boundary; bounded by row-cap + TTL.
- **First-admin protection.** Self-delete refused (already in spec §7.4). Self-disable, self-role-demote, and final-admin-delete extended to the same protection.
- **Gate.** `scripts/ws-gate/admin-probe.sh` — bootstrap admin creates second admin → second admin signs in → audit log shows both events.

### 2.2 Out of scope (defer to v1.2+)

- **SSO / SAML / OIDC.** v1.1 stays on the existing username + password + JWT + refresh-token model.
- **Multi-tenant / org-level admin.** A single deployment = a single admin pool. Tenancy is a v1.2 product surface, not a v1.1 schema concern.
- **Per-server-group permission grain.** TeamSpeak's permission editor surface is a separate v1.2 work item; v1.1 keeps the role-coarse authorization model.
- **Audit log export.** CSV / JSONL export from the UI is out — operators can query SurrealDB directly if they need bulk extract in v1.1.
- **Soft-delete + reactivate.** v1.1 uses **hard-delete** with `userId` set-null on the audit-log row (matches the `ssh_audit_log` precedent). Soft-delete adds a `disabledAt` column gymnastic that the existing `enabled: bool` flag already covers more cleanly.
- **MFA / 2FA.** Out of v1.1; flagged here for v1.2 if compliance demands it.
- **Email-driven password reset.** v1.1 has admin-driven reset only (admin sets a new password directly); no transactional-email plumbing.

## 3. User and role model

### 3.1 Existing surface (do not touch)

`crates/ts6-manager-server/src/repos/users.rs` already defines:

```rust
pub struct User {
    pub id: i64,
    pub username: String,
    pub passwordHash: String,
    pub displayName: String,
    pub role: String,
    pub enabled: bool,
    pub createdAt: DateTime<Utc>,
    pub updatedAt: DateTime<Utc>,
    pub lastLoginAt: Option<DateTime<Utc>>,
}
```

CRUD: `insert`, `find_by_id`, `find_by_username`, `list`, `count`, `update`, `set_password_hash`, `mark_login`, `delete`. `UserUpdate` already exposes `displayName`, `role`, and `enabled` as sparse optional fields — the wire-level PATCH semantics map directly.

`crates/ts6-manager-server/src/auth/extractors.rs` already defines:

- `RequireAuth(AuthUser)` — DB-current role per [spec §6.4.1](../../study-documents/ts6-manager-spec.md).
- `RequireAdmin(AuthUser)` — admin-only gate.
- `RequireModerator(AuthUser)` — admin OR moderator.

The 1.1 admin work uses these as-is.

### 3.2 Role semantics

v1.1 ships the three-role model verbatim from spec §6.1:

| Role        | What it can do |
| ----------- | -------------- |
| `admin`     | Full read/write across users, server connections, flows, music bots, widgets, settings, audit log. |
| `moderator` | Read/write on flows, music bots, widgets, and TS-control actions within servers granted by `UserServerAccess`. **Cannot manage users or read the audit log.** |
| `viewer`    | Read-only on the same scope as moderator. **Cannot manage users or read the audit log.** |

The role is a `string` column, defaulting to `viewer` (per migration `0001_baseline.surql`). The extractor normalises unknown values to `viewer` (per spec §6.1's "MUST treat any value other than admin/moderator/viewer as viewer"). v1.1 keeps that normalisation — the REST layer rejects bad role inputs at the request boundary with 400, but the read path stays defensive.

### 3.3 Why no new "operator" tier in v1.1

The brief asked: option (a) `is_admin` + new `operator` tier, or option (b) full RBAC. **Neither.** The existing three-role surface is already in code and already in the spec; v1.1 exposes it. Adding a fourth tier in v1.1 (or replacing the three with full RBAC) costs:

- A migration on `user.role` (or a new `user_role` join table).
- New extractor variants and a new role-check helper API.
- A re-audit of every `is_admin()` and `is_at_least_moderator()` call site (only one of each today, but the surface widens with every Phase 7 implementation child).
- Documentation, UI copy, and operator mental-model churn for an admin tier that the wedge does not yet need.

v1.2 revisits if the wedge ([self-host vs Discord](../../README.md)) demands per-server-group delegation. For v1.1, the three-tier surface is the wedge.

## 4. Persistence model

### 4.1 What does NOT change

- **`user` table.** Already has `username`, `passwordHash`, `displayName`, `role`, `enabled`, `createdAt`, `updatedAt`, `lastLoginAt`. No migration needed for the user model itself.
- **`refresh_token` table.** Already has the `family` / `replacedBy` shape per spec §6.5. No migration needed for the session model.
- **Existing cascade events** (`user_cascade` in `0001_baseline.surql`, `user_set_null_ssh_audit` in `0006_ssh_audit_log.surql`). The new admin-audit table mirrors the latter pattern (set-null on user-delete) so audit rows survive forensically even after the actor is gone.

### 4.2 What v1.1 adds

**One new migration:** `crates/ts6-manager-server/migrations/0010_admin_audit_log.surql`. Schema and event details live in [`audit-shape.md`](./audit-shape.md); summary here:

```surql
DEFINE SEQUENCE admin_audit_log_id BATCH 1 START 1;

DEFINE TABLE admin_audit_log SCHEMAFULL;
DEFINE FIELD actorUserId      ON admin_audit_log TYPE option<int>;
DEFINE FIELD actorUsername    ON admin_audit_log TYPE string;  -- snapshot at event time
DEFINE FIELD kind             ON admin_audit_log TYPE string;  -- event discriminant
DEFINE FIELD targetKind       ON admin_audit_log TYPE option<string>;
DEFINE FIELD targetId         ON admin_audit_log TYPE option<int>;
DEFINE FIELD targetLabel      ON admin_audit_log TYPE option<string>; -- snapshot
DEFINE FIELD payload          ON admin_audit_log TYPE option<object> FLEXIBLE; -- redacted detail
DEFINE FIELD outcome          ON admin_audit_log TYPE string;  -- "success" | "failure"
DEFINE FIELD errorMsg         ON admin_audit_log TYPE option<string>;
DEFINE FIELD requestIp        ON admin_audit_log TYPE option<string>;
DEFINE FIELD requestUserAgent ON admin_audit_log TYPE option<string>;
DEFINE FIELD occurredAt       ON admin_audit_log TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD insertedAt       ON admin_audit_log TYPE datetime VALUE $value OR time::now() READONLY;

DEFINE INDEX admin_audit_log_occurred_idx ON admin_audit_log FIELDS occurredAt;
DEFINE INDEX admin_audit_log_actor_idx    ON admin_audit_log FIELDS actorUserId;
DEFINE INDEX admin_audit_log_kind_idx     ON admin_audit_log FIELDS kind;

DEFINE EVENT user_set_null_admin_audit ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    UPDATE admin_audit_log SET actorUserId = NONE WHERE actorUserId = $uid;
    UPDATE admin_audit_log SET targetId = NONE
        WHERE targetKind = 'user' AND targetId = $uid;
};

CREATE app_setting:admin_audit_retention_days CONTENT {
    key: 'admin_audit_retention_days',
    value: '365'
};
```

The pattern mirrors `ssh_audit_log` (`0006_ssh_audit_log.surql`) deliberately:

- **READONLY `occurredAt` and `insertedAt`** — supports the audit-trail tamper-resistance posture; no in-place edits.
- **Snapshot fields** (`actorUsername`, `targetLabel`) — the row remains readable even after the actor or target is renamed or deleted.
- **`userId` set-null** — the audit row survives user-delete cascades (matches the `ssh_audit_log` R7 pattern from PURA-79).
- **`app_setting` row for retention** — operator override via env var or settings UI; floor + default enforced in the repo layer (see `audit-shape.md` §3.3).

### 4.3 Migration list update

Update `crates/ts6-manager-server/src/db/migrations.rs` to register `0010_admin_audit_log.surql` and add the file to the pinned migration list in tests (the test that asserts the pinned ordering — see `feedback_run_tests_before_tagging.md` lesson learned from PURA-209).

### 4.4 Surrealdb operator notes

- `admin_audit_log` rows are **never updated, never deleted in-flight**. Only the retention janitor (§4.4 in `audit-shape.md`) prunes by `occurredAt < now - retention_days`.
- The repo layer enforces a **2 KiB cap on `payload` JSON** with a truncation sentinel; oversized payloads degrade to `{"_truncated": true, "_byteCount": <n>}` so the row still indexes cleanly.
- No row-level READ permissions in the SurrealDB schema — the `RequireAdmin` extractor is the only authorization gate (defence-in-depth via the REST layer, not the DB).

## 5. Authorization surface

### 5.1 Routes that become admin-only in v1.1

Per spec §6.12, `/api/users/*` is already documented as admin-required. v1.1 implements that surface for the first time. New admin-only routes:

| Path                         | Existing extractor used |
| ---------------------------- | ----------------------- |
| `GET /api/users`             | `RequireAdmin`          |
| `POST /api/users`            | `RequireAdmin`          |
| `GET /api/users/{id}`        | `RequireAdmin`          |
| `PATCH /api/users/{id}`      | `RequireAdmin`          |
| `DELETE /api/users/{id}`     | `RequireAdmin`          |
| `GET /api/users/{id}/sessions` | `RequireAdmin`        |
| `DELETE /api/users/{id}/sessions/{sid}` | `RequireAdmin` |
| `GET /api/audit`             | `RequireAdmin`          |

Wire shape and per-route semantics live in [`http-api.md`](./http-api.md). The deviation from spec §7.4 (which lists `PUT` not `PATCH`, and does not list `/sessions` or `/audit`) is intentional and documented in §8 below.

### 5.2 Routes that stay operator-allowed or anonymous

Per spec §6.12, nothing else changes in v1.1:

- `/api/auth/me`, `/api/auth/password` — any authenticated user.
- `/api/servers` read — any authenticated user; write — admin.
- `/api/setup/*` — anonymous, gated by user-count check (already in `auth/routes.rs`).

No write paths that are currently `RequireAuth` get demoted or promoted in v1.1.

### 5.3 First-admin / self-action protections

The route layer enforces the following on `PATCH /api/users/{id}` and `DELETE /api/users/{id}`:

1. **Self-delete refused.** Spec §7.4 already mandates `400 {"error":"Cannot delete yourself"}` when `:id == requester.id`. v1.1 keeps this verbatim and extends it to PATCH-disable and PATCH-role-demote (e.g., `admin` self-demoting to `viewer` would lock the operator out).

   | Action attempted on self | Allowed? |
   | ------------------------ | -------- |
   | `displayName` change     | yes      |
   | `password` change        | yes (separate audit event from admin reset) |
   | `enabled: false`         | **no**   |
   | `role` change away from `admin` | **no** |
   | `DELETE`                 | **no**   |

2. **Final-admin protection.** A `PATCH /api/users/{id}` that would result in **zero enabled admins** (because this is the only enabled admin, and the patch sets `enabled: false` or demotes the role) is refused with `400 {"error":"Cannot remove the last enabled admin"}`. Same check guards `DELETE /api/users/{id}` when the target is the last enabled admin.

   The check is a single `SELECT COUNT FROM user WHERE role = 'admin' AND enabled = true AND id != :id` inside the same DB transaction as the mutation; v1.1 accepts a TOCTOU race (two concurrent admin-removals) under the at-least-once execution license — the second one observes the count and 400s.

3. **Bootstrap admin is NOT specially protected.** The "first" admin is just a user with `role = 'admin'` — it earns its protection from being the **last** enabled admin, not from being the first. This avoids a brittle "id = 1 is sacred" convention.

### 5.4 Session/credential rotation on admin action

The mutating admin actions interact with session lifetime as follows:

| Action                             | Sessions of the **target** user                                 | Sessions of **other** users |
| ---------------------------------- | --------------------------------------------------------------- | --------------------------- |
| `PATCH role` (admin/mod/viewer)    | **untouched** — DB-current-role per §6.4.1 takes effect on next request | untouched |
| `PATCH enabled: false`             | **revoked** — delete all `refresh_token` rows for target. Access tokens reject on next request (§6.4.1 step 3). | untouched |
| `PATCH enabled: true` (re-enable)  | untouched — target must log in fresh to obtain new tokens.       | untouched |
| `PATCH password` (admin reset)     | **revoked** — same family-wide delete; matches spec §6.2.3 self-reset behaviour. | untouched |
| `DELETE`                           | **revoked** by `user_cascade` event (already in `0001_baseline.surql`). | untouched |

The revoke primitive is `crate::repos::refresh_tokens::delete_all_for_user`, already used by `auth::refresh::revoke_user_and_warn` for R5 reuse-detection. v1.1 calls it from the user-mutation routes directly — no new low-level primitive.

**Why role change does NOT revoke sessions.** Spec §6.4.1 deliberately reads the role fresh from DB on every request. A `viewer → admin` upgrade does not need session revocation; the next request runs with the new role. A `admin → viewer` demotion likewise — the demoted user keeps their access token until it expires (default 4 h post [PURA-209](/PURA/issues/PURA-209)), but every request runs with `viewer` privileges. Revoking on role-change would be UX-hostile (surprise logout) and adds no security beyond the §6.4.1 contract.

### 5.5 Boot-time admin assertion

On manager startup, after migrations run, assert `SELECT COUNT FROM user WHERE role = 'admin' AND enabled = true >= 1`. If the count is **zero and `user.count() > 0`**, log a `warn!` line with the path to recover (typically: edit a row directly or re-run setup) and continue. Do NOT panic — the operator needs a running manager to recover. Setup-needed (`user.count() == 0`) is unchanged behaviour.

## 6. Engine surface

### 6.1 New files

- `crates/ts6-manager-server/src/routes/users.rs` — admin user CRUD + per-user sessions sub-router.
- `crates/ts6-manager-server/src/routes/audit.rs` — audit-log read endpoint.
- `crates/ts6-manager-server/src/repos/admin_audit_log.rs` — repo for the new table.
- `crates/ts6-manager-server/src/audit.rs` — top-level writer helper (single shared entrypoint à la the SSRF validator).
- `crates/ts6-manager-server/src/ui/pages/admin/users.rs` — user list + create/edit form.
- `crates/ts6-manager-server/src/ui/pages/admin/audit.rs` — audit log viewer.
- `crates/ts6-manager-server/src/ui/pages/admin/sessions.rs` — per-user sessions pane (route nested under `users/{id}`).

### 6.2 Wire shapes (`crates/shared/src/admin.rs`, new)

Following the same pattern as the existing `crates/shared/src/auth.rs` and the v1.1 flow design's `crates/shared/src/flows.rs`. Types are camelCase per the spec §7 convention. Full shapes live in [`http-api.md`](./http-api.md).

### 6.3 Audit-write integration points

Every admin-mutating handler ends with a single `audit::record(...)` call **after** the mutation has committed. The writer is `tracing::warn!`-on-failure (operationally bad, but does not block the user-facing response — the SSH-audit module set this precedent on PURA-79). Detail in [`audit-shape.md`](./audit-shape.md) §2.

### 6.4 Concurrency, failure, restart

- **Mutations are not transactional across (user-write, audit-write)** — SurrealDB does not give us cross-statement transactions cheaply. The audit row is best-effort. The trade-off is documented and matches `ssh_audit_log` precedent (the SSH audit row write is best-effort too).
- **In-flight admin-mutation interrupted by manager restart** — either the user-write succeeded (audit row absent), or both failed. We do not reconcile. v1.1 explicitly accepts an audit-log-absent admin action under crash as a known footgun and surfaces it in the runbook.
- **Audit log read under heavy write load** — `occurredAt` is monotone, indexed; read path uses range scans. v1.1 caps page size at 100 rows and accepts the operator-visible cap; v1.2 may add cursor-based pagination if the operator UI complains.

## 7. Open questions (resolved)

| Question                                | Resolution |
| --------------------------------------- | ---------- |
| Operator tier in v1.1?                  | **No.** Three-role surface already in code; expose it. v1.2 revisits. |
| Soft-delete vs hard-delete?             | **Hard-delete** with `userId` set-null on audit log. `enabled: bool` already covers the deactivate-but-keep-row case. |
| First-admin protection?                 | **Last-enabled-admin** rule applied uniformly. No special "id=1" sacredness. |
| Audit retention?                        | **TTL of 365 d** (configurable via `admin_audit_retention_days` settings row, floor 30 d) **and** a global row cap of 100 000. Whichever fires first. Matches the SSH-audit retention convention. |
| Audit write atomicity with user-write?  | **Best-effort, post-commit.** Audit failure does not roll back the user mutation. Matches SSH-audit precedent. |
| Sessions list per user vs global?       | **Per user** in v1.1. A global "all live sessions" view is a v1.2 nicety; v1.1 needs the per-user pane for the disable/revoke flow. |
| Password reset — admin-driven only?     | **Yes.** No transactional email plumbing in v1.1. Admin sets the new password directly via PATCH (with complexity validated against §6.2.2). Audit event records that the actor was an admin, not the target. |

## 8. Deviation from spec §7.4 — documented

Spec §7.4 declares the user routes as `GET / POST / PUT / DELETE /users` and does NOT mention `/sessions` or `/audit`. v1.1 deviates as follows:

| Spec §7.4                  | v1.1                              | Why                                                                                                  |
| -------------------------- | --------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `PUT /users/:userId`       | `PATCH /api/users/{id}`           | All other v0.1.0-rc1 update routes use PATCH (`/api/servers/:configId`, the flow routes). PATCH semantically matches the partial-update behaviour the spec already prescribes (only-fields-present-are-updated). Documented in `docs/deviations/user-routes-patch-vs-put.md`. |
| (no `/sessions` route)     | `GET /api/users/{id}/sessions` and `DELETE /api/users/{id}/sessions/{sid}` | Session management is an admin-management surface; not having it forces operators to wait for token expiry or rotate the JWT secret. v1.1 adds it as a tightening of the §6.12 admin surface. |
| (no `/audit` route)        | `GET /api/audit`                  | Audit table is new in v1.1; route is the read surface. Spec §6 mentions audit posture but does not enumerate routes in §7. v1.1 adds it. |

Deviation note `docs/deviations/admin-routes-v1.1-additions.md` is filed when the implementation lands.

## 9. Coordination with the FLOW workstream

[`docs/flows/architecture.md`](../flows/architecture.md) covers the parallel flow-engine work. There is no hard schema overlap:

- Flow tables: `bot_flow`, new `bot_flow_run`.
- Admin tables: existing `user`, new `admin_audit_log`.
- Flow UI: `crates/ts6-manager-server/src/ui/pages/flows/`.
- Admin UI: `crates/ts6-manager-server/src/ui/pages/admin/`.

Both briefs land before either gate ratifies. Implementation children can be sequenced in either order; the two workstreams do not block each other.

**Soft coupling:** flow `webhook` actions and admin role changes both want to be audited. v1.1 keeps these audit streams **separate** — `bot_flow_run` is the flow audit, `admin_audit_log` is the admin audit. A unified audit view is a v1.2 product surface.

## 10. Risks and known footguns

1. **Audit-log write-after-success race.** The mutation commits before the audit row writes. If the manager crashes in the window, the action is durable but unrecorded. Mitigation: log the intent to `tracing::warn!` before the user-mutation if the row-cap is near full or the audit table is otherwise degraded; operator runbook documents the crash-window risk.
2. **`payload` JSON shape drift.** Each event kind has a different payload schema. We do **not** enforce a discriminated-union via DB schema (SurrealDB cost is high for this). The repo helper builds typed payloads in Rust; consumers parse defensively.
3. **PATCH role-demote does not revoke access token.** Documented in §5.4. A demoted admin can still hit privileged endpoints until the access token expires — but every request is re-authorized against DB-current role per §6.4.1, so the demotion is functionally effective even though the bearer hasn't been rotated.
4. **Bootstrap-admin loss recovery.** If an operator manually wipes the only admin row via SurrealDB CLI, the boot-time assertion logs a `warn!` and the manager continues; recovery requires direct DB editing or re-running `setup/init` (which only fires when `user.count() == 0`). v1.1 accepts this as a CLI-footgun, not a product surface. Runbook entry to follow.
5. **Audit log read latency under retention churn.** Retention janitor runs every 1 h. During the run, paged audit reads may see slightly stale row counts. Acceptable — the read path is forensic, not real-time.

## 11. Acceptance for the implementation children

Files only in v1.1 design ratification. After the brief is ratified, the FLOW-pattern children get filed under [PURA-230](/PURA/issues/PURA-230) (ADMIN ratify gate):

1. **A-impl-routes** (RustPlatform) — `/api/users/*`, `/api/users/{id}/sessions`, `/api/audit` per `http-api.md`.
2. **A-impl-audit** (RustPlatform + SecurityEngineer) — migration `0009`, repo, writer helper, retention janitor, audit-shape compliance per `audit-shape.md`.
3. **A-impl-ui** (DioxusLead + UXDesigner) — users page, sessions pane, audit viewer, header-gating per `ui-brief.md`.
4. **A-impl-gate** (QAEngineer or QA) — `scripts/ws-gate/admin-probe.sh` exercising bootstrap → second-admin → audit-log-shows-both.
5. **A-impl-deviations** (CTO) — file `docs/deviations/user-routes-patch-vs-put.md` and `docs/deviations/admin-routes-v1.1-additions.md` once gate (4) passes green.

## 12. References

- Spec Chapter 6 (Security Model) — `study-documents/ts6-manager-spec.md`.
- Spec Chapter 7 §7.1–§7.4 (REST API surface) — `study-documents/ts6-manager-spec.md`.
- `crates/ts6-manager-server/src/auth/{extractors,jwt,refresh,routes}.rs` — existing auth foundation.
- `crates/ts6-manager-server/src/repos/users.rs` — existing user repo.
- `crates/ts6-manager-server/migrations/0001_baseline.surql` — user/refresh-token schema.
- `crates/ts6-manager-server/migrations/0006_ssh_audit_log.surql` — audit-log precedent for the new admin audit shape.
- [PURA-209](/PURA/issues/PURA-209) — post-v1.0 bug-fix parent (runs in parallel under board + QA, untouched by Phase 7).
- [PURA-227](/PURA/issues/PURA-227) — Phase 7 epic.
- [`docs/flows/architecture.md`](../flows/architecture.md) — sibling design brief, same pattern.
