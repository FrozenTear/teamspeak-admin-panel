//! Admin-management page surfaces (v1.1, [PURA-237](/PURA/issues/PURA-237)).
//!
//! Per `docs/admin/ui-brief.md`: the admin-user list, the create/edit modal,
//! and the per-user sessions pane. The audit-log viewer is a sibling issue
//! and lands in its own module.

mod users;

pub use users::AdminUsersPage;
