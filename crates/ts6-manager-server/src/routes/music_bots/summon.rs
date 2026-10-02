//! Summon cap for one TeamSpeak server address.
//!
//! Chat requests never reach this process. The music process is what
//! enforces the cap. This route only stores the number the music
//! process has already accepted, keyed by `serverAddr`.

use std::collections::BTreeSet;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::response::Response;
use axum::routing::get;
use tracing::{error, warn};
use ts6_manager_shared::music_bots as wire;

use crate::app_state::AppState;
use crate::auth::extractors::{RequireAuth, RequireModerator};
use crate::db::Database;
use crate::music_runtime::MusicBotFront;
use crate::routes::music_bots::bots::{bot_visibility, require_server_write};
use crate::routes::music_bots::{internal, map_music_runtime_error, validation};

pub(super) fn router() -> Router<AppState> {
    Router::new().route("/api/music-summon-caps", get(list).put(set))
}

async fn list(
    State(state): State<AppState>,
    RequireAuth(user): RequireAuth,
) -> Result<Json<wire::SummonCapList>, Response> {
    let rows = crate::repos::music_summon_cap::list(&state.db)
        .await
        .map_err(|err| {
            error!(error = %err, "summon cap list failed");
            internal("summon cap list failed")
        })?;
    let mut caps = Vec::with_capacity(rows.len());
    let mut seen = BTreeSet::new();
    for row in rows {
        let server_addr = music_bot::canon_server_addr(&row.serverAddr);
        if server_addr.is_empty() || !seen.insert(server_addr.clone()) {
            continue;
        }
        if bot_visibility(&state, &user, &server_addr).await?.is_none() {
            continue;
        }
        let cap = u32::try_from(row.cap)
            .map_err(|_| internal("stored summon cap is outside the range the page can show"))?;
        caps.push(wire::SummonCap { server_addr, cap });
    }
    Ok(Json(wire::SummonCapList { caps }))
}

async fn set(
    State(state): State<AppState>,
    RequireModerator(user): RequireModerator,
    Json(req): Json<wire::SummonCap>,
) -> Result<Json<wire::SummonCap>, Response> {
    let server_addr = music_bot::canon_server_addr(&req.server_addr);
    if server_addr.is_empty() {
        return Err(validation("serverAddr must not be empty"));
    }
    if req.cap > music_bot::MAX_SUMMON_CAP {
        return Err(validation("summon cap is too large"));
    }
    let _server = require_server_write(&state, &user, &server_addr).await?;
    let mutex = state.music_bots.summon_save_mutex(&server_addr);
    let guard = mutex.clone().lock_owned().await;

    if let Err(err) = crate::repos::music_summon_cap::get(&state.db, &server_addr).await {
        error!(server = %server_addr, error = %err, "summon cap lookup failed");
        return Err(internal("summon cap lookup failed"));
    }

    let mut unsettled = UnsettledCap {
        front: state.music_bots.supervisor.clone(),
        db: state.db.clone(),
        server: server_addr.clone(),
        forwarded: None,
        pending: Some(req.cap),
        settled: false,
        handed_off: false,
        mutex: Some(mutex),
        guard: Some(guard),
    };

    if let Err(err) = forward_cap(&state.music_bots.supervisor, &server_addr, req.cap).await {
        error!(
            server = %server_addr,
            cap = req.cap,
            error = %err,
            "summon cap forward failed; reading the number the process holds"
        );
        // A failed forward is not a reject. Read until a read succeeds.
        // The new number is stored. The stored number leaves the row.
        // A third number, including no cap, is restored onto the stored
        // row and read again before this returns.
        let outcome = confirm_pending(&unsettled.front, &unsettled.db, &server_addr, req.cap).await;
        unsettled.pending = None;
        unsettled.settled = true;
        return match outcome {
            AlignOutcome::CaughtUp(cap) => {
                unsettled.forwarded = Some(cap);
                Ok(Json(wire::SummonCap { server_addr, cap }))
            }
            AlignOutcome::Restored(_) => Err(map_music_runtime_error(err)),
        };
    }
    unsettled.pending = None;
    unsettled.forwarded = Some(req.cap);

    if let Err(err) = store_cap(&state.db, &server_addr, req.cap).await {
        error!(
            server = %server_addr,
            cap = req.cap,
            error = %err,
            "summon cap was not stored; restoring the stored number"
        );
        let outcome = align_held(&mut unsettled).await;
        unsettled.settled = true;
        let cap = number_that_stuck(req.cap, outcome)
            .map_err(|()| internal("summon cap was not stored"))?;
        return Ok(Json(wire::SummonCap { server_addr, cap }));
    }

    unsettled.settled = true;
    Ok(Json(wire::SummonCap {
        server_addr,
        cap: req.cap,
    }))
}

