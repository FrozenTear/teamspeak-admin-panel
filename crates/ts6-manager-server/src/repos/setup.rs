//! Atomic two-row insert for `POST /api/setup/init` (spec §7.2 + PURA-22).
//!
//! The setup wizard creates the bootstrap admin user **and** the first
//! `server_connection` row. Both rows must commit together — landing only
//! the user would lock the deployment into a half-initialised state where
//! `needsSetup` flips to `false` (user count > 0) but the dashboard has no
//! server to point at, and a follow-up retry would 409.
//!
//! Atomicity is enforced at the database boundary by wrapping the two
//! `CREATE` statements in a SurrealQL `BEGIN TRANSACTION; … COMMIT
//! TRANSACTION;` block. If either statement fails (schema violation,
//! unique-index conflict, sequence exhaustion, crash mid-write) SurrealDB
//! cancels the transaction and neither row lands. That makes the operation
//! crash-safe — closing the residual R-S5.2 risk left open by PURA-22's
//! application-level rollback.
//!
//! Concurrent one-shot enforcement still lives at the handler boundary as
//! a `tokio::sync::Mutex` (`AppState::setup_lock`); SurrealDB's MVCC only
//! guarantees at-most-one-success across concurrent writers when both
//! transactions touch the same row, but the setup case has disjoint writes
//! (different usernames + different server names would each commit
//! cleanly). This helper assumes the caller already serialised access.

use anyhow::{Context, Result};

use crate::db::Database;

use super::server_connections::{
    NewServerConnection, PROJECTION as SERVER_PROJECTION, ServerConnection,
};
use super::users::{NewUser, User};

/// Insert the bootstrap admin user and first server connection inside a
/// single SurrealDB transaction. Either both rows commit, or neither
/// does — there is no half-initialised state to clean up. Caller MUST
/// hold the setup mutex; see module docs.
pub async fn init_admin_and_first_server(
    db: &Database,
    new_user: NewUser,
    new_server: NewServerConnection,
) -> Result<(User, ServerConnection)> {
    // One multi-statement query so the BEGIN/COMMIT framing applies as a
    // single atomic unit on the embedded engines we ship (SurrealKV,
    // memory). Bindings are query-scoped; the `u_` / `s_` prefixes keep
    // the user/server fields disjoint where their names overlap (e.g.
    // both records have an `enabled` field).
    let sql = format!(
        "BEGIN TRANSACTION;
         CREATE type::record('user', sequence::nextval('user_id'))
             CONTENT {{
                 username: $u_username,
                 passwordHash: $u_passwordHash,
                 displayName: $u_displayName,
                 role: $u_role,
                 enabled: $u_enabled
             }}
             RETURN {USER_PROJECTION};
         CREATE type::record('server_connection', sequence::nextval('server_connection_id'))
             CONTENT {{
                 name: $s_name,
                 host: $s_host,
                 webqueryPort: $s_webqueryPort,
                 apiKey: $s_apiKey,
                 useHttps: $s_useHttps,
                 sshPort: $s_sshPort,
                 sshUsername: $s_sshUsername,
                 sshPassword: $s_sshPassword,
                 queryBotChannel: $s_queryBotChannel,
                 queryBotNickname: $s_queryBotNickname,
                 sshBotNickname: $s_sshBotNickname,
                 enabled: $s_enabled
             }}
             RETURN {SERVER_PROJECTION};
         COMMIT TRANSACTION;",
        USER_PROJECTION = USER_PROJECTION,
        SERVER_PROJECTION = SERVER_PROJECTION,
    );

    let mut resp = db
        .query(sql)
        .bind(("u_username", new_user.username))
        .bind(("u_passwordHash", new_user.passwordHash))
        .bind(("u_displayName", new_user.displayName))
        .bind(("u_role", new_user.role))
        .bind(("u_enabled", new_user.enabled))
        .bind(("s_name", new_server.name))
        .bind(("s_host", new_server.host))
        .bind(("s_webqueryPort", new_server.webqueryPort))
        .bind(("s_apiKey", new_server.apiKey))
        .bind(("s_useHttps", new_server.useHttps))
        .bind(("s_sshPort", new_server.sshPort))
        .bind(("s_sshUsername", new_server.sshUsername))
        .bind(("s_sshPassword", new_server.sshPassword))
        .bind(("s_queryBotChannel", new_server.queryBotChannel))
        .bind(("s_queryBotNickname", new_server.queryBotNickname))
        .bind(("s_sshBotNickname", new_server.sshBotNickname))
        .bind(("s_enabled", new_server.enabled))
        .await
        .context("setup: transactional init query failed")?
        .check()
        .context("setup: transactional init reported an error (transaction rolled back)")?;

    // Response indexing on SurrealDB v3 keeps a slot per submitted statement,
    // including the control statements: 0=BEGIN, 1=user CREATE, 2=server
    // CREATE, 3=COMMIT. Verified by an ad-hoc probe against the in-memory
    // engine; if the SDK changes this, the deserialise will yield None and
    // the .context() below will surface the regression loudly.
    let user: Option<User> = resp
        .take(1)
        .context("setup: failed to deserialise admin user from tx response")?;
    let server: Option<ServerConnection> = resp
        .take(2)
        .context("setup: failed to deserialise first server from tx response")?;

    let user = user.context("setup: bootstrap admin insert returned no row")?;
    let server = server.context("setup: server insert returned no row")?;

    Ok((user, server))
}

