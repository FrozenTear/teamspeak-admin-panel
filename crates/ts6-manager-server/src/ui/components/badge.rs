//! Role + status badge primitives for the admin-management surface.
//!
//! `docs/admin/ui-brief.md` §4 specifies a badge that pairs a colour token
//! with an icon glyph and a text label — colour-only differentiation is
//! forbidden (§6). Both badges render on top of the existing `.tag` pill
//! family from `components.css` so the admin pages share the panel's
//! status-pill visual language instead of inventing one-off chips.

use dioxus::prelude::*;

/// One of the three RBAC roles (`docs/admin/architecture.md` §3.2). Unknown
/// strings are tolerated — the server is the source of truth and a future
/// role addition should degrade to a neutral badge rather than panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Admin,
    Moderator,
    Viewer,
    Unknown,
}

impl Role {
    pub fn parse(raw: &str) -> Self {
        match raw.to_ascii_lowercase().as_str() {
            "admin" => Role::Admin,
            "moderator" => Role::Moderator,
            "viewer" => Role::Viewer,
            _ => Role::Unknown,
        }
    }

    /// `.tag` colour modifier — ui-brief §4.1 colour column.
    fn tag_class(self) -> &'static str {
        match self {
            Role::Admin => "tag tag-danger",
            Role::Moderator => "tag tag-warning",
            Role::Viewer => "tag tag-neutral",
            Role::Unknown => "tag tag-neutral",
        }
    }

    /// Icon glyph standing in for the lucide icon named in ui-brief §4.1
    /// (`shield-check` / `shield-half` / `eye`). The SPA renders glyphs, not
    /// an icon font, so we use the nearest stable Unicode symbol.
    fn icon(self) -> &'static str {
        match self {
            Role::Admin => "\u{1F6E1}",    // 🛡 shield
            Role::Moderator => "\u{2696}", // ⚖ balance
            Role::Viewer => "\u{1F441}",   // 👁 eye
            Role::Unknown => "?",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Role::Admin => "Admin",
            Role::Moderator => "Moderator",
            Role::Viewer => "Viewer",
            Role::Unknown => "Unknown",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            Role::Admin => "Full read/write across the panel and audit log.",
            Role::Moderator => "Read/write on granted servers; cannot manage users.",
            Role::Viewer => "Read-only on granted servers.",
            Role::Unknown => "Unrecognised role — managed outside this panel.",
        }
    }
}

/// Derived account status — ui-brief §4.2. `Disabled` wins over the
/// login-history split because a disabled account's sign-in state is the
/// operator's first concern regardless of whether it ever logged in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UserStatus {
    Active,
    Disabled,
    NeverSignedIn,
}

impl UserStatus {
    /// Fold `enabled` + login history into the badge state.
    pub fn derive(enabled: bool, has_logged_in: bool) -> Self {
        if !enabled {
            UserStatus::Disabled
        } else if has_logged_in {
            UserStatus::Active
        } else {
            UserStatus::NeverSignedIn
        }
    }

    fn tag_class(self) -> &'static str {
        match self {
            UserStatus::Active => "tag tag-success",
            UserStatus::Disabled => "tag tag-neutral",
            UserStatus::NeverSignedIn => "tag tag-warning",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            UserStatus::Active => "\u{2713}",        // ✓
            UserStatus::Disabled => "\u{2298}",      // ⊘
            UserStatus::NeverSignedIn => "\u{2026}", // …
        }
    }

    fn label(self) -> &'static str {
        match self {
            UserStatus::Active => "Active",
            UserStatus::Disabled => "Disabled",
            UserStatus::NeverSignedIn => "Never signed in",
        }
    }

    fn tooltip(self) -> &'static str {
        match self {
            UserStatus::Active => "Sign-in enabled.",
            UserStatus::Disabled => "Sign-in disabled; all sessions revoked.",
            UserStatus::NeverSignedIn => "User has not signed in since creation.",
        }
    }
}

/// Role pill — colour + icon + text label, with the capability summary on
/// hover. `aria-hidden` on the glyph keeps the screen-reader announcement to
/// the text label (the glyph is decorative; the label carries the meaning).
#[component]
pub fn RoleBadge(role: String) -> Element {
    let role = Role::parse(&role);
    rsx! {
        span { class: "{role.tag_class()}", title: "{role.tooltip()}",
            span { class: "tag-icn", aria_hidden: "true", "{role.icon()}" }
            "{role.label()}"
        }
    }
}

/// Status pill — Active / Disabled / Never signed in (ui-brief §4.2).
#[component]
pub fn StatusBadge(status: UserStatus) -> Element {
    rsx! {
        span { class: "{status.tag_class()}", title: "{status.tooltip()}",
            span { class: "tag-icn", aria_hidden: "true", "{status.icon()}" }
            "{status.label()}"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parse_is_case_insensitive() {
        assert_eq!(Role::parse("ADMIN"), Role::Admin);
        assert_eq!(Role::parse("Moderator"), Role::Moderator);
        assert_eq!(Role::parse("viewer"), Role::Viewer);
        assert_eq!(Role::parse("root"), Role::Unknown);
    }

    #[test]
    fn user_status_disabled_wins_over_login_history() {
        // A disabled account that logged in before is still "Disabled" —
        // the sign-in state is the operator's first concern.
        assert_eq!(UserStatus::derive(false, true), UserStatus::Disabled);
        assert_eq!(UserStatus::derive(false, false), UserStatus::Disabled);
    }

    #[test]
    fn user_status_splits_enabled_accounts_by_login_history() {
        assert_eq!(UserStatus::derive(true, true), UserStatus::Active);
        assert_eq!(UserStatus::derive(true, false), UserStatus::NeverSignedIn);
    }
}