/// Re-read the stored cap and put the process on that number. The align
/// task owns the guard, so dropping this handler does not release the
/// lock before that task finishes. A failed restore is not proof the
/// process still holds the forwarded number: that path reads until a
/// read succeeds.
async fn align_held(unsettled: &mut UnsettledCap) -> AlignOutcome {
    let Some(mutex) = unsettled.mutex.clone() else {
        return AlignOutcome::Restored(None);
    };
    // Set before the guard moves. Drop then leaves this task in charge
    // of the lock instead of starting a second align.
    unsettled.handed_off = true;
    let guard = unsettled.guard.take();
    let front = unsettled.front.clone();
    let db = unsettled.db.clone();
    let server = unsettled.server.clone();
    let forwarded = unsettled.forwarded;
    let pending = unsettled.pending;
    let handle = tokio::spawn(async move {
        align_cap(front, db, server, forwarded, pending, mutex, guard).await
    });
    match handle.await {
        Ok(outcome) => outcome,
        Err(_) => AlignOutcome::Restored(None),
    }
}

/// The number the page should show after a store write failed and align
/// ran. A catch-up that landed, or a store that already held this save's
/// number, is that number. Anything else leaves the previous number up.
fn number_that_stuck(requested: u32, outcome: AlignOutcome) -> Result<u32, ()> {
    match outcome {
        AlignOutcome::CaughtUp(cap) => Ok(cap),
        AlignOutcome::Restored(Some(cap)) if cap == requested => Ok(cap),
        AlignOutcome::Restored(_) => Err(()),
    }
}

/// Send the cap to the music process. An `Err` does not by itself mean
/// the process rejected the number: the process may already hold it.
async fn forward_cap(
    front: &MusicBotFront,
    server: &str,
    cap: u32,
) -> Result<(), crate::music_runtime::MusicRuntimeError> {
    #[cfg(test)]
    if save_fault::take_forward_error_before_accept(server) {
        return Err(crate::music_runtime::MusicRuntimeError::Unavailable(
            "summon cap forward failed before the process changed".into(),
        ));
    }
    let result = front.set_summon_cap(server, cap).await;
    #[cfg(test)]
    if result.is_ok() && save_fault::take_forward_error_after_accept(server) {
        return Err(crate::music_runtime::MusicRuntimeError::Unavailable(
            "summon cap forward failed after the process accepted the number".into(),
        ));
    }
    result
}

/// The cap the process holds, once a read succeeds. Failed reads wait.
async fn read_process_until_success(front: &MusicBotFront, server: &str) -> Option<u32> {
    let mut wait = Duration::from_millis(20);
    loop {
        #[cfg(test)]
        if save_fault::fail_process_read(server) {
            warn!(server = %server, "summon cap process read failed");
            if let Some(release) = save_fault::note_pause(server) {
                let _ = release.await;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_millis(200));
            continue;
        }
        match front.read_summon_cap(server).await {
            Ok(cap) => return cap,
            Err(err) => {
                warn!(
                    server = %server,
                    error = %err,
                    "summon cap process read failed"
                );
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(200));
    }
}

/// Write `cap` while the save lock is still held. Returns only when the
/// store shows that number. The process is already on it, so this does
/// not put the process back on the old number.
async fn store_accepted(db: &Database, server: &str, cap: u32) {
    let mut wait = Duration::from_millis(50);
    loop {
        match store_cap(db, server, cap).await {
            Ok(()) => return,
            Err(err) => {
                error!(
                    server = %server,
                    cap,
                    error = %err,
                    "summon cap the process accepted was not stored"
                );
            }
        }
        match read_cap_retry(db, server).await {
            Ok(Some(stored)) if stored == cap => return,
            Ok(_) => {}
            Err(err) => {
                error!(
                    server = %server,
                    error = %err,
                    "summon cap re-read failed; the captured number is not restored"
                );
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(200));
    }
}

async fn restore_with_retry(
    front: &MusicBotFront,
    server: &str,
    cap: Option<u32>,
) -> Result<(), crate::music_runtime::MusicRuntimeError> {
    #[cfg(test)]
    if save_fault::fail_restore(server) {
        return Err(crate::music_runtime::MusicRuntimeError::Unavailable(
            "summon cap restore failed before the process changed".into(),
        ));
    }
    let mut wait = Duration::from_millis(20);
    let mut last = None;
    for _ in 0..5 {
        match front.restore_summon_cap(server, cap).await {
            Ok(()) => {
                // An error after the restore was applied is not proof
                // the process is still on the number from before.
                #[cfg(test)]
                if save_fault::take_restore_error_after_apply(server) {
                    return Err(crate::music_runtime::MusicRuntimeError::Unavailable(
                        "summon cap restore failed after the process changed".into(),
                    ));
                }
                return Ok(());
            }
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(200));
            }
        }
    }
    Err(last.expect("restore retried"))
}

