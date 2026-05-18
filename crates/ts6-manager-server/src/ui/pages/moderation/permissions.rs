//! `/moderation/permissions` — the read-only permissions reference surface.
//! PURA-379 (PURA-369 Phase C); spec is the PURA-371 UX brief §6.
//!
//! Two panels behind a [`Tabs`] bar:
//!
//! - **Catalog browser** — the whole `permissionlist` (~510 perms), scoped
//!   by the same category rail + search + type filter as the editor's
//!   Catalog view ([`super::permeditor`]), but every row is inert. A row
//!   expands to reveal its description, raw `permsid`, scope, and numeric
//!   `permid`. Deep-linkable: `?permsid=` pre-seeds the search so a group
//!   editor can link "View in catalog".
//! - **Client permission lookup** — `permoverview/{cldbid}` for a picked
//!   client (plus an optional channel). Each effective permission row
//!   carries a **SOURCE** column naming the group / channel it inherits
//!   from. This is the inheritance display — it answers "why does this
//!   client have this permission" without a tree widget.
//!
//! ## Read-only, for every role
//!
//! There is no write affordance anywhere on this page (UX brief §7.11):
//! permissions are *edited* through a server group, channel group, or
//! client — never against the catalog. The page renders for any
//! authenticated session with server access.
//!
//! ## Why no `permfind` call
//!
//! `permissionlist` already carries `{permid, permname, permdesc}`, so the
//! catalog search resolves a raw numeric id or a name substring entirely
//! client-side. `permfind` resolves *where a permission is assigned*, which
//! is the lookup the Client panel answers through `permoverview` instead —
//! so the catalog browser never needs a `permfind` round-trip.

use std::collections::HashMap;
use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{
    ChannelTreeNode, ClientListItem, PermOverviewItem, PermissionCatalogItem, ServerGroupItem,
};

use crate::client::api::{self, ApiError};
use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonSize, ButtonVariant, TabItem, TabPanel, Tabs,
};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

use super::format_error;
use super::permeditor::{PERM_SCOPES, PermKind, is_companion, perm_kind, perm_scope};

/// Catalog view caps the rendered row count — an unscoped catalog is ~510
/// rows (~255 with companions hidden). The rail + search keep the working
/// set small; this is the belt-and-braces ceiling so an unfiltered tab
/// never tries to mount the whole list at once.
const CATALOG_RENDER_CAP: usize = 250;

/// Type-filter segment, local to this page (the editor's is private).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TypeFilter {
    All,
    Boolean,
    Numeric,
}

impl TypeFilter {
    fn matches(self, kind: PermKind) -> bool {
        match self {
            TypeFilter::All => true,
            TypeFilter::Boolean => kind == PermKind::Boolean,
            TypeFilter::Numeric => kind == PermKind::Numeric,
        }
    }
}

/// Substring match over a permission's name, description and numeric id.
fn catalog_matches(item: &PermissionCatalogItem, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    item.permname.to_lowercase().contains(query)
        || item.permdesc.to_lowercase().contains(query)
        || item.permid.to_string().contains(query)
}

/// `/moderation/permissions` — top-level page. Resolves the active server,
/// then hands `server_id` / `sid` to the two panels. `permsid` is the
/// optional deep-link query param that pre-seeds the catalog search.
#[component]
pub fn PermissionsCatalogPage(permsid: Option<String>) -> Element {
    let session = use_session();
    if matches!(*session.state.read(), AuthState::Anonymous) {
        // AppShell bounces anon sessions to /login; render nothing so there
        // is no flash of operator chrome.
        return rsx! { "" };
    }

    let storage = session.storage.clone();
    let servers_ctx = use_servers_context();

    let server = active_server::resolve(&servers_ctx.data.read(), &*storage);
    let Some(server) = server else {
        return rsx! {
            div { class: "crumb", "Moderation · Permissions" }
            h1 { "Permissions" }
            div { class: "empty",
                div { class: "icon", "🔑" }
                h3 { "No server selected" }
                p { "Select a server to browse its permission catalog." }
            }
        };
    };
    let server_id = server.id;
    let server_name = server.name.clone();
    let sid = active_server::DEFAULT_VIRTUAL_SERVER_ID;

    // A deep link lands on the Catalog tab; otherwise Catalog is the default.
    let mut active_tab = use_signal(|| String::from("catalog"));
    let tabs = vec![
        TabItem::new("catalog", "Catalog browser"),
        TabItem::new("lookup", "Client lookup"),
    ];
    let current = active_tab.read().clone();

    rsx! {
        div { class: "crumb", "Moderation · Permissions · {server_name}" }
        h1 { "Permissions" }
        p { class: "info-hint",
            "A read-only reference: browse the full permission catalog, or look up every permission a client effectively holds and where it comes from. Permissions are edited through a server group, channel group, or client — never here."
        }

        Tabs {
            tabs: tabs.clone(),
            active: current.clone(),
            id: "perm-ref".to_string(),
            aria_label: "Permission reference panels".to_string(),
            onselect: move |id: String| active_tab.set(id),
        }

        TabPanel { id: "catalog".to_string(), tabs_id: "perm-ref".to_string(), active: current == "catalog",
            CatalogBrowser { server_id, sid, deeplink: permsid.clone() }
        }
        TabPanel { id: "lookup".to_string(), tabs_id: "perm-ref".to_string(), active: current == "lookup",
            ClientLookup { server_id, sid }
        }
    }
}

