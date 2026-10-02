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
    connection_id: u64,
    identity_path: PathBuf,
    home: Option<u64>,
    at: Option<u64>,
    subscribed: bool,
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
    /// Always empty. Refusals are not recorded.
    refused: Vec<u16>,
    /// Always empty. There is no wait queue.
    waiters: Vec<u16>,
}

struct SummonState {
    identity_dir: PathBuf,
    next_connection: AtomicU64,
    next_identity: u64,
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

    /// The session has subscribed on its own connection and knows the
    /// tech-support channel it landed in.
    pub fn mark_ready(&self, server: &str, slot: u32, home: u64, clients: &[ListedClient]) {
        let mut state = self.lock();
        let Some(slot) = state.slot_mut(server, slot) else {
            return;
        };
        slot.home = Some(home);
        slot.at = Some(home);
        slot.subscribed = true;
        let _ = clients;
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
        let Some(pool) = state.servers.get_mut(server) else {
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
            .servers
            .get(server)
            .map(|pool| pool.slots.len())
            .unwrap_or(0)
    }

    pub fn cap(&self, server: &str) -> Option<u32> {
        self.lock().servers.get(server).and_then(|pool| pool.cap)
    }

    pub fn armed(&self, server: &str) -> bool {
        self.lock()
            .servers
            .get(server)
            .is_some_and(|pool| pool.armed)
    }

    pub fn saved_ids(&self, server: &str) -> Vec<u64> {
        self.lock()
            .servers
            .get(server)
            .map(|pool| pool.saved_bots.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn identity_paths(&self, server: &str) -> Vec<PathBuf> {
        self.lock()
            .servers
            .get(server)
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
            .servers
            .get(server)
            .map(|pool| pool.refused.len())
            .unwrap_or(0)
    }

    pub fn waiter_len(&self, server: &str) -> usize {
        self.lock()
            .servers
            .get(server)
            .map(|pool| pool.waiters.len())
            .unwrap_or(0)
    }

    pub fn slot_ids(&self, server: &str) -> Vec<u32> {
        self.lock()
            .servers
            .get(server)
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
    fn note_push(&mut self, server: &str, bot_id: u64, cap: Option<u32>, saved_identity: &Path) {
        let arm_at = {
            let pool = self.servers.entry(server.to_string()).or_default();
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
            .servers
            .get(server)
            .map(|pool| pool.saved_identities.clone())
            .unwrap_or_default();
        let have = self
            .servers
            .get(server)
            .map(|pool| pool.slots.len())
            .unwrap_or(0);
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
            .servers
            .get(server)
            .is_some_and(|pool| !pool.saved_bots.is_empty());
        {
            let pool = self.servers.entry(server.to_string()).or_default();
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
            .servers
            .get(server)
            .map(|pool| pool.saved_identities.clone())
            .unwrap_or_default();
        while self
            .servers
            .get(server)
            .is_some_and(|pool| pool.slots.len() < cap as usize)
        {
            self.push_slot(server, &saved);
        }
        Ok(())
    }

    fn push_slot(&mut self, server: &str, saved: &[PathBuf]) {
        let slot_no = self
            .servers
            .get(server)
            .map(|pool| {
                pool.slots
                    .iter()
                    .map(|slot| slot.slot)
                    .max()
                    .map(|slot| slot + 1)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let identity_path = self.allocate_identity(server, slot_no, saved);
        let connection_id = self.next_connection.fetch_add(1, Ordering::Relaxed);
        let (stop, _) = watch::channel(false);
        let pool = self.servers.entry(server.to_string()).or_default();
        pool.slots.push(QuietSlot {
            slot: slot_no,
            connection_id,
            identity_path,
            home: None,
            at: None,
            subscribed: false,
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
        let armed = self.servers.get(server).is_some_and(|pool| pool.armed);
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
        if let Some(occupant) = self.occupant(server, channel) {
            if occupant != slot_id {
                return Vec::new();
            }
            return self.keep_on_occupant(server, slot_id, arg);
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

    fn keep_on_occupant(
        &mut self,
        server: &str,
        slot_id: u32,
        arg: String,
    ) -> Vec<QuietInstruction> {
        let Some(slot) = self.slot_mut(server, slot_id) else {
            return Vec::new();
        };
        let Phase::Out {
            request,
            arrived,
            playing,
            ..
        } = &mut slot.phase
        else {
            return Vec::new();
        };
        *request = arg.clone();
        if *arrived {
            *playing = true;
            let mut instr = QuietInstruction::bare(slot_id);
            instr.play = Some(arg);
            instr.kept_request = instr.play.clone();
            return vec![instr];
        }
        let mut instr = QuietInstruction::bare(slot_id);
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
        let Phase::Out {
            caller,
            channel,
            request,
            arrived,
            playing,
        } = &slot.phase
        else {
            return Vec::new();
        };
        let caller = *caller;
        let channel = *channel;
        let request = request.clone();
        let arrived = *arrived;
        let playing = *playing;
        match clients.iter().find(|client| client.id == caller) {
            None => self.finish(server, slot_id),
            Some(client) if client.channel_id != channel => {
                let new_channel = client.channel_id;
                let Some(slot) = self.slot_mut(server, slot_id) else {
                    return Vec::new();
                };
                if let Phase::Out {
                    channel, arrived, ..
                } = &mut slot.phase
                {
                    *channel = new_channel;
                    *arrived = own_channel == new_channel;
                }
                let mut instr = QuietInstruction::bare(slot_id);
                if own_channel != new_channel {
                    instr.move_to = Some(new_channel);
                }
                if own_channel == new_channel
                    && let Some(slot) = self.slot_mut(server, slot_id)
                    && let Phase::Out { playing, .. } = &mut slot.phase
                {
                    *playing = true;
                }
                let _ = (request, arrived, playing);
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
                        ..
                    } => {
                        *arrived = true;
                        *playing = true;
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
        let cap = self
            .servers
            .get(server)
            .and_then(|pool| pool.cap)
            .unwrap_or(0) as usize;
        let len = self
            .servers
            .get(server)
            .map(|pool| pool.slots.len())
            .unwrap_or(0);
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
        let Some(pool) = self.servers.get_mut(server) else {
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
        let pool = self.servers.get(server)?;
        pool.slots.iter().find_map(|slot| match &slot.phase {
            Phase::Out {
                channel: occupied, ..
            } if *occupied == channel => Some(slot.slot),
            _ => None,
        })
    }

    fn slot(&self, server: &str, slot_id: u32) -> Option<&QuietSlot> {
        self.servers
            .get(server)
            .and_then(|pool| pool.slots.iter().find(|slot| slot.slot == slot_id))
    }

    fn slot_mut(&mut self, server: &str, slot_id: u32) -> Option<&mut QuietSlot> {
        self.servers
            .get_mut(server)
            .and_then(|pool| pool.slots.iter_mut().find(|slot| slot.slot == slot_id))
    }
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
        let again = director.on_chat(
            SERVER,
            slots[0],
            11,
            "!play https://cdn.example/two.mp3",
            &[person(11, 5)],
        );
        assert_eq!(
            again[0].play.as_deref(),
            Some("https://cdn.example/two.mp3")
        );
        assert!(again[0].move_to.is_none());
        assert_eq!(director.connection_id(SERVER, slots[0]), kept);

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
}
