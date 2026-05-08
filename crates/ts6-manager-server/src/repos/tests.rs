//! End-to-end repo tests against an in-memory SurrealDB.
//!
//! These satisfy spec §4.5's verification list at the repo layer:
//! schema roundtrip, cascade-on-user-delete, composite-unique enforcement,
//! and reuse-detection lookup by `replacedBy`.

use chrono::{Duration, Utc};

use super::{refresh_tokens, server_connections, server_user_grants, users};
use crate::db::{connect_in_memory, migrations};

async fn setup() -> std::sync::Arc<crate::db::Database> {
    let db = connect_in_memory().await.expect("in-memory connect");
    migrations::run(&db).await.expect("migrations run");
    db
}

#[tokio::test]
async fn user_insert_and_find_by_username_roundtrip() {
    let db = setup().await;
    let inserted = users::insert(
        &db,
        users::NewUser {
            username: "alice".into(),
            passwordHash: "$2b$12$abcdef".into(),
            displayName: "Alice".into(),
            role: "admin".into(),
            enabled: true,
        },
    )
    .await
    .expect("insert");

    // 0001_baseline.surql defines sequences with `START 1`, so first row
    // gets id 1. Spec §4.1 only requires "auto-assigned 32-bit integer",
    // not a specific starting value — assert the id is non-negative so
    // the test survives a future sequence-config tweak.
    assert!(inserted.id >= 0);
    assert_eq!(inserted.username, "alice");
    assert_eq!(inserted.role, "admin");
    assert!(inserted.enabled);
    assert!(inserted.lastLoginAt.is_none());

    let found = users::find_by_username(&db, "alice")
        .await
        .expect("find_by_username")
        .expect("row should exist");
    assert_eq!(found.id, inserted.id);
    assert_eq!(found.passwordHash, "$2b$12$abcdef");
}

#[tokio::test]
async fn user_username_uniqueness_is_enforced() {
    let db = setup().await;
    users::insert(
        &db,
        users::NewUser {
            username: "bob".into(),
            passwordHash: "x".into(),
            displayName: "Bob".into(),
            role: "viewer".into(),
            enabled: true,
        },
    )
    .await
    .expect("first insert");

    let dup = users::insert(
        &db,
        users::NewUser {
            username: "bob".into(),
            passwordHash: "y".into(),
            displayName: "Bob 2".into(),
            role: "viewer".into(),
            enabled: true,
        },
    )
    .await;
    assert!(dup.is_err(), "duplicate username should be rejected");
}

#[tokio::test]
async fn deleting_user_cascades_to_refresh_tokens_and_grants() {
    // Spec §4.5 cascade test: deleting a User MUST remove their
    // RefreshToken and UserServerAccess rows. Covers the R5 cleanup half.
    let db = setup().await;

    let user = users::insert(
        &db,
        users::NewUser {
            username: "carol".into(),
            passwordHash: "h".into(),
            displayName: "Carol".into(),
            role: "moderator".into(),
            enabled: true,
        },
    )
    .await
    .expect("insert user");

    let server = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "primary".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:00:00:00".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .expect("insert server");

    refresh_tokens::insert(
        &db,
        refresh_tokens::NewRefreshToken {
            token: "t".repeat(128),
            userId: user.id,
            expiresAt: Utc::now() + Duration::days(7),
            family: Some("fam-1".into()),
        },
    )
    .await
    .expect("insert token");

    server_user_grants::insert(&db, user.id, server.id)
        .await
        .expect("insert grant");

    assert_eq!(
        refresh_tokens::list_for_user(&db, user.id)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        server_user_grants::list_for_user(&db, user.id)
            .await
            .unwrap()
            .len(),
        1
    );

    users::delete(&db, user.id).await.expect("delete user");

    assert!(
        refresh_tokens::list_for_user(&db, user.id)
            .await
            .unwrap()
            .is_empty(),
        "refresh_tokens must cascade-delete with user"
    );
    assert!(
        server_user_grants::list_for_user(&db, user.id)
            .await
            .unwrap()
            .is_empty(),
        "server_user_grant must cascade-delete with user"
    );
}

#[tokio::test]
async fn refresh_token_reuse_detection_lookup_by_replaced_by() {
    // Spec §6.5.4 — when a (rotated) old token is presented, the lookup
    // must find the predecessor row whose `replacedBy` records the new
    // token, returning the affected user id for family revocation.
    let db = setup().await;

    let user = users::insert(
        &db,
        users::NewUser {
            username: "dave".into(),
            passwordHash: "h".into(),
            displayName: "Dave".into(),
            role: "viewer".into(),
            enabled: true,
        },
    )
    .await
    .expect("insert user");

    let r1 = refresh_tokens::insert(
        &db,
        refresh_tokens::NewRefreshToken {
            token: "old-token".into(),
            userId: user.id,
            expiresAt: Utc::now() + Duration::days(7),
            family: Some("fam-1".into()),
        },
    )
    .await
    .expect("insert r1");

    refresh_tokens::set_replaced_by(&db, &r1.token, "new-token")
        .await
        .expect("rotate");

    let predecessor = refresh_tokens::find_predecessor_by_replaced_by(&db, "new-token")
        .await
        .expect("lookup")
        .expect("predecessor must be found");
    assert_eq!(predecessor.userId, user.id);
    assert_eq!(predecessor.token, "old-token");
    assert_eq!(predecessor.replacedBy.as_deref(), Some("new-token"));
}

