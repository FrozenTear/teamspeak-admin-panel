//! Shared permission editor for the moderation group surfaces — PURA-375.
//!
//! [`PermissionEditor`] is the assigned-first / catalog drill-in editor the
//! UX brief (PURA-371 §4.2.1) specifies. It is built here for server groups
//! and reused verbatim by channel groups ([PURA-378](/PURA/issues/PURA-378))
//! — pass `channel_group: true` so the negate/skip flags, which TS6
//! channel-group permissions do not carry, are suppressed. The scope /
//! kind / companion helpers are `pub(crate)` so the read-only catalog
//! browser ([PURA-379](/PURA/issues/PURA-379)) can share the taxonomy.
//!
//! ## Editing model (UX brief §3)
//!
//! - **Assigned view** (default) — only the permissions the group actually
//!   sets (`servergrouppermlist`), typically 5–30 rows. This is the landing
//!   surface; the full catalog is never the default.
//! - **Catalog view** — the whole `permissionlist`, scoped by a category
//!   rail (the six TS6 permission scopes), filtered by search + type. Each
//!   already-set permission is flagged with a `Set` tag; the rest carry an
//!   `Add` affordance.
//! - **One batched Save** — edits accumulate into a draft; Save diffs the
//!   draft against the fetched baseline and issues `…a016ddperm` /
//!   `…delperm` per changed row. Lifted from `grants.rs`.

use std::sync::Arc;

use dioxus::prelude::*;
use ts6_manager_shared::control::{GroupPermItem, GroupPermSetRequest, PermissionCatalogItem};

use crate::client::api;
use crate::client::dioxus::use_auth_gate;
use crate::client::session::RefreshGate;
use crate::ui::components::Switch;
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::components::{Banner, BannerVariant, Button, ButtonSize, ButtonType, ButtonVariant};

use super::format_error;

/// The six TS6 permission scopes, in the order the official client lists
/// them — the category rail's primary axis (UX brief Appendix A.2). `Other`
/// is the catch-all for anything whose `permsid` prefix is unrecognised.
pub(crate) const PERM_SCOPES: [&str; 7] = [
    "Global",
    "Virtual Server",
    "Channel",
    "Group",
    "Client",
    "File Transfer",
    "Other",
];

/// The typed class of a permission, inferred from the `permsid` prefix.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PermKind {
    /// `b_*` — a yes/no permission. Rendered as a [`Switch`]; value 0/1.
    Boolean,
    /// `i_*` — a numeric / skill-level permission. Rendered as a stepper.
    Numeric,
}

impl PermKind {
    /// Compact type-chip label (UX brief Appendix A.2 vocabulary).
    fn chip(self) -> &'static str {
        match self {
            PermKind::Boolean => "bool",
            PermKind::Numeric => "num",
        }
    }

    fn chip_class(self) -> &'static str {
        match self {
            PermKind::Boolean => "perm-chip perm-chip--bool",
            PermKind::Numeric => "perm-chip perm-chip--num",
        }
    }
}

/// Classify a permission by its `permsid` prefix. `b_` → boolean; everything
/// else (`i_`, and the rare unprefixed entries) is treated as numeric.
pub(crate) fn perm_kind(permsid: &str) -> PermKind {
    if permsid.starts_with("b_") {
        PermKind::Boolean
    } else {
        PermKind::Numeric
    }
}

/// Map a `permsid` onto one of [`PERM_SCOPES`]. The catalog ships no
/// machine-readable category metadata, so the rail groups on the stable
/// `permsid` namespace prefix (UX brief §11 residual risk → Appendix A.2
/// resolution: the prefixes *are* the six scopes).
pub(crate) fn perm_scope(permsid: &str) -> &'static str {
    let body = permsid
        .strip_prefix("b_")
        .or_else(|| permsid.strip_prefix("i_"))
        .unwrap_or(permsid);
    if body.starts_with("serverinstance") || body.starts_with("serverquery") {
        "Global"
    } else if body.starts_with("virtualserver") {
        "Virtual Server"
    } else if body.starts_with("channel") {
        "Channel"
    } else if body.starts_with("client") {
        "Client"
    } else if body.starts_with("group") || body.starts_with("icon") {
        "Group"
    } else if body.starts_with("ft") {
        "File Transfer"
    } else {
        "Other"
    }
}