/// If this save is dropped before it settles, keep the per-server lock
/// and put the process and the store on the same number. Dropping the
/// handler is not a reject: `pending` is the number this save may already
/// have handed to the process.
pub(super) struct UnsettledCap {
    front: MusicBotFront,
    db: std::sync::Arc<Database>,
    server: String,
    /// Set only after a forward returned success, or a process read
    /// showed this number. The catch-up write uses it.
    forwarded: Option<u32>,
    /// The number still being confirmed. Set before the forward, and
    /// cleared once a process read succeeds. A drop keeps it.
    pending: Option<u32>,
    settled: bool,
    /// An align task already owns the guard. Drop must not start another.
    handed_off: bool,
    mutex: Option<std::sync::Arc<tokio::sync::Mutex<()>>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for UnsettledCap {
    fn drop(&mut self) {
        if self.settled || self.handed_off {
            return;
        }
        let Some(mutex) = self.mutex.take() else {
            return;
        };
        let guard = self.guard.take();
        let front = self.front.clone();
        let db = self.db.clone();
        let server = std::mem::take(&mut self.server);
        let forwarded = self.forwarded;
        let pending = self.pending;
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let guard = match guard {
                    Some(held) => held,
                    None => mutex.clone().lock_owned().await,
                };
                // A drop after the process is known to hold the new
                // number stores that number. It does not put the old
                // number back.
                if pending.is_none()
                    && let Some(live) = forwarded
                {
                    let _guard = guard;
                    store_accepted(&db, &server, live).await;
                    return;
                }
                align_cap(front, db, server, forwarded, pending, mutex, Some(guard)).await;
            });
        }
    }
}

/// The process and the store after align. `Restored` means both sides
/// already showed the stored number, or the process was put back on it.
/// `CaughtUp` means the store was written to the number the process holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AlignOutcome {
    Restored(Option<u32>),
    CaughtUp(u32),
}

/// Restores to attempt when a dropped save never recorded an accepted
/// number. The lock is held the whole time.
const UNACCEPTED_ALIGN_TRIES: u32 = 3;

async fn align_cap(
    front: MusicBotFront,
    db: std::sync::Arc<Database>,
    server: String,
    forwarded: Option<u32>,
    pending: Option<u32>,
    mutex: std::sync::Arc<tokio::sync::Mutex<()>>,
    guard: Option<tokio::sync::OwnedMutexGuard<()>>,
) -> AlignOutcome {
    let _guard = match guard {
        Some(held) => held,
        None => mutex.lock_owned().await,
    };
    if let Some(live) = pending {
        return confirm_pending(&front, &db, &server, live).await;
    }
    if let Some(live) = forwarded {
        return align_accepted(&front, &db, &server, live).await;
    }
    let mut wait = Duration::from_millis(50);
    let mut unaccepted = 0u32;
    loop {
        match read_cap_retry(&db, &server).await {
            Ok(stored) => {
                if restore_with_retry(&front, &server, stored).await.is_ok() {
                    return AlignOutcome::Restored(stored);
                }
                unaccepted += 1;
                if unaccepted >= UNACCEPTED_ALIGN_TRIES {
                    return AlignOutcome::Restored(stored);
                }
            }
            Err(err) => {
                error!(
                    server = %server,
                    error = %err,
                    "summon cap re-read failed; the captured number is not restored"
                );
                unaccepted += 1;
                if unaccepted >= UNACCEPTED_ALIGN_TRIES {
                    return AlignOutcome::Restored(None);
                }
            }
        }
        #[cfg(test)]
        if let Some(release) = save_fault::note_pause(&server) {
            let _ = release.await;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(200));
    }
}