// `SERVER_PROJECTION` is re-exported from `repos::server_connections` so the
// transactional `RETURN` below stays in lock-step with normal reads — PURA-77
// drifted this projection silently and PURA-98 had to fix it after the fact.
// `USER_PROJECTION` stays inline because the user table has no equivalent
// drift history; if it grows D-* fields, mirror the server-side approach.
const USER_PROJECTION: &str = "
    record::id(id) AS id,
    username,
    passwordHash,
    displayName,
    role,
    enabled,
    createdAt,
    updatedAt,
    lastLoginAt
";

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{server_connections, users};
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

    /// Force any `server_connection` insert with a duplicate `host` to fail
    /// with a unique-index violation. Used by the fault-injection tests
    /// below so we can exercise the second-statement-fails branch without
    /// reaching for runtime fault-injection scaffolding.
    async fn add_unique_host_index(db: &Database) {
        db.query("DEFINE INDEX server_connection_host_unique ON server_connection FIELDS host UNIQUE;")
            .await
            .unwrap()
            .check()
            .unwrap();
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
        // user.username must reject the bootstrap insert and the
        // transaction rolls back before any server row lands.
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

    /// PURA-36 regression: residual risk R-S5.2. A failure of the *second*
    /// statement (server CREATE) must roll back the *first* (user CREATE).
    /// Before BEGIN/COMMIT this assertion would fail because the
    /// application-level rollback was best-effort and only ran for the Rust
    /// error path — a SurrealKV crash between the two inserts would leave
    /// the user row behind.
    #[tokio::test]
    async fn server_insert_failure_leaves_no_partial_state() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        add_unique_host_index(&db).await;

        // Seed the conflicting host so the second CREATE inside the
        // transaction fails on the unique index.
        server_connections::insert(
            &db,
            server_input("PreExisting", "ts.example.com"),
        )
        .await
        .unwrap();

        let res = init_admin_and_first_server(
            &db,
            user_input("admin"),
            server_input("Primary", "ts.example.com"),
        )
        .await;
        assert!(res.is_err(), "duplicate host must abort init");

        // The whole transaction rolled back: no admin user landed.
        let users = users::list(&db).await.unwrap();
        assert!(
            users.is_empty(),
            "user insert must roll back when server insert fails: {users:?}"
        );

        // And the server table is unchanged: only the pre-seeded row remains.
        let servers = server_connections::list(&db).await.unwrap();
        assert_eq!(
            servers.len(),
            1,
            "server table must be unchanged on rollback: {servers:?}"
        );
        assert_eq!(servers[0].name, "PreExisting");
    }

    /// After a failed init, a retry with non-conflicting inputs must
    /// succeed cleanly — the failed transaction must not leave behind any
    /// state (e.g. half-allocated sequence values that block retries, or
    /// shadow rows that confuse `users::count` in `/api/setup/status`).
    #[tokio::test]
    async fn retry_after_server_failure_succeeds_cleanly() {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        add_unique_host_index(&db).await;

        server_connections::insert(
            &db,
            server_input("PreExisting", "ts.example.com"),
        )
        .await
        .unwrap();

        // First attempt fails on the duplicate host.
        let first = init_admin_and_first_server(
            &db,
            user_input("admin"),
            server_input("Primary", "ts.example.com"),
        )
        .await;
        assert!(first.is_err());
        assert_eq!(users::count(&db).await.unwrap(), 0);

        // Second attempt with a fresh host commits.
        let (user, server) = init_admin_and_first_server(
            &db,
            user_input("admin"),
            server_input("Primary", "ts2.example.com"),
        )
        .await
        .unwrap();
        assert_eq!(user.username, "admin");
        assert_eq!(server.host, "ts2.example.com");

        assert_eq!(users::count(&db).await.unwrap(), 1);
        let servers = server_connections::list(&db).await.unwrap();
        assert_eq!(servers.len(), 2, "pre-existing + new = 2; got {servers:?}");
    }
}
