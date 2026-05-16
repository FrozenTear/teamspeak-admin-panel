# Moderation data model (Phase 9.0)

Reference for the `0011_moderation.surql` migration and the `repos::moderation_*`
/ `repos::user_permissions` modules. Companion to the
[PURA-262 design brief](/PURA/issues/PURA-262#document-plan) §5–§8.

## 1. Tables

| Table | Shape | Mutability |
|---|---|---|
| `moderation_case` | one row per actioned subject on a vserver | mutable (status transitions) |
| `moderation_case_action` | append-only per-case timeline | INSERT-only (+ cascade delete) |
| `moderation_note` | free-text notes on a subject UID | mutable (edit + delete) |
| `user_permission` | `moderation.*` grant rows | mutable (grant / revoke) |

All four use int primary keys via `sequence::nextval(...)` and carry foreign
keys as plain `int`s — consistent with every pre-9.0 table. The brief §5
describes `moderation_case_action.caseId` as a "record link"; we deviate to a
plain `int` FK for schema consistency and the `repos::record_id_to_i64` helper.
Referential integrity is held by the `moderation_case_cascade` event.

## 2. Subject identity

Cases and notes key on `subjectUid` — a durable TS6 client UID — never on a
nickname. `subjectNicknameSnapshot` (case) is a point-in-time display copy;
nickname churn never forks a subject's history.

## 3. Cascade posture

- **Delete a `moderation_case`** → its `moderation_case_action` rows are
  deleted (`moderation_case_cascade` event). An orphan action row would be
  unreachable from every read path.
- **Delete a `user`** (`user_cascade_moderation` event):
  - `moderation_case.openedByUserId`, `moderation_case_action.actorUserId`,
    `moderation_note.authorUserId`, `user_permission.grantedByUserId` →
    set to `NONE`. The moderation record survives; only operator linkage
    clears (mirrors `admin_audit_log`).
  - `user_permission` rows where the deleted user is the **subject** → hard
    deleted (a grant for a non-existent user is meaningless).

## 4. Note retention / GDPR posture (resolved open design item)

The brief §8 flags moderator-note retention as an open design item delegated
to `9.0-data`. Resolution:

### 4.1 Retention — no automatic TTL

`moderation_note` has **no expiry janitor**. Moderator notes are moderation
history retained under a legitimate-interest basis; an automatic TTL would
silently destroy moderation context an operator relies on. This is a
deliberate, documented divergence from `admin_audit_log`, which *does* have a
retention janitor because audit rows accumulate unboundedly per event.

No `retention_days` column or `app_setting` ships in 9.0. If operators later
need tunable note retention it can land as an `app_setting` key plus a janitor
without a schema change — explicitly out of scope here (YAGNI).

### 4.2 Access & portability — GDPR Art. 15 / 20

`repos::moderation_notes::list_for_subject(uid)` returns every note for a
subject UID. It backs both the per-user history pane and any subject-data
export the routes workstream chooses to expose.

### 4.3 Erasure — GDPR Art. 17

`repos::moderation_notes::purge_for_subject(uid)` **hard-deletes** every note
for a subject UID and returns the count removed. Hard delete, not soft — a
soft-deleted row still holds the personal data, so a soft delete would not
satisfy an erasure request. The route layer that calls `purge_for_subject`
must write an `admin_audit_log` row recording that an erasure occurred (the
*fact* of erasure is not itself personal data).

### 4.4 Cases & actions are not erased with notes

`moderation_case` / `moderation_case_action` are records of moderation
*decisions*. They are retained under GDPR Art. 17(3) (establishment/exercise
of legal claims; compliance) and are **not** purged by a note erasure. After a
subject's TS identity is gone the case still carries `subjectUid` +
`subjectNicknameSnapshot` only. This matches how `admin_audit_log` survives a
user delete.

## 5. Workstream split with 9.0-rbac (PURA-284)

`user_permission` is listed under both the data model (§5) and the rbac
workstream (§9) of the brief. It is defined **once**, here in
`0011_moderation.surql`, and `repos::user_permissions` provides the table CRUD
(`grant` / `revoke` / `holds` / `permissions_for_user`).

PURA-284 (9.0-rbac) owns everything *above* the table: the `RequirePermission`
extractor, the `admin → all` short-circuit, the `moderator` role-default
permission set, the permission-catalog constants, and the grant-management UI.
PURA-284 adds **no migration of its own**.