// ── Panel A — catalog browser ───────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct CatalogBrowserProps {
    server_id: i64,
    sid: i64,
    /// Deep-link `permsid` — pre-seeds the search box on first mount.
    deeplink: Option<String>,
}

#[component]
fn CatalogBrowser(props: CatalogBrowserProps) -> Element {
    let gate = use_auth_gate();
    let server_id = props.server_id;
    let sid = props.sid;

    let mut catalog_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_catalog(gate, server_id, sid).await }
        }
    });

    // Search is seeded once from the deep-link `permsid`; subsequent typing
    // is the operator's own. A group editor's "View in catalog" link lands
    // on a fresh page mount, so seeding on init is enough.
    let mut search = use_signal(|| props.deeplink.clone().unwrap_or_default());
    let mut type_filter = use_signal(|| TypeFilter::All);
    let mut category: Signal<Option<String>> = use_signal(|| None);
    let mut show_companions = use_signal(|| false);
    let mut open_row: Signal<Option<String>> = use_signal(|| None);

    let snapshot = catalog_res.read().clone();
    let Some(result) = snapshot else {
        return rsx! {
            ul { class: "perm-skeleton",
                for i in 0..6 {
                    li { key: "{i}", class: "skeleton perm-skeleton-row" }
                }
            }
        };
    };
    let catalog = match result {
        Ok(rows) => rows,
        Err(e) => {
            return rsx! {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Could not load the permission catalog".to_string(),
                    "{format_error(&e)}"
                    div { class: "perm-editor-retry",
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            onclick: move |_| catalog_res.restart(),
                            "Retry"
                        }
                    }
                }
            };
        }
    };

    let query = search.read().trim().to_lowercase();
    let active_type = *type_filter.read();
    let companions = *show_companions.read();
    let selected = category.read().clone();

    // Pre-filter: companion perms, type, search. The rail counts and the
    // row list both work off this set.
    let visible: Vec<&PermissionCatalogItem> = catalog
        .iter()
        .filter(|c| companions || !is_companion(&c.permname))
        .filter(|c| active_type.matches(perm_kind(&c.permname)))
        .filter(|c| catalog_matches(c, &query))
        .collect();

    let scope_count = |scope: &str| {
        visible
            .iter()
            .filter(|c| perm_scope(&c.permname) == scope)
            .count()
    };

    let scoped: Vec<&PermissionCatalogItem> = match &selected {
        Some(scope) => visible
            .iter()
            .copied()
            .filter(|c| perm_scope(&c.permname) == scope.as_str())
            .collect(),
        None => visible.clone(),
    };
    let total = scoped.len();
    let capped = total > CATALOG_RENDER_CAP;
    let rendered: Vec<&PermissionCatalogItem> =
        scoped.iter().copied().take(CATALOG_RENDER_CAP).collect();

    rsx! {
        div { class: "perm-catalog",
            nav { class: "perm-rail", "aria-label": "Permission categories",
                ul {
                    li {
                        button {
                            r#type: "button",
                            class: if selected.is_none() { "perm-rail-item is-active" } else { "perm-rail-item" },
                            onclick: move |_| category.set(None),
                            span { "All scopes" }
                            span { class: "perm-rail-count", "{visible.len()}" }
                        }
                    }
                    for scope in PERM_SCOPES {
                        {
                            let count = scope_count(scope);
                            let is_active = selected.as_deref() == Some(scope);
                            rsx! {
                                li { key: "{scope}",
                                    button {
                                        r#type: "button",
                                        class: if is_active { "perm-rail-item is-active" } else { "perm-rail-item" },
                                        disabled: count == 0,
                                        onclick: move |_| category.set(Some(scope.to_string())),
                                        span { "{scope}" }
                                        span { class: "perm-rail-count", "{count}" }
                                    }
                                }
                            }
                        }
                    }
                }
                label { class: "perm-companion-toggle",
                    input {
                        r#type: "checkbox",
                        checked: companions,
                        onchange: move |e| show_companions.set(e.checked()),
                    }
                    span { "Show companion (needed-power) perms" }
                }
            }

            div { class: "perm-catalog-list",
                div { class: "perm-toolbar",
                    input {
                        class: "input perm-search",
                        r#type: "search",
                        placeholder: "Search by name, description or id…",
                        "aria-label": "Search the permission catalog",
                        value: "{search.read()}",
                        oninput: move |e| search.set(e.value()),
                    }
                    div { class: "tabs perm-type-filter", role: "tablist", "aria-label": "Filter by type",
                        for (label , tf) in [("All", TypeFilter::All), ("Bool", TypeFilter::Boolean), ("Num", TypeFilter::Numeric)] {
                            button {
                                key: "{label}",
                                r#type: "button",
                                role: "tab",
                                class: if active_type == tf { "tab is-active" } else { "tab" },
                                "aria-selected": if active_type == tf { "true" } else { "false" },
                                onclick: move |_| type_filter.set(tf),
                                "{label}"
                            }
                        }
                    }
                }

                if catalog.is_empty() {
                    div { class: "empty",
                        div { class: "icon", "📖" }
                        h3 { "Empty catalog" }
                        p { "The server returned no permissions. This is unusual — TeamSpeak ships a full catalog." }
                    }
                } else if rendered.is_empty() {
                    div { class: "empty",
                        div { class: "icon", "🔍" }
                        h3 { "No matching permissions" }
                        p { "No catalog permission matches the current filter." }
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            onclick: move |_| {
                                search.set(String::new());
                                type_filter.set(TypeFilter::All);
                                category.set(None);
                            },
                            "Clear filters"
                        }
                    }
                } else {
                    ul { class: "perm-list",
                        for c in rendered.iter() {
                            CatalogRow {
                                key: "{c.permid}",
                                permid: c.permid,
                                permname: c.permname.clone(),
                                permdesc: c.permdesc.clone(),
                                open: open_row.read().as_deref() == Some(c.permname.as_str()),
                                on_toggle: EventHandler::new(move |name: String| {
                                    let cur = open_row.peek().clone();
                                    if cur.as_deref() == Some(name.as_str()) {
                                        open_row.set(None);
                                    } else {
                                        open_row.set(Some(name));
                                    }
                                }),
                            }
                        }
                    }
                    div { class: "perm-list-foot",
                        span { class: "info-hint",
                            if capped {
                                "Showing {CATALOG_RENDER_CAP} of {total}. Pick a category or refine the search to narrow the list."
                            } else {
                                "{total} permission(s)"
                            }
                        }
                    }
                }
            }
        }
    }
}

