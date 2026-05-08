//! Atomic two-row insert for `POST /api/setup/init` (spec §7.2 + PURA-22).
//!
//! The setup wizard creates the bootstrap admin user **and** the first
//! `server_connection` row. Both rows must commit together — landing only
//! the user would lock the deployment into a half-initialised state where
//! `needsSetup` flips to `false` (user count > 0) but the dashboard has no
//! server to point at, and a follow-up retry would 409.
//!
//! SurrealDB's MVCC `BEGIN/COMMIT` guarantees at-most-one-success across
//! concurrent writers when both transactions touch the same row, but that
//! does not extend to disjoint writes (two different usernames + two
//! different server names would each commit cleanly). Concurrent
//! one-shot enforcement therefore lives at the handler boundary as a
//! `tokio::sync::Mutex` (`AppState::setup_lock`); this helper assumes the
//! caller already serialised access.
//!
//! Failure model: insert the user first, then the server. If the server
//! insert fails we delete the user row best-effort and propagate the
//! original error. The cascading-delete event in `0001_baseline.surql`
//! cleans dependent `refresh_token` / `server_user_grant` rows on user
//! delete, so the rollback is observably equivalent to "neither row
//! existed". A rollback that itself fails is logged at ERROR with the user
//! id so the operator can clean up — the alternative (silently leaving a
//! half-init) is worse.

use anyhow::{Context, Result};

use crate::db::Database;

use super::server_connections::{self, NewServerConnection, ServerConnection};
use super::users::{self, NewUser, User};

/// Insert the bootstrap admin user and first server connection in a way
/// that observably succeeds together or rolls back together. Caller MUST
/// hold the setup mutex — see module docs.
pub async fn init_admin_and_first_server(
    db: &Database,
    new_user: NewUser,
    new_server: NewServerConnection,
) -> Result<(User, ServerConnection)> {
    // Insert the user first so the unique-username index fires before we
    // touch the server table — a duplicate-username error there is cheaper
    // to recover from than an orphaned server_connection row.
    let user = users::insert(db, new_user)
        .await
        .context("setup: bootstrap admin insert failed")?;

    match server_connections::insert(db, new_server).await {
        Ok(server) => Ok((user, server)),
        Err(server_err) => {
            // Best-effort rollback. The `user_cascade` event takes care of
            // any dependent rows (none expected at this point — login
            // hasn't happened, so no refresh_token, no grants), so a plain
            // DELETE is sufficient.
            if let Err(rollback_err) = users::delete(db, user.id).await {
                tracing::error!(
                    user_id = user.id,
                    rollback_err = %rollback_err,
                    server_err = %server_err,
                    "setup: server insert failed AND rollback of admin user failed; \
                     deployment is in a half-initialised state and requires manual cleanup"
                );
            } else {
                tracing::warn!(
                    user_id = user.id,
                    server_err = %server_err,
                    "setup: server insert failed; rolled back the admin user"
                );
            }
            Err(server_err).context("setup: server insert failed; user rolled back")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};

    fn user_input(name: &str) -> NewUser {
        NewUser {
            username: name.into(),
            passwordHash: "$argon2id$v=19$m=19456,t=2,p=1$pseudo$pseudo".into(),
            displayName: name.into(),
            role: "admin".into(),
            enabled: true,
        }
    }

    fn server_input(name: &str, host: &str) -> NewServerConnection {
        NewServerConnection {
            name: name.into(),
            host: host.into(),
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
        }
    }

    #[tokio::test]
    async fn happy_path_creates_both_rows() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        let (user, server) = init_admin_and_first_server(
            &db,
            user_input("admin"),
            server_input("Primary", "ts.example.com"),
        )
        .await
        .unwrap();
        assert_eq!(user.username, "admin");
        assert_eq!(user.role, "admin");
        assert_eq!(server.name, "Primary");

        let users = users::list(&db).await.unwrap();
        let servers = server_connections::list(&db).await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(servers.len(), 1);
    }

    #[tokio::test]
    async fn user_insert_failure_leaves_no_partial_state() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();

        // Pre-seed a user with the same username; the unique index on
        // user.username must reject the bootstrap insert before we ever
        // touch server_connection.
        users::insert(&db, user_input("admin")).await.unwrap();

        let res = init_admin_and_first_server(
            &db,
            user_input("admin"),
            server_input("Primary", "ts.example.com"),
        )
        .await;
        assert!(res.is_err(), "duplicate username must abort init");

        // server_connection table must still be empty — no orphan row.
        let servers = server_connections::list(&db).await.unwrap();
        assert!(
            servers.is_empty(),
            "server insert must not run when user insert fails: {servers:?}"
        );
    }
}
