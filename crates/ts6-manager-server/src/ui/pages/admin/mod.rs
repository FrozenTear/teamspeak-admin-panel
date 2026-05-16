//! Admin-management page surfaces (v1.1, [PURA-237](/PURA/issues/PURA-237) /
//! [PURA-238](/PURA/issues/PURA-238)).
//!
//! Route map per `docs/admin/ui-brief.md` §2: the admin-user list + the
//! create/edit modal + the per-user sessions pane (`users`), and the
//! audit-log viewer (`audit`).

mod audit;
mod users;

pub use audit::AuditPage;
pub use users::AdminUsersPage;