/// A `_needed_*` companion permission — the "needed power" twin of a
/// `*_power` skill permission. The catalog ships ~255 of these; they double
/// the catalog's apparent size, so the Catalog view hides them behind an
/// opt-in toggle (issue scope: "hide the 255 `i_needed_*` companion perms").
pub(crate) fn is_companion(permsid: &str) -> bool {
    permsid.contains("_needed_")
}

/// One editable permission row in the draft. Presence in the draft `Vec`
/// means the permission is set on the group; removing it drops the row.
#[derive(Clone, PartialEq, Debug)]
struct DraftPerm {
    permsid: String,
    value: i64,
    negated: bool,
    skip: bool,
}

impl DraftPerm {
    fn from_item(p: &GroupPermItem) -> Self {
        Self {
            permsid: p.permsid.clone(),
            value: p.permvalue,
            negated: p.permnegated != 0,
            skip: p.permskip != 0,
        }
    }
}

/// Sort a draft set by `permsid` so the dirty check is order-insensitive.
fn sorted(mut v: Vec<DraftPerm>) -> Vec<DraftPerm> {
    v.sort_by(|a, b| a.permsid.cmp(&b.permsid));
    v
}

/// Which list view the editor is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Assigned,
    Catalog,
}

/// Type-filter segment.
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

/// Catalog view caps the rendered row count — a fully-unscoped catalog is
/// ~400 rows. The rail + search keep the working set small; this is the
/// belt-and-braces ceiling so an unfiltered Catalog tab never tries to
/// mount the whole list at once.
const CATALOG_RENDER_CAP: usize = 200;

#[derive(Props, Clone, PartialEq)]
pub struct PermissionEditorProps {
    /// Absolute path to the group's permission collection — supports
    /// `GET` (list), `PUT` (upsert one), `DELETE ?permsid=` (drop one).
    /// e.g. `/api/servers/{cid}/vs/{sid}/server-groups/{sgid}/permissions`.
    pub perms_path: String,
    /// Absolute path to the read-only permission catalog
    /// (`/api/servers/{cid}/vs/{sid}/permissions`).
    pub catalog_path: String,
    /// Whether the operator may save. Non-admins get a read-only editor.
    pub can_write: bool,
    /// `true` for channel groups — TS6 channel-group permissions carry only
    /// a value, so the negate/skip disclosure is suppressed.
    #[props(default)]
    pub channel_group: bool,
}

