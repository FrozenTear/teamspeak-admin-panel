//! `/watch` — play a MoQ broadcast from the media sidecar.
//!
//! The page opens a panel session (`POST /api/watch/sessions`), refreshes
//! the subscribe ticket before it expires, reads the shared playhead, and
//! deletes the session on Leave or unmount. WebTransport uses the grant's
//! `relayUrl`, `alpn`, tracks, and `certHash`. The ticket itself is not
//! sent on that connection.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use dioxus::prelude::*;

use crate::client::dioxus::{use_auth_gate, use_session};
use crate::client::session::RefreshGate;
use crate::client::store::AuthState;
use crate::client::watch::{
    self, PlayheadView, RelayCert, TicketGrant, WatchFailure, format_playhead,
    format_ticket_countdown, player_mount_key, relay_cert, ticket_refresh_delay_ms,
};
use crate::ui::components::{
    Banner, BannerVariant, Button, ButtonVariant, CertTrust, PlayerDebug, VideoPlayer,
};
use crate::ui::routes::Route;

const DEFAULT_BROADCAST: &str = "lavfi-spike";

/// Signals the watch loops and click handlers share. `Signal` is `Copy`.
#[derive(Clone, Copy)]
struct WatchIo {
    live: Signal<Option<TicketGrant>>,
    expires_at: Signal<i64>,
    failure: Signal<Option<WatchFailure>>,
    conflict: Signal<Option<WatchFailure>>,
    busy: Signal<bool>,
    playhead: Signal<Option<PlayheadView>>,
    unmuted: Signal<bool>,
}

#[component]
pub fn WatchPage() -> Element {
    let session = use_session();
    let gate = use_auth_gate();
    let nav = use_navigator();

    let mut broadcast: Signal<String> = use_signal(|| DEFAULT_BROADCAST.to_string());
    let mut io = WatchIo {
        live: use_signal(|| None),
        expires_at: use_signal(|| 0),
        failure: use_signal(|| None),
        conflict: use_signal(|| None),
        busy: use_signal(|| false),
        playhead: use_signal(|| None),
        unmuted: use_signal(|| false),
    };
    let now_unix: Signal<i64> = use_signal(watch::unix_now);
    let debug: Signal<PlayerDebug> = use_signal(PlayerDebug::default);

    let stop = use_hook(|| Rc::new(Cell::new(false)));
    let slot: Rc<RefCell<Option<String>>> = use_hook(|| Rc::new(RefCell::new(None)));
    {
        let stop = stop.clone();
        let slot = slot.clone();
        let gate = gate.clone();
        let _leave = use_hook(move || Rc::new(LeaveGuard { stop, slot, gate }));
    }

    #[cfg(target_arch = "wasm32")]
    {
        let gate = gate.clone();
        let slot = slot.clone();
        let stop = stop.clone();
        let mut now_unix = now_unix;
        let _tick = use_future(move || {
            let gate = gate.clone();
            let slot = slot.clone();
            let stop = stop.clone();
            async move {
                loop {
                    gloo_timers::future::TimeoutFuture::new(1_000).await;
                    if stop.get() {
                        break;
                    }
                    now_unix.set(watch::unix_now());
                    refresh_if_due(&gate, &slot, &stop, io).await;
                    poll_playhead(&gate, &slot, &stop, io).await;
                }
            }
        });
    }

    let watching = io.live.read().is_some();
    let broadcast_value = broadcast.read().clone();
    let gate_for_watch = gate.clone();
    let slot_for_watch = slot.clone();
    let gate_for_take = gate.clone();
    let slot_for_take = slot.clone();
    let gate_for_leave = gate.clone();
    let slot_for_leave = slot.clone();

    let sign_in = move |_| {
        let next = current_authed_path();
        session.replace(AuthState::Anonymous);
        nav.replace(Route::LoginPage { next: Some(next) });
    };

    let (conn, decoded, dropped, waits) = {
        let dbg = debug.read();
        (
            dbg.state.describe(),
            dbg.frames_decoded,
            dbg.frames_dropped,
            dbg.keyframe_waits,
        )
    };
    let countdown = io
        .live
        .read()
        .as_ref()
        .map(|grant| format_ticket_countdown(*now_unix.read(), grant.ticket_expires_at))
        .unwrap_or_else(|| "—".into());
    let debug_line = format!(
        "conn {conn} · decoded {decoded} · dropped {dropped} · key waits {waits} · ticket {countdown}"
    );
    let playhead_line = io
        .playhead
        .read()
        .as_ref()
        .map(|head| format_playhead(head.position_ms, head.paused))
        .unwrap_or_else(|| "waiting for playhead".into());
    let grant = io.live.read().clone();
    let failure = io.failure.read().clone();
    let conflict = io.conflict.read().clone();
    let busy = *io.busy.read();
    let unmuted = *io.unmuted.read();

    rsx! {
        div { class: "crumb", "Watch" }
        section { class: "page-header",
            div { class: "page-title-block",
                h1 { "Watch" }
                p { class: "page-lede",
                    "Play a MoQ broadcast from the media sidecar. Chromium (Chrome or Edge) is required."
                }
            }
        }
        section { class: "stack-md",
            if let Some(err) = failure.as_ref() {
                Banner {
                    variant: BannerVariant::Danger,
                    title: err.title().to_string(),
                    "{err.body()}"
                    if err.needs_sign_in() {
                        div { class: "banner-actions",
                            button {
                                r#type: "button",
                                class: "btn btn-primary",
                                onclick: sign_in,
                                "Sign in again"
                            }
                        }
                    }
                }
            }
            if let Some(active) = conflict.as_ref() {
                Banner {
                    variant: BannerVariant::Warning,
                    title: active.title().to_string(),
                    "{active.body()}"
                    div { class: "banner-actions",
                        Button {
                            variant: ButtonVariant::Primary,
                            disabled: busy,
                            onclick: move |_| queue_open(true, gate_for_take.clone(), slot_for_take.clone(), broadcast, io),
                            "Take over"
                        }
                        Button {
                            variant: ButtonVariant::Secondary,
                            onclick: move |_| io.conflict.set(None),
                            "Cancel"
                        }
                    }
                }
            }
            label { class: "field",
                span { class: "field-label", "Broadcast" }
                input {
                    class: "input",
                    id: "watch-broadcast",
                    value: "{broadcast_value}",
                    disabled: watching || busy,
                    oninput: move |evt| broadcast.set(evt.value()),
                }
            }
            div { class: "page-actions",
                if watching {
                    Button {
                        variant: ButtonVariant::Secondary,
                        onclick: move |_| {
                            release_session(&slot_for_leave, &gate_for_leave);
                            clear_player(io);
                        },
                        "Leave"
                    }
                    if !unmuted {
                        Button {
                            variant: ButtonVariant::Primary,
                            onclick: move |_| io.unmuted.set(true),
                            "Click to unmute"
                        }
                    }
                } else {
                    Button {
                        variant: ButtonVariant::Primary,
                        disabled: busy || broadcast_value.trim().is_empty(),
                        loading: busy,
                        onclick: move |_| queue_open(false, gate_for_watch.clone(), slot_for_watch.clone(), broadcast, io),
                        "Watch"
                    }
                }
            }
            if let Some(grant) = grant.as_ref() {
                { live_player(grant, unmuted, debug) }
                p { class: "muted", "Playhead {playhead_line}" }
                p {
                    class: "muted",
                    style: "font-family: ui-monospace, monospace; font-size: 12px;",
                    "{debug_line}"
                }
            }
        }
    }
}