#[tokio::test]
async fn refresh_token_family_lookup_returns_all_in_family() {
    let db = setup().await;
    let user = users::insert(
        &db,
        users::NewUser {
            username: "erin".into(),
            passwordHash: "h".into(),
            displayName: "Erin".into(),
            role: "viewer".into(),
            enabled: true,
        },
    )
    .await
    .unwrap();

    for i in 0..3 {
        refresh_tokens::insert(
            &db,
            refresh_tokens::NewRefreshToken {
                token: format!("t-{i}"),
                userId: user.id,
                expiresAt: Utc::now() + Duration::days(7),
                family: Some("fam-x".into()),
            },
        )
        .await
        .unwrap();
    }
    refresh_tokens::insert(
        &db,
        refresh_tokens::NewRefreshToken {
            token: "other-fam".into(),
            userId: user.id,
            expiresAt: Utc::now() + Duration::days(7),
            family: Some("fam-y".into()),
        },
    )
    .await
    .unwrap();

    let fam_x = refresh_tokens::list_for_family(&db, "fam-x").await.unwrap();
    assert_eq!(fam_x.len(), 3);
    let fam_y = refresh_tokens::list_for_family(&db, "fam-y").await.unwrap();
    assert_eq!(fam_y.len(), 1);
}

#[tokio::test]
async fn server_user_grant_composite_uniqueness_is_enforced() {
    let db = setup().await;
    let user = users::insert(
        &db,
        users::NewUser {
            username: "frank".into(),
            passwordHash: "h".into(),
            displayName: "Frank".into(),
            role: "moderator".into(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    let server = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "edge".into(),
            host: "edge.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
            useHttps: true,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .unwrap();

    server_user_grants::insert(&db, user.id, server.id)
        .await
        .expect("first grant");
    let dup = server_user_grants::insert(&db, user.id, server.id).await;
    assert!(
        dup.is_err(),
        "duplicate (userId, serverConfigId) must be rejected"
    );
}

#[tokio::test]
async fn deleting_server_cascades_to_grants() {
    // Spec §4.2.4 cascade — user_server_grant rows for a deleted
    // TsServerConfig must disappear with it.
    let db = setup().await;
    let user = users::insert(
        &db,
        users::NewUser {
            username: "gina".into(),
            passwordHash: "h".into(),
            displayName: "Gina".into(),
            role: "moderator".into(),
            enabled: true,
        },
    )
    .await
    .unwrap();
    let server = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "doomed".into(),
            host: "doomed.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .unwrap();
    server_user_grants::insert(&db, user.id, server.id)
        .await
        .unwrap();

    server_connections::delete(&db, server.id).await.unwrap();

    assert!(
        server_user_grants::list_for_server(&db, server.id)
            .await
            .unwrap()
            .is_empty(),
        "grants for the deleted server must cascade"
    );
}

#[tokio::test]
async fn user_set_password_hash_and_mark_login_persist() {
    let db = setup().await;
    let user = users::insert(
        &db,
        users::NewUser {
            username: "hank".into(),
            passwordHash: "old".into(),
            displayName: "Hank".into(),
            role: "viewer".into(),
            enabled: true,
        },
    )
    .await
    .unwrap();

    users::set_password_hash(&db, user.id, "new-hash".into())
        .await
        .unwrap();
    let after = users::find_by_id(&db, user.id).await.unwrap().unwrap();
    assert_eq!(after.passwordHash, "new-hash");

    users::mark_login(&db, user.id).await.unwrap();
    let after_login = users::find_by_id(&db, user.id).await.unwrap().unwrap();
    assert!(after_login.lastLoginAt.is_some());
}

// ====================================================================
// D-SSH-AUTH (PURA-77) — schema + binder coverage for the SSHBridge
// auth-model fields added in `0005_ssh_bridge_auth.surql`. See
// study-documents/ts6-manager-impl-deviations.md → D-SSH-AUTH for the
// full rationale and external-contract audit.
// ====================================================================

/// A row inserted with the new fields left as `None` MUST come back with
/// the migration-side defaults applied: `controlPath = 'webquery'`,
/// `sshAuthMethod = 'password'`, and the three option fields NULL. This
/// is the load-bearing invariant for the existing `POST /api/servers`
/// handler that has not been taught about the new fields yet — it relies
/// on the defaults to produce a row consistent with the spec's password-
/// only auth model.
#[tokio::test]
async fn server_connection_ssh_bridge_auth_defaults_apply_when_unset() {
    let db = setup().await;
    let row = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "default-row".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
            useHttps: false,
            sshPort: 10022,
            sshUsername: None,
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: None,
            sshAuthMethod: None,
            sshPrivateKey: None,
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .expect("insert");

    assert_eq!(row.controlPath, "webquery");
    assert_eq!(row.sshAuthMethod, "password");
    assert!(row.sshPrivateKey.is_none());
    assert!(row.sshKeyAgentSocket.is_none());
    assert!(row.sshHostKeyFingerprint.is_none());
}

/// Explicit non-default values must round-trip verbatim through the repo
/// for both the agent-auth path (`sshAuthMethod = 'agent'`, agent socket
/// path stored plaintext) and the strict-fingerprint host-key verifier.
#[tokio::test]
async fn server_connection_ssh_bridge_auth_explicit_values_round_trip() {
    let db = setup().await;
    let row = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "agent-row".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: "enc:0:0:0".into(),
            useHttps: true,
            sshPort: 10022,
            sshUsername: Some("serveradmin".into()),
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: Some("ssh".into()),
            sshAuthMethod: Some("agent".into()),
            sshPrivateKey: None,
            sshKeyAgentSocket: Some("/run/user/1000/ssh-agent.socket".into()),
            sshHostKeyFingerprint: Some(
                "SHA256:abcd1234abcd1234abcd1234abcd1234abcd1234ab".into(),
            ),
        },
    )
    .await
    .expect("insert");

    assert_eq!(row.controlPath, "ssh");
    assert_eq!(row.sshAuthMethod, "agent");
    assert_eq!(
        row.sshKeyAgentSocket.as_deref(),
        Some("/run/user/1000/ssh-agent.socket")
    );
    assert_eq!(
        row.sshHostKeyFingerprint.as_deref(),
        Some("SHA256:abcd1234abcd1234abcd1234abcd1234abcd1234ab")
    );

    // Read-back via list() also surfaces the same values — proves the
    // PROJECTION includes the new fields (not just the post-CREATE RETURN).
    let listed = server_connections::list(&db).await.unwrap();
    let stored = listed.iter().find(|r| r.name == "agent-row").unwrap();
    assert_eq!(stored.controlPath, "ssh");
    assert_eq!(stored.sshAuthMethod, "agent");
    assert_eq!(stored.sshKeyAgentSocket, row.sshKeyAgentSocket);
    assert_eq!(stored.sshHostKeyFingerprint, row.sshHostKeyFingerprint);
}

/// `sshPrivateKey` is the only D-SSH-AUTH field that holds operator-
/// supplied secret material, so it MUST round-trip through the spec
/// §6.3.2 envelope (`enc:<iv>:<tag>:<ct>`). The repo treats ciphertext as
/// an opaque string; this test pins the seal/unseal round-trip end-to-end
/// from the row write back through the row read.
#[tokio::test]
async fn server_connection_ssh_private_key_seals_and_unseals_through_row() {
    crate::crypto::init("test-seed-pura-77-key-roundtrip");
    let db = setup().await;

    let plaintext_key = "-----BEGIN OPENSSH PRIVATE KEY-----\nfake-key-bytes\n-----END OPENSSH PRIVATE KEY-----\n";
    let sealed = crate::crypto::seal(plaintext_key).expect("seal private key");
    assert!(
        sealed.starts_with("enc:"),
        "sealed envelope must use the spec §6.3.2 prefix"
    );

    let row = server_connections::insert(
        &db,
        server_connections::NewServerConnection {
            name: "key-row".into(),
            host: "ts.example.com".into(),
            webqueryPort: 10080,
            apiKey: crate::crypto::seal("api-k").expect("seal api key"),
            useHttps: false,
            sshPort: 10022,
            sshUsername: Some("serveradmin".into()),
            sshPassword: None,
            queryBotChannel: None,
            queryBotNickname: None,
            sshBotNickname: None,
            enabled: true,
            controlPath: Some("ssh".into()),
            sshAuthMethod: Some("key".into()),
            sshPrivateKey: Some(sealed.clone()),
            sshKeyAgentSocket: None,
            sshHostKeyFingerprint: None,
        },
    )
    .await
    .expect("insert");

    let stored_ciphertext = row
        .sshPrivateKey
        .as_deref()
        .expect("private key must persist");
    assert!(
        stored_ciphertext.starts_with("enc:"),
        "stored value must remain in the spec §6.3.2 envelope, never plaintext"
    );
    assert_eq!(stored_ciphertext, sealed, "ciphertext must round-trip verbatim");

    let unsealed = crate::crypto::unseal(stored_ciphertext).expect("unseal");
    assert_eq!(unsealed, plaintext_key, "unsealed text must equal the operator's key");
}
