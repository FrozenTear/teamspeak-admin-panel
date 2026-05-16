//! Client-side mirror of the server `moderation.*` permission catalog
//! (`auth/permissions.rs`, Phase 9.0-rbac / PURA-284).
//!
//! The server `RequirePermission` extractor is the **authoritative** gate:
//! every `/api/moderation/*` route re-resolves the caller's effective
//! grants against the DB-current role. This module exists only so the SPA
//! can suppress write affordances the caller almost certainly cannot use,
//! rather than rendering a button that 403s on click — the same
//! best-effort visual-gate pattern the audit/users surfaces use with the
//! JWT `role` claim.
//!
//! ## Why role-derived, not grant-derived
//!
//! The session blob carries only `role` — there is no self-service
//! "my effective permissions" endpoint (`GET /api/users/{id}/permissions`
//! is admin-only). So the visual gate is derived from role:
//!
//! - `admin`     → holds the entire catalog.
//! - `moderator` → holds [`MODERATOR_DEFAULT`] (catalog minus `ban_ip`).
//! - anything else → holds nothing.
//!
//! An explicit `user_permission` grant that diverges from the role default
//! (e.g. a moderator individually granted `ban_ip`) is invisible to this
//! gate — the server still honours it, so the action is never *wrongly
//! allowed*, only conservatively hidden. Closing that gap needs a
//! `GET /api/auth/me/permissions` route; tracked as a 9.0 follow-up.

/// One catalog entry: `(permission key, operator label, description)`.
/// The key is the stable wire contract checked server-side; the label and
/// description are presentation-only.
pub type CatalogEntry = (&'static str, &'static str, &'static str);

/// The full `moderation.*` catalog — 11 permissions, plan §6. Order is the
/// display order in the grant UI (grouped: complaint, case, action, note,
/// history).
pub const CATALOG: &[CatalogEntry] = &[
    (
        "moderation.complaint.view",
        "View complaints",
        "See the TS6 complaint queue for a virtual server.",
    ),
    (
        "moderation.complaint.resolve",
        "Resolve complaints",
        "Dismiss complaints from the queue (complaindel / complaindelall).",
    ),
    (
        "moderation.case.view",
        "View cases",
        "Open the moderation case queue and read case detail.",
    ),
    (
        "moderation.case.manage",
        "Manage cases",
        "Open, resolve, and reopen moderation cases.",
    ),
    (
        "moderation.action.kick",
        "Kick",
        "Kick a connected client from the server as a case action.",
    ),
    (
        "moderation.action.mute",
        "Mute / unmute",
        "Mute or unmute a connected client as a case action.",
    ),
    (
        "moderation.action.ban",
        "Ban by identity",
        "Ban a case subject by their durable unique identifier (UID).",
    ),
    (
        "moderation.action.ban_ip",
        "Ban by IP address",
        "Ban a raw IP address — affects every client sharing that address.",
    ),
    (
        "moderation.note.view",
        "View notes",
        "Read moderator notes attached to a subject.",
    ),
    (
        "moderation.note.write",
        "Write notes",
        "Add moderator notes to a subject or a case timeline.",
    ),
    (
        "moderation.history.view",
        "View history",
        "See a subject's full cross-case moderation history.",
    ),
];

/// The IP-ban permission — surfaced as a named constant because it is the
/// one catalog member with a collateral-damage warning attached and the
/// one excluded from the moderator role default.
pub const BAN_IP: &str = "moderation.action.ban_ip";

/// Server `MODERATOR_DEFAULT_PERMISSIONS` (`auth/permissions.rs`) — the
/// entire catalog **except** `moderation.action.ban_ip`. Kept in sync with
/// the server by the `moderator_default_is_catalog_minus_ban_ip` test
/// there; [`moderator_default_matches_catalog_minus_ban_ip`] pins the
/// mirror on this side.
pub const MODERATOR_DEFAULT: &[&str] = &[
    "moderation.complaint.view",
    "moderation.complaint.resolve",
    "moderation.case.view",
    "moderation.case.manage",
    "moderation.action.kick",
    "moderation.action.mute",
    "moderation.action.ban",
    "moderation.note.view",
    "moderation.note.write",
    "moderation.history.view",
];

/// `true` when `permission` is a member of the catalog.
pub fn is_known(permission: &str) -> bool {
    CATALOG.iter().any(|(key, _, _)| *key == permission)
}

/// Best-effort visual gate: does an account with `role` hold `permission`
/// by role default? See the module docs — this is conservative (never a
/// false *allow*) and the server gate remains authoritative.
pub fn role_holds(role: &str, permission: &str) -> bool {
    match role.to_ascii_lowercase().as_str() {
        "admin" => is_known(permission),
        "moderator" => MODERATOR_DEFAULT.contains(&permission),
        _ => false,
    }
}

/// `true` when `role` may reach the moderation surfaces at all — the
/// page-level role gate. `viewer` and unknown roles are bounced to an
/// in-page 403 surface.
pub fn role_can_moderate(role: &str) -> bool {
    matches!(role.to_ascii_lowercase().as_str(), "admin" | "moderator")
}

/// The operator label for a catalog key, or the raw key if unknown (so a
/// stale explicit grant for a since-removed permission still renders
/// legibly in the grant UI).
pub fn label_for(permission: &str) -> &str {
    CATALOG
        .iter()
        .find(|(key, _, _)| *key == permission)
        .map(|(_, label, _)| *label)
        .unwrap_or(permission)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_eleven_namespaced_entries() {
        assert_eq!(CATALOG.len(), 11, "plan §6 lists 11 moderation.* perms");
        for (key, _, _) in CATALOG {
            assert!(
                key.starts_with("moderation."),
                "every catalog key is moderation.-namespaced: {key}"
            );
        }
    }

    /// Pins the mirror against the server `MODERATOR_DEFAULT_PERMISSIONS`:
    /// the moderator default is exactly the catalog minus `ban_ip`.
    #[test]
    fn moderator_default_matches_catalog_minus_ban_ip() {
        let expected: Vec<&str> = CATALOG
            .iter()
            .map(|(key, _, _)| *key)
            .filter(|key| *key != BAN_IP)
            .collect();
        assert_eq!(MODERATOR_DEFAULT, expected.as_slice());
        assert!(!MODERATOR_DEFAULT.contains(&BAN_IP));
    }

    #[test]
    fn role_holds_is_fail_closed() {
        // admin holds everything, including ban_ip.
        assert!(role_holds("admin", BAN_IP));
        assert!(role_holds("ADMIN", "moderation.case.view"));
        // moderator holds the default set but never ban_ip.
        assert!(role_holds("moderator", "moderation.action.ban"));
        assert!(!role_holds("moderator", BAN_IP));
        // viewer / unknown / empty hold nothing.
        assert!(!role_holds("viewer", "moderation.case.view"));
        assert!(!role_holds("", "moderation.case.view"));
        // a non-catalog permission is never held.
        assert!(!role_holds("admin", "moderation.bogus"));
    }

    #[test]
    fn page_gate_admits_only_admin_and_moderator() {
        assert!(role_can_moderate("admin"));
        assert!(role_can_moderate("Moderator"));
        assert!(!role_can_moderate("viewer"));
        assert!(!role_can_moderate(""));
    }
}
