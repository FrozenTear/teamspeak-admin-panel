//! Phase 9.2 token mint / verify + TS6 poke delivery (PURA-306,
//! workstream `9.2-tokens` of [PURA-269](/PURA/issues/PURA-269) §9).
//!
//! This module owns the **crypto + lifecycle** half of the Phase 9.2
//! token store; the **storage substrate** (`moderation_token` table +
//! `repos::moderation_tokens`) is PURA-311. Both halves are pinned by
//! the [Token Store Security Spec](/PURA/issues/PURA-306#document-token-security-spec)
//! — this file implements §1 (split-token format), §3 (SHA-256 hashing),
//! §4 (atomic single-use + constant-time compare) and §7 (poke channel).
//!
//! ## Two token kinds
//!
//! - `report_challenge` — UID-bound, 15-min TTL. Minted when a connected
//!   client asks for a report link, delivered to that client over the
//!   TS6 control channel ([`deliver_report_challenge`]).
//! - `appeal` — case-scoped, 30-day TTL. Minted by
//!   [`crate::routes::moderation`]'s action endpoint whenever a punitive
//!   action is appended, and the appeal URL is embedded in the kick/ban
//!   reason so a disconnected subject keeps the link.
//!
//! Both are **single-use** and stored **hashed at rest** — only the
//! SHA-256 `secretHash` is ever persisted, never the plaintext secret
//! (spec hook 1). The plaintext wire token exists exactly twice in its
//! lifetime: in the [`MintedToken`] returned at mint, and in the inbound
//! request at verify.
//!
//! ## Split-token format (spec §1)
//!
//! The wire token is `<lookup_id>.<secret>`:
//!
//! - `lookup_id` — `LOOKUP_BYTES` of CSPRNG, hex. **Not secret.** Used
//!   only as the indexed key for the row fetch, so the lookup is an
//!   exact index hit and never a timing oracle on the secret.
//! - `secret` — `SECRET_BYTES` of CSPRNG, hex. The only part that
//!   authenticates. 24 bytes = 192-bit, comfortably over the ≥128-bit
//!   spec floor.
//!
//! Verify fetches the row by `lookup_id`, constant-time-compares
//! `SHA-256(secret)` against the stored hash, and only then issues the
//! atomic single-use `consume` UPDATE — so a wrong secret never burns a
//! valid token, and single-use rests on a predicate inside the mutating
//! statement (no TOCTOU). See [`verify_and_consume`].

// `verify_and_consume` / `deliver_report_challenge` / the URL helpers are
// consumed by the 9.2 public routes (PURA-307), not yet in-tree.
#![allow(dead_code)]

use chrono::{Duration, Utc};
use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::control::{ControlBackend, ControlBackendError};
use crate::db::Database;
use crate::repos::app_settings;
use crate::repos::moderation_tokens::{self, ModerationToken, NewModerationToken};

/// Non-secret lookup handle width — 12 CSPRNG bytes (96-bit). Carried in
/// the wire token so the row fetch is an indexed exact match.
const LOOKUP_BYTES: usize = 12;
/// Secret half width — 24 CSPRNG bytes (192-bit), over the spec's
/// ≥128-bit floor.
const SECRET_BYTES: usize = 24;

const KIND_REPORT_CHALLENGE: &str = "report_challenge";
const KIND_APPEAL: &str = "appeal";

/// Report-challenge TTL — spec §6 / brief §2.
pub const REPORT_CHALLENGE_TTL: Duration = Duration::minutes(15);
/// Appeal-token TTL — spec §6 / brief §2.
pub const APPEAL_TTL: Duration = Duration::days(30);

/// `app_settings` key holding the externally-reachable base URL the
/// public report/appeal forms are served from. When unset, minting still
/// succeeds but no URL is embedded — the operator hands the link out from
/// the 9.0 panel instead (brief §3.2).
pub const PUBLIC_BASE_URL_SETTING: &str = "moderation.public_base_url";