/// Assigned-first permission editor. See the module docs for the model.
#[component]
pub fn PermissionEditor(props: PermissionEditorProps) -> Element {
    let gate = use_auth_gate();
    let toaster = use_toaster();

    let perms_path = props.perms_path.clone();
    let catalog_path = props.catalog_path.clone();
    let can_write = props.can_write;
    let channel_group = props.channel_group;

    // Assigned permissions — the draft baseline.
    let mut assigned_res = use_resource({
        let gate = gate.clone();
        let perms_path = perms_path.clone();
        move || {
            let gate = gate.clone();
            let perms_path = perms_path.clone();
            async move {
                api::authorized_get_json::<Vec<GroupPermItem>>(&gate, &api::api_base(), &perms_path)
                    .await
            }
        }
    });

    // Catalog — names + descriptions. A catalog failure degrades the editor
    // (rows fall back to the raw `permsid`) rather than blocking it.
    let catalog_res = use_resource({
        let gate = gate.clone();
        let catalog_path = catalog_path.clone();
        move || {
            let gate = gate.clone();
            let catalog_path = catalog_path.clone();
            async move {
                api::authorized_get_json::<Vec<PermissionCatalogItem>>(
                    &gate,
                    &api::api_base(),
                    &catalog_path,
                )
                .await
            }
        }
    });

    // Draft + baseline. `draft` is `None` until the first fetch seeds it (and
    // again after a Save, so the post-save state re-seeds from server truth).
    let mut draft: Signal<Option<Vec<DraftPerm>>> = use_signal(|| None);
    let mut baseline: Signal<Vec<DraftPerm>> = use_signal(Vec::new);
    let mut busy = use_signal(|| false);
    let mut save_errors: Signal<Vec<String>> = use_signal(Vec::new);

    let mut view = use_signal(|| View::Assigned);
    let mut search = use_signal(String::new);
    let mut type_filter = use_signal(|| TypeFilter::All);
    let mut category: Signal<Option<String>> = use_signal(|| None);
    let mut show_companions = use_signal(|| false);
    // The `permsid` whose negate/skip disclosure is open, if any.
    let mut open_row: Signal<Option<String>> = use_signal(|| None);

    // Seed the draft once the assigned list resolves.
    {
        use_effect(move || {
            if let Some(Ok(rows)) = assigned_res.read_unchecked().as_ref()
                && draft.peek().is_none()
            {
                let seeded: Vec<DraftPerm> = rows.iter().map(DraftPerm::from_item).collect();
                baseline.set(seeded.clone());
                draft.set(Some(seeded));
            }
        });
    }

    let assigned_snapshot = assigned_res.read().clone();
    let Some(assigned_result) = assigned_snapshot else {
        return rsx! {
            div { class: "perm-editor",
                ul { class: "perm-skeleton",
                    for i in 0..4 {
                        li { key: "{i}", class: "skeleton perm-skeleton-row" }
                    }
                }
            }
        };
    };
    if let Err(e) = &assigned_result {
        return rsx! {
            Banner {
                variant: BannerVariant::Danger,
                title: "Could not load permissions".to_string(),
                "{format_error(e)}"
                div { class: "perm-editor-retry",
                    Button {
                        variant: ButtonVariant::Secondary,
                        size: ButtonSize::Small,
                        onclick: move |_| assigned_res.restart(),
                        "Retry"
                    }
                }
            }
        };
    }

    let Some(draft_now) = draft.read().clone() else {
        // Effect has not seeded yet — render a tick later.
        return rsx! {
            div { class: "perm-editor",
                p { class: "info-hint", "Loading permissions…" }
            }
        };
    };

    let dirty = sorted(draft_now.clone()) != sorted(baseline.read().clone());

    // Catalog lookup: permsid → description. Empty when the catalog failed.
    let catalog = match catalog_res.read().clone() {
        Some(Ok(rows)) => rows,
        _ => Vec::new(),
    };
    let catalog_failed = matches!(catalog_res.read().clone(), Some(Err(_)));
    let describe = {
        let catalog = catalog.clone();
        move |permsid: &str| -> String {
            catalog
                .iter()
                .find(|c| c.permname == permsid)
                .map(|c| c.permdesc.clone())
                .filter(|d| !d.is_empty())
                .unwrap_or_default()
        }
    };

    let query = search.read().trim().to_lowercase();
    let active_view = *view.read();
    let active_type = *type_filter.read();
    let selected_category = category.read().clone();

    // Mutation helpers are the free `apply_*` functions below — they take
    // the `Copy` draft signal by value so each event handler gets its own
    // copy without the `FnMut`-capture dance.

    let on_reset = move |_| {
        draft.set(Some(baseline.peek().clone()));
        save_errors.set(Vec::new());
    };

    let on_save = {
        let gate = gate.clone();
        let perms_path = perms_path.clone();
        move |_| {
            if *busy.peek() || !can_write {
                return;
            }
            let draft_set = draft.peek().clone().unwrap_or_default();
            let baseline_set = baseline.peek().clone();
            let gate = gate.clone();
            let perms_path = perms_path.clone();
            let toaster = toaster;
            busy.set(true);
            save_errors.set(Vec::new());
            spawn(async move {
                let failures =
                    save_batch(&gate, &perms_path, &draft_set, &baseline_set, channel_group).await;
                busy.set(false);
                if failures.is_empty() {
                    toaster.push(ToastVariant::Success, "Permissions saved", None);
                } else {
                    toaster.push(
                        ToastVariant::Danger,
                        "Some permissions did not save",
                        Some(format!(
                            "{} row(s) failed — see the editor.",
                            failures.len()
                        )),
                    );
                }
                save_errors.set(failures);
                // Re-seed from server truth so a partial failure shows the
                // real post-save state, not the optimistic draft.
                draft.set(None);
                assigned_res.restart();
            });
        }
    };

    // ── row sets ────────────────────────────────────────────────────────
    let assigned_rows: Vec<DraftPerm> = {
        let mut rows = draft_now.clone();
        rows.retain(|r| {
            active_type.matches(perm_kind(&r.permsid))
                && row_matches_query(&r.permsid, &describe(&r.permsid), &query)
        });
        sorted(rows)
    };

    rsx! {
        div { class: "perm-editor",

            if catalog_failed {
                Banner {
                    variant: BannerVariant::Warning,
                    title: "Catalog unavailable".to_string(),
                    "Permission descriptions could not be loaded — rows show the raw permission id. Editing still works."
                }
            }

            // ── toolbar: view toggle · search · type filter ─────────────
            div { class: "perm-toolbar",
                div { class: "tabs", role: "tablist", "aria-label": "Permission view",
                    button {
                        r#type: "button",
                        role: "tab",
                        class: if active_view == View::Assigned { "tab is-active" } else { "tab" },
                        "aria-selected": if active_view == View::Assigned { "true" } else { "false" },
                        onclick: move |_| view.set(View::Assigned),
                        "Assigned ({draft_now.len()})"
                    }
                    button {
                        r#type: "button",
                        role: "tab",
                        class: if active_view == View::Catalog { "tab is-active" } else { "tab" },
                        "aria-selected": if active_view == View::Catalog { "true" } else { "false" },
                        onclick: move |_| view.set(View::Catalog),
                        "Catalog"
                    }
                }
                input {
                    class: "input perm-search",
                    r#type: "search",
                    placeholder: "Filter permissions…",
                    "aria-label": "Filter permissions by name or description",
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

            // ── body ────────────────────────────────────────────────────
            if active_view == View::Assigned {
                if draft_now.is_empty() {
                    div { class: "empty",
                        div { class: "icon", "⚒" }
                        h3 { "No permissions set" }
                        p { "This group sets no permissions yet. Add one from the catalog." }
                        if can_write {
                            Button {
                                variant: ButtonVariant::Primary,
                                size: ButtonSize::Small,
                                onclick: move |_| view.set(View::Catalog),
                                "+ Add permission"
                            }
                        }
                    }
                } else if assigned_rows.is_empty() {
                    div { class: "empty",
                        div { class: "icon", "🔍" }
                        h3 { "No matching permissions" }
                        p { "No assigned permission matches the current filter." }
                        Button {
                            variant: ButtonVariant::Secondary,
                            size: ButtonSize::Small,
                            onclick: move |_| {
                                search.set(String::new());
                                type_filter.set(TypeFilter::All);
                            },
                            "Clear filters"
                        }
                    }
                } else {
                    ul { class: "perm-list",
                        for row in assigned_rows.iter().cloned() {
                            PermRow {
                                key: "{row.permsid}",
                                permsid: row.permsid.clone(),
                                desc: describe(&row.permsid),
                                value: row.value,
                                negated: row.negated,
                                skip: row.skip,
                                can_write,
                                channel_group,
                                in_group: true,
                                disclosure_open: open_row.read().as_deref() == Some(row.permsid.as_str()),
                                on_set_value: EventHandler::new(move |(p, v): (String, i64)| apply_set_value(draft, &p, v)),
                                on_set_flags: EventHandler::new(move |(p, n, s): (String, bool, bool)| apply_set_flags(draft, &p, n, s)),
                                on_toggle_disclosure: EventHandler::new(move |p: String| {
                                    let cur = open_row.peek().clone();
                                    if cur.as_deref() == Some(p.as_str()) {
                                        open_row.set(None);
                                    } else {
                                        open_row.set(Some(p));
                                    }
                                }),
                                on_remove: EventHandler::new(move |p: String| apply_remove(draft, &p)),
                                on_add: EventHandler::new(move |_: String| {}),
                            }
                        }
                    }
                    div { class: "perm-list-foot",
                        span { class: "info-hint", "{assigned_rows.len()} of {draft_now.len()} assigned" }
                        if can_write {
                            Button {
                                variant: ButtonVariant::Secondary,
                                size: ButtonSize::Small,
                                onclick: move |_| view.set(View::Catalog),
                                "+ Add permission"
                            }
                        }
                    }
                }
            } else {
                CatalogView {
                    catalog: catalog.clone(),
                    draft: draft_now.clone(),
                    query: query.clone(),
                    type_filter: active_type,
                    selected_category: selected_category.clone(),
                    show_companions: *show_companions.read(),
                    can_write,
                    on_pick_category: EventHandler::new(move |c| category.set(c)),
                    on_toggle_companions: EventHandler::new(move |_: ()| {
                        let cur = *show_companions.peek();
                        show_companions.set(!cur);
                    }),
                    on_add: EventHandler::new(move |p: String| apply_add(draft, p)),
                    on_remove: EventHandler::new(move |p: String| apply_remove(draft, &p)),
                }
            }

            // ── partial-failure honesty ─────────────────────────────────
            if !save_errors.read().is_empty() {
                Banner {
                    variant: BannerVariant::Danger,
                    title: "Some permissions failed to save".to_string(),
                    ul { class: "perm-error-list",
                        for (i , line) in save_errors.read().iter().enumerate() {
                            li { key: "{i}", "{line}" }
                        }
                    }
                }
            }

            // ── sticky save bar ─────────────────────────────────────────
            if can_write {
                div { class: "perm-save-bar",
                    Button {
                        variant: ButtonVariant::Primary,
                        kind: ButtonType::Button,
                        disabled: !dirty,
                        loading: *busy.read(),
                        onclick: on_save,
                        "Save changes"
                    }
                    Button {
                        variant: ButtonVariant::Ghost,
                        kind: ButtonType::Button,
                        disabled: !dirty || *busy.read(),
                        onclick: on_reset,
                        "Reset"
                    }
                    if dirty {
                        span { class: "info-hint", "{change_count(&draft_now, &baseline.read())} unsaved change(s)" }
                    }
                }
            }
        }
    }
}

/// The editor's in-flight draft signal — `None` until the first fetch
/// seeds it. Aliased so the `apply_*` helpers read cleanly.
type DraftSignal = Signal<Option<Vec<DraftPerm>>>;

/// Set a numeric/boolean row's value in the draft.
fn apply_set_value(mut draft: DraftSignal, permsid: &str, value: i64) {
    draft.with_mut(|d| {
        if let Some(set) = d.as_mut()
            && let Some(row) = set.iter_mut().find(|r| r.permsid == permsid)
        {
            row.value = value;
        }
    });
}

/// Set a row's negate / skip flags in the draft.
fn apply_set_flags(mut draft: DraftSignal, permsid: &str, negated: bool, skip: bool) {
    draft.with_mut(|d| {
        if let Some(set) = d.as_mut()
            && let Some(row) = set.iter_mut().find(|r| r.permsid == permsid)
        {
            row.negated = negated;
            row.skip = skip;
        }
    });
}

/// Drop a permission from the draft (remove from group).
fn apply_remove(mut draft: DraftSignal, permsid: &str) {
    draft.with_mut(|d| {
        if let Some(set) = d.as_mut() {
            set.retain(|r| r.permsid != permsid);
        }
    });
}

/// Add a catalog permission to the draft. Boolean adds default to "on" (the
/// operator added it to grant it); numeric adds default to 0, ready to tune.
fn apply_add(mut draft: DraftSignal, permsid: String) {
    draft.with_mut(|d| {
        if let Some(set) = d.as_mut()
            && !set.iter().any(|r| r.permsid == permsid)
        {
            let value = i64::from(perm_kind(&permsid) == PermKind::Boolean);
            set.push(DraftPerm {
                permsid,
                value,
                negated: false,
                skip: false,
            });
        }
    });
}

/// Number of rows that differ between the draft and the baseline — drives
/// the "N unsaved changes" hint.
fn change_count(draft: &[DraftPerm], baseline: &[DraftPerm]) -> usize {
    let mut n = 0;
    for d in draft {
        match baseline.iter().find(|b| b.permsid == d.permsid) {
            None => n += 1,
            Some(b) if b != d => n += 1,
            _ => {}
        }
    }
    n += baseline
        .iter()
        .filter(|b| !draft.iter().any(|d| d.permsid == b.permsid))
        .count();
    n
}

/// Live substring match over the permission name and its description.
fn row_matches_query(permsid: &str, desc: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    permsid.to_lowercase().contains(query) || desc.to_lowercase().contains(query)
}

/// Diff the draft against the baseline and issue the per-row WebQuery batch.
/// Returns one human line per failed row; an empty `Vec` means full success.
async fn save_batch(
    gate: &Arc<RefreshGate>,
    perms_path: &str,
    draft: &[DraftPerm],
    baseline: &[DraftPerm],
    channel_group: bool,
) -> Vec<String> {
    let mut failures = Vec::new();
    let base = api::api_base();

    // Upserts — new rows and changed rows.
    for d in draft {
        let prior = baseline.iter().find(|b| b.permsid == d.permsid);
        let changed = match prior {
            None => true,
            Some(p) => p != d,
        };
        if !changed {
            continue;
        }
        let body = GroupPermSetRequest {
            permsid: d.permsid.clone(),
            permvalue: d.value,
            // The channel-group route ignores these, but sending `false`
            // keeps the request shape uniform.
            permnegated: !channel_group && d.negated,
            permskip: !channel_group && d.skip,
        };
        if let Err(e) =
            api::authorized_put_json::<GroupPermSetRequest, ()>(gate, &base, perms_path, &body)
                .await
        {
            failures.push(format!("{}: {}", d.permsid, format_error(&e)));
        }
    }

    // Removals — baseline rows the draft dropped.
    for b in baseline {
        if draft.iter().any(|d| d.permsid == b.permsid) {
            continue;
        }
        let path = format!("{perms_path}?permsid={}", b.permsid);
        if let Err(e) = api::authorized_delete(gate, &base, &path).await {
            failures.push(format!("{}: {}", b.permsid, format_error(&e)));
        }
    }

    failures
}

// ── individual permission row ───────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct PermRowProps {
    permsid: String,
    desc: String,
    value: i64,
    negated: bool,
    skip: bool,
    can_write: bool,
    channel_group: bool,
    /// `true` in the Assigned view / for catalog rows already on the group.
    in_group: bool,
    disclosure_open: bool,
    on_set_value: EventHandler<(String, i64)>,
    on_set_flags: EventHandler<(String, bool, bool)>,
    on_toggle_disclosure: EventHandler<String>,
    on_remove: EventHandler<String>,
    on_add: EventHandler<String>,
}

/// One permission row — type chip, name, control, set-state tags and the
/// `⋯` negate/skip disclosure.
#[component]
fn PermRow(props: PermRowProps) -> Element {
    let kind = perm_kind(&props.permsid);
    let permsid = props.permsid.clone();
    let name = if props.desc.is_empty() {
        props.permsid.clone()
    } else {
        props.desc.clone()
    };

    let on_set_value = props.on_set_value;
    let on_set_flags = props.on_set_flags;
    let on_toggle = props.on_toggle_disclosure;
    let on_remove = props.on_remove;
    let on_add = props.on_add;

    rsx! {
        li { class: "perm-row",
            div { class: "perm-row-head",
                span { class: "{kind.chip_class()}", title: "{kind_title(kind)}", "{kind.chip()}" }

                div { class: "perm-row-main",
                    // Control + name.
                    if kind == PermKind::Boolean {
                        Switch {
                            label: name.clone(),
                            checked: props.value != 0,
                            disabled: !props.can_write || !props.in_group,
                            onchange: {
                                let permsid = permsid.clone();
                                move |next: bool| on_set_value.call((permsid.clone(), i64::from(next)))
                            },
                        }
                    } else {
                        div { class: "perm-num-row",
                            span { class: "perm-row-name", "{name}" }
                            input {
                                class: "input perm-num-input",
                                r#type: "number",
                                min: "0",
                                "aria-label": "Value for {props.permsid}",
                                value: "{props.value}",
                                disabled: !props.can_write || !props.in_group,
                                oninput: {
                                    let permsid = permsid.clone();
                                    move |e: FormEvent| {
                                        if let Ok(v) = e.value().trim().parse::<i64>() {
                                            on_set_value.call((permsid.clone(), v));
                                        }
                                    }
                                },
                            }
                        }
                    }
                    div { class: "perm-row-meta",
                        code { class: "mod-grant-key", "{props.permsid}" }
                        if props.in_group && props.negated {
                            span { class: "tag tag-danger", "Negated" }
                        }
                        if props.in_group && props.skip {
                            span { class: "tag tag-warning", "Skip" }
                        }
                    }
                }

                div { class: "perm-row-actions",
                    if props.in_group {
                        if props.can_write && !props.channel_group {
                            button {
                                class: "btn btn-ghost btn-sm",
                                r#type: "button",
                                "aria-label": "Negate / skip options for {props.permsid}",
                                "aria-expanded": if props.disclosure_open { "true" } else { "false" },
                                title: "Negate / skip",
                                onclick: {
                                    let permsid = permsid.clone();
                                    move |_| on_toggle.call(permsid.clone())
                                },
                                "\u{22EF}"
                            }
                        }
                        if props.can_write {
                            button {
                                class: "btn btn-ghost btn-sm row-action-danger",
                                r#type: "button",
                                "aria-label": "Remove {props.permsid} from group",
                                title: "Remove from group",
                                onclick: {
                                    let permsid = permsid.clone();
                                    move |_| on_remove.call(permsid.clone())
                                },
                                "Remove"
                            }
                        }
                    } else if props.can_write {
                        button {
                            class: "btn btn-secondary btn-sm",
                            r#type: "button",
                            "aria-label": "Add {props.permsid} to group",
                            onclick: {
                                let permsid = permsid.clone();
                                move |_| on_add.call(permsid.clone())
                            },
                            "Add"
                        }
                    } else {
                        span { class: "tag tag-neutral", "Set" }
                    }
                }
            }

            // ── negate / skip disclosure ────────────────────────────────
            if props.in_group && props.disclosure_open && props.can_write && !props.channel_group {
                div { class: "perm-disclosure",
                    label { class: "perm-flag",
                        input {
                            r#type: "checkbox",
                            checked: props.negated,
                            onchange: {
                                let permsid = permsid.clone();
                                let skip = props.skip;
                                move |e: FormEvent| on_set_flags.call((permsid.clone(), e.checked(), skip))
                            },
                        }
                        div { class: "perm-flag-text",
                            span { class: "perm-flag-name", "Negate" }
                            span { class: "perm-flag-gloss", "Explicitly deny this, overriding other groups." }
                        }
                    }
                    label { class: "perm-flag",
                        input {
                            r#type: "checkbox",
                            checked: props.skip,
                            onchange: {
                                let permsid = permsid.clone();
                                let negated = props.negated;
                                move |e: FormEvent| on_set_flags.call((permsid.clone(), negated, e.checked()))
                            },
                        }
                        div { class: "perm-flag-text",
                            span { class: "perm-flag-name", "Skip" }
                            span { class: "perm-flag-gloss", "Don't pass this down to channel-group permissions." }
                        }
                    }
                }
            }
        }
    }
}

fn kind_title(kind: PermKind) -> &'static str {
    match kind {
        PermKind::Boolean => "Boolean permission — on or off",
        PermKind::Numeric => "Numeric permission — a skill-level value",
    }
}

