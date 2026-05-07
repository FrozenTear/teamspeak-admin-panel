//! Spec Chapter 6 — authentication and authorisation.
//!
//! Phase 1 SECURITY scope (per [PURA-4 plan](/PURA/issues/PURA-4#document-plan)):
//! - [`password`] — Argon2id hashing + bcrypt-verify migration path.
//! - [`complexity`] — §6.2.2 password rules with spec-verbatim error strings.
//! - [`jwt`] — HS256 access-token codec.
//! - [`refresh`] — refresh-token rotation + reuse-detection (R5).
//! - RBAC extractors and the WS handshake land in subsequent slices.
//!
//! Risks owned: R5 (refresh-token reuse-detection — covered by [`refresh`]).

#![allow(dead_code)] // consumed by REST handlers (PURA-4 follow-up commit)

pub mod complexity;
pub mod extractors;
pub mod jwt;
pub mod password;
pub mod refresh;
pub mod routes;