fn clear_player(mut io: WatchIo) {
    io.live.set(None);
    io.playhead.set(None);
    io.unmuted.set(false);
    io.conflict.set(None);
    io.expires_at.set(0);
}

fn queue_open(
    replace: bool,
    gate: Arc<RefreshGate>,
    slot: Rc<RefCell<Option<String>>>,
    broadcast: Signal<String>,
    mut io: WatchIo,
) {
    if *io.busy.peek() {
        return;
    }
    let name = broadcast.peek().trim().to_string();
    io.busy.set(true);
    if !replace {
        io.conflict.set(None);
    }
    spawn(async move {
        open_session(gate, slot, name, replace, io).await;
    });
}

fn live_player(grant: &TicketGrant, unmuted: bool, debug: Signal<PlayerDebug>) -> Element {
    let cert = match relay_cert(grant) {
        Ok(RelayCert::System) => CertTrust::System,
        Ok(RelayCert::Sha256Hex(hex)) => CertTrust::Sha256Hex(hex),
        Err(_) => return rsx! { "" },
    };
    let muted = !unmuted;
    let key = player_mount_key(grant, muted);
    let relay_url = grant.relay_url.clone();
    let namespace = grant.broadcast.clone();
    let alpn = grant.alpn.clone();
    let video_track = grant.video_track.clone();
    let audio_track = grant.audio_track.clone();
    let trust_note = if matches!(cert, CertTrust::System) {
        "No certificate hash from the sidecar; the browser trust store is used."
    } else {
        ""
    };
    rsx! {
        VideoPlayer {
            key: "{key}",
            relay_url,
            namespace,
            autoplay: true,
            muted,
            alpn,
            video_track,
            audio_track,
            cert,
            debug: Some(debug),
        }
        if !trust_note.is_empty() {
            p { class: "muted", "{trust_note}" }
        }
    }
}