/// The process accepted `live` and the store write failed. Restore the
/// stored row. When that restore reports an error, read the process and
/// act on what the read shows. Do not finish while the two differ, and
/// do not store `live` unless a read shows it.
async fn align_accepted(
    front: &MusicBotFront,
    db: &Database,
    server: &str,
    live: u32,
) -> AlignOutcome {
    let mut wait = Duration::from_millis(50);
    loop {
        let stored = match read_cap_retry(db, server).await {
            Ok(stored) => stored,
            Err(err) => {
                error!(
                    server = %server,
                    error = %err,
                    "summon cap re-read failed; the captured number is not restored"
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(200));
                continue;
            }
        };
        if Some(live) == stored {
            return AlignOutcome::Restored(stored);
        }
        if restore_with_retry(front, server, stored).await.is_ok() {
            return AlignOutcome::Restored(stored);
        }
        // The restore's error is not the number the process holds.
        #[cfg(test)]
        if let Some(release) = save_fault::note_pause(server) {
            let _ = release.await;
        }
        let seen = read_process_until_success(front, server).await;
        if seen == stored {
            return AlignOutcome::Restored(stored);
        }
        if seen == Some(live) {
            store_accepted(db, server, live).await;
            return AlignOutcome::CaughtUp(live);
        }
        warn!(
            server = %server,
            "summon cap process and store differ; restoring again under the save lock"
        );
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(200));
    }
}

/// Read the process until a read succeeds. `live` is stored when the
/// read shows it. The stored row is left unchanged when the read shows
/// that row. Any other number, including no cap, is restored onto the
/// stored row and then read again. This does not finish while the two
/// differ.
async fn confirm_pending(
    front: &MusicBotFront,
    db: &Database,
    server: &str,
    live: u32,
) -> AlignOutcome {
    let mut wait = Duration::from_millis(50);
    loop {
        let seen = read_process_until_success(front, server).await;
        if seen == Some(live) {
            store_accepted(db, server, live).await;
            return AlignOutcome::CaughtUp(live);
        }
        let stored = match read_cap_retry(db, server).await {
            Ok(stored) => stored,
            Err(err) => {
                error!(
                    server = %server,
                    error = %err,
                    "summon cap re-read failed; the captured number is not restored"
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(200));
                continue;
            }
        };
        if stored == seen {
            return AlignOutcome::Restored(stored);
        }
        let _ = restore_with_retry(front, server, stored).await;
        let again = read_process_until_success(front, server).await;
        if again == stored {
            return AlignOutcome::Restored(stored);
        }
        if again == Some(live) {
            store_accepted(db, server, live).await;
            return AlignOutcome::CaughtUp(live);
        }
        warn!(
            server = %server,
            "summon cap process and store differ; restoring the stored number under the save lock"
        );
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_millis(200));
    }
}

async fn store_cap(db: &Database, server: &str, cap: u32) -> Result<(), anyhow::Error> {
    #[cfg(test)]
    if save_fault::fail_upsert(server) {
        anyhow::bail!("summon cap store failed before the row was written");
    }
    crate::repos::music_summon_cap::upsert(db, server, cap).await
}

async fn read_cap_retry(db: &Database, server: &str) -> Result<Option<u32>, anyhow::Error> {
    let mut wait = Duration::from_millis(20);
    let mut last = None;
    for _ in 0..5 {
        match crate::repos::music_summon_cap::get(db, server).await {
            Ok(cap) => return Ok(cap),
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_millis(200));
            }
        }
    }
    Err(last.expect("cap re-read retried"))
}