#[derive(Props, Clone, PartialEq)]
struct CatalogRowProps {
    permid: i64,
    permname: String,
    permdesc: String,
    open: bool,
    on_toggle: EventHandler<String>,
}

/// One inert catalog row. Collapsed: type chip + `permsid`. Expanded:
/// description, scope, raw `permsid`, numeric `permid`. There is no control
/// — the catalog is never editable from this surface.
#[component]
fn CatalogRow(props: CatalogRowProps) -> Element {
    let kind = perm_kind(&props.permname);
    let scope = perm_scope(&props.permname);
    let (chip, chip_class) = match kind {
        PermKind::Boolean => ("bool", "perm-chip perm-chip--bool"),
        PermKind::Numeric => ("num", "perm-chip perm-chip--num"),
    };
    let on_toggle = props.on_toggle;
    let permname = props.permname.clone();
    let desc = if props.permdesc.trim().is_empty() {
        "No description provided by the server.".to_string()
    } else {
        props.permdesc.clone()
    };

    rsx! {
        li { class: "perm-row perm-row--inert",
            button {
                class: "perm-row-head perm-row-toggle",
                r#type: "button",
                "aria-expanded": if props.open { "true" } else { "false" },
                onclick: {
                    let permname = permname.clone();
                    move |_| on_toggle.call(permname.clone())
                },
                span { class: "{chip_class}", "{chip}" }
                div { class: "perm-row-main",
                    code { class: "mod-grant-key", "{props.permname}" }
                    span { class: "tag tag-neutral", "{scope}" }
                }
                span { class: "perm-row-chevron", aria_hidden: "true",
                    if props.open { "▾" } else { "▸" }
                }
            }
            if props.open {
                div { class: "perm-disclosure perm-catalog-detail",
                    p { class: "perm-catalog-desc", "{desc}" }
                    dl { class: "perm-catalog-meta",
                        div {
                            dt { "permsid" }
                            dd { code { class: "mod-grant-key", "{props.permname}" } }
                        }
                        div {
                            dt { "Numeric id" }
                            dd { class: "mono", "{props.permid}" }
                        }
                        div {
                            dt { "Type" }
                            dd { if kind == PermKind::Boolean { "Boolean — on or off" } else { "Numeric — a skill-level value" } }
                        }
                    }
                }
            }
        }
    }
}

