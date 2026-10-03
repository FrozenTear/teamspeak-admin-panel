//! Temporary summon clients.
//!
//! A saved music bot stays a saved music bot. A summon is a separate,
//! temporary TeamSpeak client the music process starts for one request.
//! It goes to the caller's channel, plays there, and disconnects when the
//! song ends. Nothing here writes a `music_bot_runtime` row, and nothing
//! here is returned from [`crate::supervisor::BotSupervisor::list`].
//!
//! A summon line is `!play <song>` or `!radio <station>`. It reaches this
//! module from any channel in one of these ways:
//!
//! - a private message, a poke, or server chat to a saved bot, or to a
//!   summon client that is already connected;
//! - channel chat in the channel a summon client is playing in, which
//!   that client handles itself;
//! - channel or server chat the panel's server-query subscription
//!   delivered (`POST /v1/summon-heard`).
//!
//! Channel chat in a saved bot's own channel is that bot's own command,
//! as it was before summon existed.
//!
//! A summon client connects without asking for a channel. TeamSpeak 6
//! refuses the whole connection when the default channel a client names
//! at connect is full (`ChannelMaxclientsReached`); the quiet clients
//! named "Tech Support". It lands in the server's default channel, finds
//! the caller on its own client list after `channelsubscribeall`, and
//! moves there with `clientmove`. A full channel then refuses only the
//! move.
//!
//! Every summon is one new connection from the music host, and
//! TeamSpeak counts connection attempts per IP toward its antiflood
//! block. Past it the server answers every new connection from that IP
//! with `ConnectFailedBanned`, a saved bot's reconnect included. The
//! quiet pool opened all its clients at once and then retried every few
//! seconds with a fresh identity, which kept the music host refused.
//! Summon connects are paced, never retried, and a flood refusal holds
//! every summon connect to that server back for a while.
//!
//! The cap is one number per canonical server address, forwarded by the
//! API. It limits how many summon clients may be connected at once on
//! that server. Any cap above zero allows at least
//! [`MIN_PARALLEL_SUMMONS`], so two people in two channels can both
//! summon. Cap 0 turns summon off. A server with no number stored has
//! summon off; a push that omits the number does not copy another
//! server's number.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};

use crate::chat::{ParsedCommand, parse as parse_chat};
use crate::command::AudioSource;

/// Largest cap the music process will accept. A larger value is refused
/// so it is not stored. This is not a default.
pub const MAX_SUMMON_CAP: u32 = 64;

/// Summon clients a server may hold at once when its cap is above zero
/// but smaller than this. One client per server would turn a second
/// person in another channel away while the first song plays.
pub const MIN_PARALLEL_SUMMONS: u32 = 2;

/// The same line delivered by two hearing posts within this window is
/// one summon.
const DEDUP_WINDOW: Duration = Duration::from_secs(4);

/// How long the panel's copy of a channel line waits for a saved bot in
/// that channel to report that it played the line itself.
pub(crate) const PANEL_GRACE: Duration = Duration::from_millis(400);

/// TeamSpeak adds antiflood points for every connection attempt from one
/// IP. Past `virtualserver_antiflood_points_needed_ip_block` (250 by
/// default) it refuses every new connection from that IP with
/// `ConnectFailedBanned` until the points decay, a saved bot's reconnect
/// included. A local TeamSpeak 6 server let four connects through in a
/// burst and refused the fifth. Summon connects stay below that: at most
/// this many start in any [`CONNECT_WINDOW`].
const CONNECT_BURST: usize = 3;
const CONNECT_WINDOW: Duration = Duration::from_secs(60);

/// A summon that would have to wait longer than this for its connect
/// slot is refused instead of queued.
const MAX_CONNECT_WAIT: Duration = Duration::from_secs(20);

/// After the server refuses a summon client as banned or flooding, no
/// summon client connects to that server for this long. Another attempt
/// would add points and keep the block alive.
const FLOOD_COOLDOWN: Duration = Duration::from_secs(120);

/// Same key the music page uses. Defined in the shared crate so the
/// wasm client does not link this crate.
pub use ts6_manager_shared::music_bots::canon_server_addr;

/// One person on a summon client's own client list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListedClient {
    pub id: u16,
    pub channel_id: u64,
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

/// Where a summon line was heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// A private message, a poke, or server chat to one of our clients.
    Crossing,
    /// Channel or server chat the panel's server-query subscription
    /// delivered.
    Panel,
}

/// What a connected summon client is told to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCmd {
    /// Replace the song with this request.
    Play(String),
    /// Stop and disconnect.
    Stop,
}

/// The director's answer once a summon client knows the caller's channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seat {
    /// Go to that channel and play this request.
    Go { request: String },
    /// Another summon client already plays in that channel.
    Occupied,
    /// A saved bot sits in that channel. Chat there is its own command.
    SavedBot,
    /// This summon was stopped before it found the caller.
    Gone,
}

struct Summon {
    id: u64,
    caller: u16,
    request: String,
    /// The caller's channel, once this client was seated there.
    channel: Option<u64>,
    identity_path: PathBuf,
    /// When this client may open its connection.
    connect_at: Instant,
    launched: bool,
    stop: watch::Sender<bool>,
    cmds: mpsc::UnboundedSender<SessionCmd>,
    /// Handed to the session when it launches.
    cmds_rx: Option<mpsc::UnboundedReceiver<SessionCmd>>,
}