/// A save that forwarded `cap` and then stopped before the store write.
/// Dropping the guard stores `cap`. It does not put the old number back.
/// Tests use this for a handler that does not return.
#[cfg(test)]
pub(super) async fn hold_forwarded_cap(state: &AppState, server: &str, cap: u32) -> UnsettledCap {
    let mutex = state.music_bots.summon_save_mutex(server);
    let guard = mutex.clone().lock_owned().await;
    state
        .music_bots
        .supervisor
        .set_summon_cap(server, cap)
        .await
        .expect("test forward");
    UnsettledCap {
        front: state.music_bots.supervisor.clone(),
        db: state.db.clone(),
        server: server.to_string(),
        forwarded: Some(cap),
        pending: None,
        settled: false,
        handed_off: false,
        mutex: Some(mutex),
        guard: Some(guard),
    }
}

/// Align once, as a dropped save would, against `front`. `forwarded` is
/// the number that front already accepted. The database is `state.db`.
#[cfg(test)]
pub(super) async fn align_cap_for_test(
    state: &AppState,
    front: MusicBotFront,
    server: &str,
    forwarded: Option<u32>,
) -> AlignOutcome {
    let mutex = state.music_bots.summon_save_mutex(server);
    let guard = mutex.clone().lock_owned().await;
    align_cap(
        front,
        state.db.clone(),
        server.to_string(),
        forwarded,
        None,
        mutex,
        Some(guard),
    )
    .await
}

/// A save dropped before a process read succeeds. `cap` is the number
/// the forward may already have handed over. The process is not changed
/// here. Dropping the returned guard confirms it by reading.
#[cfg(test)]
pub(super) async fn hold_unconfirmed_cap(state: &AppState, server: &str, cap: u32) -> UnsettledCap {
    let mutex = state.music_bots.summon_save_mutex(server);
    let guard = mutex.clone().lock_owned().await;
    UnsettledCap {
        front: state.music_bots.supervisor.clone(),
        db: state.db.clone(),
        server: server.to_string(),
        forwarded: None,
        pending: Some(cap),
        settled: false,
        handed_off: false,
        mutex: Some(mutex),
        guard: Some(guard),
    }
}

/// Cap to carry on a saved-bot push. A lookup error is `None`: the push
/// must not invent a number and must not fail bot create.
pub(super) async fn cap_for_push(state: &AppState, server_addr: &str) -> Option<u32> {
    match crate::repos::music_summon_cap::get(&state.db, server_addr).await {
        Ok(cap) => cap,
        Err(err) => {
            warn!(
                server = %server_addr,
                error = %err,
                "summon cap lookup failed; this push will not arm summon"
            );
            None
        }
    }
}

/// After a saved-bot push, write the number the process holds.
/// The caller keeps the per-server save lock. A process with no number
/// leaves the store unchanged. This does not return while a held number
/// is still missing from the store.
pub(super) async fn store_held_cap(state: &AppState, server: &str) {
    let Some(cap) = read_process_until_success(&state.music_bots.supervisor, server).await else {
        return;
    };
    store_accepted(&state.db, server, cap).await;
}

/// Fail the next `upserts` store writes and the next `restores` restore
/// attempts for `server`. Other servers are left alone.
#[cfg(test)]
pub(super) fn arm_cap_save_fault(server: &str, upserts: u32, restores: u32) {
    save_fault::arm(server, upserts, restores);
}

/// The first value fires when align is about to wait while still holding
/// the lock. Sending on the second value lets that wait finish.
#[cfg(test)]
pub(super) fn align_pause(
    server: &str,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    save_fault::listen(server)
}

#[cfg(test)]
pub(super) fn clear_cap_save_fault(server: &str) {
    save_fault::clear(server);
}

/// The next forward for `server` applies on the process, then returns an error.
#[cfg(test)]
pub(super) fn arm_forward_error_after_accept(server: &str) {
    save_fault::arm_forward_error_after_accept(server);
}

/// The next forward for `server` returns an error and does not change the process.
#[cfg(test)]
pub(super) fn arm_forward_error_before_accept(server: &str) {
    save_fault::arm_forward_error_before_accept(server);
}

/// The next `reads` process reads for `server` fail. Later reads run.
#[cfg(test)]
pub(super) fn arm_process_read_failures(server: &str, reads: u32) {
    save_fault::arm_process_read_failures(server, reads);
}

/// The next restore that the process applies still reports an error.
#[cfg(test)]
pub(super) fn arm_restore_error_after_apply(server: &str) {
    save_fault::arm_restore_error_after_apply(server);
}

