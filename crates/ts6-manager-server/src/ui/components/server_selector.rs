//! Server selector — `components.md` §13.11 + `server-selector.md`.
//!
//! Composes the Dropdown primitive with single-select radio items, the
//! optional filter (visible only at >7 servers per §13.11), and a
//! "Manage servers…" footer that routes to `/servers`. The data source is
//! the shared [`ServersContext`] mounted by [`AppShell`] — a single
//! `GET /api/servers` resource backs both the desktop header pill and the
//! mobile-bar variant, so no extra fetches fire when both render.
//!
//! Selected-server persistence rides through [`crate::client::ui_prefs`]:
//! the operator's last pick is restored from `localStorage` on mount, and
//! every `onselect` writes back. A persisted id that no longer matches any
//! row in the live list is treated as "no selection" and silently cleared
//! so the chrome doesn't keep pointing at a deleted server.
//!
//! Two visual variants exist because the chrome surfaces the selector in
//! two slots (header pill on desktop, full-width bar above the page on
//! mobile, per `components.md` §11.3 / `server-selector.md` §2.2). The
//! variants render distinct DOM ids so both instances can mount in the
//! same document without ARIA-id collisions; `display: none` keeps only
//! one visible at any breakpoint.

use dioxus::prelude::*;
use ts6_manager_shared::servers::ServerSummary;

use crate::client::api::ApiError;
use crate::client::dioxus::use_session;
use crate::client::storage::Storage;
use crate::client::store::AuthState;
use crate::client::ui_prefs::{
    clear_selected_server_id, load_selected_server_id, save_selected_server_id,
};
use crate::ui::components::dropdown::{
    Dropdown, Menu, MenuDivider, MenuEmpty, MenuFilter, MenuFooter, MenuItem, MenuItemKind,
    MenuSection,
};
use crate::ui::layout::{ServersContext, ServersData, use_servers_context};
use crate::ui::routes::Route;

/// Filter is hidden until the list grows past this many entries. Spec
/// §13.4 ("Filter input (when used)") and §13.11 ("MenuFilter shown only
/// when servers.len() > 7").
pub const FILTER_VISIBLE_THRESHOLD: usize = 7;

/// Case-insensitive substring filter over the live server list.
pub fn filter_servers(servers: &[ServerSummary], needle: &str) -> Vec<ServerSummary> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return servers.to_vec();
    }
    servers
        .iter()
        .filter(|s| s.name.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ServerSelectorVariant {
    #[default]
    Desktop,
    Mobile,
}

impl ServerSelectorVariant {
    /// DOM-id prefix per variant. Two `ServerSelector`s mount in the same
    /// document (one in the header, one in the mobile-only bar); distinct
    /// prefixes prevent the trigger / menu / item ids from colliding.
    const fn id_prefix(self) -> &'static str {
        match self {
            ServerSelectorVariant::Desktop => "server-selector-desktop",
            ServerSelectorVariant::Mobile => "server-selector-mobile",
        }
    }

    const fn extra_anchor_class(self) -> &'static str {
        match self {
            ServerSelectorVariant::Desktop => "desktop-selector",
            ServerSelectorVariant::Mobile => "mobile-selector",
        }
    }
}

