//! Spec §6.5 — refresh-token rotation + reuse-detection (R5).
//!
//! [PURA-4](/PURA/issues/PURA-4) plan §6 / §6.6 — tests-first per the R5 bar.
//!
//! ## Threat model
//!
//! A refresh token captured by an attacker can be presented to
//! `POST /api/auth/refresh` to mint a new access token. Spec §6.5.4 requires
//! the implementation to detect when the **same** refresh token is presented
//! twice (the legitimate client used it once successfully; the attacker
//! replays it) and revoke every session for the affected user.
//!
//! ## Storage decision — preserve the predecessor row
//!
//! Spec §6.5.3 step 6 reads "Delete the old row." Taken literally, this
//! defeats §6.5.4's reuse-detection mechanism (`SELECT … WHERE replacedBy =
//! $supplied`): once the predecessor is deleted, its `replacedBy` pointer is
//! gone and reuse cannot be detected past the first rotation. We therefore
//! **keep the predecessor row** with `replacedBy` populated and rely on
//! [`crate::repos::refresh_tokens::delete_expired`] for cleanup. Any row
//! presented to [`rotate`] whose own `replacedBy` is already populated is
//! treated as a reuse signal and triggers the same family-wide revocation
//! that §6.5.4 mandates for unknown-but-referenced tokens.
//!
//! External contract is unchanged — JSON shapes, error responses, the
//! `POST /api/auth/refresh` route semantics, the 7-day default lifetime, the
//! 64-byte hex token format, and the family concept all match the spec.
//!
//! ## Concurrent rotation
//!
//! Spec §6.5.3 licenses side effects that tolerate at-least-once execution.
//! A forked family is not one of those side effects: each live successor is
//! a valid refresh credential, and rotating along either chain never looks
//! like reuse of the predecessor. `commit_rotation` is one transaction, but
//! SurrealDB 3.0.5's mem engine (SurrealMX, what `connect_in_memory` uses)
//! does not always abort the losing writer when two transactions update the
//! same record. Both can commit. After a successful commit, [`rotate`]
//! counts unreplaced rows in the family from a new query and, if more than
//! one is live, deletes that family and returns an error. The check does
//! not depend on serializable isolation. A later racer that still observes
//! the fork performs the same revoke, so the family does not stay split.
//!
//! ## Internal — fields are camelCase to match repo wire shapes
#![allow(non_snake_case)]

use chrono::{DateTime, Duration, Utc};
use rand::{Rng, RngCore, rngs::OsRng};

use crate::db::Database;
use crate::repos::{refresh_tokens, users};

/// nanoid-equivalent URL-safe alphabet (64 symbols, 6 bits per char). Spec
/// §6.5.2 calls out nanoid by reference; using the same alphabet matches the
/// reference's collision-resistance properties without pulling a new crate.
const FAMILY_ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
const FAMILY_LENGTH: usize = 21;
const REFRESH_TOKEN_BYTES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid or expired refresh token")]
    InvalidOrExpired,
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// Successful rotation result. The caller mints a fresh access token from
/// the database-current role for `user_id` (spec §6.5.3 step 7) and returns
/// `(access_token, token)` to the client.
#[derive(Debug, Clone)]
pub struct Rotated {
    pub token: String,
    pub user_id: i64,
    pub family: String,
    pub expires_at: DateTime<Utc>,
}