/// A freshly minted token: the stored (hashed) row plus the one-time
/// plaintext wire token. The `plaintext` is the **only** time the secret
/// is in hand — hand it straight to the delivery channel and drop it; it
/// is never recoverable from the row.
#[derive(Debug, Clone)]
pub struct MintedToken {
    pub row: ModerationToken,
    /// `lookup_id.secret` — never persisted, never logged.
    pub plaintext: String,
}

/// What a successfully verified-and-consumed token authorises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedToken {
    /// A report-challenge token — proves control of `uid`.
    ReportChallenge { uid: String },
    /// An appeal token — proves the holder may appeal `case_id`.
    Appeal { case_id: i64 },
}

/// Verify failure. Every "this token will not authorise you" case —
/// malformed, unknown `lookup_id`, wrong secret, expired, already used —
/// collapses to [`VerifyError::Invalid`] so the caller's error response
/// is not an enumeration oracle (spec §4).
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("invalid or expired token")]
    Invalid,
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// Poke-delivery failure for [`deliver_report_challenge`].
#[derive(Debug, thiserror::Error)]
pub enum DeliverError {
    /// No connected client currently holds the target UID — by design
    /// this is reports-only and a reporter is a connected client, so a
    /// miss means the client disconnected between request and delivery.
    #[error("target UID is not connected")]
    NotConnected,
    #[error(transparent)]
    Control(#[from] ControlBackendError),
}

// ---------------------------------------------------------------------
// Crypto primitives
// ---------------------------------------------------------------------

/// `n` bytes of CSPRNG output (`OsRng`), lowercase hex. Same RNG source
/// `auth::refresh` draws refresh tokens from.
fn rand_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    OsRng.fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Lowercase-hex SHA-256 of `input`. The token secret is high-entropy
/// (192-bit CSPRNG), so a single unsalted SHA-256 is correct — a slow
/// password hash (Argon2) would only be a self-inflicted DoS lever on
/// the public verify path (spec §3).
fn sha256_hex(input: &str) -> String {
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    hex::encode(h.finalize())
}

/// Generate a fresh token: `(wire_token, lookup_id, secret_hash)`.
fn generate() -> (String, String, String) {
    let lookup_id = rand_hex(LOOKUP_BYTES);
    let secret = rand_hex(SECRET_BYTES);
    let secret_hash = sha256_hex(&secret);
    (format!("{lookup_id}.{secret}"), lookup_id, secret_hash)
}

// ---------------------------------------------------------------------
// Mint
// ---------------------------------------------------------------------

async fn mint(
    db: &Database,
    kind: &str,
    bound_uid: Option<String>,
    case_id: Option<i64>,
    ttl: Duration,
) -> anyhow::Result<MintedToken> {
    let (plaintext, lookup_id, secret_hash) = generate();
    let row = moderation_tokens::insert(
        db,
        NewModerationToken {
            kind: kind.to_string(),
            lookupId: lookup_id,
            secretHash: secret_hash,
            boundUid: bound_uid,
            caseId: case_id,
            expiresAt: Utc::now() + ttl,
        },
    )
    .await?;
    Ok(MintedToken { row, plaintext })
}

/// Mint a `report_challenge` token bound to `uid` (spec §6).
///
/// `uid` MUST be the server-verified `client_unique_identifier` of the
/// requesting connection, never a client-supplied value — the binding is
/// only as trustworthy as its source (spec hook 3 / §7).
pub async fn mint_report_challenge(db: &Database, uid: &str) -> anyhow::Result<MintedToken> {
    mint(
        db,
        KIND_REPORT_CHALLENGE,
        Some(uid.to_string()),
        None,
        REPORT_CHALLENGE_TTL,
    )
    .await
}

/// Mint an `appeal` token scoped to `case_id` (spec §6).
pub async fn mint_appeal(db: &Database, case_id: i64) -> anyhow::Result<MintedToken> {
    mint(db, KIND_APPEAL, None, Some(case_id), APPEAL_TTL).await
}

// ---------------------------------------------------------------------
// Verify
// ---------------------------------------------------------------------

/// Verify a wire token and **atomically consume it** — single-use.
///
/// A second call with the same token (or any concurrent racing call)
/// returns [`VerifyError::Invalid`]: single-use is enforced inside the
/// `consume` UPDATE's `WHERE usedAt IS NONE` predicate, so there is no
/// check-then-act window (spec §4).
///
/// Order matters (spec §4):
/// 1. Parse `lookup_id.secret`; reject malformed shapes.
/// 2. Fetch the row by `lookup_id` (indexed, non-secret key).
/// 3. **Constant-time** compare `SHA-256(secret)` against the stored
///    hash. A wrong secret returns here — *before* `consume` — so a
///    griefer who guesses a `lookup_id` cannot burn someone's live
///    token.
/// 4. Atomically `consume`: the row is spent iff it was unused and
///    unexpired. Expired / used / unknown all collapse to `Invalid`.
pub async fn verify_and_consume(
    db: &Database,
    wire_token: &str,
) -> Result<VerifiedToken, VerifyError> {
    // 1. Parse. Both halves must be present and non-empty.
    let (lookup_id, secret) = match wire_token.split_once('.') {
        Some((l, s)) if !l.is_empty() && !s.is_empty() => (l, s),
        _ => return Err(VerifyError::Invalid),
    };

    // 2. Fetch by the non-secret lookup handle. An unknown handle is not
    //    a meaningful oracle — `lookup_id` is non-secret by design and
    //    the 96-bit space is infeasible to enumerate (spec §1).
    let Some(row) = moderation_tokens::find_by_lookup_id(db, lookup_id).await? else {
        return Err(VerifyError::Invalid);
    };

    // 3. Constant-time secret compare. Both operands are 64-char hex
    //    (SHA-256), so lengths always match; `ct_eq` does not branch on
    //    the contents.
    let supplied_hash = sha256_hex(secret);
    if supplied_hash
        .as_bytes()
        .ct_eq(row.secretHash.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(VerifyError::Invalid);
    }

    // 4. Atomic single-use consume. `None` ⇒ already used / expired.
    let Some(consumed) = moderation_tokens::consume(db, lookup_id).await? else {
        return Err(VerifyError::Invalid);
    };

    classify(&consumed)
}

/// Verify a wire token **without consuming it** — a read-only check.
///
/// The public redacted-case view (`9.2-public-routes`, PURA-307) is opened
/// with the same `appeal` token the appeal submission later spends, and a
/// subject may reload that view several times before submitting. So the
/// view path verifies non-destructively here; the token is burned exactly
/// once, at submission, via [`verify_and_consume`].
///
/// Performs the same parse → indexed fetch → constant-time secret compare
/// as [`verify_and_consume`], then checks freshness (`usedAt` unset,
/// `expiresAt` in the future) as a plain read. Every failure — malformed,
/// unknown, wrong secret, expired, already used — collapses to
/// [`VerifyError::Invalid`] so the caller is not an enumeration oracle
/// (spec §4).
pub async fn verify(db: &Database, wire_token: &str) -> Result<VerifiedToken, VerifyError> {
    let (lookup_id, secret) = match wire_token.split_once('.') {
        Some((l, s)) if !l.is_empty() && !s.is_empty() => (l, s),
        _ => return Err(VerifyError::Invalid),
    };

    let Some(row) = moderation_tokens::find_by_lookup_id(db, lookup_id).await? else {
        return Err(VerifyError::Invalid);
    };

    let supplied_hash = sha256_hex(secret);
    if supplied_hash
        .as_bytes()
        .ct_eq(row.secretHash.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(VerifyError::Invalid);
    }

    // Freshness, read-only. A consumed-but-unexpired token is `Invalid`
    // here exactly as it would be to `verify_and_consume`'s atomic check.
    if row.usedAt.is_some() || row.expiresAt <= Utc::now() {
        return Err(VerifyError::Invalid);
    }

    classify(&row)
}

/// Resolve a verified token row to what it authorises. Shared by the
/// consuming and non-consuming verify paths; a missing binding field or
/// unknown `kind` is a row-integrity bug and collapses to `Invalid`.
fn classify(row: &ModerationToken) -> Result<VerifiedToken, VerifyError> {
    match row.kind.as_str() {
        KIND_REPORT_CHALLENGE => {
            let uid = row.boundUid.clone().ok_or_else(|| {
                tracing::error!(
                    token_id = row.id,
                    "report_challenge token row is missing boundUid"
                );
                VerifyError::Invalid
            })?;
            Ok(VerifiedToken::ReportChallenge { uid })
        }
        KIND_APPEAL => {
            let case_id = row.caseId.ok_or_else(|| {
                tracing::error!(token_id = row.id, "appeal token row is missing caseId");
                VerifyError::Invalid
            })?;
            Ok(VerifiedToken::Appeal { case_id })
        }
        other => {
            tracing::error!(
                token_id = row.id,
                kind = other,
                "unknown moderation_token kind"
            );
            Err(VerifyError::Invalid)
        }
    }
}

// ---------------------------------------------------------------------
// URL composition
// ---------------------------------------------------------------------

/// Build the appeal-form URL for `wire_token`. `base` is the configured
/// public base URL ([`PUBLIC_BASE_URL_SETTING`]); when `None` a
/// site-relative path is returned (useless to a disconnected subject,
/// but never a panic — the minting hook checks for `None` first).
pub fn appeal_url(base: Option<&str>, wire_token: &str) -> String {
    public_url(base, "appeal", wire_token)
}

/// Build the report-form URL for `wire_token`.
///
/// The report-challenge token is UID-bound, not server-bound, but the
/// report form needs the `serverConfigId` / `virtualServerId` scope to
/// file the report (`PublicReportRequest`). The poke is the only channel
/// that reaches the proven UID, so the scope rides the URL it delivers —
/// the form page reads all three from the query string. See [`appeal_url`]
/// for the base-URL handling.
pub fn report_url(
    base: Option<&str>,
    wire_token: &str,
    server_config_id: i64,
    virtual_server_id: i64,
) -> String {
    let path = format!(
        "/moderation/report?token={wire_token}\
         &serverConfigId={server_config_id}&virtualServerId={virtual_server_id}"
    );
    match base {
        Some(b) => format!("{}{path}", b.trim_end_matches('/')),
        None => path,
    }
}

fn public_url(base: Option<&str>, form: &str, wire_token: &str) -> String {
    let path = format!("/moderation/{form}?token={wire_token}");
    match base {
        Some(b) => format!("{}{path}", b.trim_end_matches('/')),
        None => path,
    }
}

/// Read the configured public base URL. A missing key, empty value, or
/// lookup error all yield `None` — minting must never fail on a missing
/// operator config.
pub async fn public_base_url(db: &Database) -> Option<String> {
    match app_settings::get(db, PUBLIC_BASE_URL_SETTING).await {
        Ok(Some(s)) if !s.value.trim().is_empty() => Some(s.value.trim().to_string()),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(err = %e, "appeal base-URL lookup failed; embedding no link");
            None
        }
    }
}