/// Header server-selector. Reads its data from [`ServersContext`] and
/// persists the operator's pick through [`ui_prefs`].
#[allow(non_snake_case)]
#[component]
pub fn ServerSelector(
    #[props(default = ServerSelectorVariant::Desktop)] variant: ServerSelectorVariant,
) -> Element {
    let ctx: ServersContext = use_servers_context();
    let session = use_session();
    let storage = session.storage.clone();

    // Hydrate from localStorage on mount. The signal stays `Option<i64>` so
    // a missing pref / never-picked state cleanly maps to "no selection".
    let mut selected: Signal<Option<i64>> = use_signal({
        let storage = storage.clone();
        move || load_selected_server_id(&*storage)
    });
    let mut open = use_signal(|| false);
    let mut active_id: Signal<Option<String>> = use_signal(|| None::<String>);
    let mut filter = use_signal(String::new);

    let prefix = variant.id_prefix();
    let trigger_id = format!("{prefix}-trigger");
    let menu_id = format!("{prefix}-menu");

    // Reset filter and active cursor whenever the menu closes so the next
    // open starts clean — matches the "filter autofocuses on open" contract
    // in §13.4.
    use_effect(move || {
        if !*open.read() {
            filter.set(String::new());
            active_id.set(None);
        }
    });

    let data_snap = ctx.data.read().clone();
    let rows = data_snap.rows().to_vec();
    let filter_text = filter.read().clone();
    let filtered = filter_servers(&rows, &filter_text);
    let show_filter = rows.len() > FILTER_VISIBLE_THRESHOLD;

    // Whenever the live list arrives, reconcile the persisted id: if it
    // doesn't match any row, treat the operator as un-picked AND clear the
    // stale localStorage entry. Done in an effect so we react to
    // out-of-tab edits and post-fetch arrival uniformly.
    {
        let rows_for_effect = rows.clone();
        let storage_for_effect = storage.clone();
        use_effect(move || {
            let cur = *selected.read();
            if let Some(id) = cur
                && !rows_for_effect.iter().any(|s| s.id == id)
            {
                selected.set(None);
                clear_selected_server_id(&*storage_for_effect);
            }
        });
    }

    let selected_id = *selected.read();
    let selected_name =
        selected_id_to_label(selected_id, &rows, &data_snap).unwrap_or_else(|| match data_snap {
            ServersData::Loading => "Loading servers…".to_string(),
            ServersData::Error(_) => "Servers unavailable".to_string(),
            ServersData::Loaded(_) => "Select a server".to_string(),
        });

    let aria_expanded = if *open.read() { "true" } else { "false" };
    let trigger_id_for_attr = trigger_id.clone();
    let menu_id_for_attr = menu_id.clone();

    let toggle_open = move |_| {
        let next = !*open.read();
        if next {
            // Initialise active_id to the selected server (if any), else
            // the first item — matches the spec's "selected = active on
            // open" expectation so screen readers don't see an unfocused
            // menu on first arrow press.
            let initial = (*selected.read())
                .map(|id| id.to_string())
                .or_else(|| ctx.data.read().rows().first().map(|s| s.id.to_string()));
            active_id.set(initial);
        }
        open.set(next);
    };

    let anchor_class = format!("server-selector-anchor {}", variant.extra_anchor_class());

    rsx! {
        div { class: "{anchor_class}",
            Dropdown {
                trigger_id: trigger_id.clone(),
                menu_id: menu_id.clone(),
                open: open,
                active_id: active_id,
                trigger: rsx! {
                    button {
                        class: "selector",
                        r#type: "button",
                        id: "{trigger_id_for_attr}",
                        "aria-haspopup": "menu",
                        "aria-expanded": "{aria_expanded}",
                        "aria-controls": "{menu_id_for_attr}",
                        "aria-label": "Switch server",
                        onclick: toggle_open,
                        span { class: "mark", "⬢" }
                        span { class: "label", "{selected_name}" }
                        span { class: "chev", "▾" }
                    }
                },

                Menu {
                    id: menu_id.clone(),
                    labelled_by: trigger_id.clone(),
                    active_id: active_id.read().clone(),

                    if show_filter {
                        MenuFilter {
                            value: filter,
                            placeholder: "Filter servers…".to_string(),
                            aria_label: "Filter servers".to_string(),
                        }
                    }

                    { match &data_snap {
                        ServersData::Loading => rsx! {
                            MenuEmpty { text: "Loading servers…".to_string() }
                        },
                        ServersData::Error(err) => rsx! {
                            MenuEmpty { text: error_copy(err) }
                        },
                        ServersData::Loaded(_) => {
                            if filtered.is_empty() {
                                if filter_text.trim().is_empty() {
                                    rsx! {
                                        MenuEmpty { text: "No servers configured yet.".to_string() }
                                    }
                                } else {
                                    rsx! {
                                        MenuEmpty { text: format!("No servers match \"{}\".", filter_text.trim()) }
                                    }
                                }
                            } else {
                                let storage_for_section = storage.clone();
                                rsx! {
                                    MenuSection { label: "Servers".to_string(),
                                        for s in filtered.iter() {
                                            ServerSelectorItem {
                                                key: "{s.id}",
                                                summary: s.clone(),
                                                prefix: prefix.to_string(),
                                                checked: selected_id == Some(s.id),
                                                onselect: {
                                                    let server_id = s.id;
                                                    let storage_for_pick = storage_for_section.clone();
                                                    let mut selected_for_pick = selected;
                                                    move |()| {
                                                        selected_for_pick.set(Some(server_id));
                                                        save_selected_server_id(&*storage_for_pick, server_id);
                                                    }
                                                },
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } }

                    MenuDivider {}
                    MenuFooter {
                        { match &data_snap {
                            // PURA-225 — when the list fetch was an
                            // Unauthorized envelope the operator has no
                            // session left, so a link to /servers (which
                            // would render the same 401 banner) is the
                            // wrong affordance. Replace the footer with a
                            // sign-in escape so the dropdown is a usable
                            // exit from every authed surface, not just
                            // /servers.
                            ServersData::Error(err) if err.is_unauthorized() => rsx! {
                                SignInAgainMenuButton {}
                            },
                            _ => rsx! {
                                a {
                                    class: "btn btn-secondary btn-sm",
                                    href: "/servers",
                                    "Manage servers…"
                                }
                            },
                        } }
                    }
                }
            }
        }
    }
}

/// PURA-225 — sign-in escape rendered inside the server selector's footer
/// when the `/api/servers` fetch was a 401. Mirrors the `SignInAgainButton`
/// on the `/servers` page so an operator who hits the trap on any other
/// authed surface (where the only visible 401 affordance is the dropdown's
/// "Servers unavailable" pill) still has a one-click path back to `/login`.
#[allow(non_snake_case)]
#[component]
fn SignInAgainMenuButton() -> Element {
    let session = use_session();
    let nav = use_navigator();
    let on_click = move |_| {
        let next = current_authed_path();
        session.replace(AuthState::Anonymous);
        nav.replace(Route::LoginPage { next: Some(next) });
    };
    rsx! {
        button {
            r#type: "button",
            class: "btn btn-primary btn-sm",
            onclick: on_click,
            "Sign in again"
        }
    }
}

/// Same fallback as the page-level CTA — capture the current path on WASM
/// so `?next=` round-trips through the login screen.
fn current_authed_path() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let loc = window.location();
            let mut out = loc.pathname().unwrap_or_else(|_| "/".into());
            if let Ok(search) = loc.search()
                && !search.is_empty()
            {
                out.push_str(&search);
            }
            return out;
        }
        "/".into()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        "/".into()
    }
}

/// Best-effort label for the trigger pill. `None` means the caller should
/// fall back to a state-aware placeholder (loading / error / "Select a
/// server"); `Some` is always a server name pulled from the live list.
fn selected_id_to_label(
    selected: Option<i64>,
    rows: &[ServerSummary],
    _data: &ServersData,
) -> Option<String> {
    let id = selected?;
    rows.iter().find(|s| s.id == id).map(|s| s.name.clone())
}

/// Human-readable copy for the in-menu error empty-state. We keep it short
/// because the dropdown isn't the right surface for full diagnostics — the
/// 502 envelope details surface on the dashboard banner instead.
fn error_copy(err: &ApiError) -> String {
    match err {
        ApiError::Unauthorized(_) => "Session expired. Sign in again.".into(),
        ApiError::BadGateway { error, .. } => {
            format!("Could not reach TeamSpeak ({error}).")
        }
        ApiError::Client { status, .. } => format!("Servers list rejected ({status})."),
        ApiError::Server { .. } | ApiError::Transport(_) => {
            "Servers list temporarily unavailable.".into()
        }
        ApiError::Deserialise(_) => "Unexpected response from /api/servers.".into(),
        ApiError::UnsupportedTarget => "Live server list unavailable in this view.".into(),
    }
}

#[derive(Props, Clone, PartialEq)]
struct ServerSelectorItemProps {
    summary: ServerSummary,
    prefix: String,
    checked: bool,
    onselect: EventHandler<()>,
}

#[allow(non_snake_case)]
#[component]
fn ServerSelectorItem(props: ServerSelectorItemProps) -> Element {
    let id = format!("{}-item-{}", props.prefix, props.summary.id);
    let summary = props.summary.clone();
    let checked = props.checked;
    let check_glyph = if checked { "✓" } else { "" };
    rsx! {
        MenuItem {
            id: id,
            kind: MenuItemKind::Radio { checked },
            rich: true,
            onselect: props.onselect,
            span { class: "check", "{check_glyph}" }
            span { class: "label", "{summary.name}" }
            span { class: "meta", "{summary.host}" }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::dioxus::{DioxusSession, provide_auth_gate};
    use crate::client::storage::MemoryStore;
    use crate::client::store::AuthState;
    use chrono::Utc;
    use std::sync::Arc;
    use ts6_manager_shared::auth::UserInfo;

    fn fixture(id: i64, name: &str) -> ServerSummary {
        let now = Utc::now();
        ServerSummary {
            id,
            name: name.into(),
            host: format!("ts{id}.example.com"),
            webquery_port: 10080,
            use_https: true,
            ssh_port: 10022,
            ssh_username: None,
            has_ssh_credentials: false,
            query_bot_channel: None,
            query_bot_nickname: None,
            ssh_bot_nickname: None,
            enabled: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn filter_visible_threshold_is_seven() {
        // Pin §13.11's literal "> 7" so a future tweak to the threshold
        // forces a docs/code review of the rationale (Hick's law applied
        // to short menus).
        assert_eq!(FILTER_VISIBLE_THRESHOLD, 7);
    }

    #[test]
    fn filter_servers_matches_substring_case_insensitively() {
        let list = vec![fixture(1, "Tournament server"), fixture(2, "Backup")];
        let hits = filter_servers(&list, "tour");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "Tournament server");
    }

    #[test]
    fn filter_servers_empty_needle_returns_all() {
        let list = vec![fixture(1, "A"), fixture(2, "B")];
        assert_eq!(filter_servers(&list, "   ").len(), 2);
    }

    #[test]
    fn filter_servers_no_match_returns_empty() {
        let list = vec![fixture(1, "Primary")];
        assert!(filter_servers(&list, "staging-007").is_empty());
    }

    #[test]
    fn variants_emit_distinct_id_prefixes() {
        assert_ne!(
            ServerSelectorVariant::Desktop.id_prefix(),
            ServerSelectorVariant::Mobile.id_prefix(),
            "two ServerSelectors mount in the same document; their DOM ids must not collide"
        );
    }

    #[test]
    fn selected_id_to_label_resolves_against_loaded_rows() {
        let rows = vec![fixture(7, "Primary")];
        assert_eq!(
            selected_id_to_label(Some(7), &rows, &ServersData::Loaded(rows.clone())),
            Some("Primary".into())
        );
    }

    #[test]
    fn selected_id_to_label_returns_none_for_unknown_id() {
        let rows = vec![fixture(7, "Primary")];
        assert_eq!(
            selected_id_to_label(Some(999), &rows, &ServersData::Loaded(rows.clone())),
            None
        );
        assert_eq!(
            selected_id_to_label(None, &rows, &ServersData::Loaded(rows.clone())),
            None
        );
    }

    #[test]
    fn error_copy_for_unauthorized_uses_session_expired_phrasing() {
        let s = error_copy(&ApiError::Unauthorized("Invalid or expired token".into()));
        assert!(s.contains("Session expired"), "got: {s}");
    }

    #[test]
    fn error_copy_for_bad_gateway_mentions_teamspeak() {
        let s = error_copy(&ApiError::BadGateway {
            error: "TeamSpeak API Error".into(),
            code: Some(1153),
            details: Some("invalid serverID".into()),
        });
        assert!(s.contains("TeamSpeak"), "got: {s}");
    }

    // ── SSR markup contract ────────────────────────────────────────────────
    //
    // Renders the closed-state ServerSelector inside a synthetic Router (for
    // `Link`/`use_route` consumers downstream — none today, but the dropdown's
    // children may compose ordinary Dioxus router primitives in the future)
    // and asserts the trigger ARIA attributes.

    #[derive(Clone, Routable, Debug, PartialEq)]
    #[rustfmt::skip]
    enum SelectorTestRoute {
        #[route("/")]
        SelectorHarness {},
    }

    #[component]
    fn SelectorHarness() -> Element {
        // Provide the contexts the production tree mounts so the selector
        // doesn't panic when it reaches for session / context state.
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 1,
                    username: "rsoot".into(),
                    display_name: "Robert Soot".into(),
                    role: "admin".into(),
                },
            }),
            storage: Arc::new(MemoryStore::new()),
        });
        use_context_provider(|| provide_auth_gate(session));
        // Synthetic context: a single fixture row so the loaded path
        // renders without firing a real fetch.
        use_context_provider(|| ServersContext {
            data: Signal::new(ServersData::Loaded(vec![fixture(1, "Primary")])),
        });
        rsx! { ServerSelector { variant: ServerSelectorVariant::Desktop } }
    }

    fn render_selector_harness() -> String {
        let mut dom = VirtualDom::new(|| {
            rsx! { Router::<SelectorTestRoute> {} }
        });
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn trigger_carries_full_aria_haspopup_contract() {
        let html = render_selector_harness();
        assert!(
            html.contains(r#"aria-haspopup="menu""#),
            "trigger missing aria-haspopup='menu': {html}"
        );
        assert!(
            html.contains(r#"aria-expanded="false""#),
            "closed selector should expose aria-expanded='false': {html}"
        );
        assert!(
            html.contains(r#"aria-controls="server-selector-desktop-menu""#),
            "trigger must aria-control its own menu id: {html}"
        );
        assert!(
            html.contains(r#"aria-label="Switch server""#),
            "trigger missing aria-label='Switch server': {html}"
        );
    }

    #[test]
    fn trigger_id_matches_aria_controls_target() {
        let html = render_selector_harness();
        assert!(
            html.contains(r#"id="server-selector-desktop-trigger""#),
            "trigger id must match the menu's aria-labelledby reference: {html}"
        );
    }

    #[test]
    fn trigger_falls_back_to_select_placeholder_when_no_pref_persisted() {
        // The harness mounts a Loaded context with a single fixture row
        // and an empty MemoryStore — no persisted pref means the trigger
        // shows the "Select a server" placeholder, not the row name.
        let html = render_selector_harness();
        assert!(
            html.contains("Select a server"),
            "expected default placeholder when no pref persisted: {html}"
        );
    }

    /// Variant of the harness that pre-seeds the persisted pref so the
    /// trigger label resolves through the loaded list.
    #[component]
    fn SelectorHarnessWithPref() -> Element {
        let storage = Arc::new(MemoryStore::from_iter([(
            crate::client::ui_prefs::SELECTED_SERVER_STORAGE_KEY,
            "1",
        )]));
        let session = use_context_provider(|| DioxusSession {
            state: SyncSignal::new_maybe_sync(AuthState::Authenticated {
                access: "stub-access".into(),
                refresh: "stub-refresh".into(),
                user: UserInfo {
                    id: 1,
                    username: "rsoot".into(),
                    display_name: "Robert Soot".into(),
                    role: "admin".into(),
                },
            }),
            storage,
        });
        use_context_provider(|| provide_auth_gate(session));
        use_context_provider(|| ServersContext {
            data: Signal::new(ServersData::Loaded(vec![fixture(1, "Primary")])),
        });
        rsx! { ServerSelector { variant: ServerSelectorVariant::Desktop } }
    }

    #[test]
    fn trigger_renders_persisted_server_name_when_pref_matches_loaded_row() {
        let mut dom = VirtualDom::new(SelectorHarnessWithPref);
        dom.rebuild_in_place();
        let html = dioxus_ssr::render(&dom);
        assert!(
            html.contains("Primary"),
            "expected pref-matched server name in trigger label: {html}"
        );
        assert!(
            !html.contains("Select a server"),
            "placeholder should NOT render when pref resolves: {html}"
        );
    }
}