// ── catalog view ────────────────────────────────────────────────────────

#[derive(Props, Clone, PartialEq)]
struct CatalogViewProps {
    catalog: Vec<PermissionCatalogItem>,
    draft: Vec<DraftPerm>,
    query: String,
    type_filter: TypeFilter,
    selected_category: Option<String>,
    show_companions: bool,
    can_write: bool,
    on_pick_category: EventHandler<Option<String>>,
    on_toggle_companions: EventHandler<()>,
    on_add: EventHandler<String>,
    on_remove: EventHandler<String>,
}

/// The full-catalog drill-in: a scope rail + the filtered, capped row list.
#[component]
fn CatalogView(props: CatalogViewProps) -> Element {
    let on_pick = props.on_pick_category;
    let on_toggle_companions = props.on_toggle_companions;
    let on_add = props.on_add;
    let on_remove = props.on_remove;

    if props.catalog.is_empty() {
        return rsx! {
            div { class: "empty",
                div { class: "icon", "📖" }
                h3 { "Catalog unavailable" }
                p { "The permission catalog could not be loaded. Try again from the Assigned view." }
            }
        };
    }

    // Pre-filter: companion perms, type, search. The rail counts and the
    // row list both work off this set.
    let query = props.query.clone();
    let type_filter = props.type_filter;
    let show_companions = props.show_companions;
    let visible: Vec<&PermissionCatalogItem> = props
        .catalog
        .iter()
        .filter(|c| show_companions || !is_companion(&c.permname))
        .filter(|c| type_filter.matches(perm_kind(&c.permname)))
        .filter(|c| row_matches_query(&c.permname, &c.permdesc, &query))
        .collect();

    // Rail: scope → count over the visible set.
    let scope_count = |scope: &str| {
        visible
            .iter()
            .filter(|c| perm_scope(&c.permname) == scope)
            .count()
    };

    let selected = props.selected_category.clone();
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
                            onclick: move |_| on_pick.call(None),
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
                                        onclick: move |_| on_pick.call(Some(scope.to_string())),
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
                        checked: show_companions,
                        onchange: move |_| on_toggle_companions.call(()),
                    }
                    span { "Show companion (needed-power) perms" }
                }
            }

            div { class: "perm-catalog-list",
                if rendered.is_empty() {
                    div { class: "empty",
                        div { class: "icon", "🔍" }
                        h3 { "No matching permissions" }
                        p { "No catalog permission matches the current filter." }
                    }
                } else {
                    ul { class: "perm-list",
                        for c in rendered.iter() {
                            {
                                let permsid = c.permname.clone();
                                let assigned = props.draft.iter().find(|d| d.permsid == permsid);
                                let (value, negated, skip) = assigned
                                    .map(|d| (d.value, d.negated, d.skip))
                                    .unwrap_or((0, false, false));
                                rsx! {
                                    PermRow {
                                        key: "{permsid}",
                                        permsid: permsid.clone(),
                                        desc: c.permdesc.clone(),
                                        value,
                                        negated,
                                        skip,
                                        can_write: props.can_write,
                                        channel_group: true,
                                        in_group: assigned.is_some(),
                                        disclosure_open: false,
                                        on_set_value: EventHandler::new(move |_: (String, i64)| {}),
                                        on_set_flags: EventHandler::new(move |_: (String, bool, bool)| {}),
                                        on_toggle_disclosure: EventHandler::new(move |_: String| {}),
                                        on_remove: EventHandler::new(move |p| on_remove.call(p)),
                                        on_add: EventHandler::new(move |p| on_add.call(p)),
                                    }
                                }
                            }
                        }
                    }
                    if capped {
                        p { class: "info-hint perm-cap-hint",
                            "Showing {CATALOG_RENDER_CAP} of {total}. Pick a category or refine the search to narrow the list."
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_is_inferred_from_prefix() {
        assert_eq!(perm_kind("b_client_kick_from_channel"), PermKind::Boolean);
        assert_eq!(perm_kind("i_channel_modify_power"), PermKind::Numeric);
        // Unprefixed entries fall back to numeric.
        assert_eq!(perm_kind("weird_perm"), PermKind::Numeric);
    }

    #[test]
    fn scope_maps_known_prefixes() {
        assert_eq!(perm_scope("b_virtualserver_modify_name"), "Virtual Server");
        assert_eq!(perm_scope("i_channel_modify_power"), "Channel");
        assert_eq!(perm_scope("b_client_kick_from_server"), "Client");
        assert_eq!(perm_scope("i_group_modify_power"), "Group");
        assert_eq!(perm_scope("b_ft_file_upload"), "File Transfer");
        assert_eq!(perm_scope("b_serverinstance_modify_settings"), "Global");
        assert_eq!(perm_scope("i_icon_id"), "Group");
        assert_eq!(perm_scope("b_made_up_thing"), "Other");
    }

    #[test]
    fn companion_perms_are_the_needed_twins() {
        assert!(is_companion("i_channel_needed_modify_power"));
        assert!(is_companion("i_client_needed_kick_from_server_power"));
        assert!(!is_companion("i_channel_modify_power"));
        assert!(!is_companion("b_client_kick_from_channel"));
    }

    #[test]
    fn query_matches_name_and_description() {
        assert!(row_matches_query(
            "b_client_kick_from_channel",
            "Kick a client",
            "kick"
        ));
        assert!(row_matches_query(
            "i_channel_modify_power",
            "Power to modify",
            "modify"
        ));
        assert!(row_matches_query("anything", "desc", ""));
        assert!(!row_matches_query("b_client_kick", "Kick a client", "ban"));
    }

    fn dp(permsid: &str, value: i64) -> DraftPerm {
        DraftPerm {
            permsid: permsid.to_string(),
            value,
            negated: false,
            skip: false,
        }
    }

    #[test]
    fn change_count_tallies_adds_edits_and_removes() {
        let baseline = vec![dp("b_a", 1), dp("i_b", 10), dp("b_c", 1)];
        // b_a unchanged, i_b edited, b_c removed, b_d added.
        let draft = vec![dp("b_a", 1), dp("i_b", 25), dp("b_d", 1)];
        assert_eq!(change_count(&draft, &baseline), 3);
    }

    #[test]
    fn change_count_zero_when_identical() {
        let set = vec![dp("b_a", 1), dp("i_b", 10)];
        assert_eq!(change_count(&set, &set.clone()), 0);
    }

    #[test]
    fn sorted_is_order_insensitive() {
        let a = vec![dp("i_b", 1), dp("b_a", 1)];
        let b = vec![dp("b_a", 1), dp("i_b", 1)];
        assert_eq!(sorted(a), sorted(b));
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
}
