//! `/admin/users` — admin user management ([PURA-237](/PURA/issues/PURA-237)).
//!
//! Implements three of the four ui-brief surfaces in one route, following
//! the modal-in-page pattern the `/servers` index already uses
//! (`ui::pages::servers_index`):
//!
//! - **User list** — dense table with role / status badges, last-login, and
//!   per-row actions (`docs/admin/ui-brief.md` §3.1).
//! - **Create / edit modal** — `docs/admin/ui-brief.md` §3.2 / §3.4. Username,
//!   display name, password (with a CSPRNG suggest button), role.
//! - **Sessions pane** — §3.3 Sessions tab, surfaced as a modal off the
//!   per-row "sessions" action.
//!
//! The audit-log viewer (§3.5) is a sibling issue and is out of scope here.
//!
//! ## Protections surfaced client-side
//!
//! The route layer is authoritative — it enforces self-action and
//! last-enabled-admin rules with a `400` (`docs/admin/architecture.md` §5.3).
//! This page surfaces the same rules *preemptively* so the operator never
//! arms a doomed action: the disable / delete affordances grey out with an
//! explanatory tooltip when the row is the operator themselves or the only
//! enabled admin, and the edit modal locks the role / enabled controls when
//! the operator is editing their own row.

use chrono::{DateTime, Utc};
use dioxus::prelude::*;
use ts6_manager_shared::admin::{AdminSession, AdminUser, AdminUserCreate, AdminUserPatch};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::store::AuthState;
use crate::client::users as users_client;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonType, ButtonVariant, Field, PasswordInput, RoleBadge,
    StatusBadge, TextInput, UserStatus,
};

// ── Page ────────────────────────────────────────────────────────────────

#[component]
pub fn AdminUsersPage() -> Element {
    let session = use_session();

    // AppShell already bounces anonymous sessions; render nothing for the
    // frame between the auth-state flip and the redirect effect firing.
    let snapshot = session.state.read().clone();
    let current_user = match &snapshot {
        AuthState::Anonymous => return rsx! { "" },
        AuthState::Authenticated { user, .. } => user.clone(),
    };

    // Defense-in-depth — the `/api/users` routes enforce `RequireAdmin`, but
    // a non-admin who deep-links `/admin/users` should land on a permission
    // surface rather than a table that 403s every row action (ui-brief §5).
    if !current_user.role.eq_ignore_ascii_case("admin") {
        return rsx! { InsufficientPermissions {} };
    }

    let gate = use_auth_gate();
    let toaster = use_toaster();

    let mut users: Signal<Vec<AdminUser>> = use_signal(Vec::new);
    let mut error: Signal<Option<ApiError>> = use_signal(|| None::<ApiError>);
    let mut loading: Signal<bool> = use_signal(|| true);
    let mut reload: Signal<u64> = use_signal(|| 0u64);
    let mut modal: Signal<Option<Modal>> = use_signal(|| None::<Modal>);

    let fetch = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { users_client::list(&gate, &api::api_base()).await }
        }
    });

    use_effect(move || match &*fetch.read_unchecked() {
        Some(Ok(list)) => {
            users.set(list.clone());
            error.set(None);
            loading.set(false);
        }
        Some(Err(e)) => {
            error.set(Some(e.clone()));
            loading.set(false);
        }
        None => loading.set(true),
    });

    let bump = move || reload.with_mut(|n| *n += 1);
    let current_id = current_user.id;

    // Power toggle: enabling is non-destructive (fire-and-confirm-by-toast);
    // disabling revokes every session, so it routes through a confirm modal.
    let on_row_action = {
        let gate = gate.clone();
        move |(action, user): (RowAction, AdminUser)| match action {
            RowAction::Edit => modal.set(Some(Modal::Edit(user))),
            RowAction::Delete => modal.set(Some(Modal::Delete(user))),
            RowAction::Disable => modal.set(Some(Modal::Disable(user))),
            RowAction::ResetPassword => modal.set(Some(Modal::ResetPassword(user))),
            RowAction::Sessions => modal.set(Some(Modal::Sessions(user))),
            RowAction::Enable => {
                let gate = gate.clone();
                let mut bump = bump;
                let toaster = toaster;
                spawn(async move {
                    let patch = AdminUserPatch {
                        enabled: Some(true),
                        ..Default::default()
                    };
                    match users_client::patch(&gate, &api::api_base(), user.id, &patch).await {
                        Ok(_) => {
                            toaster.push(
                                ToastVariant::Success,
                                format!("Enabled {}", user.display_name),
                                None,
                            );
                            bump();
                        }
                        Err(e) => toaster.push(
                            ToastVariant::Danger,
                            "Could not enable user",
                            Some(format_err(&e)),
                        ),
                    }
                });
            }
        }
    };

    let on_close = move |_: ()| {
        modal.set(None);
        reload.with_mut(|n| *n += 1);
    };

    let rows = users.read().clone();
    let err_snapshot = error.read().clone();
    let is_loading = *loading.read();

    rsx! {
        div { class: "crumb", "Admin" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Admin users" }
                p { class: "page-lede",
                    "Manage the operators who can sign in to this panel. Roles gate what each operator can see and change; the audit log records every mutation."
                }
            }
            div { class: "page-actions",
                Button {
                    variant: ButtonVariant::Primary,
                    onclick: move |_| modal.set(Some(Modal::Create)),
                    "+ Add admin user"
                }
            }
        }

        if let Some(err) = err_snapshot.as_ref() {
            Banner {
                variant: BannerVariant::Danger,
                title: "Could not load users".to_string(),
                "{format_err(err)}"
            }
        }

        section { class: "stack-md",
            if is_loading && rows.is_empty() {
                div { class: "card", aria_busy: "true",
                    span { class: "sr-only", role: "status", "aria-live": "polite",
                        "Loading admin users\u{2026}"
                    }
                    div { "aria-hidden": "true",
                        for _ in 0..3 {
                            div { class: "skeleton skeleton-line wide" }
                        }
                    }
                }
            } else if rows.is_empty() && err_snapshot.is_none() {
                UsersEmptyState {}
            } else if !rows.is_empty() {
                UsersTable {
                    rows: rows.clone(),
                    current_user_id: current_id,
                    on_action: EventHandler::new({
                        let on_row_action = on_row_action.clone();
                        move |payload| on_row_action.clone()(payload)
                    }),
                }
            }
        }

        { match modal.read().clone() {
            Some(Modal::Create) => rsx! {
                CreateUserModal { on_close: EventHandler::new(on_close) }
            },
            Some(Modal::Edit(user)) => {
                let is_self = user.id == current_id;
                rsx! {
                    EditUserModal {
                        user: user,
                        is_self: is_self,
                        on_close: EventHandler::new(on_close),
                    }
                }
            }
            Some(Modal::Disable(user)) => rsx! {
                DisableUserModal { user: user, on_close: EventHandler::new(on_close) }
            },
            Some(Modal::Delete(user)) => rsx! {
                DeleteUserModal { user: user, on_close: EventHandler::new(on_close) }
            },
            Some(Modal::ResetPassword(user)) => rsx! {
                ResetPasswordModal { user: user, on_close: EventHandler::new(on_close) }
            },
            Some(Modal::Sessions(user)) => rsx! {
                SessionsModal { user: user, on_close: EventHandler::new(on_close) }
            },
            None => rsx! { "" },
        } }
    }
}

