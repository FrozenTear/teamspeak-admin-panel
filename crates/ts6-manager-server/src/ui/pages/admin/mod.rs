//! Admin-management page surfaces for the operator SPA (v1.1).
//!
//! Route map per `docs/admin/ui-brief.md` §2. PURA-238 ships the audit-log
//! viewer; the `/admin/users*` surfaces land via the sibling PURA-237
//! child and slot their modules in alongside `audit` here.

mod audit;

pub use audit::AuditPage;