// ── Panel B — client permission lookup ──────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct ClientLookupProps {
    server_id: i64,
    sid: i64,
}

#[component]
fn ClientLookup(props: ClientLookupProps) -> Element {
    let gate = use_auth_gate();
    let server_id = props.server_id;
    let sid = props.sid;

    // Picker sources — clients populate the select; channels are the
    // optional scope; the catalog + server-group list resolve permission
    // names and the SOURCE column. All four are loaded once.
    let clients_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                api::authorized_get_json::<Vec<ClientListItem>>(
                    &gate,
                    &api::api_base(),
                    &format!("/api/servers/{server_id}/vs/{sid}/clients"),
                )
                .await
            }
        }
    });
    let channels_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                api::authorized_get_json::<Vec<ChannelTreeNode>>(
                    &gate,
                    &api::api_base(),
                    &format!("/api/servers/{server_id}/vs/{sid}/channels"),
                )
                .await
            }
        }
    });
    let catalog_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move { fetch_catalog(gate, server_id, sid).await }
        }
    });
    let groups_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                api::authorized_get_json::<Vec<ServerGroupItem>>(
                    &gate,
                    &api::api_base(),
                    &format!("/api/servers/{server_id}/vs/{sid}/server-groups"),
                )
                .await
            }
        }
    });

    // `0` = no client picked yet (the empty state); else the chosen cldbid.
    let mut picked_client = use_signal(|| 0i64);
    // `0` = whole-server scope; else the optional channel scope.
    let mut picked_channel = use_signal(|| 0i64);

    let cldbid = *picked_client.read();
    let cid = *picked_channel.read();

    // `permoverview` is re-fetched whenever the client or channel changes.
    let mut overview_res = use_resource({
        let gate = gate.clone();
        move || {
            let gate = gate.clone();
            async move {
                if cldbid == 0 {
                    return Ok(Vec::new());
                }
                let mut path =
                    format!("/api/servers/{server_id}/vs/{sid}/permissions/overview/{cldbid}");
                if cid != 0 {
                    path.push_str(&format!("?cid={cid}"));
                }
                api::authorized_get_json::<Vec<PermOverviewItem>>(&gate, &api::api_base(), &path)
                    .await
            }
        }
    });

    let clients = match clients_res.read().clone() {
        Some(Ok(rows)) => rows,
        _ => Vec::new(),
    };
    let channels = match channels_res.read().clone() {
        Some(Ok(rows)) => rows,
        _ => Vec::new(),
    };

    // permid → permsid name, for joining overview rows against the catalog.
    let perm_names: HashMap<i64, String> = match catalog_res.read().clone() {
        Some(Ok(rows)) => rows.into_iter().map(|c| (c.permid, c.permname)).collect(),
        _ => HashMap::new(),
    };
    // sgid → group name, for the SOURCE column.
    let group_names: HashMap<i64, String> = match groups_res.read().clone() {
        Some(Ok(rows)) => rows.into_iter().map(|g| (g.sgid, g.name)).collect(),
        _ => HashMap::new(),
    };
    let channel_names: HashMap<i64, String> = channels
        .iter()
        .map(|c| (c.cid, c.channel_name.clone()))
        .collect();

    let clients_loading = clients_res.read().is_none();
    let clients_failed = matches!(clients_res.read().clone(), Some(Err(_)));

    rsx! {
        div { class: "perm-lookup",
            if clients_failed {
                Banner {
                    variant: BannerVariant::Warning,
                    title: "Could not load the client list".to_string(),
                    "The client picker is unavailable. Reload the page to try again."
                }
            }

            div { class: "perm-lookup-pickers",
                div { class: "field",
                    label { class: "field-label", r#for: "perm-lookup-client", "Client" }
                    select {
                        id: "perm-lookup-client",
                        class: "input",
                        disabled: clients_loading || clients_failed,
                        onchange: move |e| {
                            picked_client.set(e.value().parse::<i64>().unwrap_or(0));
                        },
                        option { value: "0", "Select a client…" }
                        for c in clients.iter().filter(|c| c.client_type == 0) {
                            option {
                                key: "{c.client_database_id}",
                                value: "{c.client_database_id}",
                                selected: c.client_database_id == cldbid,
                                "{c.client_nickname} (cldbid {c.client_database_id})"
                            }
                        }
                    }
                    p { class: "field-help",
                        "Connected clients only — offline accounts do not appear in the picker."
                    }
                }
                div { class: "field",
                    label { class: "field-label", r#for: "perm-lookup-channel", "Channel (optional)" }
                    select {
                        id: "perm-lookup-channel",
                        class: "input",
                        onchange: move |e| {
                            picked_channel.set(e.value().parse::<i64>().unwrap_or(0));
                        },
                        option { value: "0", "Whole server" }
                        for ch in channels.iter() {
                            option {
                                key: "{ch.cid}",
                                value: "{ch.cid}",
                                selected: ch.cid == cid,
                                "{ch.channel_name}"
                            }
                        }
                    }
                    p { class: "field-help",
                        "Scope the overview to one channel to see channel-specific permissions."
                    }
                }
            }

            if cldbid == 0 {
                div { class: "empty",
                    div { class: "icon", "🔎" }
                    h3 { "Pick a client" }
                    p { "Choose a client above to see every permission they effectively hold and where each one comes from." }
                }
            } else {
                match overview_res.read().clone() {
                    None => rsx! {
                        ul { class: "perm-skeleton",
                            for i in 0..5 {
                                li { key: "{i}", class: "skeleton perm-skeleton-row" }
                            }
                        }
                    },
                    Some(Err(e)) => rsx! {
                        Banner {
                            variant: BannerVariant::Danger,
                            title: "Could not load the permission overview".to_string(),
                            "{format_error(&e)}"
                            div { class: "perm-editor-retry",
                                Button {
                                    variant: ButtonVariant::Secondary,
                                    size: ButtonSize::Small,
                                    onclick: move |_| overview_res.restart(),
                                    "Retry"
                                }
                            }
                        }
                    },
                    Some(Ok(rows)) if rows.is_empty() => rsx! {
                        div { class: "empty",
                            div { class: "icon", "∅" }
                            h3 { "No permissions" }
                            p { "This client holds no permissions in the selected scope." }
                        }
                    },
                    Some(Ok(rows)) => {
                        let mut sorted = rows.clone();
                        sorted.sort_by(|a, b| {
                            let an = perm_names.get(&a.p).map(String::as_str).unwrap_or("");
                            let bn = perm_names.get(&b.p).map(String::as_str).unwrap_or("");
                            an.cmp(bn).then(a.p.cmp(&b.p)).then(a.t.cmp(&b.t))
                        });
                        rsx! {
                            p { class: "info-hint",
                                "{sorted.len()} effective permission row(s). A permission listed more than once is contributed by more than one source."
                            }
                            table { class: "data-table", "aria-label": "Effective client permissions",
                                thead {
                                    tr {
                                        th { scope: "col", "Permission" }
                                        th { scope: "col", class: "num-col", "Value" }
                                        th { scope: "col", "Source" }
                                    }
                                }
                                tbody {
                                    for (i , row) in sorted.iter().enumerate() {
                                        {
                                            let name = perm_names
                                                .get(&row.p)
                                                .cloned()
                                                .unwrap_or_else(|| format!("permid {}", row.p));
                                            let source = source_label(row, &group_names, &channel_names);
                                            rsx! {
                                                tr { key: "{i}",
                                                    td {
                                                        code { class: "mod-grant-key", "{name}" }
                                                        if row.n != 0 {
                                                            span { class: "tag tag-danger", "Negated" }
                                                        }
                                                        if row.s != 0 {
                                                            span { class: "tag tag-warning", "Skip" }
                                                        }
                                                    }
                                                    td { class: "num-col mono", "{row.v}" }
                                                    td { "{source}" }
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
        }
    }
}

/// Resolve a `permoverview` row's origin to an operator-facing SOURCE label.
/// `t` tags the origin kind (`0` server group, `1` client, `2` channel) and
/// `id1` carries the originating `sgid` / `cid` (`routes/control/permissions`
/// fixture findings). A name miss degrades to the raw id rather than failing.
fn source_label(
    row: &PermOverviewItem,
    group_names: &HashMap<i64, String>,
    channel_names: &HashMap<i64, String>,
) -> String {
    match row.t {
        0 => match group_names.get(&row.id1) {
            Some(name) => format!("Server group · {name}"),
            None => format!("Server group · sgid {}", row.id1),
        },
        1 => "Client (direct grant)".to_string(),
        2 => match channel_names.get(&row.id1) {
            Some(name) => format!("Channel · {name}"),
            None => format!("Channel · cid {}", row.id1),
        },
        other => format!("Unknown origin (t={other})"),
    }
}

async fn fetch_catalog(
    gate: Arc<RefreshGate>,
    server_id: i64,
    sid: i64,
) -> Result<Vec<PermissionCatalogItem>, ApiError> {
    let path = format!("/api/servers/{server_id}/vs/{sid}/permissions");
    api::authorized_get_json(&gate, &api::api_base(), &path).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat(permid: i64, name: &str, desc: &str) -> PermissionCatalogItem {
        PermissionCatalogItem {
            permid,
            permname: name.to_string(),
            permdesc: desc.to_string(),
        }
    }

    #[test]
    fn catalog_search_matches_name_desc_and_id() {
        let item = cat(8470, "b_client_kick_from_channel", "Kick a client");
        assert!(catalog_matches(&item, ""));
        assert!(catalog_matches(&item, "kick"));
        assert!(catalog_matches(&item, "b_client"));
        // Description substring.
        assert!(catalog_matches(&item, "client"));
        // Raw numeric id.
        assert!(catalog_matches(&item, "8470"));
        assert!(!catalog_matches(&item, "ban"));
    }

    #[test]
    fn type_filter_partitions_by_kind() {
        assert!(TypeFilter::All.matches(PermKind::Boolean));
        assert!(TypeFilter::All.matches(PermKind::Numeric));
        assert!(TypeFilter::Boolean.matches(PermKind::Boolean));
        assert!(!TypeFilter::Boolean.matches(PermKind::Numeric));
        assert!(TypeFilter::Numeric.matches(PermKind::Numeric));
        assert!(!TypeFilter::Numeric.matches(PermKind::Boolean));
    }

    #[test]
    fn source_label_resolves_each_origin_kind() {
        let mut groups = HashMap::new();
        groups.insert(6, "Server Admin".to_string());
        let mut channels = HashMap::new();
        channels.insert(3, "Lobby".to_string());

        let sg = PermOverviewItem {
            t: 0,
            id1: 6,
            ..Default::default()
        };
        assert_eq!(
            source_label(&sg, &groups, &channels),
            "Server group · Server Admin"
        );

        // Unknown sgid degrades to the raw id, never panics.
        let sg_miss = PermOverviewItem {
            t: 0,
            id1: 99,
            ..Default::default()
        };
        assert_eq!(
            source_label(&sg_miss, &groups, &channels),
            "Server group · sgid 99"
        );

        let client = PermOverviewItem {
            t: 1,
            id1: 0,
            ..Default::default()
        };
        assert_eq!(
            source_label(&client, &groups, &channels),
            "Client (direct grant)"
        );

        let chan = PermOverviewItem {
            t: 2,
            id1: 3,
            ..Default::default()
        };
        assert_eq!(source_label(&chan, &groups, &channels), "Channel · Lobby");

        let weird = PermOverviewItem {
            t: 7,
            id1: 0,
            ..Default::default()
        };
        assert_eq!(
            source_label(&weird, &groups, &channels),
            "Unknown origin (t=7)"
        );
    }
}
