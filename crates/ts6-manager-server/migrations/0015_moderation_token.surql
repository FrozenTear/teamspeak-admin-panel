-- =====================================================================
-- TS6 Manager — Phase 9.2 token store (PURA-311).
--
-- Follow-up to Workstream 1 of the Phase 9.2 Appeals breakdown
-- (PURA-269 §9). The Token Store Security Spec (PURA-306) §2 recommended
-- folding `moderation_token` into 0014's appeals migration; that was not
-- operationally possible — PURA-305 had an active checked-out run when
-- this was scoped, so its migration could not be amended without
-- clobbering that run. As 9.2-data owner this lands as a clean follow-up
-- migration instead; spec §2 explicitly delegates the migration-number
-- call to 9.2-data. `0014` is the appeals migration, so this is `0015`.
-- (`0013_moderation_unban_action_kind` is owned by a concurrent branch
-- and slots in on fan-in — see the note in `db/migrations.rs`.)
--
-- `moderation_token` is the unified split-token store behind both the
-- report-challenge and appeal flows (spec §1 / §2): one row per minted
-- token, a `kind` discriminator, and — the hook-1 storage invariant —
-- only the SHA-256 `secretHash` is ever persisted, never the plaintext
-- secret. Lookup is by the non-secret `lookupId` (UNIQUE index); the
-- secret-hash constant-time compare and the atomic single-use `consume`
-- UPDATE live in `repos::moderation_tokens` (spec §4). Token mint/verify
-- itself is owned by SecurityEngineer (PURA-306) — this migration only
-- provides the storage substrate.
--
-- SurrealDB v3 SCHEMAFULL gotchas (project memory): nullable fields use
-- `option<...>` (`boundUid`, `caseId`, `usedAt`); there is no `object`
-- field, so no `FLEXIBLE` is needed; any `ORDER BY` idiom is honoured in
-- the repo projection, not here.
--
-- FK convention (see 0011 / 0014 headers): `caseId` is a plain `int` FK
-- into `moderation_case`, not a SurrealDB record link. Referential
-- integrity for a deleted case is kept by the cascade event below. The
-- row's own primary key is a `sequence::nextval` int, consistent with
-- every other table in the schema.
-- =====================================================================

DEFINE SEQUENCE moderation_token_id BATCH 1 START 1;

DEFINE TABLE moderation_token SCHEMAFULL;

-- Discriminator: 'report_challenge' | 'appeal' (spec §2). Kept a plain
-- string with the value set documented inline — mirrors
-- `moderation_report.status` in 0014 rather than a schema-level ASSERT.
DEFINE FIELD kind        ON moderation_token TYPE string;

-- Non-secret indexed handle — base64url of 12 random bytes (spec §1).
-- Carried in the wire token so the row lookup is an exact index hit and
-- never a timing oracle on the secret.
DEFINE FIELD lookupId    ON moderation_token TYPE string;

-- SHA-256 of the secret half, lowercase hex. NEVER the plaintext secret
-- (spec §2 / §3 storage invariant — high-entropy token, single SHA-256,
-- no salt).
DEFINE FIELD secretHash  ON moderation_token TYPE string;

-- Binding — exactly one is set per row, by `kind` (spec §2 / §6):
--   report_challenge -> boundUid (the server-verified reporter UID)
--   appeal           -> caseId   (plain int FK into moderation_case)
DEFINE FIELD boundUid    ON moderation_token TYPE option<string>;
DEFINE FIELD caseId      ON moderation_token TYPE option<int>;

-- Lifecycle. Expiry is enforced by the `expiresAt > time::now()`
-- predicate inside the `consume` UPDATE (spec §4), never by app-side
-- clock math alone.
DEFINE FIELD expiresAt   ON moderation_token TYPE datetime;
-- Single-use marker: NONE until consumed, then set exactly once and
-- atomically by `repos::moderation_tokens::consume` (spec §4).
DEFINE FIELD usedAt      ON moderation_token TYPE option<datetime>;
DEFINE FIELD createdAt   ON moderation_token TYPE datetime
    VALUE $value OR time::now() READONLY;

-- lookupId is the lookup key — must be unique and indexed (spec §2). The
-- UNIQUE constraint also makes a lookupId collision at mint a hard error
-- rather than a silent overwrite.
DEFINE INDEX moderation_token_lookup ON moderation_token FIELDS lookupId UNIQUE;
-- "one pending appeal per case" / appeal re-issue reads filter on caseId
-- (spec §2), and the cascade below deletes by caseId.
DEFINE INDEX moderation_token_case  ON moderation_token FIELDS caseId;

-- --------------------------------------------------------------------
-- Cascade event
-- --------------------------------------------------------------------

-- Deleting a `moderation_case` deletes its appeal tokens — an orphan
-- token is unreachable and a needless live secret (spec §2). Mirrors the
-- 0011 `moderation_case_cascade` posture; named distinctly so each
-- migration owns its own table's cascade rule (SurrealDB fires every
-- event defined on the table). No `user`-delete cascade is needed —
-- `moderation_token` carries no operator FK.
DEFINE EVENT moderation_token_case_cascade ON moderation_case
    WHEN $event = "DELETE" THEN {
    LET $cid = record::id($before.id);
    DELETE moderation_token WHERE caseId = $cid;
};
