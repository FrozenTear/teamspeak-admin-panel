//! Spec Chapter 6 — authentication and authorisation.
//!
//! Phase 1 SECURITY scope (per [PURA-4 plan](/PURA/issues/PURA-4#document-plan)):
//! - [`password`] — Argon2id hashing + bcrypt-verify migration path.
//! - [`complexity`] — §6.2.2 password rules with spec-verbatim error strings.
//! - JWT, refresh tokens, RBAC extractors, and WS handshake land in
//!   subsequent slices once the DATA ticket provides the SurrealDB schema.
//!
//! Risks owned: R5 (refresh-token reuse-detection bug — in the next slice).

#![allow(dead_code)] // consumed by REST handlers (PURA-4 follow-up commit)

pub mod complexity;
pub mod password;