/// Append the appeal URL to a kick/ban `reason` so a disconnected or
/// banned subject keeps the link in the reason text TS6 renders locally
/// (brief §2). Returns the bare `reason` unchanged when no public base
/// URL is configured.
pub async fn reason_with_appeal_url(db: &Database, reason: &str, wire_token: &str) -> String {
    match public_base_url(db).await {
        Some(base) => format!(
            "{reason}\n\nAppeal this action: {}",
            appeal_url(Some(&base), wire_token)
        ),
        None => reason.to_string(),
    }
}

// ---------------------------------------------------------------------
// TS6 poke delivery (spec hook 3)
// ---------------------------------------------------------------------

/// Resolve a `client_unique_identifier` to the `clid` of its live
/// connection, if any. Only real voice clients (`client_type == 0`) are
/// considered — never ServerQuery clients.
fn resolve_clid(clients: &[crate::webquery::models::ClientEntry], uid: &str) -> Option<i64> {
    clients
        .iter()
        .find(|c| c.client_type == 0 && c.client_unique_identifier == uid)
        .map(|c| c.clid)
}

/// Deliver a freshly minted report-challenge token to the connected
/// client that holds `uid`, over the TS6 control channel (spec §7).
///
/// **Anti-spoofing (security hook 3).** `uid` MUST be a value the caller
/// read from server-side state — the requesting connection's own
/// `client_unique_identifier` — never a client-supplied field. This
/// function then *re-resolves* `uid → clid` against a live `clientlist`
/// at delivery time and messages **only** that `clid` via
/// `sendtextmessage(targetmode=1)` (single client). A `clid` is reused
/// across reconnects, so re-resolving here guarantees the token is never
/// delivered to a connection that does not currently hold the UID. The
/// message is never broadcast (`targetmode` 2/3) — the token reaches one
/// connection or none.
pub async fn deliver_report_challenge(
    backend: &dyn ControlBackend,
    server_config_id: i64,
    sid: i64,
    uid: &str,
    wire_token: &str,
    base_url: Option<&str>,
) -> Result<(), DeliverError> {
    let clients = backend.clientlist_with_flags(sid, &["-uid"]).await?;
    let clid = resolve_clid(&clients, uid).ok_or(DeliverError::NotConnected)?;

    let msg = format!(
        "[Moderation] To file your report, open this single-use link \
         (valid 15 minutes): {}",
        report_url(base_url, wire_token, server_config_id, sid)
    );
    // targetmode=1 — a private message to exactly this clid.
    backend.sendtextmessage(sid, 1, clid, &msg).await?;
    Ok(())
}

