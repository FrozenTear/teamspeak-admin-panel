//! `/admin/permissions` — per-user `moderation.*` grant editor. PURA-287.
//!
//! Admin-only. Wired to the PURA-284 grant surface:
//!
//! - `GET /api/users` — the user picker.
//! - `GET /api/users/{id}/permissions` — the user's resolved `effective`
//!   set and raw `explicitGrants`.
//! - `PUT /api/users/{id}/permissions` — replace-all of the explicit
//!   grant set.
//!
//! ## What the operator actually edits
//!
//! The server resolves `effective = role-defaults ∪ explicit-grants`,
//! intersected with the catalog. Explicit grants are **additive** — there
//! is no per-user revoke of a role default. So this editor manages the
//! *explicit grant set*: one checkbox per catalog permission. A permission
//! already held via the role default is tagged "Role default"; ticking it
//! as an explicit grant is harmless but redundant. An `admin` user holds
//! the whole catalog implicitly, so the editor shows a notice for those.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};
use ts6_manager_shared::admin::AdminUser;

use crate::client::api::{self};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonVariant};

use super::perm;
use super::{AccessDenied, format_error};

/// `GET|PUT /api/users/{id}/permissions` response — mirrors the server
/// `UserPermissionsResponse` (`routes/users.rs`, `#[serde(camelCase)]`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UserPermissions {
    user_id: i64,
    effective: Vec<String>,
    explicit_grants: Vec<String>,
}

/// `PUT /api/users/{id}/permissions` body — mirrors `UserPermissionsUpdate`
/// (no rename: the single field is already lowercase).
#[derive(Debug, Clone, Serialize)]
struct UserPermissionsUpdate {
    permissions: Vec<String>,
}

