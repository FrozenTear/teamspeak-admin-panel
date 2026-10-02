//! Temporary summon clients.
//!
//! A saved music bot stays a saved music bot. A summon uses a separate
//! pool of quiet clients that the music process owns. The pool is keyed
//! by the server address carried on a bot push. Nothing here writes a
//! `music_bot_runtime` row, and nothing here is returned from
//! [`crate::supervisor::BotSupervisor::list`].
//!
//! The cap is whatever the API forwarded. This module does not invent one.
//! A push that omits the number does not arm summon for that server, and
//! it does not copy another server's number.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::chat::{ParsedCommand, parse as parse_chat};
use crate::command::AudioSource;

/// Channel path passed to `Connection::channel` so the quiet client
/// lands in tech support on the initial connect. Later moves are
/// `clientmove` on that same connection.
pub const TECH_SUPPORT_CHANNEL: &str = "Tech Support";

/// Largest cap the music process will accept. A larger value is refused
/// so it is not stored. This is not a default.
pub const MAX_SUMMON_CAP: u32 = 64;

/// Moves toward the caller's channel before the summon is sent home.
const MAX_MOVE_ATTEMPTS: u8 = 3;

/// Same key the music page uses. Defined in the shared crate so the
/// wasm client does not link this crate.
pub use ts6_manager_shared::music_bots::canon_server_addr;

/// One person already on a quiet client's own client list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedClient {
    pub id: u16,
    pub channel_id: u64,
}

/// What one quiet client should do on its own voice connection.
///
/// `reply` stays `None`. A line typed into the channel the client is
/// sitting in does not reach people in other channels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuietInstruction {
    pub slot: u32,
    pub move_to: Option<u64>,
    pub play: Option<String>,
    pub stop_audio: bool,
    pub shutdown: bool,
    pub reply: Option<String>,
    /// The song text, when the decision kept it. Absent for a refusal:
    /// those are not recorded.
    pub kept_request: Option<String>,
    pub silence: bool,
}

impl QuietInstruction {
    fn bare(slot: u32) -> Self {
        Self {
            slot,
            move_to: None,
            play: None,
            stop_audio: false,
            shutdown: false,
            reply: None,
            kept_request: None,
            silence: false,
        }
    }
}

/// Arguments the live session needs. Absent unless the process asked
/// for real TeamSpeak connections.
#[derive(Clone)]
pub struct LiveQuiet {
    pub yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    /// Stored with the session so a later resolve can read it. The cold
    /// `ytsearch1:` pipeline does not call the YouTube Data API.
    #[allow(dead_code)]
    pub yt_api_key: Arc<RwLock<Option<String>>>,
}

struct QuietSlot {
    slot: u32,
    /// Bumped for every slot this process creates. A session that has
    /// exited keeps its old value and cannot steer the replacement.
    generation: u64,
    connection_id: u64,
    identity_path: PathBuf,
    home: Option<u64>,
    at: Option<u64>,
    subscribed: bool,
    /// False while a reconnect has not finished rebuilding the client list.
    list_trusted: bool,
    launched: bool,
    phase: Phase,
    frames_sent: u64,
    frames_blocked: u64,
    stop: watch::Sender<bool>,
}

enum Phase {
    Sitting,
    Out {
        caller: u16,
        channel: u64,
        request: String,
        arrived: bool,
        playing: bool,
        move_attempts: u8,
    },
}

impl QuietSlot {
    fn is_sitting(&self) -> bool {
        matches!(self.phase, Phase::Sitting)
    }
}

#[derive(Default)]
struct ServerPool {
    cap: Option<u32>,
    armed: bool,
    saved_bots: BTreeSet<u64>,
    saved_identities: Vec<PathBuf>,
    slots: Vec<QuietSlot>,
    /// Replacements for sessions that ended while this server stayed armed.
    relaunches_due: u32,
    relaunch_attempt: u32,
    relaunch_not_before: Option<Instant>,
    /// Always empty. Refusals are not recorded.
    refused: Vec<u16>,
    /// Always empty. There is no wait queue.
    waiters: Vec<u16>,
}

struct SummonState {
    identity_dir: PathBuf,
    next_connection: AtomicU64,
    next_identity: u64,
    next_slot: u32,
    next_generation: u64,
    servers: BTreeMap<String, ServerPool>,
    /// Always zero. A missing sender is not resolved with a server lookup.
    server_lookups: u32,
}

/// Per-server pool of quiet clients.
#[derive(Clone)]
pub struct SummonDirector {
    inner: Arc<Mutex<SummonState>>,
    live: Arc<Mutex<Option<LiveQuiet>>>,
}

pub(crate) struct SlotLaunch {
    pub(crate) server: String,
    pub(crate) slot: u32,
    pub(crate) generation: u64,
    pub(crate) identity_path: PathBuf,
    pub(crate) stop: watch::Receiver<bool>,
}