#[derive(Default)]
struct ServerPool {
    cap: Option<u32>,
    armed: bool,
    saved_bots: BTreeSet<u64>,
    saved_identities: Vec<PathBuf>,
    /// The address as a saved bot dials it. Summon clients dial the same
    /// string, not the lowercased pool key.
    dial: Option<String>,
    summons: Vec<Summon>,
    /// Last client list a summon client on this server reported.
    clients: Vec<ListedClient>,
    /// Lines already acted on, so a second hearing post is not a second
    /// summon.
    recent: VecDeque<(u16, String, Instant)>,
    /// Lines a saved bot answers for. See [`saved_bot_claims_line`].
    claimed: VecDeque<(u16, String, Instant)>,
    /// Start times of summon connects inside the pacing window.
    connects: VecDeque<Instant>,
    /// No summon client connects before this, after a flood refusal.
    flood_until: Option<Instant>,
}

impl ServerPool {
    /// When the next summon client may connect, or `None` when that is
    /// more than [`MAX_CONNECT_WAIT`] away.
    fn next_connect_slot(&mut self, now: Instant) -> Option<Instant> {
        self.connects
            .retain(|at| now.saturating_duration_since(*at) < CONNECT_WINDOW);
        let mut at = now;
        if let Some(until) = self.flood_until {
            if until > at {
                at = until;
            } else {
                self.flood_until = None;
            }
        }
        if self.connects.len() >= CONNECT_BURST {
            let mut starts: Vec<Instant> = self.connects.iter().copied().collect();
            starts.sort();
            let gate = starts[starts.len() - CONNECT_BURST] + CONNECT_WINDOW;
            if gate > at {
                at = gate;
            }
        }
        (at.saturating_duration_since(now) <= MAX_CONNECT_WAIT).then_some(at)
    }

    fn prune(&mut self, now: Instant) {
        let fresh = |at: &Instant| now.saturating_duration_since(*at) < DEDUP_WINDOW;
        self.recent.retain(|(_, _, at)| fresh(at));
        self.claimed.retain(|(_, _, at)| fresh(at));
    }

    fn limit(&self) -> usize {
        if self.armed {
            parallel_limit(self.cap)
        } else {
            0
        }
    }
}

struct SummonState {
    identity_dir: PathBuf,
    next_summon: u64,
    servers: BTreeMap<String, ServerPool>,
}

/// Summon clients a server may hold at once for a stored cap. Zero means
/// summon is off.
pub fn parallel_limit(cap: Option<u32>) -> usize {
    match cap {
        None | Some(0) => 0,
        Some(cap) => cap.max(MIN_PARALLEL_SUMMONS) as usize,
    }
}

/// The process's live director, so a saved bot can hand a summon line
/// over. Tests use their own director and do not install this.
fn live_director() -> Option<SummonDirector> {
    LIVE_DIRECTOR
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone()
}

static LIVE_DIRECTOR: Mutex<Option<SummonDirector>> = Mutex::new(None);

/// A saved bot heard a summon line as a private message, a poke, or
/// server chat. The caller is not in that bot's channel.
pub(crate) fn saved_bot_heard_crossing(server: &str, caller: u16, line: &str) {
    if let Some(director) = live_director() {
        director.hear(server, caller, line, Heard::Crossing);
    }
}

/// A saved bot answers for this line: it played the caller's channel
/// chat, or the caller sits in that bot's channel. The panel's copy of
/// the same line must not summon a second client into that channel.
pub(crate) fn saved_bot_claims_line(server: &str, caller: u16, line: &str) {
    if let Some(director) = live_director() {
        director.note_saved_bot_line(server, caller, line);
    }
}

/// Per-server summon clients.
#[derive(Clone)]
pub struct SummonDirector {
    inner: Arc<Mutex<SummonState>>,
    live: Arc<Mutex<Option<LiveQuiet>>>,
}

pub(crate) struct SlotLaunch {
    pub(crate) server: String,
    pub(crate) dial: String,
    pub(crate) summon: u64,
    pub(crate) caller: u16,
    pub(crate) identity_path: PathBuf,
    pub(crate) connect_at: Instant,
    pub(crate) stop: watch::Receiver<bool>,
    pub(crate) cmds: mpsc::UnboundedReceiver<SessionCmd>,
}

