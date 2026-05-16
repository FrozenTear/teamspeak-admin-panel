//! `user_permission` repo — Phase 9.0 fine-grained grants (PURA-285,
//! migration 0011).
//!
//! Storage layer for the `moderation.*` permission catalog (design brief
//! §6). One row per `(subjectUserId, permission)` pair — the
//! `user_permission_unique` index in migration 0011 enforces that, and
//! [`grant`] is written to be idempotent against it.
//!
//! **Workstream split (coordination with 9.0-rbac / PURA-284):** this
//! repo owns the *table CRUD only*. The `RequirePermission` extractor,
//! the `admin → all` short-circuit, the `moderator` role-default set, and
//! the grant-management UI all belong to PURA-284 and build on top of
//! [`permissions_for_user`] / [`holds`]. No permission-catalog constants
//! live here — that catalog is 9.0-rbac's to define.

#![allow(non_snake_case)]
#![allow(dead_code)] // consumed by 9.0-rbac (PURA-284) + 9.0-routes (PURA-286)

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use surrealdb::types::SurrealValue;

use crate::db::Database;

#[derive(Debug, Clone, Serialize, Deserialize, SurrealValue)]
#[surreal(crate = "surrealdb::types")]
pub struct UserPermission {
    pub id: i64,
    pub subjectUserId: i64,
    pub permission: String,
    pub grantedByUserId: Option<i64>,
    pub grantedAt: DateTime<Utc>,
}

const PROJECTION: &str = "
    record::id(id) AS id,
    subjectUserId,
    permission,
    grantedByUserId,
    grantedAt
";

/// Grant a permission to a user. Idempotent: a re-grant of an existing
/// `(subjectUserId, permission)` pair returns the existing row unchanged
/// rather than creating a duplicate or erroring on the unique index.
pub async fn grant(
    db: &Database,
    subject_user_id: i64,
    permission: &str,
    granted_by_user_id: Option<i64>,
) -> Result<UserPermission> {
    if let Some(existing) = find(db, subject_user_id, permission).await? {
        return Ok(existing);
    }
    let sql = format!(
        "CREATE type::record('user_permission', sequence::nextval('user_permission_id'))
            CONTENT {{
                subjectUserId:   $subjectUserId,
                permission:      $permission,
                grantedByUserId: $grantedByUserId
            }}
            RETURN {PROJECTION};"
    );
    let mut resp = db
        .query(sql)
        .bind(("subjectUserId", subject_user_id))
        .bind(("permission", permission.to_string()))
        .bind(("grantedByUserId", granted_by_user_id))
        .await
        .context("user_permission grant query failed")?
        .check()?;
    let row: Option<UserPermission> = resp.take(0)?;
    row.context("user_permission grant returned no row")
}

/// Revoke a permission from a user. Returns `true` when a grant existed.
pub async fn revoke(db: &Database, subject_user_id: i64, permission: &str) -> Result<bool> {
    // SurrealDB's `DELETE` returns nothing to project, so existence is
    // checked first via the unique-keyed lookup.
    let existed = find(db, subject_user_id, permission).await?.is_some();
    db.query(
        "DELETE user_permission
            WHERE subjectUserId = $subjectUserId AND permission = $permission;",
    )
    .bind(("subjectUserId", subject_user_id))
    .bind(("permission", permission.to_string()))
    .await
    .context("user_permission revoke query failed")?
    .check()?;
    Ok(existed)
}

/// Look up one specific grant.
pub async fn find(
    db: &Database,
    subject_user_id: i64,
    permission: &str,
) -> Result<Option<UserPermission>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM user_permission
            WHERE subjectUserId = $subjectUserId AND permission = $permission
            LIMIT 1;"
    );
    let mut resp = db
        .query(sql)
        .bind(("subjectUserId", subject_user_id))
        .bind(("permission", permission.to_string()))
        .await
        .context("user_permission find query failed")?
        .check()?;
    let rows: Vec<UserPermission> = resp.take(0)?;
    Ok(rows.into_iter().next())
}

/// `true` when the user has the explicit grant. Note this is the *raw*
/// table check — the 9.0-rbac extractor layers the `admin → all`
/// short-circuit and the role-default set on top.
pub async fn holds(db: &Database, subject_user_id: i64, permission: &str) -> Result<bool> {
    Ok(find(db, subject_user_id, permission).await?.is_some())
}