async fn open_session(
    gate: Arc<RefreshGate>,
    slot: Rc<RefCell<Option<String>>>,
    broadcast: String,
    replace: bool,
    mut io: WatchIo,
) {
    if broadcast.is_empty() {
        io.failure.set(Some(WatchFailure::InvalidBroadcast));
        io.busy.set(false);
        return;
    }
    match watch::create_session(&gate, &broadcast, replace).await {
        Ok(grant) => {
            let cert_err = relay_cert(&grant).err();
            io.expires_at.set(grant.ticket_expires_at);
            *slot.borrow_mut() = Some(grant.session_id.clone());
            io.playhead.set(None);
            io.live.set(Some(grant));
            io.conflict.set(None);
            io.failure.set(cert_err);
            io.busy.set(false);
        }
        Err(err @ WatchFailure::SessionActive { .. }) => {
            io.conflict.set(Some(err));
            io.failure.set(None);
            io.busy.set(false);
        }
        Err(err) => {
            io.failure.set(Some(err));
            io.conflict.set(None);
            io.busy.set(false);
        }
    }
}

async fn refresh_if_due(
    gate: &Arc<RefreshGate>,
    slot: &Rc<RefCell<Option<String>>>,
    stop: &Rc<Cell<bool>>,
    mut io: WatchIo,
) {
    if stop.get() {
        return;
    }
    let Some(session_id) = slot.borrow().clone() else {
        return;
    };
    let exp = *io.expires_at.peek();
    if exp == 0 || ticket_refresh_delay_ms(watch::unix_now(), exp) > 1_000 {
        return;
    }
    match watch::refresh_ticket(gate, &session_id).await {
        Ok(grant) => {
            if stop.get() || slot.borrow().as_deref() != Some(session_id.as_str()) {
                return;
            }
            io.expires_at.set(grant.ticket_expires_at);
            io.live.set(Some(grant));
        }
        Err(err) => {
            if stop.get() || slot.borrow().as_deref() != Some(session_id.as_str()) {
                return;
            }
            apply_fatal(slot, gate, &mut io, err);
        }
    }
}

async fn poll_playhead(
    gate: &Arc<RefreshGate>,
    slot: &Rc<RefCell<Option<String>>>,
    stop: &Rc<Cell<bool>>,
    mut io: WatchIo,
) {
    if stop.get() || slot.borrow().is_none() {
        return;
    }
    let Some(broadcast) = io.live.peek().as_ref().map(|grant| grant.broadcast.clone()) else {
        return;
    };
    // Once a second is enough for a display clock. Skip the odd tick so
    // the playhead request is about every two seconds.
    if watch::unix_now() % 2 != 0 {
        return;
    }
    match watch::get_playhead(gate, &broadcast).await {
        Ok(view) => {
            if !stop.get() && slot.borrow().is_some() {
                io.playhead.set(Some(view));
            }
        }
        Err(err @ WatchFailure::SessionExpired(_)) | Err(err @ WatchFailure::SessionGone) => {
            if stop.get() || slot.borrow().is_none() {
                return;
            }
            apply_fatal(slot, gate, &mut io, err);
        }
        Err(_) => {}
    }
}

fn apply_fatal(
    slot: &Rc<RefCell<Option<String>>>,
    gate: &Arc<RefreshGate>,
    io: &mut WatchIo,
    err: WatchFailure,
) {
    let fatal = matches!(
        err,
        WatchFailure::SessionExpired(_)
            | WatchFailure::SessionGone
            | WatchFailure::RelayUnconfigured
    );
    io.failure.set(Some(err));
    if fatal {
        release_session(slot, gate);
        clear_player(*io);
    }
}

fn release_session(slot: &Rc<RefCell<Option<String>>>, gate: &Arc<RefreshGate>) {
    let Some(id) = slot.borrow_mut().take() else {
        return;
    };
    spawn_delete(gate.clone(), id);
}

fn spawn_delete(gate: Arc<RefreshGate>, session_id: String) {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_bindgen_futures::spawn_local(async move {
            let _ = watch::delete_session(&gate, &session_id).await;
        });
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (gate, session_id);
    }
}

fn current_authed_path() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            let loc = window.location();
            let mut out = loc.pathname().unwrap_or_else(|_| "/watch".into());
            if let Ok(search) = loc.search()
                && !search.is_empty()
            {
                out.push_str(&search);
            }
            return out;
        }
    }
    "/watch".into()
}

struct LeaveGuard {
    stop: Rc<Cell<bool>>,
    slot: Rc<RefCell<Option<String>>>,
    gate: Arc<RefreshGate>,
}

impl Drop for LeaveGuard {
    fn drop(&mut self) {
        self.stop.set(true);
        release_session(&self.slot, &self.gate);
    }
}