#[component]
fn InsufficientPermissions() -> Element {
    rsx! {
        div { class: "crumb", "Admin" }
        div { class: "empty",
            div { class: "icon", "\u{1F512}" }
            h3 { "Insufficient permissions" }
            p {
                "Admin user management is restricted to operators with the "
                strong { "admin" }
                " role. Ask an admin to grant you access."
            }
        }
    }
}

#[component]
fn UsersEmptyState() -> Element {
    // ui-brief §3.1 — only reachable in DB-recovery mode; the route layer
    // refuses to delete the last admin, so a populated table is the norm.
    rsx! {
        Banner { variant: BannerVariant::Danger, title: "No admin users exist".to_string(),
            "Run setup to create the first admin, or restore a row directly via the database."
        }
    }
}

// ── Row actions / modal routing ─────────────────────────────────────────

/// Which per-row affordance was clicked. Kept separate from [`Modal`] so the
/// non-modal "enable" path (a direct PATCH) is expressible without a phantom
/// modal variant.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowAction {
    Edit,
    Disable,
    Enable,
    ResetPassword,
    Sessions,
    Delete,
}

/// The single modal that can be open at a time.
#[derive(Clone, PartialEq)]
enum Modal {
    Create,
    Edit(AdminUser),
    Disable(AdminUser),
    Delete(AdminUser),
    ResetPassword(AdminUser),
    Sessions(AdminUser),
}

// ── User table ──────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct UsersTableProps {
    rows: Vec<AdminUser>,
    /// Id of the signed-in operator — drives the self-action greying.
    current_user_id: i64,
    on_action: EventHandler<(RowAction, AdminUser)>,
}