#[component]
pub fn PermissionGrantsPage() -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        return rsx! { "" };
    }

    let is_admin = session
        .state
        .read()
        .user()
        .map(|u| u.role.eq_ignore_ascii_case("admin"))
        .unwrap_or(false);
    if !is_admin {
        return rsx! {
            AccessDenied {
                crumb: "Admin · Permissions".to_string(),
                heading: "Permission grants".to_string(),
                detail: "Permission management is available to admin accounts only.".to_string(),
            }
        };
    }

    let gate = use_auth_gate();

    // User picker — a failure here blocks the whole surface, so it is
    // surfaced rather than swallowed.
    let users = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                api::authorized_get_json::<Vec<AdminUser>>(&gate, &api::api_base(), "/api/users")
                    .await
            }
        }
    });

    let mut selected: Signal<Option<i64>> = use_signal(|| None);

    let users_snapshot = users.read().clone();

    rsx! {
        div { class: "crumb", "Admin · Permissions" }
        h1 { "Permission grants" }
        p { class: "info-hint",
            "Grant moderation permissions to individual accounts. Grants are additive on top of "
            "the account's role defaults; the server enforces every grant on each request."
        }

        match users_snapshot {
            None => rsx! {
                p { class: "info-hint", "Loading users…" }
            },
            Some(Err(e)) => rsx! {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load users".to_string(),
                    "{format_error(&e)}"
                }
            },
            Some(Ok(user_list)) => rsx! {
                div { class: "form-row mod-user-picker",
                    label { r#for: "grant-user", "Account" }
                    select {
                        id: "grant-user",
                        class: "input",
                        onchange: move |e| {
                            selected.set(e.value().parse::<i64>().ok());
                        },
                        option { value: "", "Select an account…" }
                        for u in user_list.iter() {
                            option { key: "{u.id}", value: "{u.id}",
                                "{u.display_name} ({u.username}) · {u.role}"
                            }
                        }
                    }
                }

                match *selected.read() {
                    None => rsx! {
                        div { class: "empty",
                            div { class: "icon", "⚒" }
                            h3 { "No account selected" }
                            p { "Pick an account above to view and edit its moderation grants." }
                        }
                    },
                    Some(user_id) => {
                        let user = user_list.iter().find(|u| u.id == user_id).cloned();
                        match user {
                            Some(user) => rsx! {
                                GrantEditor { key: "{user_id}", user }
                            },
                            None => rsx! {
                                p { class: "info-hint", "That account is no longer in the list." }
                            },
                        }
                    }
                }
            },
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct GrantEditorProps {
    user: AdminUser,
}

#[component]
fn GrantEditor(props: GrantEditorProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let user = props.user.clone();
    let user_id = user.id;
    let role = user.role.clone();
    let is_admin_user = role.eq_ignore_ascii_case("admin");

    // Editable explicit-grant draft + the fetched baseline (for the dirty
    // check and Reset). `None` until the first fetch resolves.
    let mut draft: Signal<Option<Vec<String>>> = use_signal(|| None);
    let mut baseline: Signal<Vec<String>> = use_signal(Vec::new);
    let mut busy = use_signal(|| false);

    let perms = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                let path = format!("/api/users/{user_id}/permissions");
                api::authorized_get_json::<UserPermissions>(&gate, &api::api_base(), &path).await
            }
        }
    });

    // Seed the draft from the fetched explicit-grant set once it arrives.
    {
        use_effect(move || {
            if let Some(Ok(p)) = perms.read_unchecked().as_ref()
                && draft.peek().is_none()
            {
                draft.set(Some(p.explicit_grants.clone()));
                baseline.set(p.explicit_grants.clone());
            }
        });
    }

    let perms_snapshot = perms.read().clone();
    let Some(result) = perms_snapshot else {
        return rsx! {
            p { class: "info-hint", "Loading grants…" }
        };
    };
    if let Err(e) = result {
        return rsx! {
            Banner {
                variant: BannerVariant::Danger,
                title: "Could not load grants".to_string(),
                "{format_error(&e)}"
            }
        };
    }

    let draft_now = match draft.read().clone() {
        Some(d) => d,
        None => {
            // Effect has not seeded the draft yet — render a tick later.
            return rsx! {
                p { class: "info-hint", "Loading grants…" }
            };
        }
    };
    let mut sorted_baseline = baseline.read().clone();
    sorted_baseline.sort();
    let mut sorted_draft = draft_now.clone();
    sorted_draft.sort();
    let dirty = sorted_baseline != sorted_draft;

    let toggle = move |permission: String| {
        draft.with_mut(|d| {
            if let Some(set) = d.as_mut() {
                if let Some(idx) = set.iter().position(|p| *p == permission) {
                    set.remove(idx);
                } else {
                    set.push(permission);
                }
            }
        });
    };

    let on_reset = move |_| {
        draft.set(Some(baseline.peek().clone()));
    };

    let on_save = {
        let gate = gate.clone();
        move |_| {
            if *busy.peek() || is_admin_user {
                return;
            }
            let permissions = draft.peek().clone().unwrap_or_default();
            let gate = gate.clone();
            let toaster = toaster;
            busy.set(true);
            spawn(async move {
                let body = UserPermissionsUpdate { permissions };
                let path = format!("/api/users/{user_id}/permissions");
                let res = api::authorized_put_json::<_, UserPermissions>(
                    &gate,
                    &api::api_base(),
                    &path,
                    &body,
                )
                .await;
                busy.set(false);
                match res {
                    Ok(updated) => {
                        toaster.push(ToastVariant::Success, "Grants saved", None);
                        draft.set(Some(updated.explicit_grants.clone()));
                        baseline.set(updated.explicit_grants);
                    }
                    Err(e) => {
                        toaster.push(
                            ToastVariant::Danger,
                            "Could not save grants",
                            Some(format_error(&e)),
                        );
                    }
                }
            });
        }
    };

    rsx! {
        section { class: "stack-md mod-panel",
            h2 { "{user.display_name}" }
            p { class: "info-hint", "Role: {role} · {user.username}" }

            if is_admin_user {
                Banner {
                    variant: BannerVariant::Info,
                    title: "Admin accounts hold everything".to_string(),
                    "An admin account implicitly holds the entire moderation catalog. "
                    "Explicit grants below have no additional effect and cannot be saved."
                }
            }

            ul { class: "mod-grant-list",
                for (key , label , desc) in perm::CATALOG.iter() {
                    {
                        let key_s = key.to_string();
                        let granted = draft_now.iter().any(|p| p == key);
                        let via_role = perm::role_holds(&role, key);
                        let is_ban_ip = *key == perm::BAN_IP;
                        let mut toggle = toggle;
                        rsx! {
                            li {
                                key: "{key}",
                                class: if is_ban_ip { "mod-grant mod-grant--danger" } else { "mod-grant" },
                                label { class: "mod-grant-main",
                                    input {
                                        r#type: "checkbox",
                                        checked: granted,
                                        disabled: is_admin_user,
                                        onchange: move |_| toggle(key_s.clone()),
                                    }
                                    div { class: "mod-grant-text",
                                        div { class: "mod-grant-name",
                                            span { "{label}" }
                                            if via_role {
                                                span { class: "mod-badge mod-badge--role", "Role default" }
                                            }
                                            if is_ban_ip {
                                                span { class: "mod-badge mod-badge--actioned", "High impact" }
                                            }
                                        }
                                        span { class: "mod-grant-desc", "{desc}" }
                                        span { class: "mod-grant-key mono", "{key}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "mod-grant-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    disabled: is_admin_user || !dirty,
                    loading: *busy.read(),
                    onclick: on_save,
                    "Save grants"
                }
                Button {
                    variant: ButtonVariant::Ghost,
                    disabled: !dirty,
                    onclick: on_reset,
                    "Reset"
                }
                if dirty {
                    span { class: "info-hint", "Unsaved changes" }
                }
            }
        }
    }
}
