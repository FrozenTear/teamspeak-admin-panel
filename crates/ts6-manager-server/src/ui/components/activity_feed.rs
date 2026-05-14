//! Operator activity feed — PURA-73.
//!
//! Mounted in the AppShell so it stays alive across route changes; a
//! per-server WS subscription on the `clients` topic feeds it. Each
//! envelope is translated into a short human-readable line ("Operator X
//! kicked client Y") and pushed to the head of a bounded ring; old
//! entries fall off the tail at `MAX_ENTRIES`.
//!
//! Notable events also fire a toast through the [`super::toast::Toaster`]
//! context so a moderator notices a kick / ban regardless of which page
//! they are on (verification 4 — kick a client and observe propagation).

use std::collections::VecDeque;

use dioxus::prelude::*;
use serde_json::Value;

use crate::client::dioxus::use_session;
use crate::client::ws::{WsEvent, use_ws_hub};
use crate::ui::components::toast::{ToastVariant, use_toaster};
use crate::ui::layout::use_servers_context;
use crate::ui::pages::active_server;

/// Max entries kept in the in-memory feed. Older ones fall off the tail
/// when the ring overflows. 50 is enough for "last few minutes" without
/// pushing the DOM into an obviously bad render time.
const MAX_ENTRIES: usize = 50;

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityEntry {
    pub id: u64,
    pub kind: String,
    pub message: String,
    /// Unix-millis. Rendered relative to "now" by the feed; the raw value
    /// stays in the entry so the page can re-render on every animation
    /// frame without losing precision.
    pub ts: i64,
    pub variant: ToastVariant,
}

#[derive(Clone, Copy)]
pub struct ActivityFeed {
    pub entries: Signal<VecDeque<ActivityEntry>>,
}

pub fn provide_activity_feed() -> ActivityFeed {
    let entries: Signal<VecDeque<ActivityEntry>> = use_signal(VecDeque::new);
    let feed = ActivityFeed { entries };
    use_context_provider(|| feed);
    feed
}

pub fn use_activity_feed() -> ActivityFeed {
    use_context::<ActivityFeed>()
}

impl ActivityFeed {
    pub fn push(&self, entry: ActivityEntry) {
        let mut entries = self.entries;
        let mut g = entries.write();
        g.push_front(entry);
        while g.len() > MAX_ENTRIES {
            g.pop_back();
        }
    }
}

/// Subscribe to the selected server's `clients` topic. Translates every
/// envelope into an [`ActivityEntry`] + toast. Mounted from the AppShell
/// so the subscription lives across route changes.
#[component]
pub fn ActivityFeedSubscription() -> Element {
    let hub = use_ws_hub();
    let feed = use_activity_feed();
    let toaster = use_toaster();
    let servers_ctx = use_servers_context();
    let session = use_session();
    let storage = session.storage.clone();

    // Track which server is currently subscribed so we can re-subscribe
    // when the operator picks a different one. The selection lives in
    // localStorage today (`ui_prefs::SELECTED_SERVER_STORAGE_KEY`); the
    // `active_server::resolve` helper applies the same precedence rule
    // the dashboard does (persisted-id-if-present, else first row).
    let mut active_server_id: Signal<Option<i64>> = use_signal(|| None::<i64>);

    {
        let storage = storage.clone();
        use_effect(move || {
            let snap = servers_ctx.data.read().clone();
            let target = active_server::resolve(&snap, &*storage).map(|s| s.id);
            if target != *active_server_id.read() {
                active_server_id.set(target);
            }
        });
    }

    let _resource = use_resource(move || {
        let hub = hub.clone();
        let feed = feed;
        let toaster = toaster;
        let server_id = *active_server_id.read();
        async move {
            let Some(sid) = server_id else { return };
            let topic = format!("server:{sid}:clients");
            let mut handle = hub.subscribe(topic.clone()).await;
            let Some(mut rx) = handle.take_receiver() else {
                return;
            };
            // Hold the handle for the lifetime of the subscription so the
            // Drop on it (in turn) issues an unsubscribe when this future
            // is cancelled by Dioxus on the next selection change.
            let _drop_guard = handle;
            use futures::stream::StreamExt;
            while let Some(env) = rx.next().await {
                if let Some(entry) = translate(&env) {
                    feed.push(entry.clone());
                    if let Some((variant, title, detail)) = toast_for(&env) {
                        toaster.push(variant, title, detail);
                    }
                    // Suppress the unused-binding hint when the entry has
                    // already been consumed by the feed push above.
                    let _ = entry;
                } else if env.kind == "dropped" {
                    let reason = env
                        .data
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    toaster.push(
                        ToastVariant::Warning,
                        "Live event dropped",
                        Some(format!("Reason: {reason}. Some updates may be missing.")),
                    );
                }
            }
        }
    });

    rsx! { "" }
}