#[cfg(test)]
mod save_fault {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    struct Fault {
        upserts_left: u32,
        restores_left: u32,
        forward_error_after_accept: bool,
        forward_error_before_accept: bool,
        process_reads_left: u32,
        restore_error_after_apply: bool,
        paused: Option<tokio::sync::oneshot::Sender<()>>,
        release: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    impl Fault {
        fn empty() -> Self {
            Self {
                upserts_left: 0,
                restores_left: 0,
                forward_error_after_accept: false,
                forward_error_before_accept: false,
                process_reads_left: 0,
                restore_error_after_apply: false,
                paused: None,
                release: None,
            }
        }
    }

    fn table() -> &'static Mutex<HashMap<String, Fault>> {
        static TABLE: OnceLock<Mutex<HashMap<String, Fault>>> = OnceLock::new();
        TABLE.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn arm(server: &str, upserts: u32, restores: u32) {
        let key = music_bot::canon_server_addr(server);
        table()
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .insert(
                key,
                Fault {
                    upserts_left: upserts,
                    restores_left: restores,
                    ..Fault::empty()
                },
            );
    }

    pub(super) fn listen(
        server: &str,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let key = music_bot::canon_server_addr(server);
        let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        let fault = map.entry(key).or_insert_with(Fault::empty);
        fault.paused = Some(paused_tx);
        fault.release = Some(release_rx);
        (paused_rx, release_tx)
    }

    pub(super) fn fail_upsert(server: &str) -> bool {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        let Some(fault) = map.get_mut(&key) else {
            return false;
        };
        if fault.upserts_left == 0 {
            return false;
        }
        fault.upserts_left -= 1;
        true
    }

    pub(super) fn fail_restore(server: &str) -> bool {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        let Some(fault) = map.get_mut(&key) else {
            return false;
        };
        if fault.restores_left == 0 {
            return false;
        }
        fault.restores_left -= 1;
        true
    }

    pub(super) fn note_pause(server: &str) -> Option<tokio::sync::oneshot::Receiver<()>> {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        let fault = map.get_mut(&key)?;
        if let Some(tx) = fault.paused.take() {
            let _ = tx.send(());
        }
        fault.release.take()
    }

    pub(super) fn arm_forward_error_after_accept(server: &str) {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        map.entry(key)
            .or_insert_with(Fault::empty)
            .forward_error_after_accept = true;
    }

    pub(super) fn arm_restore_error_after_apply(server: &str) {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        map.entry(key)
            .or_insert_with(Fault::empty)
            .restore_error_after_apply = true;
    }

    pub(super) fn take_restore_error_after_apply(server: &str) -> bool {
        take_flag(server, |fault| {
            let armed = fault.restore_error_after_apply;
            fault.restore_error_after_apply = false;
            armed
        })
    }

    pub(super) fn arm_process_read_failures(server: &str, reads: u32) {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        map.entry(key)
            .or_insert_with(Fault::empty)
            .process_reads_left = reads;
    }

    pub(super) fn fail_process_read(server: &str) -> bool {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        let Some(fault) = map.get_mut(&key) else {
            return false;
        };
        if fault.process_reads_left == 0 {
            return false;
        }
        fault.process_reads_left -= 1;
        true
    }

    pub(super) fn arm_forward_error_before_accept(server: &str) {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        map.entry(key)
            .or_insert_with(Fault::empty)
            .forward_error_before_accept = true;
    }

    pub(super) fn take_forward_error_after_accept(server: &str) -> bool {
        take_flag(server, |fault| {
            let armed = fault.forward_error_after_accept;
            fault.forward_error_after_accept = false;
            armed
        })
    }

    pub(super) fn take_forward_error_before_accept(server: &str) -> bool {
        take_flag(server, |fault| {
            let armed = fault.forward_error_before_accept;
            fault.forward_error_before_accept = false;
            armed
        })
    }

    fn take_flag(server: &str, flag: impl FnOnce(&mut Fault) -> bool) -> bool {
        let key = music_bot::canon_server_addr(server);
        let mut map = table().lock().unwrap_or_else(|err| err.into_inner());
        map.get_mut(&key).is_some_and(flag)
    }

    pub(super) fn clear(server: &str) {
        let key = music_bot::canon_server_addr(server);
        table()
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .remove(&key);
    }
}