#[component]
fn UsersTable(props: UsersTableProps) -> Element {
    // ui-brief §3.1 last-enabled-admin protection: a count of the enabled
    // admins lets each row decide whether disable / delete would strand the
    // panel with zero admins.
    let enabled_admin_count = props
        .rows
        .iter()
        .filter(|u| u.enabled && u.role.eq_ignore_ascii_case("admin"))
        .count();

    rsx! {
        table { class: "data-table", "aria-label": "Admin users",
            thead {
                tr {
                    th { scope: "col", "Username" }
                    th { scope: "col", "Display name" }
                    th { scope: "col", "Role" }
                    th { scope: "col", "Status" }
                    th { scope: "col", "Created" }
                    th { scope: "col", "Last login" }
                    th { scope: "col", "Sessions" }
                    th { scope: "col", class: "actions-col",
                        span { class: "sr-only", "Actions" }
                    }
                }
            }
            tbody {
                for user in props.rows.iter() {
                    {
                        let user = user.clone();
                        let is_self = user.id == props.current_user_id;
                        let is_last_admin = user.enabled
                            && user.role.eq_ignore_ascii_case("admin")
                            && enabled_admin_count <= 1;
                        let status = UserStatus::derive(user.enabled, user.last_login_at.is_some());
                        let on_action = props.on_action;
                        rsx! {
                            tr { key: "{user.id}",
                                td {
                                    code { class: "user-username", "{user.username}" }
                                }
                                td { "{user.display_name}" }
                                td { RoleBadge { role: user.role.clone() } }
                                td { StatusBadge { status: status } }
                                td { class: "user-ts", title: "{absolute(user.created_at)}",
                                    "{format_relative(Some(user.created_at))}"
                                }
                                td { class: "user-ts",
                                    title: "{user.last_login_at.map(absolute).unwrap_or_default()}",
                                    "{format_relative(user.last_login_at)}"
                                }
                                td { class: "num", "{user.active_session_count}" }
                                td { class: "row-actions",
                                    RowActionButtons {
                                        user: user.clone(),
                                        is_self: is_self,
                                        is_last_admin: is_last_admin,
                                        on_action: EventHandler::new({
                                            let user = user.clone();
                                            move |a: RowAction| on_action.call((a, user.clone()))
                                        }),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct RowActionButtonsProps {
    user: AdminUser,
    is_self: bool,
    is_last_admin: bool,
    on_action: EventHandler<RowAction>,
}

#[component]
fn RowActionButtons(props: RowActionButtonsProps) -> Element {
    let user = props.user.clone();
    let name = user.display_name.clone();
    let on_action = props.on_action;

    // Disable + delete are refused server-side for self-action and for the
    // last enabled admin; grey them out with the reason in the tooltip.
    let block_reason = if props.is_self {
        Some("You cannot disable or delete your own account.")
    } else if props.is_last_admin {
        Some("This is the only enabled admin — the panel must keep at least one.")
    } else {
        None
    };

    rsx! {
        button {
            class: "btn btn-ghost btn-sm",
            r#type: "button",
            "aria-label": "Edit user {name}",
            title: "Edit",
            onclick: move |_| on_action.call(RowAction::Edit),
            "\u{270E}"
        }
        if user.enabled {
            button {
                class: "btn btn-ghost btn-sm",
                r#type: "button",
                "aria-label": "Disable user {name}",
                title: block_reason.unwrap_or("Disable"),
                disabled: block_reason.is_some(),
                "aria-disabled": "{block_reason.is_some()}",
                onclick: move |_| on_action.call(RowAction::Disable),
                "\u{23FB}"
            }
        } else {
            button {
                class: "btn btn-ghost btn-sm",
                r#type: "button",
                "aria-label": "Enable user {name}",
                title: "Enable",
                onclick: move |_| on_action.call(RowAction::Enable),
                "\u{23FB}"
            }
        }
        button {
            class: "btn btn-ghost btn-sm",
            r#type: "button",
            "aria-label": "Reset password for {name}",
            title: "Reset password",
            onclick: move |_| on_action.call(RowAction::ResetPassword),
            "\u{1F511}"
        }
        button {
            class: "btn btn-ghost btn-sm",
            r#type: "button",
            "aria-label": "View sessions for {name}",
            title: "Sessions",
            onclick: move |_| on_action.call(RowAction::Sessions),
            "\u{1F5A5}"
        }
        button {
            class: "btn btn-ghost btn-sm row-action-danger",
            r#type: "button",
            "aria-label": "Delete user {name}",
            title: block_reason.unwrap_or("Delete"),
            disabled: block_reason.is_some(),
            "aria-disabled": "{block_reason.is_some()}",
            onclick: move |_| on_action.call(RowAction::Delete),
            "\u{1F5D1}"
        }
    }
}

// ── Create modal ────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct CreateUserModalProps {
    on_close: EventHandler<()>,
}

#[component]
fn CreateUserModal(props: CreateUserModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;

    let mut username = use_signal(String::new);
    let mut display_name = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut role = use_signal(|| String::from("viewer"));
    let mut reveal = use_signal(|| false);
    let mut submitting = use_signal(|| false);
    let mut username_err: Signal<Option<String>> = use_signal(|| None);
    let mut password_err: Signal<Option<String>> = use_signal(|| None);
    let mut banner: Signal<Option<String>> = use_signal(|| None);

    let suggest = move |_| {
        let pw = generate_password();
        copy_to_clipboard(&pw);
        password.set(pw);
        reveal.set(true);
        password_err.set(None);
    };

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let username_v = username.read().trim().to_string();
        let display_v = display_name.read().trim().to_string();
        let password_v = password.read().clone();
        username_err.set(None);
        password_err.set(None);
        banner.set(None);

        if let Some(msg) = username_violation(&username_v) {
            username_err.set(Some(msg.to_string()));
            return;
        }
        if display_v.is_empty() {
            banner.set(Some("Display name is required.".into()));
            return;
        }
        if let Some(msg) = password_violation(&password_v) {
            password_err.set(Some(msg.to_string()));
            return;
        }

        let body = AdminUserCreate {
            username: username_v.clone(),
            password: password_v,
            display_name: display_v,
            role: Some(role.read().clone()),
        };
        let gate = gate.clone();
        submitting.set(true);
        spawn(async move {
            let res = users_client::create(&gate, &api::api_base(), &body).await;
            submitting.set(false);
            match res {
                Ok(created) => {
                    toaster.push(
                        ToastVariant::Success,
                        format!("User \u{201c}{}\u{201d} created", created.username),
                        None,
                    );
                    on_close.call(());
                }
                Err(ApiError::Client { status: 409, .. }) => {
                    username_err.set(Some("A user with this username already exists.".into()));
                }
                Err(ApiError::Client {
                    status: 400,
                    message,
                }) => {
                    // The server re-validates §6.2.2; surface its verbatim
                    // message under the most likely offending field.
                    if message.to_lowercase().contains("password") {
                        password_err.set(Some(message));
                    } else if message.to_lowercase().contains("username") {
                        username_err.set(Some(message));
                    } else {
                        banner.set(Some(message));
                    }
                }
                Err(e) => banner.set(Some(format_err(&e))),
            }
        });
    };

    rsx! {
        ModalShell {
            title: "Add admin user".to_string(),
            labelled_by: "create-user-title".to_string(),
            on_close: move |_| on_close.call(()),
            on_submit: on_submit,
            body: rsx! {
                if let Some(msg) = banner.read().as_ref() {
                    Banner { variant: BannerVariant::Danger, title: "Could not create user".to_string(),
                        "{msg}"
                    }
                }
                Field {
                    label: "Username".to_string(),
                    id: "create-user-username".to_string(),
                    required: true,
                    helper: "Lowercase letters, numbers, and . _ - only. Cannot be changed later.".to_string(),
                    error: username_err.read().clone(),
                    TextInput {
                        id: "create-user-username".to_string(),
                        name: "username".to_string(),
                        value: username.read().clone(),
                        required: true,
                        disabled: *submitting.read(),
                        error: username_err.read().is_some(),
                        oninput: move |e: FormEvent| username.set(e.value()),
                        onchange: move |e: FormEvent| username.set(e.value()),
                    }
                }
                Field {
                    label: "Display name".to_string(),
                    id: "create-user-display".to_string(),
                    required: true,
                    TextInput {
                        id: "create-user-display".to_string(),
                        name: "displayName".to_string(),
                        value: display_name.read().clone(),
                        required: true,
                        disabled: *submitting.read(),
                        oninput: move |e: FormEvent| display_name.set(e.value()),
                        onchange: move |e: FormEvent| display_name.set(e.value()),
                    }
                }
                PasswordField {
                    id: "create-user-password".to_string(),
                    label: "Password".to_string(),
                    value: password.read().clone(),
                    reveal: *reveal.read(),
                    disabled: *submitting.read(),
                    error: password_err.read().clone(),
                    on_input: EventHandler::new(move |v: String| password.set(v)),
                    on_toggle_reveal: EventHandler::new(move |_| reveal.toggle()),
                    on_suggest: EventHandler::new(suggest),
                }
                RoleRadioGroup {
                    name: "create-role".to_string(),
                    selected: role.read().clone(),
                    disabled: *submitting.read(),
                    on_select: EventHandler::new(move |v: String| role.set(v)),
                }
            },
            footer: rsx! {
                Button {
                    variant: ButtonVariant::Secondary,
                    kind: ButtonType::Button,
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                Button {
                    variant: ButtonVariant::Primary,
                    kind: ButtonType::Submit,
                    loading: *submitting.read(),
                    disabled: *submitting.read(),
                    "Create user"
                }
            },
        }
    }
}

// ── Edit modal ──────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct EditUserModalProps {
    user: AdminUser,
    /// `true` when the operator is editing their own row — locks the role
    /// and enabled controls so a self-demote / self-disable is impossible
    /// (architecture §5.3).
    is_self: bool,
    on_close: EventHandler<()>,
}

#[component]
fn EditUserModal(props: EditUserModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let user = props.user.clone();
    let user_id = user.id;
    let is_self = props.is_self;
    let original_display = user.display_name.clone();
    let original_role = user.role.clone();
    let original_enabled = user.enabled;

    let mut display_name = use_signal(|| original_display.clone());
    let mut role = use_signal(|| original_role.clone());
    let mut enabled = use_signal(|| original_enabled);
    let mut password = use_signal(String::new);
    let mut reveal = use_signal(|| false);
    let mut submitting = use_signal(|| false);
    let mut password_err: Signal<Option<String>> = use_signal(|| None);
    let mut banner: Signal<Option<String>> = use_signal(|| None);

    let suggest = move |_| {
        let pw = generate_password();
        copy_to_clipboard(&pw);
        password.set(pw);
        reveal.set(true);
        password_err.set(None);
    };

    let username_label = user.username.clone();
    // `original_display` is moved into `on_submit` (dirty-tracking compares
    // against it); keep a separate copy for the modal title.
    let title_display = original_display.clone();

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let display_v = display_name.read().trim().to_string();
        let role_v = role.read().clone();
        let enabled_v = *enabled.read();
        let password_v = password.read().clone();
        password_err.set(None);
        banner.set(None);

        if display_v.is_empty() {
            banner.set(Some("Display name is required.".into()));
            return;
        }
        if !password_v.is_empty()
            && let Some(msg) = password_violation(&password_v)
        {
            password_err.set(Some(msg.to_string()));
            return;
        }

        // Dirty-tracking — PATCH only the fields the operator changed
        // (http-api.md §3.2). An all-`None` patch is rejected with 400, so
        // a no-op submit is short-circuited here.
        let mut patch = AdminUserPatch::default();
        if display_v != original_display {
            patch.display_name = Some(display_v);
        }
        if role_v != original_role {
            patch.role = Some(role_v);
        }
        if enabled_v != original_enabled {
            patch.enabled = Some(enabled_v);
        }
        if !password_v.is_empty() {
            patch.password = Some(password_v);
        }
        if patch.display_name.is_none()
            && patch.role.is_none()
            && patch.enabled.is_none()
            && patch.password.is_none()
        {
            banner.set(Some("No changes to save.".into()));
            return;
        }

        let gate = gate.clone();
        submitting.set(true);
        spawn(async move {
            let res = users_client::patch(&gate, &api::api_base(), user_id, &patch).await;
            submitting.set(false);
            match res {
                Ok(updated) => {
                    toaster.push(
                        ToastVariant::Success,
                        format!("Saved changes to {}", updated.display_name),
                        None,
                    );
                    on_close.call(());
                }
                Err(ApiError::Client {
                    status: 400,
                    message,
                }) => {
                    if message.to_lowercase().contains("password") {
                        password_err.set(Some(message));
                    } else {
                        banner.set(Some(message));
                    }
                }
                Err(e) => banner.set(Some(format_err(&e))),
            }
        });
    };

    rsx! {
        ModalShell {
            title: format!("Edit {title_display}"),
            labelled_by: "edit-user-title".to_string(),
            on_close: move |_| on_close.call(()),
            on_submit: on_submit,
            body: rsx! {
                if let Some(msg) = banner.read().as_ref() {
                    Banner { variant: BannerVariant::Danger, title: "Could not save".to_string(),
                        "{msg}"
                    }
                }
                if is_self {
                    Banner { variant: BannerVariant::Info, title: "Editing your own account".to_string(),
                        "Role and sign-in controls are locked — ask another admin to change your own role or disable your account."
                    }
                }
                Field {
                    label: "Username".to_string(),
                    id: "edit-user-username".to_string(),
                    helper: "Usernames cannot be changed after creation.".to_string(),
                    TextInput {
                        id: "edit-user-username".to_string(),
                        name: "username".to_string(),
                        value: username_label.clone(),
                        readonly: true,
                        disabled: true,
                    }
                }
                Field {
                    label: "Display name".to_string(),
                    id: "edit-user-display".to_string(),
                    required: true,
                    TextInput {
                        id: "edit-user-display".to_string(),
                        name: "displayName".to_string(),
                        value: display_name.read().clone(),
                        required: true,
                        disabled: *submitting.read(),
                        oninput: move |e: FormEvent| display_name.set(e.value()),
                        onchange: move |e: FormEvent| display_name.set(e.value()),
                    }
                }
                PasswordField {
                    id: "edit-user-password".to_string(),
                    label: "Reset password".to_string(),
                    helper: Some("Leave blank to keep the current password. Setting a new one signs the user out of every session.".to_string()),
                    value: password.read().clone(),
                    reveal: *reveal.read(),
                    disabled: *submitting.read(),
                    error: password_err.read().clone(),
                    on_input: EventHandler::new(move |v: String| password.set(v)),
                    on_toggle_reveal: EventHandler::new(move |_| reveal.toggle()),
                    on_suggest: EventHandler::new(suggest),
                }
                RoleRadioGroup {
                    name: "edit-role".to_string(),
                    selected: role.read().clone(),
                    disabled: *submitting.read() || is_self,
                    on_select: EventHandler::new(move |v: String| role.set(v)),
                }
                label { class: "toggle-row",
                    input {
                        r#type: "checkbox",
                        checked: *enabled.read(),
                        disabled: *submitting.read() || is_self,
                        onchange: move |e: FormEvent| enabled.set(e.checked()),
                    }
                    span { class: "toggle-label", "Sign-in enabled" }
                }
            },
            footer: rsx! {
                Button {
                    variant: ButtonVariant::Secondary,
                    kind: ButtonType::Button,
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                Button {
                    variant: ButtonVariant::Primary,
                    kind: ButtonType::Submit,
                    loading: *submitting.read(),
                    disabled: *submitting.read(),
                    "Save changes"
                }
            },
        }
    }
}

// ── Disable / Delete confirm modals ─────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct ConfirmModalProps {
    user: AdminUser,
    on_close: EventHandler<()>,
}

/// Which destructive operation a [`DestructiveConfirm`] applies. Kept as a
/// plain enum (not an `AdminUserPatch` prop) so the props struct can derive
/// `PartialEq` without the shared wire type having to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConfirmKind {
    Disable,
    Delete,
}

#[component]
fn DisableUserModal(props: ConfirmModalProps) -> Element {
    let user = props.user.clone();
    let display = user.display_name.clone();
    let body = rsx! {
        "Disabling "
        strong { "{display}" }
        " will sign them out of all sessions immediately. They keep their account and can be re-enabled later."
    };
    rsx! {
        DestructiveConfirm {
            user: user.clone(),
            title: "Disable user".to_string(),
            labelled_by: "disable-user-title".to_string(),
            confirm_label: "Disable and revoke sessions".to_string(),
            success_toast: format!("Disabled {display}"),
            kind: ConfirmKind::Disable,
            body: body,
            on_close: props.on_close,
        }
    }
}

#[component]
fn DeleteUserModal(props: ConfirmModalProps) -> Element {
    let user = props.user.clone();
    let display = user.display_name.clone();
    let body = rsx! {
        "Deleting "
        strong { "{display}" }
        " is permanent. Their sessions and per-server grants will be removed. Audit log entries remain."
    };
    rsx! {
        DestructiveConfirm {
            user: user.clone(),
            title: "Delete user".to_string(),
            labelled_by: "delete-user-title".to_string(),
            confirm_label: "Delete user permanently".to_string(),
            success_toast: format!("Deleted {display}"),
            kind: ConfirmKind::Delete,
            body: body,
            on_close: props.on_close,
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct DestructiveConfirmProps {
    user: AdminUser,
    title: String,
    labelled_by: String,
    confirm_label: String,
    success_toast: String,
    kind: ConfirmKind,
    body: Element,
    on_close: EventHandler<()>,
}

#[component]
fn DestructiveConfirm(props: DestructiveConfirmProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let user_id = props.user.id;
    let kind = props.kind;
    let success_toast = props.success_toast.clone();

    let mut submitting = use_signal(|| false);
    let mut banner: Signal<Option<String>> = use_signal(|| None);

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() {
            return;
        }
        let gate = gate.clone();
        let success_toast = success_toast.clone();
        submitting.set(true);
        banner.set(None);
        spawn(async move {
            let base = api::api_base();
            let res = match kind {
                ConfirmKind::Delete => users_client::delete(&gate, &base, user_id).await,
                ConfirmKind::Disable => {
                    let patch = AdminUserPatch {
                        enabled: Some(false),
                        ..Default::default()
                    };
                    users_client::patch(&gate, &base, user_id, &patch)
                        .await
                        .map(|_| ())
                }
            };
            submitting.set(false);
            match res {
                Ok(()) => {
                    toaster.push(ToastVariant::Success, success_toast, None);
                    on_close.call(());
                }
                // The server enforces self-action / last-admin rules; if one
                // fires, surface the verbatim 400 in the modal body rather
                // than letting the operator retry a doomed action.
                Err(ApiError::Client {
                    status: 400,
                    message,
                }) => banner.set(Some(message)),
                Err(e) => banner.set(Some(format_err(&e))),
            }
        });
    };

    rsx! {
        ModalShell {
            title: props.title.clone(),
            labelled_by: props.labelled_by.clone(),
            on_close: move |_| on_close.call(()),
            on_submit: on_submit,
            body: rsx! {
                if let Some(msg) = banner.read().as_ref() {
                    Banner { variant: BannerVariant::Danger, title: "Action refused".to_string(),
                        "{msg}"
                    }
                }
                p { {props.body.clone()} }
            },
            footer: rsx! {
                Button {
                    variant: ButtonVariant::Secondary,
                    kind: ButtonType::Button,
                    onclick: move |_| on_close.call(()),
                    "Cancel"
                }
                Button {
                    variant: ButtonVariant::Danger,
                    kind: ButtonType::Submit,
                    loading: *submitting.read(),
                    disabled: *submitting.read(),
                    "{props.confirm_label}"
                }
            },
        }
    }
}

// ── Reset-password modal ────────────────────────────────────────────────

#[component]
fn ResetPasswordModal(props: ConfirmModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let user = props.user.clone();
    let user_id = user.id;
    let display = user.display_name.clone();

    // The generated password is created up-front so the operator can copy it
    // before confirming; `done` flips after the PATCH lands so the modal can
    // keep the password visible for one last copy.
    let mut new_password = use_signal(generate_password);
    let mut reveal = use_signal(|| true);
    let mut submitting = use_signal(|| false);
    let mut done = use_signal(|| false);
    let mut banner: Signal<Option<String>> = use_signal(|| None);

    let regenerate = move |_| {
        let pw = generate_password();
        new_password.set(pw);
    };
    let copy = move |_| copy_to_clipboard(&new_password.read());

    let on_submit = move |evt: FormEvent| {
        evt.prevent_default();
        if *submitting.read() || *done.read() {
            return;
        }
        let gate = gate.clone();
        let display = display.clone();
        let patch = AdminUserPatch {
            password: Some(new_password.read().clone()),
            ..Default::default()
        };
        submitting.set(true);
        banner.set(None);
        spawn(async move {
            let res = users_client::patch(&gate, &api::api_base(), user_id, &patch).await;
            submitting.set(false);
            match res {
                Ok(_) => {
                    toaster.push(
                        ToastVariant::Success,
                        format!("Password reset for {display}"),
                        None,
                    );
                    done.set(true);
                }
                Err(ApiError::Client {
                    status: 400,
                    message,
                }) => banner.set(Some(message)),
                Err(e) => banner.set(Some(format_err(&e))),
            }
        });
    };

    let is_done = *done.read();
    let display_name = user.display_name.clone();

    rsx! {
        ModalShell {
            title: "Reset password".to_string(),
            labelled_by: "reset-password-title".to_string(),
            on_close: move |_| on_close.call(()),
            on_submit: on_submit,
            body: rsx! {
                if let Some(msg) = banner.read().as_ref() {
                    Banner { variant: BannerVariant::Danger, title: "Could not reset password".to_string(),
                        "{msg}"
                    }
                }
                if is_done {
                    Banner { variant: BannerVariant::Success, title: "Password reset".to_string(),
                        "{display_name} has been signed out of every session. Share the new password below through a secure channel."
                    }
                } else {
                    p {
                        "Resetting "
                        strong { "{display_name}" }
                        "'s password will sign them out of all sessions and require you to share the new password."
                    }
                }
                Field {
                    label: "New password".to_string(),
                    id: "reset-password-value".to_string(),
                    helper: "Generated in your browser. The operator will not be able to retrieve it later — only reset it again.".to_string(),
                    div { class: "password-row",
                        input {
                            class: "input",
                            id: "reset-password-value",
                            r#type: if *reveal.read() { "text" } else { "password" },
                            value: "{new_password.read()}",
                            readonly: true,
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            r#type: "button",
                            "aria-label": if *reveal.read() { "Hide password" } else { "Show password" },
                            onclick: move |_| reveal.toggle(),
                            if *reveal.read() { "Hide" } else { "Show" }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            r#type: "button",
                            "aria-label": "Copy password to clipboard",
                            onclick: copy,
                            "Copy"
                        }
                        if !is_done {
                            button {
                                class: "btn btn-ghost btn-sm",
                                r#type: "button",
                                "aria-label": "Generate a different password",
                                onclick: regenerate,
                                "Regenerate"
                            }
                        }
                    }
                }
            },
            footer: rsx! {
                if is_done {
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Button,
                        onclick: move |_| on_close.call(()),
                        "Done"
                    }
                } else {
                    Button {
                        variant: ButtonVariant::Secondary,
                        kind: ButtonType::Button,
                        onclick: move |_| on_close.call(()),
                        "Cancel"
                    }
                    Button {
                        variant: ButtonVariant::Danger,
                        kind: ButtonType::Submit,
                        loading: *submitting.read(),
                        disabled: *submitting.read(),
                        "Reset password and revoke sessions"
                    }
                }
            },
        }
    }
}

// ── Sessions modal ──────────────────────────────────────────────────────

#[component]
fn SessionsModal(props: ConfirmModalProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();
    let on_close = props.on_close;
    let user = props.user.clone();
    let user_id = user.id;
    let display = user.display_name.clone();

    let reload = use_signal(|| 0u64);
    let mut confirming: Signal<Option<i64>> = use_signal(|| None);

    let fetch = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            let _ = *reload.read();
            async move { users_client::list_sessions(&gate, &api::api_base(), user_id).await }
        }
    });

    let revoke = {
        let gate = gate.clone();
        move |sid: i64| {
            let gate = gate.clone();
            let mut reload = reload;
            let mut confirming = confirming;
            spawn(async move {
                match users_client::revoke_session(&gate, &api::api_base(), user_id, sid).await {
                    Ok(()) => {
                        toaster.push(ToastVariant::Success, "Session revoked", None);
                        confirming.set(None);
                        reload.with_mut(|n| *n += 1);
                    }
                    Err(e) => toaster.push(
                        ToastVariant::Danger,
                        "Could not revoke session",
                        Some(format_err(&e)),
                    ),
                }
            });
        }
    };

    let snapshot = fetch.read_unchecked().clone();

    rsx! {
        ModalShell {
            title: format!("Sessions \u{2014} {display}"),
            labelled_by: "sessions-title".to_string(),
            on_close: move |_| on_close.call(()),
            on_submit: move |evt: FormEvent| evt.prevent_default(),
            body: rsx! {
                { match snapshot {
                    None => rsx! {
                        p { class: "muted", role: "status", "aria-live": "polite",
                            "Loading sessions\u{2026}"
                        }
                    },
                    Some(Err(e)) => rsx! {
                        Banner { variant: BannerVariant::Danger, title: "Could not load sessions".to_string(),
                            "{format_err(&e)}"
                        }
                    },
                    Some(Ok(sessions)) if sessions.is_empty() => rsx! {
                        p { class: "muted", "This user has no recorded sessions." }
                    },
                    Some(Ok(sessions)) => rsx! {
                        table { class: "data-table", "aria-label": "Refresh-token sessions",
                            thead {
                                tr {
                                    th { scope: "col", "Created" }
                                    th { scope: "col", "Expires" }
                                    th { scope: "col", "Family" }
                                    th { scope: "col", "State" }
                                    th { scope: "col", class: "actions-col",
                                        span { class: "sr-only", "Actions" }
                                    }
                                }
                            }
                            tbody {
                                for s in sessions.iter() {
                                    {
                                        let s = s.clone();
                                        let state = session_state(&s);
                                        let sid = s.id;
                                        let is_confirming = *confirming.read() == Some(sid);
                                        let revoke = revoke.clone();
                                        rsx! {
                                            tr { key: "{sid}",
                                                td { class: "user-ts", title: "{absolute(s.created_at)}",
                                                    "{format_relative(Some(s.created_at))}"
                                                }
                                                td { class: "user-ts", title: "{absolute(s.expires_at)}",
                                                    "{format_relative(Some(s.expires_at))}"
                                                }
                                                td {
                                                    code { class: "user-username", "{family_short(&s.family)}" }
                                                }
                                                td {
                                                    span { class: state.1, "{state.0}" }
                                                }
                                                td { class: "row-actions",
                                                    if state.0 == "Active" {
                                                        if is_confirming {
                                                            button {
                                                                class: "btn btn-danger btn-sm",
                                                                r#type: "button",
                                                                "aria-label": "Confirm revoke session {sid}",
                                                                onclick: move |_| revoke.clone()(sid),
                                                                "Confirm revoke"
                                                            }
                                                        } else {
                                                            button {
                                                                class: "btn btn-ghost btn-sm row-action-danger",
                                                                r#type: "button",
                                                                "aria-label": "Revoke session {sid}",
                                                                title: "Revoke this session and every session in its family",
                                                                onclick: move |_| confirming.set(Some(sid)),
                                                                "Revoke"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    },
                } }
            },
            footer: rsx! {
                Button {
                    variant: ButtonVariant::Secondary,
                    kind: ButtonType::Button,
                    onclick: move |_| on_close.call(()),
                    "Close"
                }
            },
        }
    }
}

// ── Shared modal chrome ─────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct ModalShellProps {
    title: String,
    labelled_by: String,
    on_close: EventHandler<MouseEvent>,
    on_submit: EventHandler<FormEvent>,
    body: Element,
    footer: Element,
}

/// Backdrop + dialog frame shared by every admin modal. Mirrors the
/// `modal-backdrop` / `modal` markup `servers_index` and `music_bots` use so
/// the admin surface inherits the panel's existing focus-trap CSS.
#[component]
fn ModalShell(props: ModalShellProps) -> Element {
    let on_close = props.on_close;
    let on_submit = props.on_submit;
    rsx! {
        div { class: "modal-backdrop", onclick: move |evt| on_close.call(evt),
            form {
                class: "modal",
                onclick: move |evt| evt.stop_propagation(),
                onsubmit: move |evt| on_submit.call(evt),
                role: "dialog",
                "aria-modal": "true",
                "aria-labelledby": "{props.labelled_by}",
                div { class: "modal-header",
                    h2 { id: "{props.labelled_by}", "{props.title}" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        r#type: "button",
                        "aria-label": "Close",
                        onclick: move |evt| on_close.call(evt),
                        "\u{2715}"
                    }
                }
                div { class: "modal-body stack-md", {props.body.clone()} }
                div { class: "modal-footer", {props.footer.clone()} }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct PasswordFieldProps {
    id: String,
    label: String,
    #[props(default)]
    helper: Option<String>,
    value: String,
    reveal: bool,
    disabled: bool,
    error: Option<String>,
    on_input: EventHandler<String>,
    on_toggle_reveal: EventHandler<()>,
    on_suggest: EventHandler<()>,
}

/// Password entry + "suggest a strong password" + reveal toggle, with the
/// secure-channel warning copy from ui-brief §3.2.
#[component]
fn PasswordField(props: PasswordFieldProps) -> Element {
    let on_input = props.on_input;
    let on_toggle = props.on_toggle_reveal;
    let on_suggest = props.on_suggest;
    let helper = props
        .helper
        .clone()
        .unwrap_or_else(|| "Use the suggestion button for a strong random password.".to_string());
    rsx! {
        Field {
            label: props.label.clone(),
            id: props.id.clone(),
            required: true,
            helper: helper,
            error: props.error.clone(),
            div { class: "password-row",
                PasswordInput {
                    id: props.id.clone(),
                    name: "password".to_string(),
                    autocomplete: "new-password".to_string(),
                    value: props.value.clone(),
                    reveal: props.reveal,
                    disabled: props.disabled,
                    error: props.error.is_some(),
                    oninput: move |e: FormEvent| on_input.call(e.value()),
                    onchange: move |e: FormEvent| on_input.call(e.value()),
                }
                button {
                    class: "btn btn-ghost btn-sm",
                    r#type: "button",
                    "aria-label": if props.reveal { "Hide password" } else { "Show password" },
                    disabled: props.disabled,
                    onclick: move |_| on_toggle.call(()),
                    if props.reveal { "Hide" } else { "Show" }
                }
                button {
                    class: "btn btn-secondary btn-sm",
                    r#type: "button",
                    disabled: props.disabled,
                    onclick: move |_| on_suggest.call(()),
                    "Suggest"
                }
            }
        }
        p { class: "field-help password-warning", role: "note",
            "Share this password through a secure channel. The operator will not be able to retrieve it later — only reset it."
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct RoleRadioGroupProps {
    name: String,
    selected: String,
    disabled: bool,
    on_select: EventHandler<String>,
}

/// Three-role radio group with the one-line capability summary each option
/// carries (ui-brief §3.2; copy from architecture §3.2).
#[component]
fn RoleRadioGroup(props: RoleRadioGroupProps) -> Element {
    let options = [
        ("viewer", "Viewer", "Read-only on granted servers."),
        (
            "moderator",
            "Moderator",
            "Read/write on granted servers; cannot manage users.",
        ),
        (
            "admin",
            "Admin",
            "Full read/write across the panel and audit log.",
        ),
    ];
    rsx! {
        fieldset { class: "role-fieldset",
            legend { class: "field-label", "Role" }
            for (value, label, summary) in options {
                {
                    let value = value.to_string();
                    let checked = props.selected.eq_ignore_ascii_case(&value);
                    let on_select = props.on_select;
                    let group = props.name.clone();
                    let radio_id = format!("{}-{}", props.name, value);
                    rsx! {
                        label { class: "role-option", r#for: "{radio_id}",
                            input {
                                id: "{radio_id}",
                                r#type: "radio",
                                name: "{group}",
                                value: "{value}",
                                checked: checked,
                                disabled: props.disabled,
                                onchange: move |_| on_select.call(value.clone()),
                            }
                            span { class: "role-option-body",
                                span { class: "role-option-label", "{label}" }
                                span { class: "role-option-summary", "{summary}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────

/// Spec §6.2.2 complexity check, mirrored client-side for fast feedback.
/// Returns the first violation's verbatim message (the server re-validates
/// and is authoritative — `auth::complexity` owns the canonical strings).
fn password_violation(pw: &str) -> Option<&'static str> {
    const SPECIAL: &str = "!@#$%^&*()_+-=[]{}|;':\",./<>?";
    if pw.chars().count() < 8 {
        return Some("Password must be at least 8 characters long");
    }
    if !pw.chars().any(|c| c.is_ascii_uppercase()) {
        return Some("Password must contain at least one uppercase letter");
    }
    if !pw.chars().any(|c| c.is_ascii_lowercase()) {
        return Some("Password must contain at least one lowercase letter");
    }
    if !pw.chars().any(|c| c.is_ascii_digit()) {
        return Some("Password must contain at least one digit");
    }
    if !pw.chars().any(|c| SPECIAL.contains(c)) {
        return Some("Password must contain at least one special character");
    }
    None
}

/// `[a-z0-9._-]+`, 1..=64 — ui-brief §3.2 username rules.
fn username_violation(username: &str) -> Option<&'static str> {
    if username.is_empty() {
        return Some("Username is required.");
    }
    if username.chars().count() > 64 {
        return Some("Maximum 64 characters.");
    }
    let ok = username
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    if !ok {
        return Some("Only lowercase letters, numbers, `.`, `_`, `-`.");
    }
    None
}

/// Visually-unambiguous character classes — no `0/O`, `1/l/I` — so an
/// operator copying a generated password by eye does not transcribe it
/// wrong. One class is force-seeded so the result always satisfies §6.2.2.
const PW_LOWER: &[u8] = b"abcdefghijkmnpqrstuvwxyz";
const PW_UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
const PW_DIGIT: &[u8] = b"23456789";
const PW_SPECIAL: &[u8] = b"!@#$%^&*-_=+";
const PW_LEN: usize = 16;

/// Generate a 16-char password guaranteed to pass spec §6.2.2 (one of each
/// required class, then a random fill).
fn generate_password() -> String {
    let bytes = random_bytes(PW_LEN + 1);
    let mut chars: Vec<char> = Vec::with_capacity(PW_LEN);
    chars.push(PW_UPPER[bytes[0] as usize % PW_UPPER.len()] as char);
    chars.push(PW_LOWER[bytes[1] as usize % PW_LOWER.len()] as char);
    chars.push(PW_DIGIT[bytes[2] as usize % PW_DIGIT.len()] as char);
    chars.push(PW_SPECIAL[bytes[3] as usize % PW_SPECIAL.len()] as char);
    let pool: Vec<u8> = [PW_LOWER, PW_UPPER, PW_DIGIT, PW_SPECIAL].concat();
    for b in bytes.iter().take(PW_LEN).skip(4) {
        chars.push(pool[*b as usize % pool.len()] as char);
    }
    // Rotate so the force-seeded classes are not always in fixed positions.
    let shift = (bytes[PW_LEN] as usize) % chars.len();
    chars.rotate_left(shift);
    chars.into_iter().collect()
}

/// CSPRNG bytes from `crypto.getRandomValues` (ui-brief §3.2). The non-WASM
/// fallback is for SSR / unit tests only — the server re-hashes whatever it
/// receives and an SSR-rendered password is never shown to a human.
fn random_bytes(n: usize) -> Vec<u8> {
    #[cfg(target_arch = "wasm32")]
    {
        let mut buf = vec![0u8; n];
        if let Some(window) = web_sys::window()
            && let Ok(crypto) = window.crypto()
            && crypto.get_random_values_with_u8_array(&mut buf).is_ok()
        {
            return buf;
        }
        buf
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use std::time::{SystemTime, UNIX_EPOCH};
        let mut seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15)
            | 1;
        (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed & 0xff) as u8
            })
            .collect()
    }
}

/// Copy `text` to the system clipboard. No-op (and harmless) when the
/// Clipboard API is unavailable or off-WASM.
fn copy_to_clipboard(text: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = text;
    }
}

/// Derive the session badge — (label, css class). Rotated tokens (with a
/// `replacedBy`) are spent; an expired-but-not-rotated token is forensic.
fn session_state(s: &AdminSession) -> (&'static str, &'static str) {
    if s.replaced_by.is_some() {
        ("Rotated", "tag tag-neutral")
    } else if s.expires_at <= Utc::now() {
        ("Expired", "tag tag-warning")
    } else {
        ("Active", "tag tag-success")
    }
}

/// First 8 chars of the session family id; the operator only needs enough to
/// tell two login chains apart (http-api.md §2.4).
fn family_short(family: &Option<String>) -> String {
    match family {
        Some(f) if f.chars().count() > 8 => {
            format!("{}\u{2026}", f.chars().take(8).collect::<String>())
        }
        Some(f) => f.clone(),
        None => "\u{2014}".to_string(),
    }
}

/// Relative "x ago" / "in x" rendering. `None` → "Never" so a never-logged-in
/// user reads cleanly instead of showing the epoch.
fn format_relative(ts: Option<DateTime<Utc>>) -> String {
    let Some(ts) = ts else {
        return "Never".to_string();
    };
    let now = Utc::now();
    let delta = now.signed_duration_since(ts);
    let secs = delta.num_seconds();
    let future = secs < 0;
    let secs = secs.abs();
    let phrase = if secs < 45 {
        // Negative deltas (operator clock skew) collapse here too.
        return "just now".to_string();
    } else if secs < 90 {
        "1 min".to_string()
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else if secs < 86_400 {
        format!("{} h", secs / 3600)
    } else if secs < 7 * 86_400 {
        format!("{} d", secs / 86_400)
    } else {
        return ts.format("%Y-%m-%d").to_string();
    };
    if future {
        format!("in {phrase}")
    } else {
        format!("{phrase} ago")
    }
}

/// Absolute timestamp for the `title` hover (ui-brief §3.1 "absolute on hover").
fn absolute(ts: DateTime<Utc>) -> String {
    ts.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// One-line operator-facing error string for a failed admin API call.
fn format_err(err: &ApiError) -> String {
    match err {
        ApiError::Unauthorized(_) => "Session expired — sign in again to retry.".into(),
        ApiError::SessionAnonymous => "Session not ready yet — retry in a moment.".into(),
        ApiError::Client { status: 403, .. } => "You don't have permission to manage users.".into(),
        ApiError::Client { status: 404, .. } => {
            "That user no longer exists. Refresh the page.".into()
        }
        ApiError::Client { status, message } | ApiError::Server { status, message } => {
            format!("{status}: {message}")
        }
        ApiError::BadGateway { error, .. } => format!("Upstream error: {error}"),
        ApiError::Transport(m) => format!("Network error: {m}"),
        ApiError::Deserialise(m) => format!("Unexpected response: {m}"),
        ApiError::UnsupportedTarget => "Unsupported on this build target.".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_password_satisfies_complexity() {
        for _ in 0..200 {
            let pw = generate_password();
            assert_eq!(pw.chars().count(), PW_LEN, "length must be {PW_LEN}: {pw}");
            assert!(
                password_violation(&pw).is_none(),
                "generated password failed §6.2.2: {pw} -> {:?}",
                password_violation(&pw)
            );
        }
    }

    #[test]
    fn password_violation_matches_spec_strings() {
        assert_eq!(
            password_violation("aA1!"),
            Some("Password must be at least 8 characters long")
        );
        assert_eq!(
            password_violation("alllower1!"),
            Some("Password must contain at least one uppercase letter")
        );
        assert_eq!(
            password_violation("ALLUPPER1!"),
            Some("Password must contain at least one lowercase letter")
        );
        assert_eq!(
            password_violation("NoDigits!!"),
            Some("Password must contain at least one digit")
        );
        assert_eq!(
            password_violation("NoSpecial1"),
            Some("Password must contain at least one special character")
        );
        assert_eq!(password_violation("Hunter2!ok"), None);
    }

    #[test]
    fn username_violation_enforces_charset_and_length() {
        assert_eq!(username_violation(""), Some("Username is required."));
        assert_eq!(
            username_violation("Has Spaces"),
            Some("Only lowercase letters, numbers, `.`, `_`, `-`.")
        );
        assert_eq!(
            username_violation("UPPER"),
            Some("Only lowercase letters, numbers, `.`, `_`, `-`.")
        );
        assert_eq!(
            username_violation(&"a".repeat(65)),
            Some("Maximum 64 characters.")
        );
        assert_eq!(username_violation("admin.2_user-3"), None);
    }

    #[test]
    fn format_relative_handles_never_and_buckets() {
        assert_eq!(format_relative(None), "Never");
        let now = Utc::now();
        assert_eq!(
            format_relative(Some(now - chrono::Duration::seconds(10))),
            "just now"
        );
        assert_eq!(
            format_relative(Some(now - chrono::Duration::minutes(5))),
            "5 min ago"
        );
        assert_eq!(
            format_relative(Some(now - chrono::Duration::hours(3))),
            "3 h ago"
        );
        assert_eq!(
            format_relative(Some(now - chrono::Duration::days(2))),
            "2 d ago"
        );
    }

    #[test]
    fn family_short_truncates_long_ids() {
        assert_eq!(
            family_short(&Some("0123456789abcdef".to_string())),
            "01234567\u{2026}"
        );
        assert_eq!(family_short(&Some("short".to_string())), "short");
        assert_eq!(family_short(&None), "\u{2014}");
    }

    // ── SSR markup contract ────────────────────────────────────────────

    use std::sync::Arc;

    use crate::client::dioxus::{DioxusSession, provide_auth_gate};
    use crate::client::storage::MemoryStore;
    use crate::ui::components::provide_toaster;
    use ts6_manager_shared::auth::UserInfo;

    fn fixture(id: i64, username: &str, role: &str, enabled: bool, logged_in: bool) -> AdminUser {
        let now = Utc::now();
        AdminUser {
            id,
            username: username.into(),
            display_name: format!("{username} display"),
            role: role.into(),
            enabled,
            created_at: now,
            updated_at: now,
            last_login_at: logged_in.then_some(now),
            active_session_count: if logged_in { 1 } else { 0 },
        }
    }

    fn render(root: fn() -> Element) -> String {
        let mut dom = VirtualDom::new(root);
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    fn single_admin_table() -> Element {
        rsx! {
            table {
                UsersTable {
                    rows: vec![fixture(1, "alice", "admin", true, true)],
                    current_user_id: 99,
                    on_action: EventHandler::new(|_| {}),
                }
            }
        }
    }

    fn two_admin_table() -> Element {
        rsx! {
            table {
                UsersTable {
                    rows: vec![
                        fixture(1, "alice", "admin", true, true),
                        fixture(2, "bob", "admin", true, false),
                        fixture(3, "carol", "viewer", true, true),
                    ],
                    current_user_id: 99,
                    on_action: EventHandler::new(|_| {}),
                }
            }
        }
    }

    #[test]
    fn users_table_renders_columns_and_role_badges() {
        let html = render(two_admin_table);
        for header in ["Username", "Role", "Status", "Last login", "Sessions"] {
            assert!(html.contains(header), "missing `{header}` header: {html}");
        }
        assert!(html.contains("alice"), "row username missing: {html}");
        // Role + status badges paired with their text label (ui-brief §4).
        assert!(html.contains("Admin"), "role badge label missing: {html}");
        assert!(html.contains("Viewer"), "viewer role badge missing: {html}");
        assert!(
            html.contains("Never signed in"),
            "never-signed-in status badge missing: {html}"
        );
    }

    /// ui-brief §3.1 — the only enabled admin cannot be disabled or deleted;
    /// the affordances grey out preemptively with the reason in the tooltip.
    #[test]
    fn last_enabled_admin_disable_and_delete_are_greyed() {
        let html = render(single_admin_table);
        assert!(
            html.contains("only enabled admin"),
            "last-admin tooltip must explain why the action is blocked: {html}"
        );
        let disabled_count = html.matches(r#"aria-disabled="true""#).count();
        assert_eq!(
            disabled_count, 2,
            "exactly the disable + delete affordances should be greyed: {html}"
        );
    }

    /// With two enabled admins, neither row is the last one — disable and
    /// delete stay live.
    #[test]
    fn non_last_admin_actions_are_not_greyed() {
        let html = render(two_admin_table);
        assert!(
            !html.contains(r#"aria-disabled="true""#),
            "no row-action should be greyed when more than one admin exists: {html}"
        );
    }

    #[component]
    fn AdminHarness() -> Element {
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 1,
                    username: "admin".into(),
                    display_name: "Admin".into(),
                    role: "admin".into(),
                },
            }),
            storage: Arc::new(MemoryStore::new()),
        });
        use_context_provider(|| provide_auth_gate(session));
        let _ = provide_toaster();
        rsx! { AdminUsersPage {} }
    }

    #[component]
    fn ViewerHarness() -> Element {
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 2,
                    username: "vic".into(),
                    display_name: "Vic Viewer".into(),
                    role: "viewer".into(),
                },
            }),
            storage: Arc::new(MemoryStore::new()),
        });
        use_context_provider(|| provide_auth_gate(session));
        let _ = provide_toaster();
        rsx! { AdminUsersPage {} }
    }

    #[test]
    fn admin_session_renders_the_page_shell() {
        let html = render(|| rsx! { AdminHarness {} });
        // The page header + create CTA render in every data state, so they
        // are the stable contract regardless of whether the SSR pass let
        // the `use_resource` fetch resolve.
        assert!(html.contains("Admin users"), "page heading missing: {html}");
        assert!(
            html.contains("+ Add admin user"),
            "create CTA missing: {html}"
        );
    }

    /// ui-brief §5 — a non-admin who deep-links `/admin/users` must land on a
    /// permission surface, never the table.
    #[test]
    fn non_admin_session_lands_on_insufficient_permissions() {
        let html = render(|| rsx! { ViewerHarness {} });
        assert!(
            html.contains("Insufficient permissions"),
            "viewer must see the permission guard: {html}"
        );
        assert!(
            !html.contains("+ Add admin user"),
            "viewer must NOT see the create CTA: {html}"
        );
    }
}
