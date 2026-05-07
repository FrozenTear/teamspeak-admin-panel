-- =====================================================================
-- TS6 Manager — Phase 1 DATA slice 1 baseline (PURA-10)
--
-- Spec: study-documents/ts6-manager-spec.md
--   §4.2.1 User              §4.2.3 UserServerAccess
--   §4.2.2 RefreshToken      §4.2.4 TsServerConfig
--   §4.2 cascade rules,      §6.5 refresh-token shape (family / replacedBy)
--
-- Field names are external-contract identifiers (they appear in JSON wire
-- types per Chapter 7). Document field names match the spec verbatim
-- (camelCase) so REST handlers can serialise rows without per-field rename
-- attributes.
--
-- D8 deviation (board-ratified 2026-05-07): the reference's SQLite is replaced
-- by SurrealDB v3. Cascading delete is implemented via DEFINE EVENT instead
-- of SQL FK CASCADE, but the observable behaviour matches §4.2 / §4.5.
--
-- Identifier scheme: per §4.1 every primary key is an auto-assigned integer.
-- Each table gets a SEQUENCE; CREATE statements pin the record id to the next
-- sequence value so wire-shape `id: int` and the SurrealDB record id are the
-- same number. Repos extract the int via `record::id($r)`.
-- =====================================================================

-- ---- Sequences --------------------------------------------------------
-- BATCH 1 keeps the per-batch allocation small (we generate one id per insert
-- in the priority slice; nothing to gain by pre-allocating). START 1 matches
-- the conventional "auto-increment from 1" the spec implies in §4.1 and keeps
-- 0 free as a sentinel for "no id" callers might reach for later.
DEFINE SEQUENCE user_id              BATCH 1 START 1;
DEFINE SEQUENCE refresh_token_id     BATCH 1 START 1;
DEFINE SEQUENCE server_connection_id BATCH 1 START 1;
DEFINE SEQUENCE server_user_grant_id BATCH 1 START 1;

-- ---- User (§4.2.1) ----------------------------------------------------
DEFINE TABLE user SCHEMAFULL;
DEFINE FIELD username      ON user TYPE string;
DEFINE FIELD passwordHash  ON user TYPE string;
DEFINE FIELD displayName   ON user TYPE string;
DEFINE FIELD role          ON user TYPE string DEFAULT 'viewer';
DEFINE FIELD enabled       ON user TYPE bool   DEFAULT true;
DEFINE FIELD createdAt     ON user TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt     ON user TYPE datetime VALUE time::now();
DEFINE FIELD lastLoginAt   ON user TYPE option<datetime>;
DEFINE INDEX user_username_unique ON user FIELDS username UNIQUE;

-- ---- RefreshToken (§4.2.2 + §6.5) -------------------------------------
DEFINE TABLE refresh_token SCHEMAFULL;
DEFINE FIELD token       ON refresh_token TYPE string;
DEFINE FIELD userId      ON refresh_token TYPE int;
DEFINE FIELD expiresAt   ON refresh_token TYPE datetime;
DEFINE FIELD createdAt   ON refresh_token TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD family      ON refresh_token TYPE option<string>;
DEFINE FIELD replacedBy  ON refresh_token TYPE option<string>;
DEFINE INDEX refresh_token_token_unique ON refresh_token FIELDS token UNIQUE;
DEFINE INDEX refresh_token_user_idx     ON refresh_token FIELDS userId;
DEFINE INDEX refresh_token_replaced_idx ON refresh_token FIELDS replacedBy;
DEFINE INDEX refresh_token_family_idx   ON refresh_token FIELDS family;

-- ---- TsServerConfig (§4.2.4) -----------------------------------------
-- queryBotNickname and sshBotNickname are added by the two follow-up
-- migrations 0002 and 0003 (the spec's "two nickname-field migrations",
-- §4.4) so the migration history mirrors the reference's append-only shape.
DEFINE TABLE server_connection SCHEMAFULL;
DEFINE FIELD name             ON server_connection TYPE string;
DEFINE FIELD host             ON server_connection TYPE string;
DEFINE FIELD webqueryPort     ON server_connection TYPE int  DEFAULT 10080;
-- apiKey ciphertext format is `enc:<iv-hex>:<tag-hex>:<ct-hex>` per §6.3.2.
-- Plaintext-passthrough on read is allowed during migration (§6.3.3); the
-- column type stays `string` so legacy rows can sit alongside encrypted ones.
DEFINE FIELD apiKey           ON server_connection TYPE string;
DEFINE FIELD useHttps         ON server_connection TYPE bool DEFAULT false;
DEFINE FIELD sshPort          ON server_connection TYPE int  DEFAULT 10022;
DEFINE FIELD sshUsername      ON server_connection TYPE option<string>;
DEFINE FIELD sshPassword      ON server_connection TYPE option<string>;
DEFINE FIELD queryBotChannel  ON server_connection TYPE option<string>;
DEFINE FIELD enabled          ON server_connection TYPE bool DEFAULT true;
DEFINE FIELD createdAt        ON server_connection TYPE datetime VALUE $value OR time::now() READONLY;
DEFINE FIELD updatedAt        ON server_connection TYPE datetime VALUE time::now();

-- ---- UserServerAccess (§4.2.3) ---------------------------------------
DEFINE TABLE server_user_grant SCHEMAFULL;
DEFINE FIELD userId          ON server_user_grant TYPE int;
DEFINE FIELD serverConfigId  ON server_user_grant TYPE int;
DEFINE INDEX server_user_grant_unique     ON server_user_grant FIELDS userId, serverConfigId UNIQUE;
DEFINE INDEX server_user_grant_user_idx   ON server_user_grant FIELDS userId;
DEFINE INDEX server_user_grant_server_idx ON server_user_grant FIELDS serverConfigId;

-- ---- Cascading delete events (§4.2 cascade rules) --------------------
-- $before holds the deleted row; record::id($before.id) extracts the
-- integer portion of the record id so it matches the int FK columns.
DEFINE EVENT user_cascade ON user WHEN $event = "DELETE" THEN {
    LET $uid = record::id($before.id);
    DELETE refresh_token     WHERE userId = $uid;
    DELETE server_user_grant WHERE userId = $uid;
};

DEFINE EVENT server_connection_cascade ON server_connection WHEN $event = "DELETE" THEN {
    LET $sid = record::id($before.id);
    DELETE server_user_grant WHERE serverConfigId = $sid;
};