impl SummonDirector {
    pub fn new(identity_dir: impl Into<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SummonState {
                identity_dir: identity_dir.into(),
                next_summon: 1,
                servers: BTreeMap::new(),
            })),
            live: Arc::new(Mutex::new(None)),
        }
    }

    /// Remember a saved bot. `cap` is the number carried on this push.
    /// It applies only when this server has no number yet. A number the
    /// process already accepted is kept, and an unarmed pool arms with
    /// that number. `None` does not invent a number and does not read
    /// any other server.
    pub fn note_push(&self, server: &str, bot_id: u64, cap: Option<u32>, saved_identity: &Path) {
        self.lock().note_push(server, bot_id, cap, saved_identity);
    }

    /// Store the cap for this server. It arms summon only when at least
    /// one saved bot for this server is already known. A value above
    /// [`MAX_SUMMON_CAP`] is refused and stored nowhere. A summon client
    /// that is already playing keeps its song.
    pub fn accept_cap(&self, server: &str, cap: u32) -> Result<(), String> {
        self.lock().accept_cap(server, cap)
    }

    /// Put this server back to a cap the database still has. `None` means
    /// nothing is stored: the number is cleared and no new summon starts.
    /// A summon client that is already playing keeps its song.
    pub fn restore_cap(&self, server: &str, cap: Option<u32>) -> Result<(), String> {
        let mut state = self.lock();
        match cap {
            Some(cap) => state.accept_cap(server, cap),
            None => state.clear_cap(server),
        }
    }

    /// A saved bot is gone. The last one on a server disarms summon there
    /// and stops that server's summon clients. The stored cap is left in
    /// place.
    pub fn forget_saved(&self, server: &str, bot_id: u64) {
        self.lock().forget_saved(server, bot_id);
    }

    /// A summon line one of our clients heard, or the panel delivered.
    /// Starts a summon client when the line is new, the server is armed,
    /// and the server is below its limit. A line for a channel that
    /// already has a summon client goes to that client.
    pub fn hear(&self, server: &str, caller: u16, line: &str, heard: Heard) {
        self.lock()
            .hear(server, caller, line, heard, Instant::now());
        self.launch_pending();
    }

    /// Channel or server chat the panel's server-query subscription
    /// delivered. The notify carries a host and no voice port. A missing port is the
    /// default voice port, so this reaches only that pool. An explicit
    /// different port is a separate pool and is left alone.
    ///
    /// A saved bot in the caller's channel answers for its own channel. It
    /// usually says so before the panel's copy arrives; the copy waits
    /// [`PANEL_GRACE`] so a late report still wins.
    pub fn hear_for_host(&self, host: &str, caller: u16, line: &str) {
        let server = canon_server_addr(host);
        if server.is_empty() || song_arg(line).is_none() {
            return;
        }
        if !self.is_live() {
            self.hear(&server, caller, line, Heard::Panel);
            return;
        }
        let director = self.clone();
        let line = line.to_string();
        crate::runtime::voice_runtime().spawn(async move {
            tokio::time::sleep(PANEL_GRACE).await;
            director.hear(&server, caller, &line, Heard::Panel);
        });
    }

    /// A saved bot answers for this line. See [`saved_bot_claims_line`].
    pub fn note_saved_bot_line(&self, server: &str, caller: u16, line: &str) {
        self.lock()
            .note_saved_bot_line(server, caller, line, Instant::now());
    }

    /// The summon client found its caller in `channel`. `saved_bot_here`
    /// is true when a saved bot's identity is on that channel's list.
    pub fn seat(&self, server: &str, summon: u64, channel: u64, saved_bot_here: bool) -> Seat {
        self.lock().seat(server, summon, channel, saved_bot_here)
    }

    /// The server refused a summon client as banned or flooding. No
    /// summon client connects to it for [`FLOOD_COOLDOWN`].
    pub fn note_flood_refusal(&self, server: &str) {
        let mut state = self.lock();
        if let Some(pool) = state.pool_mut(server) {
            pool.flood_until = Some(Instant::now() + FLOOD_COOLDOWN);
        }
    }

    /// The client list a connected summon client sees.
    pub fn note_clients(&self, server: &str, summon: u64, clients: &[ListedClient]) {
        let mut state = self.lock();
        let Some(pool) = state.pool_mut(server) else {
            return;
        };
        if pool.summons.iter().any(|s| s.id == summon) {
            pool.clients = clients.to_vec();
        }
    }

    /// The summon client disconnected. Its place is free. Nothing starts
    /// a replacement: a summon is one request.
    pub fn session_ended(&self, server: &str, summon: u64) {
        let mut state = self.lock();
        let Some(pool) = state.pool_mut(server) else {
            return;
        };
        if let Some(index) = pool.summons.iter().position(|s| s.id == summon) {
            let removed = pool.summons.remove(index);
            let _ = removed.stop.send(true);
        }
        if pool.summons.is_empty() {
            pool.clients.clear();
        }
    }

    /// Stop one summon client. Saved bots for the server stay.
    pub fn drop_summon(&self, server: &str, summon: u64) -> bool {
        let mut state = self.lock();
        let Some(pool) = state.pool_mut(server) else {
            return false;
        };
        let Some(index) = pool.summons.iter().position(|s| s.id == summon) else {
            return false;
        };
        let removed = pool.summons.remove(index);
        let _ = removed.stop.send(true);
        let _ = removed.cmds.send(SessionCmd::Stop);
        true
    }

    /// Saved-bot identity files for this server. A summon client reads
    /// their UIDs to see whether the caller's channel has a saved bot.
    pub fn saved_identities(&self, server: &str) -> Vec<PathBuf> {
        self.lock()
            .pool(server)
            .map(|pool| pool.saved_identities.clone())
            .unwrap_or_default()
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
        *LIVE_DIRECTOR.lock().unwrap_or_else(|err| err.into_inner()) = Some(self.clone());
        self.launch_pending();
    }

    /// Summon clients this server holds right now: connecting, moving, or
    /// playing.
    pub fn quiet_count(&self, server: &str) -> usize {
        self.lock()
            .pool(server)
            .map(|pool| pool.summons.len())
            .unwrap_or(0)
    }

    /// Summon clients this server may hold at once. Zero when summon is
    /// off or not armed.
    pub fn limit(&self, server: &str) -> usize {
        self.lock().pool(server).map(ServerPool::limit).unwrap_or(0)
    }

    pub fn cap(&self, server: &str) -> Option<u32> {
        self.lock().pool(server).and_then(|pool| pool.cap)
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

    pub fn summon_ids(&self, server: &str) -> Vec<u64> {
        self.lock()
            .pool(server)
            .map(|pool| pool.summons.iter().map(|s| s.id).collect())
            .unwrap_or_default()
    }

    pub fn identity_paths(&self, server: &str) -> Vec<PathBuf> {
        self.lock()
            .pool(server)
            .map(|pool| {
                pool.summons
                    .iter()
                    .map(|s| s.identity_path.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Song a summon client was asked for, if it is still here.
    pub fn request(&self, server: &str, summon: u64) -> Option<String> {
        self.lock()
            .summon(server, summon)
            .map(|s| s.request.clone())
    }

    /// Channel a summon client was seated in, if any.
    pub fn seated_channel(&self, server: &str, summon: u64) -> Option<u64> {
        self.lock().summon(server, summon).and_then(|s| s.channel)
    }

    fn is_live(&self) -> bool {
        self.live
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .is_some()
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
            let summon = launch.summon;
            crate::runtime::voice_runtime().spawn(async move {
                match crate::quiet_session::run(director, launch, live).await {
                    Ok(outcome) => {
                        tracing::info!(%server, summon, %outcome, "summon client left");
                    }
                    Err(err) => {
                        // `{:#}` keeps the source chain. The TeamSpeak
                        // error under "summon client handshake" is the
                        // part that says why the server refused.
                        tracing::warn!(
                            %server,
                            summon,
                            error = %format!("{err:#}"),
                            "summon client failed"
                        );
                    }
                }
            });
        }
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

    fn summon(&self, server: &str, summon: u64) -> Option<&Summon> {
        self.pool(server)?.summons.iter().find(|s| s.id == summon)
    }

    fn note_push(&mut self, server: &str, bot_id: u64, cap: Option<u32>, saved_identity: &Path) {
        let pool = self.pool_entry(server);
        pool.saved_bots.insert(bot_id);
        if !pool
            .saved_identities
            .iter()
            .any(|path| path == saved_identity)
        {
            pool.saved_identities.push(saved_identity.to_path_buf());
        }
        if pool.dial.is_none() && !server.trim().is_empty() {
            pool.dial = Some(server.trim().to_string());
        }
        // An armed pool already has its number. A later push must not
        // replace it.
        if pool.armed {
            return;
        }
        if pool.cap.is_some() {
            // The process already accepted that number. The value on this
            // push is not a newer save.
            pool.armed = true;
        } else if let Some(cap) = cap {
            pool.cap = Some(cap);
            pool.armed = true;
        }
    }

    fn accept_cap(&mut self, server: &str, cap: u32) -> Result<(), String> {
        if server.trim().is_empty() {
            return Err("server address is empty".into());
        }
        if cap > MAX_SUMMON_CAP {
            return Err(format!("cap {cap} is above {MAX_SUMMON_CAP}"));
        }
        let pool = self.pool_entry(server);
        pool.cap = Some(cap);
        if !pool.saved_bots.is_empty() {
            pool.armed = true;
        }
        Ok(())
    }

    fn clear_cap(&mut self, server: &str) -> Result<(), String> {
        if server.trim().is_empty() {
            return Err("server address is empty".into());
        }
        let Some(pool) = self.pool_mut(server) else {
            return Ok(());
        };
        pool.cap = None;
        pool.armed = false;
        // A client that is not seated yet has not played anything. It
        // stops. A seated client keeps its song.
        let waiting: Vec<usize> = pool
            .summons
            .iter()
            .enumerate()
            .filter(|(_, s)| s.channel.is_none())
            .map(|(index, _)| index)
            .rev()
            .collect();
        for index in waiting {
            let removed = pool.summons.remove(index);
            let _ = removed.stop.send(true);
        }
        Ok(())
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
        pool.clients.clear();
        for summon in std::mem::take(&mut pool.summons) {
            let _ = summon.stop.send(true);
        }
    }

    fn note_saved_bot_line(&mut self, server: &str, caller: u16, line: &str, now: Instant) {
        if song_arg(line).is_none() {
            return;
        }
        let Some(pool) = self.pool_mut(server) else {
            return;
        };
        pool.prune(now);
        pool.claimed.push_back((caller, line.to_string(), now));
    }

    fn hear(&mut self, server: &str, caller: u16, line: &str, heard: Heard, now: Instant) {
        let Some(arg) = song_arg(line) else {
            return;
        };
        let next_id = self.next_summon;
        let identity_dir = self.identity_dir.clone();
        let Some(pool) = self.pool_mut(server) else {
            return;
        };
        let limit = pool.limit();
        if limit == 0 {
            return;
        }
        pool.prune(now);
        if heard == Heard::Panel
            && pool
                .claimed
                .iter()
                .any(|(who, text, _)| *who == caller && text == line)
        {
            // A saved bot in that channel played it.
            return;
        }
        if pool
            .recent
            .iter()
            .any(|(who, text, _)| *who == caller && text == line)
        {
            return;
        }
        pool.recent.push_back((caller, line.to_string(), now));

        let caller_at = pool
            .clients
            .iter()
            .find(|client| client.id == caller)
            .map(|client| client.channel_id);
        if let Some(channel) = caller_at
            && let Some(there) = pool.summons.iter().find(|s| s.channel == Some(channel))
        {
            // That channel already has a summon client. It heard channel
            // chat itself; anything else is handed to it.
            if heard == Heard::Crossing {
                let _ = there.cmds.send(SessionCmd::Play(arg));
            }
            return;
        }
        if let Some(waiting) = pool
            .summons
            .iter_mut()
            .find(|s| s.caller == caller && s.channel.is_none())
        {
            // Still on its way. Play the newer line when it arrives.
            waiting.request = arg;
            return;
        }
        if pool.summons.len() >= limit {
            tracing::info!(
                server = %canon_server_addr(server),
                caller,
                limit,
                "summon refused: this server already has its limit of summon clients"
            );
            return;
        }
        let Some(connect_at) = pool.next_connect_slot(now) else {
            tracing::info!(
                server = %canon_server_addr(server),
                caller,
                "summon refused: another connect now could trip the server's flood protection"
            );
            return;
        };
        pool.connects.push_back(connect_at);
        let identity_path = summon_identity_path(&identity_dir, server, next_id, pool);
        let (stop, _) = watch::channel(false);
        let (cmds, cmds_rx) = mpsc::unbounded_channel();
        pool.summons.push(Summon {
            id: next_id,
            caller,
            request: arg,
            channel: None,
            identity_path,
            connect_at,
            launched: false,
            stop,
            cmds,
            cmds_rx: Some(cmds_rx),
        });
        self.next_summon = next_id.saturating_add(1);
    }

    fn seat(&mut self, server: &str, summon: u64, channel: u64, saved_bot_here: bool) -> Seat {
        let Some(pool) = self.pool_mut(server) else {
            return Seat::Gone;
        };
        if !pool.summons.iter().any(|s| s.id == summon) {
            return Seat::Gone;
        }
        if saved_bot_here {
            return Seat::SavedBot;
        }
        if pool
            .summons
            .iter()
            .any(|s| s.id != summon && s.channel == Some(channel))
        {
            return Seat::Occupied;
        }
        let Some(seated) = pool.summons.iter_mut().find(|s| s.id == summon) else {
            return Seat::Gone;
        };
        seated.channel = Some(channel);
        Seat::Go {
            request: seated.request.clone(),
        }
    }

    fn claim_unlaunched(&mut self) -> Vec<SlotLaunch> {
        let mut out = Vec::new();
        for (server, pool) in &mut self.servers {
            let dial = pool.dial.clone().unwrap_or_else(|| server.clone());
            for summon in &mut pool.summons {
                if summon.launched {
                    continue;
                }
                let Some(cmds) = summon.cmds_rx.take() else {
                    continue;
                };
                summon.launched = true;
                out.push(SlotLaunch {
                    server: server.clone(),
                    dial: dial.clone(),
                    summon: summon.id,
                    caller: summon.caller,
                    identity_path: summon.identity_path.clone(),
                    connect_at: summon.connect_at,
                    stop: summon.stop.subscribe(),
                    cmds,
                });
            }
        }
        out
    }
}

/// A file under the summon identity directory that no saved bot and no
/// other summon client on any server uses.
fn summon_identity_path(identity_dir: &Path, server: &str, id: u64, pool: &ServerPool) -> PathBuf {
    let slug = server_slug(&canon_server_addr(server));
    let name = format!("summon-{id}.identity");
    debug_assert!(
        !is_saved_bot_identity_name(&name),
        "a summon identity must not use a saved-bot file name"
    );
    let path = identity_dir.join(&slug).join(name);
    debug_assert!(
        !pool.saved_identities.iter().any(|saved| saved == &path),
        "a summon identity must not be a saved bot's file"
    );
    path
}

fn song_arg(line: &str) -> Option<String> {
    match parse_chat(line) {
        Ok(ParsedCommand::Play { arg } | ParsedCommand::Radio { arg }) => Some(arg),
        _ => None,
    }
}

/// True for `!play <song>` and `!radio <station>`.
pub fn is_summon_line(line: &str) -> bool {
    song_arg(line).is_some()
}

/// A summoned song is resolved from the request text when playback
/// starts. It is not a saved bot's library path and not another
/// client's identity or queue.
pub fn cold_audio_source(arg: &str) -> AudioSource {
    let trimmed = unwrap_bbcode_url(arg.trim());
    // The fixture end-to-end test plays an in-process tone. A release
    // build sends that text to the search like any other.
    #[cfg(feature = "lifecycle-e2e")]
    if trimmed.starts_with("synthetic:") {
        return AudioSource::Url(trimmed.to_string());
    }
    if is_http(trimmed) {
        return AudioSource::Url(trimmed.to_string());
    }
    if let Some(query) = yt_query(trimmed) {
        return AudioSource::Url(format!("ytsearch1:{query}"));
    }
    AudioSource::Url(format!("ytsearch1:{trimmed}"))
}

/// TeamSpeak clients send a typed link as `[URL]link[/URL]`, or as
/// `[URL=link]text[/URL]`. The link is the request.
fn unwrap_bbcode_url(arg: &str) -> &str {
    let lower = arg.to_ascii_lowercase();
    if !lower.starts_with("[url") || !lower.ends_with("[/url]") {
        return arg;
    }
    let inner = &arg[..arg.len() - "[/url]".len()];
    if let Some(rest) = inner
        .strip_prefix("[URL]")
        .or_else(|| inner.strip_prefix("[url]"))
    {
        return rest.trim();
    }
    let open = &inner[4..];
    if let Some(rest) = open.strip_prefix('=')
        && let Some((link, _text)) = rest.split_once(']')
    {
        return link.trim();
    }
    arg
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

/// Write a new identity at `path`. A file left there by an earlier
/// summon is removed first, so every summon is a fresh key and never a
/// copy of another client's file.
pub async fn mint_quiet_identity(path: &Path) -> anyhow::Result<tsclientlib::Identity> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if is_saved_bot_identity_name(name) {
        anyhow::bail!("refusing to mint a saved-bot identity name");
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(anyhow::Error::new(err)
                .context(format!("remove old summon identity {}", path.display())));
        }
    }
    ts6_voice_fixture::load_or_create_identity(path).await
}

/// Remove a summon identity once its client is gone. A saved-bot file
/// name is never removed here.
pub async fn discard_quiet_identity(path: &Path) {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if is_saved_bot_identity_name(name) {
        return;
    }
    if let Err(err) = tokio::fs::remove_file(path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(path = %path.display(), error = %err, "summon identity was not removed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: &str = "voice.example:9987";
    const OTHER: &str = "other.example:9987";
    const PLAY: &str = "!play https://cdn.example/one.mp3";
    const SONG: &str = "https://cdn.example/one.mp3";

    fn director() -> SummonDirector {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(1);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("quiet-summon-{}-{n}", std::process::id()));
        SummonDirector::new(dir)
    }

    fn armed(cap: u32) -> SummonDirector {
        let director = director();
        director.note_push(
            SERVER,
            7,
            Some(cap),
            Path::new("/data/music-bot-identities/bot-7.identity"),
        );
        director
    }

    fn person(id: u16, channel: u64) -> ListedClient {
        ListedClient {
            id,
            channel_id: channel,
        }
    }

    fn only_summon(director: &SummonDirector) -> u64 {
        let ids = director.summon_ids(SERVER);
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids[0]
    }

    #[test]
    fn nothing_stored_has_no_cap_and_does_not_borrow() {
        let director = director();
        assert_eq!(director.cap(SERVER), None);
        assert_eq!(director.limit(SERVER), 0);
        director.note_push(OTHER, 1, Some(2), Path::new("/data/bot-1.identity"));
        assert_eq!(director.cap(SERVER), None);
        assert_eq!(director.limit(SERVER), 0);
        assert_eq!(director.cap(OTHER), Some(2));
        director.hear(SERVER, 3, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 0);
    }

    #[test]
    fn push_without_a_number_does_not_arm_or_copy() {
        let director = director();
        director.note_push(OTHER, 1, Some(4), Path::new("/data/bot-1.identity"));
        director.note_push(SERVER, 2, None, Path::new("/data/bot-2.identity"));
        assert!(!director.armed(SERVER));
        assert_eq!(director.cap(SERVER), None);
        director.hear(SERVER, 3, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 0);
    }

    #[test]
    fn a_second_push_does_not_raise_the_cap() {
        let director = armed(2);
        director.note_push(
            SERVER,
            8,
            Some(9),
            Path::new("/data/music-bot-identities/bot-8.identity"),
        );
        assert_eq!(director.cap(SERVER), Some(2));
        assert_eq!(director.saved_ids(SERVER), vec![7, 8]);
        assert_eq!(director.limit(SERVER), 2);
    }

    #[test]
    fn accept_before_any_bot_stores_the_number_and_starts_nobody() {
        let director = director();
        director.accept_cap(SERVER, 3).unwrap();
        assert_eq!(director.cap(SERVER), Some(3));
        assert!(!director.armed(SERVER));
        director.hear(SERVER, 3, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 0);

        // A push that carries no number arms the number already held.
        director.note_push(SERVER, 1, None, Path::new("/data/bot-1.identity"));
        assert!(director.armed(SERVER));
        assert_eq!(director.cap(SERVER), Some(3));
        assert_eq!(director.limit(SERVER), 3);
    }

    #[test]
    fn a_push_keeps_the_cap_the_process_already_accepted() {
        let director = director();
        director.accept_cap(SERVER, 4).unwrap();
        director.note_push(SERVER, 1, Some(1), Path::new("/data/bot-1.identity"));
        assert_eq!(director.cap(SERVER), Some(4));
        assert!(director.armed(SERVER));
    }

    #[test]
    fn cap_above_the_ceiling_is_refused_and_not_stored() {
        let director = armed(2);
        assert!(director.accept_cap(SERVER, MAX_SUMMON_CAP + 1).is_err());
        assert_eq!(director.cap(SERVER), Some(2));
        assert!(director.accept_cap("  ", 1).is_err());
        director.accept_cap(SERVER, MAX_SUMMON_CAP).unwrap();
        assert_eq!(director.limit(SERVER), MAX_SUMMON_CAP as usize);
    }

    #[test]
    fn any_cap_above_zero_lets_two_channels_summon() {
        assert_eq!(parallel_limit(None), 0);
        assert_eq!(parallel_limit(Some(0)), 0);
        assert_eq!(parallel_limit(Some(1)), 2);
        assert_eq!(parallel_limit(Some(2)), 2);
        assert_eq!(parallel_limit(Some(5)), 5);

        let director = armed(1);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 11, "!play yt:second song", Heard::Crossing);
        let ids = director.summon_ids(SERVER);
        assert_eq!(
            ids.len(),
            2,
            "a second person in another channel is not turned away"
        );
        assert_eq!(
            director.seat(SERVER, ids[0], 42, false),
            Seat::Go {
                request: SONG.into()
            }
        );
        assert_eq!(
            director.seat(SERVER, ids[1], 43, false),
            Seat::Go {
                request: "yt:second song".into()
            }
        );
    }

    #[test]
    fn past_the_limit_nothing_starts_and_a_finished_summon_frees_its_place() {
        let director = armed(2);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 11, PLAY, Heard::Crossing);
        director.hear(SERVER, 12, PLAY, Heard::Crossing);
        let ids = director.summon_ids(SERVER);
        assert_eq!(ids.len(), 2);

        director.session_ended(SERVER, ids[0]);
        assert_eq!(director.quiet_count(SERVER), 1);
        director.hear(
            SERVER,
            12,
            "!play https://cdn.example/two.mp3",
            Heard::Crossing,
        );
        assert_eq!(director.quiet_count(SERVER), 2);
    }

    #[test]
    fn cap_zero_turns_summon_off() {
        let director = armed(0);
        assert!(director.armed(SERVER));
        assert_eq!(director.limit(SERVER), 0);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 0);
    }

    #[test]
    fn a_summon_from_any_channel_is_seated_there() {
        // The quiet pool's old home was "Tech Support". A caller there is
        // seated there like a caller anywhere else.
        for channel in [1_u64, 2, 42] {
            let director = armed(2);
            director.hear(SERVER, 10, PLAY, Heard::Crossing);
            let id = only_summon(&director);
            assert_eq!(
                director.seat(SERVER, id, channel, false),
                Seat::Go {
                    request: SONG.into()
                }
            );
            assert_eq!(director.seated_channel(SERVER, id), Some(channel));
        }
    }

    #[test]
    fn one_channel_gets_one_summon_client() {
        let director = armed(3);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(
            SERVER,
            11,
            "!radio https://radio.example/live",
            Heard::Crossing,
        );
        let ids = director.summon_ids(SERVER);
        assert_eq!(ids.len(), 2);
        assert!(matches!(
            director.seat(SERVER, ids[0], 42, false),
            Seat::Go { .. }
        ));
        assert_eq!(director.seat(SERVER, ids[1], 42, false), Seat::Occupied);
    }

    #[test]
    fn a_saved_bot_channel_is_not_summoned_into() {
        let director = armed(2);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        let id = only_summon(&director);
        assert_eq!(director.seat(SERVER, id, 42, true), Seat::SavedBot);
        assert_eq!(director.seated_channel(SERVER, id), None);
    }

    #[test]
    fn one_line_from_two_hearing_posts_is_one_summon() {
        let director = armed(4);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 10, PLAY, Heard::Panel);
        assert_eq!(director.quiet_count(SERVER), 1);
    }

    #[test]
    fn a_panel_copy_of_a_line_the_saved_bot_played_does_not_summon() {
        let director = armed(2);
        director.note_saved_bot_line(SERVER, 10, PLAY);
        director.hear(SERVER, 10, PLAY, Heard::Panel);
        assert_eq!(director.quiet_count(SERVER), 0);
        // The same person typing in another channel, as a private
        // message, is still a summon.
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 1);
    }

    #[test]
    fn a_line_for_an_occupied_channel_goes_to_its_client() {
        let director = armed(2);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        let id = only_summon(&director);
        let mut launch = director.lock().claim_unlaunched().pop().unwrap();
        assert!(matches!(
            director.seat(SERVER, id, 42, false),
            Seat::Go { .. }
        ));
        director.note_clients(SERVER, id, &[person(10, 42), person(11, 42), person(12, 9)]);

        // Someone else in that channel, by private message.
        director.hear(SERVER, 11, "!play yt:next one", Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 1);
        assert_eq!(
            launch.cmds.try_recv().unwrap(),
            SessionCmd::Play("yt:next one".into())
        );
        // The panel's copy of channel chat there: the client heard it.
        director.hear(SERVER, 11, "!play yt:third", Heard::Panel);
        assert!(launch.cmds.try_recv().is_err());
        assert_eq!(director.quiet_count(SERVER), 1);
        // Someone in another channel gets their own client.
        director.hear(SERVER, 12, "!play yt:elsewhere", Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 2);
    }

    #[test]
    fn a_caller_still_waiting_gets_the_newer_song() {
        let director = armed(2);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 10, "!play yt:changed my mind", Heard::Crossing);
        let id = only_summon(&director);
        assert_eq!(
            director.seat(SERVER, id, 42, false),
            Seat::Go {
                request: "yt:changed my mind".into()
            }
        );
    }

    #[test]
    fn lines_that_are_not_songs_start_nothing() {
        let director = armed(2);
        for line in ["!stop", "!np", "hello", "!play", ""] {
            director.hear(SERVER, 10, line, Heard::Crossing);
        }
        assert_eq!(director.quiet_count(SERVER), 0);
    }

    #[test]
    fn a_stopped_summon_cannot_be_seated() {
        let director = armed(2);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        let id = only_summon(&director);
        assert!(director.drop_summon(SERVER, id));
        assert_eq!(director.seat(SERVER, id, 42, false), Seat::Gone);
        assert_eq!(director.quiet_count(SERVER), 0);
    }

    #[test]
    fn clearing_the_cap_keeps_a_playing_client_and_stops_new_ones() {
        let director = armed(2);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 11, PLAY, Heard::Crossing);
        let ids = director.summon_ids(SERVER);
        assert!(matches!(
            director.seat(SERVER, ids[0], 42, false),
            Seat::Go { .. }
        ));
        director.restore_cap(SERVER, None).unwrap();
        assert_eq!(director.cap(SERVER), None);
        assert!(!director.armed(SERVER));
        assert_eq!(director.summon_ids(SERVER), vec![ids[0]]);
        director.hear(SERVER, 12, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 1);
    }

    #[test]
    fn the_last_saved_bot_stops_summon_clients_and_keeps_the_cap() {
        let director = armed(2);
        director.note_push(
            SERVER,
            8,
            None,
            Path::new("/data/music-bot-identities/bot-8.identity"),
        );
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        let mut launch = director.lock().claim_unlaunched().pop().unwrap();
        director.forget_saved(SERVER, 7);
        assert!(director.armed(SERVER));
        assert_eq!(director.quiet_count(SERVER), 1);
        director.forget_saved(SERVER, 8);
        assert!(!director.armed(SERVER));
        assert_eq!(director.quiet_count(SERVER), 0);
        assert_eq!(director.cap(SERVER), Some(2));
        assert!(*launch.stop.borrow_and_update());

        // A later push arms again with the number the process holds.
        director.note_push(
            SERVER,
            9,
            None,
            Path::new("/data/music-bot-identities/bot-9.identity"),
        );
        assert!(director.armed(SERVER));
        assert_eq!(director.limit(SERVER), 2);
    }

    #[test]
    fn identities_are_unique_and_not_saved_bot_files() {
        let director = armed(4);
        director.hear(SERVER, 10, PLAY, Heard::Crossing);
        director.hear(SERVER, 11, PLAY, Heard::Crossing);
        director.hear(SERVER, 12, PLAY, Heard::Crossing);
        let paths = director.identity_paths(SERVER);
        assert_eq!(paths.len(), 3);
        for (i, path) in paths.iter().enumerate() {
            let name = path.file_name().and_then(|name| name.to_str()).unwrap();
            assert!(name.starts_with("summon-"), "{name}");
            assert!(!is_saved_bot_identity_name(name), "{name}");
            assert!(!paths[..i].contains(path));
            assert_ne!(
                path.as_path(),
                Path::new("/data/music-bot-identities/bot-7.identity")
            );
        }
        assert!(is_saved_bot_identity_name("bot-1.identity"));
        assert!(is_saved_bot_identity_name("bot-42.identity"));
        assert!(!is_saved_bot_identity_name("summon-1.identity"));
    }

    #[tokio::test]
    async fn minted_identities_are_fresh_and_removed_after_use() {
        let dir = std::env::temp_dir().join(format!("summon-mint-{}", std::process::id()));
        let path = dir.join("summon-1.identity");
        let first = mint_quiet_identity(&path).await.unwrap();
        assert!(path.exists());
        let second = mint_quiet_identity(&path).await.unwrap();
        assert_ne!(
            first.key().to_pub().get_uid(),
            second.key().to_pub().get_uid(),
            "a leftover file is not reused as the next summon's key"
        );
        discard_quiet_identity(&path).await;
        assert!(!path.exists());
        assert!(
            mint_quiet_identity(&dir.join("bot-3.identity"))
                .await
                .is_err()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn connect_times(director: &SummonDirector) -> Vec<Instant> {
        let mut launches = director.lock().claim_unlaunched();
        launches.sort_by_key(|launch| launch.summon);
        launches
            .into_iter()
            .map(|launch| launch.connect_at)
            .collect()
    }

    #[test]
    fn summon_connects_are_paced_below_the_flood_block() {
        let director = armed(8);
        let t0 = Instant::now();
        for caller in 10..13 {
            director
                .lock()
                .hear(SERVER, caller, PLAY, Heard::Crossing, t0);
        }
        assert_eq!(connect_times(&director), vec![t0, t0, t0]);

        // A fourth connect inside the window would wait past the limit.
        director.lock().hear(SERVER, 13, PLAY, Heard::Crossing, t0);
        assert_eq!(director.quiet_count(SERVER), 3);

        // Later in the window it waits for its slot instead.
        let later = t0 + Duration::from_secs(45);
        director
            .lock()
            .hear(SERVER, 14, PLAY, Heard::Crossing, later);
        assert_eq!(director.quiet_count(SERVER), 4);
        assert_eq!(connect_times(&director), vec![t0 + CONNECT_WINDOW]);
    }

    #[test]
    fn a_flood_refusal_holds_every_summon_connect_back() {
        let director = armed(4);
        director.note_flood_refusal(SERVER);
        director
            .lock()
            .hear(SERVER, 10, PLAY, Heard::Crossing, Instant::now());
        assert_eq!(
            director.quiet_count(SERVER),
            0,
            "another connect now would keep the host blocked"
        );
        let after = Instant::now() + FLOOD_COOLDOWN + Duration::from_secs(1);
        director
            .lock()
            .hear(SERVER, 11, PLAY, Heard::Crossing, after);
        assert_eq!(director.quiet_count(SERVER), 1);
        assert_eq!(connect_times(&director), vec![after]);
        // The flood mark on one server does not hold back another.
        director.note_push(OTHER, 2, Some(2), Path::new("/data/bot-2.identity"));
        director.note_flood_refusal(SERVER);
        director.hear(OTHER, 12, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(OTHER), 1);
    }

    #[test]
    fn a_host_notify_matches_only_the_default_port_pool() {
        let director = armed(2);
        director.note_push(
            "voice.example:9988",
            8,
            Some(2),
            Path::new("/data/bot-8.identity"),
        );
        director.hear_for_host("Voice.Example", 10, PLAY);
        assert_eq!(director.quiet_count(SERVER), 1);
        assert_eq!(director.quiet_count("voice.example:9988"), 0);
    }

    #[test]
    fn equivalent_address_spellings_share_one_pool() {
        let director = director();
        director.note_push(
            "Voice.Example",
            1,
            Some(3),
            Path::new("/data/bot-1.identity"),
        );
        assert_eq!(director.cap("voice.example:9987"), Some(3));
        director.hear("voice.example", 10, PLAY, Heard::Crossing);
        director.hear("VOICE.EXAMPLE:9987", 11, PLAY, Heard::Crossing);
        assert_eq!(director.quiet_count(SERVER), 2);
        // Summon clients dial the address the saved bot was given.
        let launches = director.lock().claim_unlaunched();
        assert!(launches.iter().all(|l| l.dial == "Voice.Example"));
    }

    #[test]
    fn cold_source_is_not_a_saved_library_path() {
        assert_eq!(cold_audio_source(SONG), AudioSource::Url(SONG.to_string()));
        assert_eq!(
            cold_audio_source("yt: lofi beats"),
            AudioSource::Url("ytsearch1:lofi beats".into())
        );
        assert_eq!(
            cold_audio_source("some song"),
            AudioSource::Url("ytsearch1:some song".into())
        );
        assert_eq!(
            cold_audio_source("[URL]https://cdn.example/one.mp3[/URL]"),
            AudioSource::Url(SONG.into())
        );
        assert_eq!(
            cold_audio_source("[URL=https://cdn.example/one.mp3]one[/URL]"),
            AudioSource::Url(SONG.into())
        );
        assert!(!matches!(
            cold_audio_source("/music/library/a.mp3"),
            AudioSource::LibraryPath(_)
        ));
    }
}
