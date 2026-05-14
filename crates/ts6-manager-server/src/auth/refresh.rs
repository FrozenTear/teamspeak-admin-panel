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
//! ## At-least-once execution
//!
//! Spec §6.5.3 explicitly licenses "side effects compatible with at-least-
//! once execution", so we do not wrap rotation in a SurrealDB transaction.
//! The race between two concurrent rotations with the same input is benign
//! under the predecessor-preserved scheme:
//!
//! - If both observe `replacedBy = NONE` and both proceed, two successors
//!   are created in the same family. Either chain rotates normally
//!   afterwards; neither half is leaked to an attacker by this race.
//! - If one observes the other's `replacedBy` flip first, it triggers
//!   reuse-detection and revokes the entire user session set — the safe
//!   default for any ambiguous replay.
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
    refresh_tokens::set_replaced_by(db, supplied, &new_token).await?;
    refresh_tokens::insert(
        db,
        refresh_tokens::NewRefreshToken {
            token: new_token.clone(),
            userId: row.userId,
            expiresAt: new_expires,
            family: row.family.clone(),
        },
    )
    .await?;

    Ok(Rotated {
        token: new_token,
        user_id: row.userId,
        family: row.family.unwrap_or_default(),
        expires_at: new_expires,
    })
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
        let db = connect_in_memory().await.expect("in-memory connect");
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
            Some(rotated.token.as_str()),
            "old row's replacedBy must point at the new token"
        );

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
        assert_eq!(bob_rows[0].token, bob_token.token);
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
        let row = refresh_tokens::insert(
            &db,
            refresh_tokens::NewRefreshToken {
                token: generate_refresh_token(),
                userId: uid,
                expiresAt: Utc::now() - Duration::seconds(1),
                family: Some(generate_family_id()),
            },
        )
        .await
        .unwrap();

        let err = rotate(&db, &row.token, ONE_DAY)
            .await
            .expect_err("expired must error");
        assert!(matches!(err, Error::InvalidOrExpired));

        assert!(
            refresh_tokens::find_by_token(&db, &row.token)
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
        // Spec §6.5.3 license: "side effects compatible with at-least-once".
        // Two rotations against the same input may both succeed (race on
        // set_replaced_by). The result is two siblings in the same family;
        // a third rotation against the original still observes a populated
        // replacedBy and triggers reuse-detection.
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
            !successes.is_empty(),
            "at least one rotation should succeed"
        );

        // A third rotation with the original supplied must trip
        // reuse-detection and revoke the user.
        let err = rotate(&db, &supplied, ONE_DAY)
            .await
            .expect_err("post-race replay must error");
        assert!(matches!(err, Error::InvalidOrExpired));
        assert!(
            refresh_tokens::list_for_user(&db, uid)
                .await
                .unwrap()
                .is_empty(),
            "post-race replay must wipe all sessions"
        );
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
                                Some(rotated.token.as_str()),
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
                                Some(rotated.token.as_str()),
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
}