/// Render the list. Used by the Clients page side panel today; can be
/// reused as a global drawer if/when one ships.
#[component]
pub fn ActivityFeedList() -> Element {
    let feed = use_activity_feed();
    let entries = feed.entries.read().clone();
    rsx! {
        ol { class: "activity-feed",
            "aria-label": "Recent activity",
            if entries.is_empty() {
                li { class: "activity-feed-empty",
                    "No recent activity. Operator actions will appear here."
                }
            }
            for entry in entries.iter() {
                li { key: "{entry.id}",
                    class: "activity-entry activity-{entry.variant_class()}",
                    span { class: "activity-msg", "{entry.message}" }
                }
            }
        }
    }
}

impl ActivityEntry {
    fn variant_class(&self) -> &'static str {
        match self.variant {
            ToastVariant::Info => "info",
            ToastVariant::Success => "success",
            ToastVariant::Warning => "warning",
            ToastVariant::Danger => "danger",
        }
    }
}

fn translate(env: &WsEvent) -> Option<ActivityEntry> {
    let (variant, message) = match env.kind.as_str() {
        "ts:client:kicked_from_server" => (
            ToastVariant::Warning,
            format!(
                "Client {clid} kicked from server{reason}",
                clid = env.data.get("clid").and_then(Value::as_i64).unwrap_or(0),
                reason = render_reason(&env.data),
            ),
        ),
        "ts:client:kicked_from_channel" => (
            ToastVariant::Info,
            format!(
                "Client {clid} kicked from channel{reason}",
                clid = env.data.get("clid").and_then(Value::as_i64).unwrap_or(0),
                reason = render_reason(&env.data),
            ),
        ),
        "ts:client:moved" => (
            ToastVariant::Info,
            format!(
                "Client {clid} moved to channel {cid}",
                clid = env.data.get("clid").and_then(Value::as_i64).unwrap_or(0),
                cid = env.data.get("cid").and_then(Value::as_i64).unwrap_or(0),
            ),
        ),
        "ts:client:muted" => (
            ToastVariant::Info,
            format!(
                "Client {clid} muted",
                clid = env.data.get("clid").and_then(Value::as_i64).unwrap_or(0),
            ),
        ),
        "ts:client:unmuted" => (
            ToastVariant::Info,
            format!(
                "Client {clid} unmuted",
                clid = env.data.get("clid").and_then(Value::as_i64).unwrap_or(0),
            ),
        ),
        "ts:ban:added" => (
            ToastVariant::Warning,
            format!(
                "Ban added (banid {banid})",
                banid = env.data.get("banid").and_then(Value::as_i64).unwrap_or(0),
            ),
        ),
        "ts:ban:deleted" => (
            ToastVariant::Info,
            format!(
                "Ban removed (banid {banid})",
                banid = env.data.get("banid").and_then(Value::as_i64).unwrap_or(0),
            ),
        ),
        _ => return None,
    };
    Some(ActivityEntry {
        id: env.id,
        kind: env.kind.clone(),
        message,
        ts: env.ts,
        variant,
    })
}

fn render_reason(data: &Value) -> String {
    match data.get("reason").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => format!(" — \u{201C}{s}\u{201D}"),
        _ => String::new(),
    }
}

fn toast_for(env: &WsEvent) -> Option<(ToastVariant, String, Option<String>)> {
    match env.kind.as_str() {
        "ts:client:kicked_from_server" | "ts:client:kicked_from_channel" => {
            let entry = translate(env)?;
            Some((entry.variant, entry.message.clone(), None))
        }
        "ts:ban:added" => {
            let entry = translate(env)?;
            Some((entry.variant, entry.message.clone(), None))
        }
        _ => None,
    }
}