/// Generate a fresh 64-byte refresh token, hex-encoded — spec §6.5.1.
/// 128-character output drawn from `OsRng`.
pub fn generate_refresh_token() -> String {
    let mut bytes = [0u8; REFRESH_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Generate a fresh family id — spec §6.5.2. 21-character URL-safe random
/// string drawn from `OsRng`. ~125 bits of entropy; matches nanoid(21).
pub fn generate_family_id() -> String {
    let mut rng = OsRng;
    let mut id = String::with_capacity(FAMILY_LENGTH);
    for _ in 0..FAMILY_LENGTH {
        let idx: usize = rng.gen_range(0..FAMILY_ALPHABET.len());
        id.push(FAMILY_ALPHABET[idx] as char);
    }
    id
}

/// Issue the first refresh token for a user — called by the login route after
/// successful password verification. Spec §6.5.2: every login starts a new
/// family.
pub async fn issue_for_login(
    db: &Database,
    user_id: i64,
    lifetime: Duration,
) -> Result<Rotated, Error> {
    let token = generate_refresh_token();
    let family = generate_family_id();
    let expires_at = Utc::now() + lifetime;

    refresh_tokens::insert(
        db,
        refresh_tokens::NewRefreshToken {
            token: token.clone(),
            userId: user_id,
            expiresAt: expires_at,
            family: Some(family.clone()),
        },
    )
    .await?;

    Ok(Rotated {
        token,
        user_id,
        family,
        expires_at,
    })
}

/// Spec §6.5.3 — rotate the supplied refresh token.
///
/// Returns:
///
/// - `Ok(Rotated)` for valid, current, non-expired tokens whose user is
///   enabled. The caller mints a new access token.
/// - `Err(InvalidOrExpired)` for any other case. The route layer maps this
///   to HTTP 401. If the failure was a reuse signal (token unknown but
///   referenced by a `replacedBy`, or token's own `replacedBy` already set),
///   the user's entire refresh-token set has been deleted before the error
///   is returned.
pub async fn rotate(db: &Database, supplied: &str, lifetime: Duration) -> Result<Rotated, Error> {
    let row = refresh_tokens::find_by_token(db, supplied).await?;

    let Some(row) = row else {
        // Spec §6.5.3 step 1 → §6.5.4. Token does not exist; check the
        // reuse signal.
        reuse_check_or_invalid(db, supplied).await?;
        return Err(Error::InvalidOrExpired);
    };

    // **Predecessor-preserved reuse check.** A row whose `replacedBy` is
    // already populated represents a token that was rotated in a previous
    // call — replaying it is a reuse signal.
    if row.replacedBy.is_some() {
        revoke_user_and_warn(db, row.userId, "predecessor replay").await;
        return Err(Error::InvalidOrExpired);
    }

    // Spec §6.5.3 step 2 — expired or owning user disabled/missing.
    if row.expiresAt < Utc::now() {
        refresh_tokens::delete_by_token(db, supplied).await?;
        return Err(Error::InvalidOrExpired);
    }

    let user = users::find_by_id(db, row.userId).await?;
    let owner_enabled = matches!(user, Some(ref u) if u.enabled);
    if !owner_enabled {
        refresh_tokens::delete_by_token(db, supplied).await?;
        return Err(Error::InvalidOrExpired);
    }

    // Spec §6.5.3 steps 3–5. Step 6 ("delete the old row") is intentionally
    // skipped — see module-level docs.
    let new_token = generate_refresh_token();
    let new_expires = Utc::now() + lifetime;

    // R5 (THE-1010) + L4 — stamp the predecessor and insert the successor in
    // one transaction so a revocation cannot land between the two writes.
    // `Ok(None)` is the lost compare-and-swap (`replacedBy` already set, or
    // the row was deleted). A storage conflict is `Err` and fails closed.
    // `Ok(Some)` is not proof of uniqueness: the mem engine can commit two
    // such transactions. `ensure_single_live_successor` closes that.
    if refresh_tokens::commit_rotation(
        db,
        supplied,
        refresh_tokens::NewRefreshToken {
            token: new_token.clone(),
            userId: row.userId,
            expiresAt: new_expires,
            family: row.family.clone(),
        },
    )
    .await?
    .is_none()
    {
        return Err(Error::InvalidOrExpired);
    }

    ensure_single_live_successor(db, row.family.as_deref(), &new_token).await?;

    Ok(Rotated {
        token: new_token,
        user_id: row.userId,
        family: row.family.unwrap_or_default(),
        expires_at: new_expires,
    })
}

/// After `commit_rotation` returns, make sure this family has at most one
/// unreplaced row and that `new_token` is that row.
///
/// The count runs in a new query, so it sees every commit that landed
/// before it started. The racer that commits second therefore observes the
/// fork even when the engine failed to abort either transaction. Revoking
/// the family removes both successors. A racer whose own row was removed by
/// that revoke fails closed instead of handing the caller a dead bearer
/// that it already treated as success.
async fn ensure_single_live_successor(
    db: &Database,
    family: Option<&str>,
    new_token: &str,
) -> Result<(), Error> {
    if let Some(family) = family {
        let rows = refresh_tokens::list_for_family(db, family).await?;
        let live = rows.iter().filter(|row| row.replacedBy.is_none()).count();
        if live > 1 {
            tracing::warn!(
                family,
                live,
                "concurrent refresh rotation forked the family; revoking it"
            );
            refresh_tokens::delete_by_family(db, family).await?;
            return Err(Error::InvalidOrExpired);
        }
    }

    match refresh_tokens::find_by_token(db, new_token).await? {
        Some(stored) if stored.replacedBy.is_none() => Ok(()),
        _ => Err(Error::InvalidOrExpired),
    }
}

/// Present a refresh token to `POST /api/auth/logout`.
///
/// - Unknown token: no-op. Logout stays idempotent (spec §6.5.5).
/// - Live token (`replacedBy` is none): delete that row only. Other
///   families for the same user stay valid.
/// - Already-rotated predecessor (`replacedBy` is set): do **not** delete
///   just that row. The predecessor is the reuse-detection breadcrumb. A
///   thief who rotated a stolen token and then logged the old value out
///   would otherwise erase the signal and keep the successor for the rest
///   of the refresh lifetime. Treat it as reuse and revoke every refresh
///   token for the user.
pub async fn logout(db: &Database, supplied: &str) -> Result<(), Error> {
    let Some(row) = refresh_tokens::find_by_token(db, supplied).await? else {
        return Ok(());
    };
    if row.replacedBy.is_some() {
        revoke_user_and_warn(db, row.userId, "logout of rotated predecessor").await;
        return Ok(());
    }
    refresh_tokens::delete_by_token(db, supplied).await?;
    Ok(())
}

/// Spec §6.5.4 — supplied token does not exist; check `replacedBy` and, if
/// found, revoke the user's entire refresh-token set.
async fn reuse_check_or_invalid(db: &Database, supplied: &str) -> Result<(), Error> {
    if let Some(predecessor) = refresh_tokens::find_predecessor_by_replaced_by(db, supplied).await?
    {
        revoke_user_and_warn(db, predecessor.userId, "replacedBy match").await;
    }
    Ok(())
}

async fn revoke_user_and_warn(db: &Database, user_id: i64, reason: &'static str) {
    tracing::warn!(
        user_id,
        reason,
        "refresh-token reuse detected; revoking all sessions for user"
    );
    if let Err(e) = refresh_tokens::delete_all_for_user(db, user_id).await {
        // Logging only — the caller is already returning 401 to the client.
        // A failed cleanup is operationally bad but does not give the
        // attacker a usable token (the original predecessor is still there
        // with replacedBy set, and any subsequent presentation of any token
        // in this user's set will hit the same reuse check on the next call
        // and try to revoke again).
        tracing::error!(user_id, error = %e, "failed to revoke user sessions after reuse signal");
    }
}

#[cfg(test)]
mod tests {
    //! Spec §6.6 reuse-detection tests — written first per the R5 bar.

    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::{refresh_tokens, users};

    const ONE_DAY: Duration = Duration::days(1);

    async fn setup() -> std::sync::Arc<Database> {
        // `TS6_TEST_DATABASE_URL` points this module's tests at another
        // engine (for example `surrealkv:///tmp/refresh-race`). Unset keeps
        // the mem engine the rest of the suite uses.
        let db = match std::env::var("TS6_TEST_DATABASE_URL") {
            Ok(url) => {
                let db = surrealdb::engine::any::connect(&url)
                    .await
                    .expect("connect test database");
                db.use_ns(crate::config::DEFAULT_DB_NAMESPACE)
                    .use_db(crate::config::DEFAULT_DB_NAME)
                    .await
                    .expect("select namespace");
                std::sync::Arc::new(db)
            }
            Err(_) => connect_in_memory().await.expect("in-memory connect"),
        };
        migrations::run(&db).await.expect("migrations run");
        db
    }

    async fn make_user(db: &Database, username: &str) -> i64 {
        users::insert(
            db,
            users::NewUser {
                username: username.into(),
                passwordHash: "$argon2id$v=19$test".into(),
                displayName: username.into(),
                role: "viewer".into(),
                enabled: true,
            },
        )
        .await
        .expect("insert user")
        .id
    }

    #[tokio::test]
    async fn issue_for_login_creates_a_token_with_a_fresh_family() {
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.expect("issue");

        assert_eq!(issued.user_id, uid);
        assert_eq!(issued.token.len(), 128, "spec §6.5.1 — 64 bytes hex");
        assert_eq!(issued.family.len(), FAMILY_LENGTH);
        // Two consecutive logins start two different families.
        let issued2 = issue_for_login(&db, uid, ONE_DAY).await.expect("issue 2");
        assert_ne!(issued.family, issued2.family);
        assert_ne!(issued.token, issued2.token);
    }

    #[tokio::test]
    async fn rotate_returns_new_token_and_marks_old_as_replaced() {
        // Spec §6.5.3 happy path — old row is preserved with replacedBy
        // populated (see module-level decision rationale).
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();

        let rotated = rotate(&db, &issued.token, ONE_DAY).await.expect("rotate");
        assert_ne!(rotated.token, issued.token);
        assert_eq!(rotated.user_id, uid);
        assert_eq!(rotated.family, issued.family);

        let old = refresh_tokens::find_by_token(&db, &issued.token)
            .await
            .unwrap()
            .expect("predecessor row must survive for reuse-detection");
        assert_eq!(
            old.replacedBy.as_deref(),
            Some(refresh_tokens::token_at_rest(&rotated.token).as_str()),
            "old row's replacedBy stores the digest of the new token, not the bearer"
        );
        assert_ne!(
            old.token, issued.token,
            "token column must not be plaintext"
        );
        assert_eq!(old.token, refresh_tokens::token_at_rest(&issued.token));

        let new = refresh_tokens::find_by_token(&db, &rotated.token)
            .await
            .unwrap()
            .expect("successor row must exist");
        assert!(new.replacedBy.is_none());
        assert_eq!(new.family, issued.family.clone().into());
    }

    #[tokio::test]
    async fn rotation_preserves_family_id() {
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let t0 = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let t1 = rotate(&db, &t0.token, ONE_DAY).await.unwrap();
        let t2 = rotate(&db, &t1.token, ONE_DAY).await.unwrap();

        assert_eq!(t0.family, t1.family);
        assert_eq!(t1.family, t2.family);

        let fam = refresh_tokens::list_for_family(&db, &t0.family)
            .await
            .unwrap();
        // Predecessors stay around (with replacedBy) for reuse-detection;
        // every rotation adds one row.
        assert_eq!(fam.len(), 3);
    }

    #[tokio::test]
    async fn replay_old_token_after_rotation_revokes_all_user_sessions() {
        // R5 — the canonical reuse-detection test. After a successful
        // rotation, replaying the old token must revoke every refresh
        // token belonging to that user.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let rotated = rotate(&db, &issued.token, ONE_DAY).await.unwrap();

        // Sanity: before the replay, the successor is present.
        assert!(
            refresh_tokens::find_by_token(&db, &rotated.token)
                .await
                .unwrap()
                .is_some()
        );

        let err = rotate(&db, &issued.token, ONE_DAY)
            .await
            .expect_err("replay must error");
        assert!(matches!(err, Error::InvalidOrExpired));

        // Every refresh token for this user must be gone.
        assert!(
            refresh_tokens::list_for_user(&db, uid)
                .await
                .unwrap()
                .is_empty(),
            "reuse must wipe all sessions"
        );
    }

    #[tokio::test]
    async fn replay_chain_token_after_two_rotations_still_revokes() {
        // T1 → T2 → T3. Replaying T1 (whose row has replacedBy=T2) must
        // still trigger revocation. This is the case the predecessor-
        // preserved storage policy specifically protects against.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let t1 = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let t2 = rotate(&db, &t1.token, ONE_DAY).await.unwrap();
        let _t3 = rotate(&db, &t2.token, ONE_DAY).await.unwrap();

        let err = rotate(&db, &t1.token, ONE_DAY)
            .await
            .expect_err("chain replay must error");
        assert!(matches!(err, Error::InvalidOrExpired));
        assert!(
            refresh_tokens::list_for_user(&db, uid)
                .await
                .unwrap()
                .is_empty(),
            "chain replay must wipe all sessions"
        );
    }

    #[tokio::test]
    async fn logout_of_live_token_drops_only_that_row() {
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let live = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let other = issue_for_login(&db, uid, ONE_DAY).await.unwrap();

        logout(&db, &live.token).await.unwrap();

        assert!(
            refresh_tokens::find_by_token(&db, &live.token)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            refresh_tokens::find_by_token(&db, &other.token)
                .await
                .unwrap()
                .is_some(),
            "logging out one family must not revoke the user's other sessions"
        );
    }

    #[tokio::test]
    async fn logout_of_rotated_predecessor_revokes_the_successor() {
        // A thief who rotates a stolen refresh token and then POSTs the old
        // value to /logout must not be able to delete the reuse breadcrumb
        // and keep the successor for the rest of the refresh lifetime.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let rotated = rotate(&db, &issued.token, ONE_DAY).await.unwrap();

        logout(&db, &issued.token).await.unwrap();

        assert!(
            refresh_tokens::list_for_user(&db, uid)
                .await
                .unwrap()
                .is_empty(),
            "logout of a predecessor must wipe the successor too"
        );
        assert!(
            refresh_tokens::find_by_token(&db, &rotated.token)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn logout_unknown_token_is_a_noop() {
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        logout(&db, "never-issued").await.unwrap();
        assert!(
            refresh_tokens::find_by_token(&db, &issued.token)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn replay_old_token_does_not_affect_other_users() {
        // User A's reuse signal must NOT cascade onto user B's tokens.
        let db = setup().await;
        let alice = make_user(&db, "alice").await;
        let bob = make_user(&db, "bob").await;

        let alice_t1 = issue_for_login(&db, alice, ONE_DAY).await.unwrap();
        let _alice_t2 = rotate(&db, &alice_t1.token, ONE_DAY).await.unwrap();
        let bob_token = issue_for_login(&db, bob, ONE_DAY).await.unwrap();

        // Trigger reuse on alice's old token.
        let _ = rotate(&db, &alice_t1.token, ONE_DAY).await.unwrap_err();

        // Bob's token survives.
        let bob_rows = refresh_tokens::list_for_user(&db, bob).await.unwrap();
        assert_eq!(bob_rows.len(), 1);
        assert_eq!(
            bob_rows[0].token,
            refresh_tokens::token_at_rest(&bob_token.token)
        );
        // Alice's set is gone.
        assert!(
            refresh_tokens::list_for_user(&db, alice)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unknown_token_returns_error_without_revocation() {
        // A token that has never existed must 401 with no DB side effects.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();

        let bogus = "00".repeat(64); // valid hex shape but unknown
        let err = rotate(&db, &bogus, ONE_DAY)
            .await
            .expect_err("unknown token must error");
        assert!(matches!(err, Error::InvalidOrExpired));

        // Original token survives untouched (no replacedBy).
        let row = refresh_tokens::find_by_token(&db, &issued.token)
            .await
            .unwrap()
            .expect("issued row must still be present");
        assert!(row.replacedBy.is_none());
    }

    #[tokio::test]
    async fn expired_token_returns_error_and_deletes_row() {
        // Spec §6.5.3 step 2 — expired token row is deleted, error returned.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let plaintext = generate_refresh_token();
        refresh_tokens::insert(
            &db,
            refresh_tokens::NewRefreshToken {
                token: plaintext.clone(),
                userId: uid,
                expiresAt: Utc::now() - Duration::seconds(1),
                family: Some(generate_family_id()),
            },
        )
        .await
        .unwrap();

        let err = rotate(&db, &plaintext, ONE_DAY)
            .await
            .expect_err("expired must error");
        assert!(matches!(err, Error::InvalidOrExpired));

        assert!(
            refresh_tokens::find_by_token(&db, &plaintext)
                .await
                .unwrap()
                .is_none(),
            "expired row must be deleted"
        );
    }

    #[tokio::test]
    async fn disabled_user_token_returns_error_and_deletes_row() {
        // Spec §6.5.3 step 2 — owning user disabled → delete + error.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        users::update(
            &db,
            uid,
            users::UserUpdate {
                enabled: Some(false),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let err = rotate(&db, &issued.token, ONE_DAY)
            .await
            .expect_err("disabled must error");
        assert!(matches!(err, Error::InvalidOrExpired));

        assert!(
            refresh_tokens::find_by_token(&db, &issued.token)
                .await
                .unwrap()
                .is_none(),
            "row for disabled user must be deleted"
        );
    }

    #[tokio::test]
    async fn concurrent_double_rotate_with_same_input_is_benign() {
        // Two rotations of one token must not leave two live chains. Zero
        // successes is allowed: both racers can observe the fork and revoke
        // the family before returning. A replay of the original token must
        // still end with no sessions for the user (reuse-detection if one
        // chain survived, or a no-op if the family was already revoked).
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();

        let db1 = db.clone();
        let db2 = db.clone();
        let supplied = issued.token.clone();
        let s1 = supplied.clone();
        let s2 = supplied.clone();

        let (r1, r2) = tokio::join!(
            tokio::spawn(async move { rotate(&db1, &s1, ONE_DAY).await }),
            tokio::spawn(async move { rotate(&db2, &s2, ONE_DAY).await })
        );
        let r1 = r1.unwrap();
        let r2 = r2.unwrap();

        let successes: Vec<_> = [r1, r2].into_iter().filter_map(Result::ok).collect();
        assert!(
            successes.len() <= 1,
            "{} rotations succeeded — token forked into multiple live chains",
            successes.len()
        );
        let rows = refresh_tokens::list_for_family(&db, &issued.family)
            .await
            .unwrap();
        let live = rows.iter().filter(|r| r.replacedBy.is_none()).count();
        assert!(
            live <= 1,
            "family has {live} live tokens after concurrent rotation"
        );

        // Replay of the original token. If a rotation committed, the
        // predecessor is still there with `replacedBy` set (or the family
        // was already revoked). Either way the replay must fail and leave
        // the user with no refresh tokens — reuse-detection wipes every
        // session when the predecessor survived. If both transactions
        // aborted, the original row is still live and this replay is an
        // ordinary rotation; it must not create a second chain.
        let predecessor = refresh_tokens::find_by_token(&db, &supplied).await.unwrap();
        let rotation_landed = match &predecessor {
            Some(row) => row.replacedBy.is_some(),
            None => true,
        };
        let replay = rotate(&db, &supplied, ONE_DAY).await;
        if rotation_landed {
            assert!(
                matches!(replay, Err(Error::InvalidOrExpired)),
                "replay of a rotated token must be reuse, got {replay:?}"
            );
            assert!(
                refresh_tokens::list_for_user(&db, uid)
                    .await
                    .unwrap()
                    .is_empty(),
                "reuse of a rotated token must wipe all sessions"
            );
        } else {
            let rows = refresh_tokens::list_for_family(&db, &issued.family)
                .await
                .unwrap();
            let live = rows.iter().filter(|r| r.replacedBy.is_none()).count();
            assert!(
                live <= 1,
                "replay of an unrotated token left {live} live rows"
            );
        }
    }

    #[tokio::test]
    async fn deleting_user_after_rotation_removes_every_descendant_row() {
        // R5 cleanup half — deleting a user must take their refresh tokens
        // with them, even after rotations have produced multiple rows.
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let t1 = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let _t2 = rotate(&db, &t1.token, ONE_DAY).await.unwrap();
        let _t3 = rotate(&db, &_t2.token, ONE_DAY).await.unwrap();

        users::delete(&db, uid).await.unwrap();
        assert!(
            refresh_tokens::list_for_user(&db, uid)
                .await
                .unwrap()
                .is_empty(),
            "user delete cascade must wipe all rotations"
        );
    }

    #[test]
    fn generate_refresh_token_has_spec_shape() {
        let t = generate_refresh_token();
        assert_eq!(t.len(), 128, "spec §6.5.1 — 64 bytes hex");
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // CSPRNG output: two consecutive draws should differ.
        assert_ne!(t, generate_refresh_token());
    }

    #[test]
    fn generate_family_id_has_spec_shape() {
        let f = generate_family_id();
        assert_eq!(f.len(), FAMILY_LENGTH);
        assert!(f.bytes().all(|b| FAMILY_ALPHABET.contains(&b)));
        assert_ne!(f, generate_family_id());
    }

    // ---------------------------------------------------------------
    // R5 defense-in-depth — PURA-161.
    //
    // Goal: randomly compose [`rotate`] / replay / cross-user calls and
    // verify the two load-bearing R5 invariants hold for every sequence:
    //
    //   I1. Any successful `rotate(t)` leaves `t.replacedBy` populated
    //       (the predecessor-preserved storage policy this module's docs
    //       call out).
    //   I2. The first replay of any previously-rotated token wipes the
    //       owning user's *entire* refresh-token set, and never the
    //       other user's. (Cross-user isolation — the bug R5 is named
    //       for.)
    //
    // The randomised sequence is short by intent — proptest will shrink
    // counterexamples down to the minimal failing trace, which is what
    // makes this useful as a regression net. The token-format invariants
    // are already covered by the deterministic unit tests above.
    // ---------------------------------------------------------------

    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Action {
        RotateA,
        RotateB,
        ReplayAFirst,
    }

    fn action_strategy() -> impl Strategy<Value = Action> {
        prop_oneof![
            Just(Action::RotateA),
            Just(Action::RotateB),
            Just(Action::ReplayAFirst),
        ]
    }

    async fn run_sequence(seq: Vec<Action>) {
        let db = setup().await;
        let alice = make_user(&db, "alice").await;
        let bob = make_user(&db, "bob").await;
        let alice_t1 = issue_for_login(&db, alice, ONE_DAY).await.unwrap();
        let _bob_t1 = issue_for_login(&db, bob, ONE_DAY).await.unwrap();

        let mut alice_live: Option<String> = Some(alice_t1.token.clone());
        let mut bob_live: Option<String> = Some(_bob_t1.token.clone());

        for action in seq {
            match action {
                Action::RotateA => {
                    if let Some(live) = alice_live.clone() {
                        if let Ok(rotated) = rotate(&db, &live, ONE_DAY).await {
                            // I1
                            let pred = refresh_tokens::find_by_token(&db, &live)
                                .await
                                .unwrap()
                                .expect("predecessor must survive");
                            assert_eq!(
                                pred.replacedBy.as_deref(),
                                Some(refresh_tokens::token_at_rest(&rotated.token).as_str()),
                                "I1 violated: rotated predecessor missing replacedBy"
                            );
                            alice_live = Some(rotated.token);
                        } else {
                            alice_live = None;
                        }
                    }
                }
                Action::RotateB => {
                    if let Some(live) = bob_live.clone() {
                        if let Ok(rotated) = rotate(&db, &live, ONE_DAY).await {
                            let pred = refresh_tokens::find_by_token(&db, &live)
                                .await
                                .unwrap()
                                .expect("predecessor must survive");
                            assert_eq!(
                                pred.replacedBy.as_deref(),
                                Some(refresh_tokens::token_at_rest(&rotated.token).as_str()),
                                "I1 violated: rotated predecessor missing replacedBy"
                            );
                            bob_live = Some(rotated.token);
                        } else {
                            bob_live = None;
                        }
                    }
                }
                Action::ReplayAFirst => {
                    // I2: replaying alice_t1 must either (a) error and
                    // wipe alice's set if it has been rotated, or (b)
                    // succeed/fail without touching bob's set. Bob's set
                    // must survive both branches untouched.
                    let bob_rows_before = refresh_tokens::list_for_user(&db, bob).await.unwrap();

                    let pred_before = refresh_tokens::find_by_token(&db, &alice_t1.token)
                        .await
                        .unwrap();
                    let alice_t1_was_rotated = pred_before
                        .as_ref()
                        .map(|p| p.replacedBy.is_some())
                        .unwrap_or(false);

                    let result = rotate(&db, &alice_t1.token, ONE_DAY).await;

                    let bob_rows_after = refresh_tokens::list_for_user(&db, bob).await.unwrap();
                    assert_eq!(
                        bob_rows_before.len(),
                        bob_rows_after.len(),
                        "I2 violated: alice replay leaked into bob's token set"
                    );

                    if alice_t1_was_rotated {
                        assert!(result.is_err(), "replay of a rotated token must error");
                        let alice_rows = refresh_tokens::list_for_user(&db, alice).await.unwrap();
                        assert!(
                            alice_rows.is_empty(),
                            "I2 violated: replay of rotated token did not wipe alice's set"
                        );
                        alice_live = None;
                    }
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            // Tighten the case count — every case spawns a fresh DB +
            // migrations, which is the heavy part. 32 cases over short
            // sequences still shrinks effectively when something breaks.
            cases: 32,
            .. ProptestConfig::default()
        })]

        #[test]
        fn refresh_token_sequences_preserve_r5_invariants(
            seq in proptest::collection::vec(action_strategy(), 1..6)
        ) {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(run_sequence(seq));
        }
    }

    // ---------------------------------------------------------------
    // R5 (THE-1010) — concurrent-rotation anti-fork.
    //
    // The proptest harness above is single-threaded: it drives rotations
    // sequentially and so can never exercise the TOCTOU between `rotate`'s
    // read of `replacedBy` and its later write. That window is the real
    // R5 residual the re-audit was asked to probe ("concurrent rotation").
    //
    // Fire many rotations of the *same* token at once. The compare-and-swap
    // inside `commit_rotation` is not enough on the mem engine: two
    // transactions can both commit. `ensure_single_live_successor` then
    // revokes the family. The invariant: a family may end with at most ONE
    // live token (replacedBy NONE) and at most one rotation may succeed.
    // ---------------------------------------------------------------
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_rotation_of_one_token_never_forks_the_family() {
        let db = setup().await;
        let uid = make_user(&db, "alice").await;
        let issued = issue_for_login(&db, uid, ONE_DAY).await.unwrap();
        let family = issued.family.clone();

        const N: usize = 12;
        let mut handles = Vec::with_capacity(N);
        for _ in 0..N {
            let db = db.clone();
            let tok = issued.token.clone();
            handles.push(tokio::spawn(async move {
                rotate(&db, &tok, ONE_DAY).await.is_ok()
            }));
        }
        let mut oks = 0usize;
        for h in handles {
            if h.await.unwrap() {
                oks += 1;
            }
        }

        // At most one rotation may win the CAS.
        assert!(
            oks <= 1,
            "{oks} concurrent rotations succeeded — token forked into multiple live chains"
        );

        // The family must hold at most one live token: exactly one on a
        // clean win, or zero if a late reader hit the predecessor-replay
        // reuse branch and revoked the set. Two would mean a fork.
        let rows = refresh_tokens::list_for_family(&db, &family).await.unwrap();
        let live = rows.iter().filter(|r| r.replacedBy.is_none()).count();
        assert!(
            live <= 1,
            "family has {live} live tokens after concurrent rotation — fork detected"
        );
    }
}