/// Every grant row for a user, newest-first — backs the grant-management
/// UI's per-user view.
pub async fn list_for_user(db: &Database, subject_user_id: i64) -> Result<Vec<UserPermission>> {
    let sql = format!(
        "SELECT {PROJECTION} FROM user_permission
            WHERE subjectUserId = $subjectUserId
            ORDER BY grantedAt DESC, id DESC;"
    );
    let mut resp = db
        .query(sql)
        .bind(("subjectUserId", subject_user_id))
        .await
        .context("user_permission list_for_user query failed")?
        .check()?;
    Ok(resp.take(0)?)
}

/// Just the permission strings a user explicitly holds — the convenience
/// shape the `RequirePermission` extractor unions with the role-default
/// set.
pub async fn permissions_for_user(db: &Database, subject_user_id: i64) -> Result<Vec<String>> {
    Ok(list_for_user(db, subject_user_id)
        .await?
        .into_iter()
        .map(|r| r.permission)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{connect_in_memory, migrations};
    use crate::repos::users::{self, NewUser};

    async fn fresh_db() -> std::sync::Arc<Database> {
        let db = connect_in_memory().await.unwrap();
        migrations::run(&db).await.unwrap();
        db
    }

    async fn make_user(db: &Database, username: &str, role: &str) -> i64 {
        users::insert(
            db,
            NewUser {
                username: username.into(),
                passwordHash: "$argon2id$v=19$test".into(),
                displayName: username.into(),
                role: role.into(),
                enabled: true,
            },
        )
        .await
        .unwrap()
        .id
    }

    #[tokio::test]
    async fn grant_then_holds() {
        let db = fresh_db().await;
        let uid = make_user(&db, "mod1", "moderator").await;
        grant(&db, uid, "moderation.action.ban", Some(1))
            .await
            .unwrap();
        assert!(holds(&db, uid, "moderation.action.ban").await.unwrap());
        assert!(!holds(&db, uid, "moderation.action.ban_ip").await.unwrap());
    }

    #[tokio::test]
    async fn grant_is_idempotent() {
        let db = fresh_db().await;
        let uid = make_user(&db, "mod2", "moderator").await;
        let first = grant(&db, uid, "moderation.case.view", None).await.unwrap();
        let second = grant(&db, uid, "moderation.case.view", Some(7))
            .await
            .unwrap();
        // Same row — the re-grant did not create a duplicate.
        assert_eq!(first.id, second.id);
        assert_eq!(list_for_user(&db, uid).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revoke_removes_the_grant() {
        let db = fresh_db().await;
        let uid = make_user(&db, "mod3", "moderator").await;
        grant(&db, uid, "moderation.note.write", None)
            .await
            .unwrap();
        assert!(revoke(&db, uid, "moderation.note.write").await.unwrap());
        assert!(!holds(&db, uid, "moderation.note.write").await.unwrap());
        assert!(
            !revoke(&db, uid, "moderation.note.write").await.unwrap(),
            "second revoke is a no-op"
        );
    }

    #[tokio::test]
    async fn permissions_for_user_lists_every_grant() {
        let db = fresh_db().await;
        let uid = make_user(&db, "mod4", "moderator").await;
        grant(&db, uid, "moderation.case.view", None).await.unwrap();
        grant(&db, uid, "moderation.case.manage", None)
            .await
            .unwrap();

        let mut perms = permissions_for_user(&db, uid).await.unwrap();
        perms.sort();
        assert_eq!(
            perms,
            vec!["moderation.case.manage", "moderation.case.view"]
        );
    }

    #[tokio::test]
    async fn deleting_a_user_removes_their_grants() {
        // Pins the `user_cascade_moderation` event in migration 0011.
        let db = fresh_db().await;
        let subject = make_user(&db, "subject", "moderator").await;
        grant(&db, subject, "moderation.case.view", None)
            .await
            .unwrap();

        users::delete(&db, subject).await.unwrap();

        assert!(
            list_for_user(&db, subject).await.unwrap().is_empty(),
            "user delete cascades away their grants"
        );
    }
}