impl SummonDirector {
    pub fn new(identity_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SummonState {
                identity_dir: identity_dir.into(),
                next_connection: AtomicU64::new(1),
                next_identity: 0,
                next_slot: 0,
                next_generation: 1,
                servers: BTreeMap::new(),
                server_lookups: 0,
            })),
            live: Arc::new(Mutex::new(None)),
        }
    }

    /// Remember a saved bot. `cap` is the number carried on this push.
    /// `None` does not arm this server and does not read any other server.
    pub fn note_push(&self, server: &str, bot_id: u64, cap: Option<u32>, saved_identity: &Path) {
        {
            let mut state = self.lock();
            state.note_push(server, bot_id, cap, saved_identity);
        }
        self.launch_pending();
    }

    /// Store the cap for this server. Starts or shrinks quiet clients
    /// only when at least one saved bot for this server is already known.
    /// A value above [`MAX_SUMMON_CAP`] is refused and stored nowhere.
    pub fn accept_cap(&self, server: &str, cap: u32) -> Result<(), String> {
        {
            let mut state = self.lock();
            state.accept_cap(server, cap)?;
        }
        self.launch_pending();
        Ok(())
    }

    /// Put this server back to a cap the database still has. `None` means
    /// nothing is stored: the number is cleared and quiet clients started
    /// for the rejected number are stopped.
    pub fn restore_cap(&self, server: &str, cap: Option<u32>) -> Result<(), String> {
        {
            let mut state = self.lock();
            match cap {
                Some(cap) => state.accept_cap(server, cap)?,
                None => state.clear_cap(server)?,
            }
        }
        self.launch_pending();
        Ok(())
    }

    /// The session has subscribed on its own connection and knows the
    /// tech-support channel it landed in.
    pub fn mark_ready(&self, server: &str, slot: u32, home: u64, clients: &[ListedClient]) {
        let mut state = self.lock();
        {
            let Some(slot) = state.slot_mut(server, slot) else {
                return;
            };
            slot.home = Some(home);
            slot.at = Some(home);
            slot.subscribed = true;
            slot.list_trusted = true;
        }
        if let Some(pool) = state.pool_mut(server) {
            pool.relaunch_attempt = 0;
        }
        let _ = clients;
    }

    /// Same as [`Self::mark_ready`], ignored when `generation` is stale.
    pub fn mark_ready_gen(
        &self,
        server: &str,
        slot: u32,
        generation: u64,
        home: u64,
        clients: &[ListedClient],
    ) -> bool {
        if !self.lock().owns(server, slot, generation) {
            return false;
        }
        self.mark_ready(server, slot, home, clients);
        true
    }

    /// The quiet client's connection dropped and came back. The client
    /// list is not trusted until [`Self::note_list_trusted`]. Home and
    /// the current summon are left as they were.
    pub fn mark_reconnected(&self, server: &str, slot: u32, generation: u64, at: u64) -> bool {
        let mut state = self.lock();
        let Some(slot) = state.slot_mut(server, slot) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        slot.at = Some(at);
        slot.subscribed = true;
        slot.list_trusted = false;
        true
    }

    /// The book after a fresh subscribe is the list moves may use.
    pub fn note_list_trusted(&self, server: &str, slot: u32, generation: u64) -> bool {
        let mut state = self.lock();
        let Some(slot) = state.slot_mut(server, slot) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        slot.list_trusted = true;
        true
    }

    /// The live session ended. The slot stops counting and stops looking
    /// occupied. While the server stays armed, a replacement is due after
    /// backoff. A slot id is not reused.
    pub fn session_ended(&self, server: &str, slot: u32, generation: u64) {
        let delay = {
            let mut state = self.lock();
            state.retire_session(server, slot, generation)
        };
        if let Some(delay) = delay {
            self.arm_relaunch(delay);
        }
    }

    /// A saved bot is gone. The last one on a server stops that server's
    /// quiet clients. The stored cap is left in place.
    pub fn forget_saved(&self, server: &str, bot_id: u64) {
        let mut state = self.lock();
        state.forget_saved(server, bot_id);
    }

    /// Start replacements whose backoff has elapsed.
    pub fn promote_due(&self, now: Instant) {
        {
            let mut state = self.lock();
            state.promote(now);
        }
        self.launch_pending();
    }

    #[cfg(test)]
    pub fn generation(&self, server: &str, slot: u32) -> Option<u64> {
        self.lock().slot(server, slot).map(|slot| slot.generation)
    }

    #[cfg(test)]
    pub fn relaunch_not_before(&self, server: &str) -> Option<Instant> {
        self.lock()
            .pool(server)
            .and_then(|pool| pool.relaunch_not_before)
    }

    pub fn on_chat_gen(
        &self,
        server: &str,
        slot: u32,
        generation: u64,
        sender_id: u16,
        line: &str,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        let mut state = self.lock();
        if !state.owns(server, slot, generation) {
            return Vec::new();
        }
        state.on_chat(server, slot, sender_id, line, clients)
    }

    pub fn on_book_gen(
        &self,
        server: &str,
        slot: u32,
        generation: u64,
        own_channel: u64,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        let mut state = self.lock();
        if !state.owns(server, slot, generation) {
            return Vec::new();
        }
        state.on_book(server, slot, own_channel, clients)
    }

    pub fn on_playback_finished_gen(
        &self,
        server: &str,
        slot: u32,
        generation: u64,
    ) -> Vec<QuietInstruction> {
        let mut state = self.lock();
        if !state.owns(server, slot, generation) {
            return Vec::new();
        }
        state.finish(server, slot)
    }

    pub fn offer_frame_gen(&self, server: &str, slot: u32, generation: u64) -> bool {
        let mut state = self.lock();
        let Some(slot) = state.slot_mut(server, slot) else {
            return false;
        };
        if slot.generation != generation {
            return false;
        }
        if frame_allowed(slot) {
            slot.frames_sent += 1;
            true
        } else {
            slot.frames_blocked += 1;
            false
        }
    }

    pub fn playback_open_gen(&self, server: &str, slot: u32, generation: u64) -> bool {
        let state = self.lock();
        state
            .slot(server, slot)
            .is_some_and(|slot| slot.generation == generation && frame_allowed(slot))
    }

    pub fn on_chat(
        &self,
        server: &str,
        slot: u32,
        sender_id: u16,
        line: &str,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        let mut state = self.lock();
        state.on_chat(server, slot, sender_id, line, clients)
    }

    pub fn on_book(
        &self,
        server: &str,
        slot: u32,
        own_channel: u64,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        let mut state = self.lock();
        state.on_book(server, slot, own_channel, clients)
    }

    pub fn on_playback_finished(&self, server: &str, slot: u32) -> Vec<QuietInstruction> {
        let mut state = self.lock();
        state.finish(server, slot)
    }

    /// `true` only while this quiet client is in the caller's channel
    /// and a song is open. Sitting in tech support returns `false` and
    /// does not count a sent frame.
    pub fn offer_frame(&self, server: &str, slot: u32) -> bool {
        let mut state = self.lock();
        let Some(slot) = state.slot_mut(server, slot) else {
            return false;
        };
        if frame_allowed(slot) {
            slot.frames_sent += 1;
            true
        } else {
            slot.frames_blocked += 1;
            false
        }
    }

    pub fn playback_open(&self, server: &str, slot: u32) -> bool {
        let state = self.lock();
        state.slot(server, slot).is_some_and(frame_allowed)
    }

    /// Remove one quiet client. Saved bots for the server stay.
    pub fn drop_quiet(&self, server: &str, slot: u32) -> bool {
        let mut state = self.lock();
        let Some(pool) = state.pool_mut(server) else {
            return false;
        };
        let Some(index) = pool.slots.iter().position(|s| s.slot == slot) else {
            return false;
        };
        let removed = pool.slots.remove(index);
        let _ = removed.stop.send(true);
        true
    }

    pub fn enable_live(
        &self,
        identity_dir: PathBuf,
        yt_cookie: Arc<RwLock<Option<PathBuf>>>,
        yt_api_key: Arc<RwLock<Option<String>>>,
    ) {
        self.lock().identity_dir = identity_dir;
        *self.live.lock().unwrap_or_else(|err| err.into_inner()) = Some(LiveQuiet {
            yt_cookie,
            yt_api_key,
        });
        self.launch_pending();
    }

    pub fn quiet_count(&self, server: &str) -> usize {
        self.lock()
            .pool(server)
            .map(|pool| pool.slots.len())
            .unwrap_or(0)
    }

    pub fn cap(&self, server: &str) -> Option<u32> {
        self.lock().pool(server).and_then(|pool| pool.cap)
    }

    /// Song the quiet client is already committed to, if it is out.
    #[cfg(test)]
    pub fn assigned_request(&self, server: &str, slot: u32) -> Option<String> {
        self.lock()
            .slot(server, slot)
            .and_then(|slot| match &slot.phase {
                Phase::Out { request, .. } => Some(request.clone()),
                Phase::Sitting => None,
            })
    }

    pub fn armed(&self, server: &str) -> bool {
        self.lock().pool(server).is_some_and(|pool| pool.armed)
    }

    pub fn saved_ids(&self, server: &str) -> Vec<u64> {
        self.lock()
            .pool(server)
            .map(|pool| pool.saved_bots.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn identity_paths(&self, server: &str) -> Vec<PathBuf> {
        self.lock()
            .pool(server)
            .map(|pool| {
                pool.slots
                    .iter()
                    .map(|slot| slot.identity_path.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn connection_id(&self, server: &str, slot: u32) -> Option<u64> {
        self.lock()
            .slot(server, slot)
            .map(|slot| slot.connection_id)
    }

    pub fn subscribed(&self, server: &str, slot: u32) -> bool {
        self.lock()
            .slot(server, slot)
            .is_some_and(|slot| slot.subscribed)
    }

    pub fn frames_sent(&self, server: &str, slot: u32) -> u64 {
        self.lock()
            .slot(server, slot)
            .map(|slot| slot.frames_sent)
            .unwrap_or(0)
    }

    pub fn frames_blocked(&self, server: &str, slot: u32) -> u64 {
        self.lock()
            .slot(server, slot)
            .map(|slot| slot.frames_blocked)
            .unwrap_or(0)
    }

    pub fn server_lookups(&self) -> u32 {
        self.lock().server_lookups
    }

    pub fn refused_len(&self, server: &str) -> usize {
        self.lock()
            .pool(server)
            .map(|pool| pool.refused.len())
            .unwrap_or(0)
    }

    pub fn waiter_len(&self, server: &str) -> usize {
        self.lock()
            .pool(server)
            .map(|pool| pool.waiters.len())
            .unwrap_or(0)
    }

    pub fn slot_ids(&self, server: &str) -> Vec<u32> {
        self.lock()
            .pool(server)
            .map(|pool| pool.slots.iter().map(|slot| slot.slot).collect())
            .unwrap_or_default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SummonState> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn launch_pending(&self) {
        let live = self
            .live
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let Some(live) = live else {
            return;
        };
        let launches = self.lock().claim_unlaunched();
        for launch in launches {
            let director = self.clone();
            let live = live.clone();
            let server = launch.server.clone();
            crate::runtime::voice_runtime().spawn(async move {
                if let Err(err) = crate::quiet_session::run(director, launch, live).await {
                    tracing::warn!(
                        %server,
                        error = %err,
                        "quiet client session stopped"
                    );
                }
            });
        }
    }

    fn arm_relaunch(&self, delay: Duration) {
        let live = self
            .live
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some();
        if !live {
            return;
        }
        let director = self.clone();
        crate::runtime::voice_runtime().spawn(async move {
            tokio::time::sleep(delay).await;
            director.promote_due(Instant::now());
        });
    }
}

fn frame_allowed(slot: &QuietSlot) -> bool {
    match &slot.phase {
        Phase::Out {
            channel,
            arrived: true,
            playing: true,
            ..
        } => slot.at == Some(*channel),
        _ => false,
    }
}

impl SummonState {
    fn pool(&self, server: &str) -> Option<&ServerPool> {
        self.servers.get(&canon_server_addr(server))
    }

    fn pool_mut(&mut self, server: &str) -> Option<&mut ServerPool> {
        self.servers.get_mut(&canon_server_addr(server))
    }

    fn pool_entry(&mut self, server: &str) -> &mut ServerPool {
        self.servers.entry(canon_server_addr(server)).or_default()
    }

    fn owns(&self, server: &str, slot_id: u32, generation: u64) -> bool {
        self.slot(server, slot_id)
            .is_some_and(|slot| slot.generation == generation)
    }

    fn note_push(&mut self, server: &str, bot_id: u64, cap: Option<u32>, saved_identity: &Path) {
        let arm_at = {
            let pool = self.pool_entry(server);
            pool.saved_bots.insert(bot_id);
            if !pool
                .saved_identities
                .iter()
                .any(|path| path == saved_identity)
            {
                pool.saved_identities.push(saved_identity.to_path_buf());
            }
            match cap {
                Some(cap) if !pool.armed => {
                    pool.cap = Some(cap);
                    pool.armed = true;
                    Some(cap)
                }
                _ => None,
            }
        };
        let Some(cap) = arm_at else {
            return;
        };
        let saved = self
            .pool(server)
            .map(|pool| pool.saved_identities.clone())
            .unwrap_or_default();
        let have = self.pool(server).map(|pool| pool.slots.len()).unwrap_or(0);
        for _ in have..(cap as usize) {
            self.push_slot(server, &saved);
        }
    }

    fn accept_cap(&mut self, server: &str, cap: u32) -> Result<(), String> {
        if server.is_empty() {
            return Err("server address is empty".into());
        }
        if cap > MAX_SUMMON_CAP {
            return Err(format!("cap {cap} is above {MAX_SUMMON_CAP}"));
        }
        let known = self
            .pool(server)
            .is_some_and(|pool| !pool.saved_bots.is_empty());
        {
            let pool = self.pool_entry(server);
            pool.cap = Some(cap);
            if !known {
                return Ok(());
            }
            pool.armed = true;
            while pool.slots.len() > cap as usize {
                if let Some(index) = pool.slots.iter().position(QuietSlot::is_sitting) {
                    let removed = pool.slots.remove(index);
                    let _ = removed.stop.send(true);
                } else {
                    break;
                }
            }
        }
        let saved = self
            .pool(server)
            .map(|pool| pool.saved_identities.clone())
            .unwrap_or_default();
        while self
            .pool(server)
            .is_some_and(|pool| pool.slots.len() < cap as usize)
        {
            self.push_slot(server, &saved);
        }
        Ok(())
    }

    fn clear_cap(&mut self, server: &str) -> Result<(), String> {
        if server.is_empty() {
            return Err("server address is empty".into());
        }
        let Some(pool) = self.pool_mut(server) else {
            return Ok(());
        };
        pool.cap = None;
        pool.armed = false;
        pool.relaunches_due = 0;
        pool.relaunch_not_before = None;
        let removed = std::mem::take(&mut pool.slots);
        for slot in removed {
            let _ = slot.stop.send(true);
        }
        Ok(())
    }

    fn push_slot(&mut self, server: &str, saved: &[PathBuf]) {
        let slot_no = self.next_slot;
        self.next_slot = self.next_slot.saturating_add(1);
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let identity_path = self.allocate_identity(server, slot_no, saved);
        let connection_id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let (stop, _) = watch::channel(false);
        let pool = self.pool_entry(server);
        pool.slots.push(QuietSlot {
            slot: slot_no,
            generation,
            connection_id,
            identity_path,
            home: None,
            at: None,
            subscribed: false,
            list_trusted: false,
            launched: false,
            phase: Phase::Sitting,
            frames_sent: 0,
            frames_blocked: 0,
            stop,
        });
    }

    fn allocate_identity(&mut self, server: &str, slot: u32, saved: &[PathBuf]) -> PathBuf {
        let slug = server_slug(server);
        loop {
            self.next_identity += 1;
            let name = format!("quiet-{slug}-s{slot}-n{}.identity", self.next_identity);
            debug_assert!(
                !is_saved_bot_identity_name(&name),
                "quiet identity must not use a saved-bot file name"
            );
            let path = self.identity_dir.join("quiet").join(&slug).join(&name);
            if saved.iter().any(|existing| existing == &path) {
                continue;
            }
            let taken = self
                .servers
                .values()
                .any(|pool| pool.slots.iter().any(|slot| slot.identity_path == path));
            if taken {
                continue;
            }
            return path;
        }
    }

    fn claim_unlaunched(&mut self) -> Vec<SlotLaunch> {
        let mut out = Vec::new();
        for (server, pool) in &mut self.servers {
            for slot in &mut pool.slots {
                if slot.launched {
                    continue;
                }
                slot.launched = true;
                out.push(SlotLaunch {
                    server: server.clone(),
                    slot: slot.slot,
                    generation: slot.generation,
                    identity_path: slot.identity_path.clone(),
                    stop: slot.stop.subscribe(),
                });
            }
        }
        out
    }

    fn on_chat(
        &mut self,
        server: &str,
        slot_id: u32,
        sender_id: u16,
        line: &str,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        if is_stop(line) {
            return self.stop_for_caller(server, slot_id, sender_id, clients);
        }
        let Some(arg) = song_arg(line) else {
            return Vec::new();
        };
        let armed = self.pool(server).is_some_and(|pool| pool.armed);
        if !armed {
            return Vec::new();
        }
        if self.slot(server, slot_id).is_none() {
            return Vec::new();
        }
        if !clients.iter().any(|client| client.id == sender_id) {
            let mut instr = QuietInstruction::bare(slot_id);
            instr.kept_request = Some(arg);
            return vec![instr];
        }
        let channel = clients
            .iter()
            .find(|client| client.id == sender_id)
            .map(|client| client.channel_id)
            .expect("sender was on the list");
        // Tech Support is where this client sits. A summon from that
        // channel must not start a song there.
        let home = self.slot(server, slot_id).and_then(|slot| slot.home);
        if home == Some(channel) {
            return Vec::new();
        }
        // A channel that already has a quiet client is finished with this
        // request. No new song, no move, no extra client. The occupant
        // keeps the pipeline it already has.
        if self.occupant(server, channel).is_some() {
            return Vec::new();
        }
        let sitting = self
            .slot(server, slot_id)
            .is_some_and(QuietSlot::is_sitting);
        if !sitting {
            let mut instr = QuietInstruction::bare(slot_id);
            instr.silence = true;
            return vec![instr];
        }
        self.assign(server, slot_id, sender_id, channel, arg)
    }

    fn assign(
        &mut self,
        server: &str,
        slot_id: u32,
        sender_id: u16,
        channel: u64,
        arg: String,
    ) -> Vec<QuietInstruction> {
        let Some(slot) = self.slot_mut(server, slot_id) else {
            return Vec::new();
        };
        let here = slot.at == Some(channel);
        slot.phase = Phase::Out {
            caller: sender_id,
            channel,
            request: arg.clone(),
            arrived: here,
            playing: here,
            move_attempts: if here { 0 } else { 1 },
        };
        if here {
            let mut instr = QuietInstruction::bare(slot_id);
            instr.play = Some(arg);
            instr.kept_request = instr.play.clone();
            return vec![instr];
        }
        let mut instr = QuietInstruction::bare(slot_id);
        instr.move_to = Some(channel);
        instr.kept_request = Some(arg);
        vec![instr]
    }

    fn stop_for_caller(
        &mut self,
        server: &str,
        slot_id: u32,
        sender_id: u16,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        if !clients.iter().any(|client| client.id == sender_id) {
            return Vec::new();
        }
        let is_caller = self.slot(server, slot_id).is_some_and(
            |slot| matches!(slot.phase, Phase::Out { caller, .. } if caller == sender_id),
        );
        if !is_caller {
            return Vec::new();
        }
        self.finish(server, slot_id)
    }

    fn on_book(
        &mut self,
        server: &str,
        slot_id: u32,
        own_channel: u64,
        clients: &[ListedClient],
    ) -> Vec<QuietInstruction> {
        let Some(slot) = self.slot_mut(server, slot_id) else {
            return Vec::new();
        };
        slot.at = Some(own_channel);
        if !slot.list_trusted {
            return Vec::new();
        }
        let Phase::Out {
            caller,
            channel,
            request,
            arrived,
            playing,
            move_attempts,
        } = &slot.phase
        else {
            return Vec::new();
        };
        let caller = *caller;
        let channel = *channel;
        let request = request.clone();
        let arrived = *arrived;
        let playing = *playing;
        let move_attempts = *move_attempts;
        let home = slot.home;
        match clients.iter().find(|client| client.id == caller) {
            None => self.finish(server, slot_id),
            Some(client) if client.channel_id != channel => {
                let new_channel = client.channel_id;
                if home == Some(new_channel) {
                    return self.finish(server, slot_id);
                }
                if self
                    .occupant(server, new_channel)
                    .is_some_and(|other| other != slot_id)
                {
                    // The caller left, and the destination already has a
                    // quiet client. Do not follow into it.
                    return self.finish(server, slot_id);
                }
                let already_there = own_channel == new_channel;
                let Some(slot) = self.slot_mut(server, slot_id) else {
                    return Vec::new();
                };
                if let Phase::Out {
                    channel,
                    arrived,
                    playing,
                    move_attempts,
                    ..
                } = &mut slot.phase
                {
                    *channel = new_channel;
                    *arrived = already_there;
                    *move_attempts = if already_there { 0 } else { 1 };
                    if already_there {
                        *playing = true;
                    }
                }
                let mut instr = QuietInstruction::bare(slot_id);
                if !already_there {
                    instr.move_to = Some(new_channel);
                } else if !playing {
                    instr.play = Some(request);
                }
                let _ = arrived;
                vec![instr]
            }
            Some(_) if own_channel != channel => {
                if move_attempts >= MAX_MOVE_ATTEMPTS {
                    return self.finish(server, slot_id);
                }
                if let Some(slot) = self.slot_mut(server, slot_id)
                    && let Phase::Out { move_attempts, .. } = &mut slot.phase
                {
                    *move_attempts = move_attempts.saturating_add(1);
                }
                let mut instr = QuietInstruction::bare(slot_id);
                instr.move_to = Some(channel);
                vec![instr]
            }
            Some(_) if own_channel == channel && !arrived => {
                let was_playing = playing;
                let Some(slot) = self.slot_mut(server, slot_id) else {
                    return Vec::new();
                };
                let arg = match &mut slot.phase {
                    Phase::Out {
                        request,
                        arrived,
                        playing,
                        move_attempts,
                        ..
                    } => {
                        *arrived = true;
                        *playing = true;
                        *move_attempts = 0;
                        if was_playing {
                            None
                        } else {
                            Some(request.clone())
                        }
                    }
                    Phase::Sitting => return Vec::new(),
                };
                let Some(arg) = arg else {
                    return Vec::new();
                };
                let mut instr = QuietInstruction::bare(slot_id);
                instr.play = Some(arg.clone());
                instr.kept_request = Some(arg);
                vec![instr]
            }
            Some(_) => Vec::new(),
        }
    }

    fn finish(&mut self, server: &str, slot_id: u32) -> Vec<QuietInstruction> {
        let cap = self.pool(server).and_then(|pool| pool.cap).unwrap_or(0) as usize;
        let len = self.pool(server).map(|pool| pool.slots.len()).unwrap_or(0);
        if len > cap {
            return self.remove_slot(server, slot_id, true);
        }
        let home = self.slot(server, slot_id).and_then(|slot| slot.home);
        let at = self.slot(server, slot_id).and_then(|slot| slot.at);
        if let Some(slot) = self.slot_mut(server, slot_id) {
            slot.phase = Phase::Sitting;
        }
        let mut instr = QuietInstruction::bare(slot_id);
        instr.stop_audio = true;
        if let Some(home) = home
            && at != Some(home)
        {
            instr.move_to = Some(home);
        }
        vec![instr]
    }

    fn remove_slot(&mut self, server: &str, slot_id: u32, shutdown: bool) -> Vec<QuietInstruction> {
        let Some(pool) = self.pool_mut(server) else {
            return Vec::new();
        };
        let Some(index) = pool.slots.iter().position(|slot| slot.slot == slot_id) else {
            return Vec::new();
        };
        let removed = pool.slots.remove(index);
        let _ = removed.stop.send(true);
        let mut instr = QuietInstruction::bare(slot_id);
        instr.stop_audio = true;
        instr.shutdown = shutdown;
        vec![instr]
    }

    fn occupant(&self, server: &str, channel: u64) -> Option<u32> {
        let pool = self.pool(server)?;
        pool.slots.iter().find_map(|slot| match &slot.phase {
            Phase::Out {
                channel: occupied, ..
            } if *occupied == channel => Some(slot.slot),
            _ => None,
        })
    }

    fn slot(&self, server: &str, slot_id: u32) -> Option<&QuietSlot> {
        self.pool(server)?
            .slots
            .iter()
            .find(|slot| slot.slot == slot_id)
    }

    fn slot_mut(&mut self, server: &str, slot_id: u32) -> Option<&mut QuietSlot> {
        self.pool_mut(server)?
            .slots
            .iter_mut()
            .find(|slot| slot.slot == slot_id)
    }

    fn retire_session(&mut self, server: &str, slot_id: u32, generation: u64) -> Option<Duration> {
        let pool = self.pool_mut(server)?;
        let index = pool
            .slots
            .iter()
            .position(|slot| slot.slot == slot_id && slot.generation == generation)?;
        let removed = pool.slots.remove(index);
        let _ = removed.stop.send(true);
        if !pool.armed {
            return None;
        }
        pool.relaunches_due = pool.relaunches_due.saturating_add(1);
        if pool.relaunch_not_before.is_some() {
            return None;
        }
        let delay = relaunch_delay(pool.relaunch_attempt);
        pool.relaunch_attempt = pool.relaunch_attempt.saturating_add(1);
        pool.relaunch_not_before = Some(Instant::now() + delay);
        Some(delay)
    }

    fn forget_saved(&mut self, server: &str, bot_id: u64) {
        let Some(pool) = self.pool_mut(server) else {
            return;
        };
        pool.saved_bots.remove(&bot_id);
        if !pool.saved_bots.is_empty() {
            return;
        }
        pool.armed = false;
        pool.relaunches_due = 0;
        pool.relaunch_not_before = None;
        let removed = std::mem::take(&mut pool.slots);
        for slot in removed {
            let _ = slot.stop.send(true);
        }
    }

    fn promote(&mut self, now: Instant) {
        let servers: Vec<String> = self.servers.keys().cloned().collect();
        for server in servers {
            let saved = self
                .pool(&server)
                .map(|pool| pool.saved_identities.clone())
                .unwrap_or_default();
            let spawn_n = {
                let Some(pool) = self.pool_mut(&server) else {
                    continue;
                };
                if !pool.armed || pool.relaunches_due == 0 {
                    continue;
                }
                if pool.relaunch_not_before.is_some_and(|due| due > now) {
                    continue;
                }
                let cap = pool.cap.unwrap_or(0) as usize;
                let room = cap.saturating_sub(pool.slots.len());
                if room == 0 {
                    continue;
                }
                let spawn_n = (pool.relaunches_due as usize).min(room);
                pool.relaunches_due -= spawn_n as u32;
                if pool.relaunches_due == 0 {
                    pool.relaunch_not_before = None;
                }
                spawn_n
            };
            for _ in 0..spawn_n {
                self.push_slot(&server, &saved);
            }
        }
    }
}

fn relaunch_delay(attempt: u32) -> Duration {
    let shift = u32::min(attempt, 5);
    Duration::from_secs(1_u64 << shift).min(Duration::from_secs(30))
}

fn song_arg(line: &str) -> Option<String> {
    match parse_chat(line) {
        Ok(ParsedCommand::Play { arg } | ParsedCommand::Radio { arg }) => Some(arg),
        _ => None,
    }
}

fn is_stop(line: &str) -> bool {
    matches!(parse_chat(line), Ok(ParsedCommand::Stop))
}

/// A summoned song is resolved from the request text when playback
/// starts. It is not a saved bot's library path and not another
/// client's identity or queue.
pub fn cold_audio_source(arg: &str) -> AudioSource {
    let trimmed = arg.trim();
    if is_http(trimmed) {
        return AudioSource::Url(trimmed.to_string());
    }
    if let Some(query) = yt_query(trimmed) {
        return AudioSource::Url(format!("ytsearch1:{query}"));
    }
    AudioSource::Url(format!("ytsearch1:{trimmed}"))
}

fn is_http(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    lower.starts_with("https://") || lower.starts_with("http://")
}

fn yt_query(arg: &str) -> Option<&str> {
    let lower = arg.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("yt:") {
        let offset = arg.len() - rest.len();
        return Some(arg[offset..].trim());
    }
    if let Some(rest) = lower.strip_prefix("youtube:") {
        let offset = arg.len() - rest.len();
        return Some(arg[offset..].trim());
    }
    None
}

pub fn is_saved_bot_identity_name(name: &str) -> bool {
    let stem = name.strip_suffix(".identity").unwrap_or(name);
    if stem == "bot-1" {
        return true;
    }
    let Some(rest) = stem.strip_prefix("bot-") else {
        return false;
    };
    !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit())
}

fn server_slug(server: &str) -> String {
    let mut out = String::new();
    for ch in server.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Write a new identity at `path`. The file is generated, not copied
/// from another quiet client or from a saved bot. A path that already
/// exists is loaded as that file's own key (a reconnect of the same
/// quiet client), still never as a copy of a different file.
pub async fn mint_quiet_identity(path: &Path) -> anyhow::Result<tsclientlib::Identity> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if is_saved_bot_identity_name(name) {
        anyhow::bail!("refusing to mint a saved-bot identity name");
    }
    ts6_voice_fixture::load_or_create_identity(path).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    const HOME: u64 = 42;
    const SERVER: &str = "voice.example:9987";
    const OTHER: &str = "other.example:9987";

    fn director() -> SummonDirector {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(1);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("quiet-summon-{}-{n}", std::process::id()));
        SummonDirector::new(dir)
    }

    fn boot(cap: u32) -> SummonDirector {
        let director = director();
        director.note_push(
            SERVER,
            7,
            Some(cap),
            Path::new("/data/music-bot-identities/bot-7.identity"),
        );
        for slot in director.slot_ids(SERVER) {
            director.mark_ready(SERVER, slot, HOME, &[]);
        }
        director
    }

    fn person(id: u16, channel: u64) -> ListedClient {
        ListedClient {
            id,
            channel_id: channel,
        }
    }

    #[test]
    fn nothing_stored_has_no_cap_and_does_not_borrow() {
        let director = director();
        assert_eq!(director.cap(SERVER), None);
        assert_eq!(director.quiet_count(SERVER), 0);
        director.note_push(OTHER, 1, Some(2), Path::new("/data/bot-1.identity"));
        assert_eq!(director.cap(SERVER), None);
        assert_eq!(director.quiet_count(SERVER), 0);
        assert_ne!(director.cap(OTHER), Some(0));
        assert_eq!(director.cap(OTHER), Some(2));
    }

    #[test]
    fn push_without_a_number_does_not_arm_or_copy() {
        let director = director();
        director.note_push(OTHER, 1, Some(3), Path::new("/saved/bot-3.identity"));
        director.note_push(SERVER, 8, None, Path::new("/saved/bot-8.identity"));
        assert!(!director.armed(SERVER));
        assert_eq!(director.cap(SERVER), None);
        assert_eq!(director.quiet_count(SERVER), 0);
        assert_eq!(director.saved_ids(SERVER), vec![8]);
        assert_eq!(director.cap(OTHER), Some(3));
        assert_eq!(director.quiet_count(OTHER), 3);
    }

    #[test]
    fn second_push_does_not_start_more_or_raise_the_cap() {
        let director = boot(2);
        let paths = director.identity_paths(SERVER);
        let connections: Vec<_> = director
            .slot_ids(SERVER)
            .into_iter()
            .map(|slot| director.connection_id(SERVER, slot))
            .collect();
        director.note_push(SERVER, 9, Some(9), Path::new("/saved/bot-9.identity"));
        assert_eq!(director.quiet_count(SERVER), 2);
        assert_eq!(director.cap(SERVER), Some(2));
        assert_eq!(director.identity_paths(SERVER), paths);
        assert_eq!(director.saved_ids(SERVER), vec![7, 9]);
        let after: Vec<_> = director
            .slot_ids(SERVER)
            .into_iter()
            .map(|slot| director.connection_id(SERVER, slot))
            .collect();
        assert_eq!(after, connections);
        assert_eq!(director.quiet_count(SERVER), 2);
    }

    #[test]
    fn either_saved_bot_is_enough_and_they_share_one_pool() {
        let director = director();
        director.note_push(SERVER, 1, None, Path::new("/saved/bot-1.identity"));
        director.note_push(SERVER, 2, None, Path::new("/saved/bot-2.identity"));
        assert_eq!(director.quiet_count(SERVER), 0);
        director.accept_cap(SERVER, 2).unwrap();
        assert_eq!(director.quiet_count(SERVER), 2);
        assert_eq!(director.saved_ids(SERVER), vec![1, 2]);
        assert_ne!(director.quiet_count(SERVER), 4);
    }

    #[test]
    fn accept_before_any_bot_stores_the_number_and_starts_nobody() {
        let director = director();
        director.accept_cap(SERVER, 4).unwrap();
        assert_eq!(director.cap(SERVER), Some(4));
        assert_eq!(director.quiet_count(SERVER), 0);
        assert!(!director.armed(SERVER));
        director.note_push(SERVER, 1, None, Path::new("/saved/bot-4.identity"));
        assert_eq!(
            director.quiet_count(SERVER),
            0,
            "a push without the number does not arm"
        );
        director.note_push(SERVER, 2, Some(4), Path::new("/saved/bot-5.identity"));
        assert_eq!(director.quiet_count(SERVER), 4);
    }

    #[test]
    fn cap_above_the_ceiling_is_refused_and_not_stored() {
        let director = boot(1);
        let err = director.accept_cap(SERVER, 65).unwrap_err();
        assert!(err.contains("65"));
        assert_eq!(director.cap(SERVER), Some(1));
        assert_eq!(director.quiet_count(SERVER), 1);
    }

    #[test]
    fn identities_are_unique_and_not_saved_bot_files() {
        let director = boot(3);
        let paths = director.identity_paths(SERVER);
        assert_eq!(paths.len(), 3);
        let mut names = Vec::new();
        for path in &paths {
            let name = path.file_name().unwrap().to_str().unwrap();
            names.push(name.to_string());
            assert!(!is_saved_bot_identity_name(name), "{name}");
            assert_ne!(name, "bot-1.identity");
            assert!(name.starts_with("quiet-"));
            assert_ne!(path, Path::new("/data/music-bot-identities/bot-7.identity"));
        }
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 3);
    }

    #[tokio::test]
    async fn minted_identities_are_fresh_and_not_copies() {
        let root = std::env::temp_dir().join(format!("quiet-mint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let saved = root.join("bot-1.identity");
        mint_quiet_identity(&saved)
            .await
            .expect_err("bot-1.identity is a saved-bot name");
        ts6_voice_fixture::load_or_create_identity(&saved)
            .await
            .unwrap();
        let director = SummonDirector::new(root.join("pool"));
        director.note_push(SERVER, 1, Some(2), &saved);
        let paths = director.identity_paths(SERVER);
        mint_quiet_identity(&paths[0]).await.unwrap();
        mint_quiet_identity(&paths[1]).await.unwrap();
        let first = std::fs::read(&paths[0]).unwrap();
        let second = std::fs::read(&paths[1]).unwrap();
        let saved_bytes = std::fs::read(&saved).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, saved_bytes);
        assert_ne!(second, saved_bytes);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cold_source_is_not_a_saved_library_path() {
        match cold_audio_source("https://cdn.example/a.mp3") {
            AudioSource::Url(url) => assert_eq!(url, "https://cdn.example/a.mp3"),
            AudioSource::LibraryPath(_) => panic!("library path"),
        }
        match cold_audio_source("yt: red leather") {
            AudioSource::Url(url) => assert_eq!(url, "ytsearch1:red leather"),
            AudioSource::LibraryPath(_) => panic!("library path"),
        }
        match cold_audio_source("lo-fi bed") {
            AudioSource::Url(url) => assert_eq!(url, "ytsearch1:lo-fi bed"),
            AudioSource::LibraryPath(_) => panic!("library path"),
        }
    }

    #[test]
    fn different_channels_get_one_client_and_the_same_channel_does_not_stack() {
        let director = boot(2);
        let slots = director.slot_ids(SERVER);
        let first = director.on_chat(
            SERVER,
            slots[0],
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        assert_eq!(first[0].move_to, Some(5));
        assert!(first[0].play.is_none(), "no audio before the move lands");
        assert!(first[0].reply.is_none());
        assert_eq!(
            first[0].kept_request.as_deref(),
            Some("https://cdn.example/one.mp3")
        );
        assert!(!director.offer_frame(SERVER, slots[0]));
        assert_eq!(director.frames_sent(SERVER, slots[0]), 0);

        let while_moving = director.on_chat(
            SERVER,
            slots[0],
            11,
            "!play https://cdn.example/two.mp3",
            &[person(11, 5), person(10, 5)],
        );
        assert!(
            while_moving.is_empty(),
            "a second summon before arrival does not replace the pending song"
        );
        assert_eq!(
            director.assigned_request(SERVER, slots[0]).as_deref(),
            Some("https://cdn.example/one.mp3")
        );

        let arrived = director.on_book(SERVER, slots[0], 5, &[person(10, 5), person(1, 5)]);
        assert_eq!(
            arrived[0].play.as_deref(),
            Some("https://cdn.example/one.mp3")
        );
        assert!(director.offer_frame(SERVER, slots[0]));
        assert_eq!(director.frames_sent(SERVER, slots[0]), 1);

        let stacked = director.on_chat(
            SERVER,
            slots[1],
            11,
            "!play https://cdn.example/two.mp3",
            &[person(11, 5)],
        );
        assert!(
            stacked.is_empty(),
            "the other client does not join the same channel"
        );
        let kept = director.connection_id(SERVER, slots[0]);
        let paths = director.identity_paths(SERVER);
        let sent = director.frames_sent(SERVER, slots[0]);
        let blocked = director.frames_blocked(SERVER, slots[0]);
        let book = director.on_book(SERVER, slots[0], 5, &[person(11, 5), person(10, 5)]);
        let again = director.on_chat(
            SERVER,
            slots[0],
            11,
            "!play https://cdn.example/two.mp3",
            &[person(11, 5), person(10, 5)],
        );
        assert!(
            book.is_empty() && again.is_empty(),
            "a second summon does nothing: no end-of-voice, no new song, no move"
        );
        assert!(
            again
                .iter()
                .chain(book.iter())
                .all(|instr| instr.play.is_none() && !instr.stop_audio && instr.move_to.is_none())
        );
        assert_eq!(
            director.frames_blocked(SERVER, slots[0]),
            blocked,
            "the second summon does not drop or reject the frames already in flight"
        );
        assert_eq!(
            director.assigned_request(SERVER, slots[0]).as_deref(),
            Some("https://cdn.example/one.mp3"),
            "the client keeps the song it was playing"
        );
        assert!(director.playback_open(SERVER, slots[0]));
        assert!(director.offer_frame(SERVER, slots[0]));
        assert_eq!(director.frames_sent(SERVER, slots[0]), sent + 1);
        assert_eq!(director.connection_id(SERVER, slots[0]), kept);
        assert_eq!(director.identity_paths(SERVER), paths);
        assert_eq!(director.quiet_count(SERVER), 2);

        let second = director.on_chat(
            SERVER,
            slots[1],
            12,
            "!radio https://radio.example/stream",
            &[person(12, 8)],
        );
        assert_eq!(second[0].move_to, Some(8));
        assert_eq!(director.quiet_count(SERVER), 2);
    }

    #[test]
    fn past_the_cap_is_silence_without_a_queue_or_a_refusal_record() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/a.mp3",
            &[person(10, 5)],
        );
        director.on_book(SERVER, slot, 5, &[person(10, 5)]);
        let refused = director.on_chat(
            SERVER,
            slot,
            12,
            "!play https://cdn.example/b.mp3",
            &[person(12, 9)],
        );
        assert!(
            refused
                .iter()
                .all(|instr| instr.silence || instr.play.is_none())
        );
        assert!(refused.iter().all(|instr| instr.reply.is_none()));
        assert!(refused.iter().all(|instr| instr.move_to.is_none()));
        assert!(refused.iter().all(|instr| instr.kept_request.is_none()));
        assert_eq!(director.refused_len(SERVER), 0);
        assert_eq!(director.waiter_len(SERVER), 0);
        assert_eq!(director.server_lookups(), 0);
    }

    #[test]
    fn missing_sender_keeps_the_request_and_does_not_look_up_or_move() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        let before = director.connection_id(SERVER, slot);
        let instr = director.on_chat(
            SERVER,
            slot,
            99,
            "!play https://cdn.example/hidden.mp3",
            &[person(1, HOME)],
        );
        assert_eq!(instr.len(), 1);
        assert_eq!(
            instr[0].kept_request.as_deref(),
            Some("https://cdn.example/hidden.mp3")
        );
        assert!(instr[0].move_to.is_none());
        assert!(instr[0].play.is_none());
        assert!(instr[0].reply.is_none());
        assert!(!instr[0].silence);
        assert_eq!(director.server_lookups(), 0);
        assert_eq!(director.connection_id(SERVER, slot), before);
        assert_eq!(director.waiter_len(SERVER), 0);
        assert!(!director.offer_frame(SERVER, slot));
    }

    #[test]
    fn caller_move_follows_on_the_same_connection_and_idle_returns_home() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        let connection = director.connection_id(SERVER, slot);
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/a.mp3",
            &[person(10, 5)],
        );
        director.on_book(SERVER, slot, 5, &[person(10, 5)]);
        assert!(director.playback_open(SERVER, slot));
        let follow = director.on_book(SERVER, slot, 5, &[person(10, 6)]);
        assert_eq!(follow[0].move_to, Some(6));
        assert!(
            follow[0].play.is_none(),
            "follow does not start a second resolve"
        );
        assert!(!director.offer_frame(SERVER, slot));
        assert_eq!(director.connection_id(SERVER, slot), connection);
        director.on_book(SERVER, slot, 6, &[person(10, 6)]);
        assert!(director.playback_open(SERVER, slot));

        let left = director.on_book(SERVER, slot, 6, &[]);
        assert!(left[0].stop_audio);
        assert_eq!(left[0].move_to, Some(HOME));
        assert!(!left[0].shutdown);
        assert!(!director.playback_open(SERVER, slot));
        assert_eq!(director.saved_ids(SERVER), vec![7]);
        assert_eq!(director.connection_id(SERVER, slot), connection);
    }

    #[test]
    fn follow_does_not_enter_a_channel_that_already_has_a_quiet_client() {
        let director = boot(2);
        let slots = director.slot_ids(SERVER);
        director.on_chat(
            SERVER,
            slots[0],
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        director.on_book(SERVER, slots[0], 5, &[person(10, 5)]);
        director.on_chat(
            SERVER,
            slots[1],
            12,
            "!play https://cdn.example/two.mp3",
            &[person(12, 8)],
        );
        director.on_book(SERVER, slots[1], 8, &[person(12, 8)]);
        let sent = director.frames_sent(SERVER, slots[0]);
        let occupant = director.connection_id(SERVER, slots[0]);

        let follow = director.on_book(SERVER, slots[1], 8, &[person(12, 5), person(10, 5)]);
        assert!(
            follow.iter().all(|instr| instr.move_to != Some(5)),
            "the follower does not move into the occupied channel"
        );
        assert!(follow.iter().all(|instr| instr.play.is_none()));
        assert_eq!(follow[0].move_to, Some(HOME));
        assert!(follow[0].stop_audio);
        assert!(director.playback_open(SERVER, slots[0]));
        assert_eq!(
            director.assigned_request(SERVER, slots[0]).as_deref(),
            Some("https://cdn.example/one.mp3")
        );
        assert!(director.offer_frame(SERVER, slots[0]));
        assert_eq!(director.frames_sent(SERVER, slots[0]), sent + 1);
        assert_eq!(director.connection_id(SERVER, slots[0]), occupant);
        assert!(director.assigned_request(SERVER, slots[1]).is_none());
        assert_eq!(director.quiet_count(SERVER), 2);
        assert_eq!(director.identity_paths(SERVER).len(), 2);
    }

    #[test]
    fn restore_puts_the_process_back_without_waiting_for_another_push() {
        let grown = boot(2);
        assert_eq!(grown.quiet_count(SERVER), 2);
        grown.accept_cap(SERVER, 4).unwrap();
        assert_eq!(grown.cap(SERVER), Some(4));
        assert_eq!(grown.quiet_count(SERVER), 4);
        grown.restore_cap(SERVER, Some(2)).unwrap();
        assert_eq!(grown.cap(SERVER), Some(2));
        assert_eq!(grown.quiet_count(SERVER), 2);
        grown.note_push(
            SERVER,
            8,
            None,
            Path::new("/data/music-bot-identities/bot-8.identity"),
        );
        assert_eq!(grown.cap(SERVER), Some(2));
        assert_eq!(grown.quiet_count(SERVER), 2);

        let bare = director();
        bare.note_push(
            SERVER,
            7,
            None,
            Path::new("/data/music-bot-identities/bot-7.identity"),
        );
        assert!(!bare.armed(SERVER));
        bare.accept_cap(SERVER, 3).unwrap();
        assert!(bare.armed(SERVER));
        assert_eq!(bare.quiet_count(SERVER), 3);
        bare.restore_cap(SERVER, None).unwrap();
        assert_eq!(bare.cap(SERVER), None);
        assert!(!bare.armed(SERVER));
        assert_eq!(bare.quiet_count(SERVER), 0);
        bare.note_push(
            SERVER,
            9,
            None,
            Path::new("/data/music-bot-identities/bot-9.identity"),
        );
        assert!(!bare.armed(SERVER));
        assert_eq!(bare.cap(SERVER), None);
        assert_eq!(bare.quiet_count(SERVER), 0);
    }

    #[test]
    fn idle_and_stop_return_to_sitting_and_send_no_frames() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        director.on_chat(SERVER, slot, 10, "!play yt: blue monday", &[person(10, 3)]);
        director.on_book(SERVER, slot, 3, &[person(10, 3)]);
        let sent = director.frames_sent(SERVER, slot);
        let idle = director.on_playback_finished(SERVER, slot);
        assert!(idle[0].stop_audio);
        assert_eq!(idle[0].move_to, Some(HOME));
        assert!(!idle[0].shutdown);
        assert!(!director.offer_frame(SERVER, slot));
        assert_eq!(director.frames_sent(SERVER, slot), sent);
        assert!(director.frames_blocked(SERVER, slot) >= 1);
        assert_eq!(director.saved_ids(SERVER), vec![7]);

        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/a.mp3",
            &[person(10, 3)],
        );
        director.on_book(SERVER, slot, 3, &[person(10, 3)]);
        let stopped = director.on_chat(SERVER, slot, 10, "!stop", &[person(10, 3)]);
        assert!(stopped[0].stop_audio);
        assert!(!director.playback_open(SERVER, slot));
    }

    #[test]
    fn dropping_a_quiet_client_leaves_the_saved_bot() {
        let director = boot(2);
        let slot = director.slot_ids(SERVER)[0];
        assert!(director.drop_quiet(SERVER, slot));
        assert_eq!(director.quiet_count(SERVER), 1);
        assert_eq!(director.saved_ids(SERVER), vec![7]);
        assert!(!director.drop_quiet(SERVER, slot));
    }

    #[test]
    fn sitting_client_sends_no_frames() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        assert!(director.subscribed(SERVER, slot));
        assert!(!director.offer_frame(SERVER, slot));
        assert_eq!(director.frames_sent(SERVER, slot), 0);
        assert!(director.frames_blocked(SERVER, slot) >= 1);
    }

    #[test]
    fn tech_support_name_is_the_connect_channel() {
        assert_eq!(TECH_SUPPORT_CHANNEL, "Tech Support");
    }

    #[test]
    fn a_dead_session_frees_its_slot_and_a_replacement_is_a_new_identity() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        let generation = director.generation(SERVER, slot).unwrap();
        let path = director.identity_paths(SERVER);
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        director.on_book(SERVER, slot, 5, &[person(10, 5)]);
        assert!(director.playback_open(SERVER, slot));

        let not_before_missing = director.relaunch_not_before(SERVER);
        assert!(not_before_missing.is_none());
        director.session_ended(SERVER, slot, generation);
        assert_eq!(director.quiet_count(SERVER), 0);
        assert!(director.assigned_request(SERVER, slot).is_none());
        assert!(
            director
                .on_chat_gen(
                    SERVER,
                    slot,
                    generation,
                    11,
                    "!play https://cdn.example/two.mp3",
                    &[person(11, 8)],
                )
                .is_empty()
        );
        let due = director.relaunch_not_before(SERVER).unwrap();
        director.promote_due(due - Duration::from_millis(1));
        assert_eq!(director.quiet_count(SERVER), 0);
        director.promote_due(due);
        assert_eq!(director.quiet_count(SERVER), 1);
        let replacement = director.slot_ids(SERVER)[0];
        assert_ne!(replacement, slot);
        assert_ne!(director.generation(SERVER, replacement), Some(generation));
        assert_ne!(director.identity_paths(SERVER), path);
        assert!(
            director
                .on_chat_gen(
                    SERVER,
                    replacement,
                    generation,
                    11,
                    "!play https://cdn.example/two.mp3",
                    &[person(11, 8)],
                )
                .is_empty(),
            "the exited session's generation does not steer the replacement"
        );
        assert!(director.armed(SERVER));
    }

    #[test]
    fn a_move_that_does_not_land_is_retried_then_sent_home() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        let mut moved = 0;
        let mut gave_up = false;
        for _ in 0..MAX_MOVE_ATTEMPTS {
            let book = director.on_book(SERVER, slot, HOME, &[person(10, 5)]);
            if book.iter().any(|instr| instr.move_to == Some(5)) {
                moved += 1;
            }
            if book.iter().any(|instr| instr.stop_audio) {
                gave_up = true;
                assert!(book[0].move_to.is_none(), "already home, so no move");
                break;
            }
        }
        assert!(moved >= 1);
        assert!(gave_up);
        assert!(director.assigned_request(SERVER, slot).is_none());
        assert!(!director.playback_open(SERVER, slot));
    }

    #[test]
    fn a_caller_who_switches_away_and_back_is_not_stranded() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        let away = director.on_book(SERVER, slot, HOME, &[person(10, 6)]);
        assert_eq!(away[0].move_to, Some(6));
        assert!(away[0].play.is_none());
        let back = director.on_book(SERVER, slot, HOME, &[person(10, 5)]);
        assert_eq!(back[0].move_to, Some(5));
        assert!(back.iter().all(|instr| !instr.stop_audio));
        assert_eq!(
            director.assigned_request(SERVER, slot).as_deref(),
            Some("https://cdn.example/one.mp3")
        );
        let landed = director.on_book(SERVER, slot, 5, &[person(10, 5)]);
        assert_eq!(
            landed[0].play.as_deref(),
            Some("https://cdn.example/one.mp3")
        );
        assert!(director.playback_open(SERVER, slot));
    }

    #[test]
    fn a_summon_from_tech_support_does_not_play_there() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        let summoned = director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, HOME)],
        );
        assert!(summoned.is_empty());
        assert!(!director.playback_open(SERVER, slot));
        assert!(director.assigned_request(SERVER, slot).is_none());
    }

    #[test]
    fn a_reconnect_does_not_drop_a_caller_missing_from_a_partial_list() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        let generation = director.generation(SERVER, slot).unwrap();
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        director.on_book(SERVER, slot, 5, &[person(10, 5)]);
        assert!(director.mark_reconnected(SERVER, slot, generation, 5));
        let partial = director.on_book(SERVER, slot, 5, &[]);
        assert!(partial.is_empty());
        assert!(director.playback_open(SERVER, slot));
        assert_eq!(
            director.assigned_request(SERVER, slot).as_deref(),
            Some("https://cdn.example/one.mp3")
        );
        assert!(director.note_list_trusted(SERVER, slot, generation));
        let gone = director.on_book(SERVER, slot, 5, &[]);
        assert!(gone[0].stop_audio);
    }

    #[test]
    fn the_last_saved_bot_stops_quiet_clients_and_keeps_the_cap() {
        let director = boot(1);
        let slot = director.slot_ids(SERVER)[0];
        director.on_chat(
            SERVER,
            slot,
            10,
            "!play https://cdn.example/one.mp3",
            &[person(10, 5)],
        );
        director.forget_saved(SERVER, 7);
        assert_eq!(director.quiet_count(SERVER), 0);
        assert_eq!(director.cap(SERVER), Some(1));
        assert!(!director.armed(SERVER));
        assert!(director.assigned_request(SERVER, slot).is_none());
        director.note_push(SERVER, 9, None, Path::new("/saved/bot-9.identity"));
        assert!(!director.armed(SERVER));
        assert_eq!(director.quiet_count(SERVER), 0);
        director.accept_cap(SERVER, 0).unwrap();
        assert_eq!(director.cap(SERVER), Some(0));
        assert_eq!(director.quiet_count(SERVER), 0);
    }

    #[test]
    fn equivalent_address_spellings_share_one_pool() {
        let director = director();
        director.note_push(
            "Voice.Example",
            1,
            Some(1),
            Path::new("/saved/bot-1.identity"),
        );
        director.note_push(
            "voice.example:9987",
            2,
            Some(4),
            Path::new("/saved/bot-2.identity"),
        );
        assert_eq!(director.cap("VOICE.example"), Some(1));
        assert_eq!(director.quiet_count("voice.example"), 1);
        assert_eq!(director.saved_ids("voice.example:9987"), vec![1, 2]);
        director.note_push(
            "voice.example:9988",
            3,
            Some(2),
            Path::new("/saved/bot-3.identity"),
        );
        assert_eq!(director.quiet_count("voice.example:9988"), 2);
        assert_eq!(director.quiet_count("voice.example"), 1);
        assert_eq!(canon_server_addr("Voice.Example"), "voice.example:9987");
        assert_eq!(canon_server_addr("voice.example"), "voice.example:9987");
    }
}