// ---------------------------------------------------------------------
// Tests — spec §8 matrix (hooks 1 & 3).
// ---------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use chrono::DateTime;
    use std::sync::Arc;

    async fn setup() -> Arc<Database> {
        let db = connect_in_memory().await.expect("in-memory connect");
        migrations::run(&db).await.expect("migrations run");
        db
    }

    /// Insert a token row directly with a caller-chosen secret + expiry —
    /// lets the expiry / single-use tests build precise fixtures the
    /// fixed-TTL `mint_*` helpers cannot. Returns the wire token.
    async fn insert_raw(
        db: &Database,
        kind: &str,
        secret: &str,
        bound_uid: Option<&str>,
        case_id: Option<i64>,
        expires_at: DateTime<Utc>,
    ) -> String {
        let lookup_id = rand_hex(LOOKUP_BYTES);
        moderation_tokens::insert(
            db,
            NewModerationToken {
                kind: kind.to_string(),
                lookupId: lookup_id.clone(),
                secretHash: sha256_hex(secret),
                boundUid: bound_uid.map(str::to_string),
                caseId: case_id,
                expiresAt: expires_at,
            },
        )
        .await
        .expect("raw token insert");
        format!("{lookup_id}.{secret}")
    }

    #[tokio::test]
    async fn mint_secret_meets_entropy_floor_and_is_unique() {
        let db = setup().await;
        let a = mint_appeal(&db, 1).await.unwrap();
        let b = mint_appeal(&db, 1).await.unwrap();

        let (lookup, secret) = a.plaintext.split_once('.').unwrap();
        assert_eq!(lookup.len(), LOOKUP_BYTES * 2, "lookup is hex of 12 bytes");
        assert_eq!(
            secret.len(),
            SECRET_BYTES * 2,
            "secret is hex of 24 bytes — 192-bit, over the 128-bit floor"
        );
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()));
        // CSPRNG: two mints never collide on lookup or secret.
        assert_ne!(a.plaintext, b.plaintext);
    }

    #[tokio::test]
    async fn secret_is_never_stored_in_plaintext() {
        // Hook 1 — hashed at rest.
        let db = setup().await;
        let minted = mint_report_challenge(&db, "uid-alice").await.unwrap();
        let (lookup, secret) = minted.plaintext.split_once('.').unwrap();

        let row = moderation_tokens::find_by_lookup_id(&db, lookup)
            .await
            .unwrap()
            .expect("row exists");
        assert_ne!(
            row.secretHash, secret,
            "stored value must not be the secret"
        );
        assert_eq!(
            row.secretHash,
            sha256_hex(secret),
            "stored value is exactly SHA-256(secret)"
        );
        // The minted row carried back to the caller agrees.
        assert_eq!(minted.row.secretHash, sha256_hex(secret));
    }

    #[tokio::test]
    async fn verify_accepts_a_fresh_token_then_rejects_the_replay() {
        // Hook 1 — single-use.
        let db = setup().await;
        let minted = mint_appeal(&db, 42).await.unwrap();

        let first = verify_and_consume(&db, &minted.plaintext).await;
        assert_eq!(first.unwrap(), VerifiedToken::Appeal { case_id: 42 });

        let replay = verify_and_consume(&db, &minted.plaintext).await;
        assert!(
            matches!(replay, Err(VerifyError::Invalid)),
            "a consumed token must not verify a second time"
        );
    }

    #[tokio::test]
    async fn concurrent_double_consume_lets_exactly_one_win() {
        // Hook 1 — atomic single-use, no TOCTOU. Two racing verifies of
        // one token: exactly one succeeds.
        let db = setup().await;
        let minted = mint_report_challenge(&db, "uid-bob").await.unwrap();

        let db1 = db.clone();
        let db2 = db.clone();
        let t1 = minted.plaintext.clone();
        let t2 = minted.plaintext.clone();
        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { verify_and_consume(&db1, &t1).await }),
            tokio::spawn(async move { verify_and_consume(&db2, &t2).await }),
        );
        let oks = [r1.unwrap(), r2.unwrap()]
            .into_iter()
            .filter(|r| r.is_ok())
            .count();
        assert_eq!(oks, 1, "exactly one of two racing consumes may win");
    }

    #[tokio::test]
    async fn expired_token_is_rejected() {
        let db = setup().await;
        let wire = insert_raw(
            &db,
            KIND_APPEAL,
            "abcd1234",
            None,
            Some(7),
            Utc::now() - Duration::seconds(1),
        )
        .await;
        assert!(matches!(
            verify_and_consume(&db, &wire).await,
            Err(VerifyError::Invalid)
        ));
    }

    #[tokio::test]
    async fn wrong_secret_does_not_consume_the_token() {
        // Hook 1 — griefing guard. A bad secret on a known lookup_id must
        // fail the compare *before* `consume`, leaving the token usable.
        let db = setup().await;
        let minted = mint_appeal(&db, 99).await.unwrap();
        let (lookup, _) = minted.plaintext.split_once('.').unwrap();

        let forged = format!("{lookup}.deadbeefdeadbeef");
        assert!(matches!(
            verify_and_consume(&db, &forged).await,
            Err(VerifyError::Invalid)
        ));

        // The real token still works — the forgery did not burn it.
        assert_eq!(
            verify_and_consume(&db, &minted.plaintext).await.unwrap(),
            VerifiedToken::Appeal { case_id: 99 }
        );
    }

    #[tokio::test]
    async fn unknown_expired_and_used_are_indistinguishable() {
        // Hook 1 — no enumeration oracle: every failure is one error.
        let db = setup().await;

        // Unknown lookup_id.
        let unknown = verify_and_consume(&db, "00112233445566778899aabb.cafe").await;
        // Malformed (no dot).
        let malformed = verify_and_consume(&db, "no-dot-here").await;
        // Used.
        let minted = mint_appeal(&db, 1).await.unwrap();
        verify_and_consume(&db, &minted.plaintext).await.unwrap();
        let used = verify_and_consume(&db, &minted.plaintext).await;

        for r in [unknown, malformed, used] {
            assert!(matches!(r, Err(VerifyError::Invalid)));
        }
    }

    #[tokio::test]
    async fn token_kind_binding_round_trips() {
        let db = setup().await;
        let report = mint_report_challenge(&db, "uid-carol").await.unwrap();
        let appeal = mint_appeal(&db, 555).await.unwrap();

        assert_eq!(
            verify_and_consume(&db, &report.plaintext).await.unwrap(),
            VerifiedToken::ReportChallenge {
                uid: "uid-carol".to_string()
            }
        );
        assert_eq!(
            verify_and_consume(&db, &appeal.plaintext).await.unwrap(),
            VerifiedToken::Appeal { case_id: 555 }
        );
    }

    #[tokio::test]
    async fn verify_does_not_consume_the_token() {
        // Non-consuming verify — the redacted-case view may run it many
        // times, and the token must still be spendable afterwards.
        let db = setup().await;
        let minted = mint_appeal(&db, 321).await.unwrap();

        for _ in 0..3 {
            assert_eq!(
                verify(&db, &minted.plaintext).await.unwrap(),
                VerifiedToken::Appeal { case_id: 321 },
            );
        }
        // Still spendable: the consuming path wins exactly once.
        assert_eq!(
            verify_and_consume(&db, &minted.plaintext).await.unwrap(),
            VerifiedToken::Appeal { case_id: 321 },
        );
    }

    #[tokio::test]
    async fn verify_rejects_used_expired_and_wrong_secret() {
        let db = setup().await;

        // Consumed token — `verify` agrees it is no longer valid.
        let used = mint_report_challenge(&db, "uid-d").await.unwrap();
        verify_and_consume(&db, &used.plaintext).await.unwrap();
        assert!(matches!(
            verify(&db, &used.plaintext).await,
            Err(VerifyError::Invalid)
        ));

        // Expired token.
        let expired = insert_raw(
            &db,
            KIND_APPEAL,
            "secret-xyz",
            None,
            Some(5),
            Utc::now() - Duration::seconds(1),
        )
        .await;
        assert!(matches!(
            verify(&db, &expired).await,
            Err(VerifyError::Invalid)
        ));

        // Wrong secret on a live token — and the token is left unconsumed.
        let live = mint_appeal(&db, 654).await.unwrap();
        let (lookup, _) = live.plaintext.split_once('.').unwrap();
        assert!(matches!(
            verify(&db, &format!("{lookup}.deadbeef")).await,
            Err(VerifyError::Invalid)
        ));
        assert_eq!(
            verify(&db, &live.plaintext).await.unwrap(),
            VerifiedToken::Appeal { case_id: 654 },
            "a wrong-secret verify must not burn the token",
        );
    }

    #[test]
    fn url_helpers_compose_with_and_without_a_base() {
        assert_eq!(
            appeal_url(Some("https://ts.example.com"), "tok123"),
            "https://ts.example.com/moderation/appeal?token=tok123"
        );
        // Trailing slash on the base is normalised; the report URL also
        // carries the server scope the report form needs (PURA-309).
        assert_eq!(
            report_url(Some("https://ts.example.com/"), "tok123", 7, 3),
            "https://ts.example.com/moderation/report?token=tok123\
             &serverConfigId=7&virtualServerId=3"
        );
        // No base configured → site-relative path, no panic.
        assert_eq!(
            appeal_url(None, "tok123"),
            "/moderation/appeal?token=tok123"
        );
    }

    #[tokio::test]
    async fn reason_with_appeal_url_appends_only_when_configured() {
        let db = setup().await;
        // Unconfigured → reason unchanged.
        assert_eq!(
            reason_with_appeal_url(&db, "Spamming", "tok").await,
            "Spamming"
        );
        // Configured → URL appended.
        app_settings::put(&db, PUBLIC_BASE_URL_SETTING, "https://ts.example.com")
            .await
            .unwrap();
        let out = reason_with_appeal_url(&db, "Spamming", "tok").await;
        assert!(out.starts_with("Spamming\n\nAppeal this action: "));
        assert!(out.ends_with("https://ts.example.com/moderation/appeal?token=tok"));
    }

    #[test]
    fn resolve_clid_matches_uid_and_skips_query_clients() {
        use crate::webquery::models::ClientEntry;
        let voice = ClientEntry {
            clid: 10,
            client_type: 0,
            client_unique_identifier: "uid-target".into(),
            ..Default::default()
        };
        let query = ClientEntry {
            clid: 11,
            client_type: 1, // ServerQuery — never a poke target.
            client_unique_identifier: "uid-target".into(),
            ..Default::default()
        };

        let clients = vec![query, voice];
        assert_eq!(resolve_clid(&clients, "uid-target"), Some(10));
        assert_eq!(resolve_clid(&clients, "uid-absent"), None);
    }
}
