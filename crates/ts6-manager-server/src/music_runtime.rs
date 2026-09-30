//! Fullstack front for the Contabo music unit.
//!
//! When `MUSIC_RUNTIME_URL` is set, this process does **not** spawn a
//! Voice send loop. Panel/API stay here; decode → Opus → wire send
//! lives in `ts6-manager-music`. Tests and hosts without the env keep
//! the in-process [`BotSupervisor`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use music_bot::runtime_api::{
    BugReportContextResponse, ListResponse, MutateOp, SendRequest, SettingsRequest, SpawnRequest,
    SpawnResponse, StoreOp, WireError, store_err_from_wire,
};
use music_bot::{
    BotCommand, BotConfig, BotEvent, BotId, BotInfo, BotSupervisor, LibraryEntry, LibraryEntryId,
    MusicBotStore, NewLibraryEntry, NewTrack, PlaylistName, SendError, StoreError, StoreResult,
    Track, TrackId,
};
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::{info, warn};

/// Environment variable holding the shared bearer for the music control
/// API. This is the music crate's constant (`music-bot-audio`), re-exported
/// by the runtime. Read at process start only. Never a file, a database
/// row, or a CLI flag.
pub use music_bot::runtime_http::MUSIC_RUNTIME_TOKEN_ENV;

/// Shared bearer presented to `ts6-manager-music`.
///
/// `Debug` and `Display` never include the secret. The plaintext is
/// kept only so this process can send `Authorization`. It is not
/// written to logs, audit rows, or response bodies.
#[derive(Clone)]
pub struct MusicRuntimeToken(String);

impl std::fmt::Debug for MusicRuntimeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MusicRuntimeToken([redacted])")
    }
}

impl std::fmt::Display for MusicRuntimeToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted]")
    }
}

impl MusicRuntimeToken {
    /// Missing, empty, and whitespace-only values are `Ok(None)` (no
    /// `Authorization` header). Open-vs-bearer and the non-UTF-8
    /// refusal come from [`music_bot::runtime_http::ControlAuth`], so a
    /// stray space cannot disagree with the music process. A present
    /// non-UTF-8 value is that error and does not mean "no auth".
    /// [`crate::config::Config::load`] reads the same variable with
    /// `var_os` and passes it to [`Self::from_os_value`].
    pub fn from_env() -> Result<Option<Self>, music_bot::runtime_http::ControlAuthError> {
        match std::env::var_os(MUSIC_RUNTIME_TOKEN_ENV) {
            None => Ok(None),
            Some(value) => Self::from_os_value(Some(value.as_os_str())),
        }
    }

    /// `None` is unset. UTF-8 values follow [`Self::parse`]. A non-UTF-8
    /// `OsStr` is the runtime's [`music_bot::runtime_http::ControlAuthError`]
    /// and does not echo the bytes.
    pub fn from_os_value(
        raw: Option<&std::ffi::OsStr>,
    ) -> Result<Option<Self>, music_bot::runtime_http::ControlAuthError> {
        let auth = music_bot::runtime_http::ControlAuth::from_os_value(raw)?;
        if auth.is_open() {
            return Ok(None);
        }
        let text = raw.and_then(|value| value.to_str()).unwrap_or("");
        Ok(Self::parse(text))
    }

    /// `None`, `""`, and whitespace-only are unset. Any other value is
    /// the trimmed bearer [`music_bot::runtime_http::ControlAuth::parse`]
    /// hashes. The runtime does not trim the presented bearer.
    /// `ControlAuth` stores only the digest, so the credential sent is
    /// that same `trim`.
    pub fn parse(raw: &str) -> Option<Self> {
        if music_bot::runtime_http::ControlAuth::parse(Some(raw)).is_open() {
            return None;
        }
        let token = raw.trim();
        if token.is_empty() {
            None
        } else {
            Some(Self(token.to_string()))
        }
    }

    fn authorization_header(&self) -> Result<reqwest::header::HeaderValue, ()> {
        let mut value = Vec::with_capacity(self.0.len() + 7);
        value.extend_from_slice(b"Bearer ");
        value.extend_from_slice(self.0.as_bytes());
        reqwest::header::HeaderValue::from_bytes(&value).map_err(|_| ())
    }
}

/// Remote music-unit hop failed. REST maps this to 5xx so Panel does
/// not treat a down unit as “no bots” or “spawned id 0”.
///
/// [`MusicRuntimeError::Unauthorized`] carries the runtime HTTP status.
/// Browser routes match that status: 401 becomes 502
/// `{"error":"music_runtime_auth"}` so the browser does not treat it as
/// the panel session expiring. The token is not part of this error.
#[derive(Debug, Clone)]
pub enum MusicRuntimeError {
    Unauthorized(reqwest::StatusCode),
    Unavailable(String),
}

impl MusicRuntimeError {
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Unauthorized(status) if *status == reqwest::StatusCode::UNAUTHORIZED)
    }

    fn unauthorized() -> Self {
        Self::Unauthorized(reqwest::StatusCode::UNAUTHORIZED)
    }
}

impl std::fmt::Display for MusicRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized(status) if *status == reqwest::StatusCode::UNAUTHORIZED => {
                f.write_str("music runtime authentication failed")
            }
            Self::Unauthorized(status) => write!(f, "music runtime http status {status}"),
            Self::Unavailable(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for MusicRuntimeError {}

fn runtime_err(msg: impl Into<String>) -> MusicRuntimeError {
    MusicRuntimeError::Unavailable(msg.into())
}

/// Command / shutdown failure. [`FrontSendError::Unauthorized`] carries
/// the runtime HTTP status. A 401 must not be reported as "bot not found".
#[derive(Debug)]
pub enum FrontSendError {
    ActorGone,
    Full,
    Unauthorized(reqwest::StatusCode),
}

impl From<SendError> for FrontSendError {
    fn from(err: SendError) -> Self {
        match err {
            SendError::ActorGone => Self::ActorGone,
            SendError::Full => Self::Full,
        }
    }
}

impl std::fmt::Display for FrontSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ActorGone => f.write_str("bot actor has exited"),
            Self::Full => f.write_str("bot command queue is full"),
            Self::Unauthorized(status) if *status == reqwest::StatusCode::UNAUTHORIZED => {
                f.write_str("music runtime authentication failed")
            }
            Self::Unauthorized(status) => write!(f, "music runtime http status {status}"),
        }
    }
}

/// Store hop that may be a runtime HTTP status rather than a
/// [`StoreError`]. Routes match [`FrontStoreError::Unauthorized`]'s
/// status. A backend message is not an auth classifier.
#[derive(Debug)]
pub enum FrontStoreError {
    Unauthorized(reqwest::StatusCode),
    Store(StoreError),
}

impl FrontStoreError {
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Unauthorized(status) if *status == reqwest::StatusCode::UNAUTHORIZED)
    }

    fn unauthorized() -> Self {
        Self::Unauthorized(reqwest::StatusCode::UNAUTHORIZED)
    }

    /// Trait boundary only. Callers that classify 401 use this enum.
    fn into_store(self) -> StoreError {
        match self {
            Self::Store(err) => err,
            Self::Unauthorized(status) => {
                StoreError::Backend(format!("music runtime http status {status}"))
            }
        }
    }
}

impl std::fmt::Display for FrontStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized(status) => write!(f, "music runtime http status {status}"),
            Self::Store(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for FrontStoreError {}

fn lift_store<T>(result: StoreResult<T>) -> Result<T, FrontStoreError> {
    result.map_err(FrontStoreError::Store)
}

impl std::error::Error for FrontSendError {}

#[derive(Clone)]
pub struct MusicBotFront {
    inner: FrontInner,
}

#[derive(Clone)]
enum FrontInner {
    Local(Arc<BotSupervisor>),
    Remote(RemoteMusicRuntime),
}

impl MusicBotFront {
    pub fn local(supervisor: Arc<BotSupervisor>) -> Self {
        Self {
            inner: FrontInner::Local(supervisor),
        }
    }

    pub fn remote(base_url: impl Into<String>) -> Self {
        Self::remote_with_token(base_url, None)
    }

    /// `token` is `None` when `MUSIC_RUNTIME_TOKEN` is unset. That is
    /// today's loopback behaviour: no `Authorization` header.
    pub fn remote_with_token(
        base_url: impl Into<String>,
        token: Option<MusicRuntimeToken>,
    ) -> Self {
        Self {
            inner: FrontInner::Remote(RemoteMusicRuntime::new(base_url, token)),
        }
    }

    /// Latch tests. The 401 backoff is zero so the pump-stopped signal
    /// is not a wall-clock sleep. Production [`Self::remote_with_token`]
    /// keeps the one-second backoff.
    #[cfg(test)]
    fn remote_for_latch_tests(
        base_url: impl Into<String>,
        token: Option<MusicRuntimeToken>,
    ) -> Self {
        let mut runtime = RemoteMusicRuntime::new(base_url, token);
        runtime.auth_backoff = Duration::ZERO;
        Self {
            inner: FrontInner::Remote(runtime),
        }
    }

    #[cfg(test)]
    fn auth_notifies(&self) -> (Arc<Notify>, Arc<Notify>) {
        match &self.inner {
            FrontInner::Remote(runtime) => (
                Arc::clone(&runtime.auth_latched),
                Arc::clone(&runtime.auth_pump_stopped),
            ),
            FrontInner::Local(_) => panic!("auth latch is remote-only"),
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.inner, FrontInner::Remote(_))
    }

    pub async fn next_id_hint(&self) -> Result<u64, MusicRuntimeError> {
        Ok(self.list().await?.iter().map(|i| i.id.0).max().unwrap_or(0) + 1)
    }

    pub async fn list(&self) -> Result<Vec<BotInfo>, MusicRuntimeError> {
        match &self.inner {
            FrontInner::Local(s) => Ok(s.list().await),
            FrontInner::Remote(r) => r.list().await,
        }
    }

    pub async fn spawn(
        &self,
        config: BotConfig,
        yt_cookie: Arc<RwLock<Option<PathBuf>>>,
        yt_api_key: Arc<RwLock<Option<String>>>,
    ) -> Result<BotId, MusicRuntimeError> {
        match &self.inner {
            FrontInner::Local(s) => Ok(s.spawn(config, yt_cookie, yt_api_key).await),
            FrontInner::Remote(r) => r.spawn(SpawnRequest { config, id: None }).await,
        }
    }

    pub async fn spawn_with_id(
        &self,
        id: BotId,
        config: BotConfig,
        yt_cookie: Arc<RwLock<Option<PathBuf>>>,
        yt_api_key: Arc<RwLock<Option<String>>>,
    ) -> Result<BotId, MusicRuntimeError> {
        match &self.inner {
            FrontInner::Local(s) => Ok(s.spawn_with_id(id, config, yt_cookie, yt_api_key).await),
            FrontInner::Remote(r) => {
                r.spawn(SpawnRequest {
                    config,
                    id: Some(id.0),
                })
                .await
            }
        }
    }

    pub async fn send(&self, id: BotId, cmd: BotCommand) -> Result<(), FrontSendError> {
        match &self.inner {
            FrontInner::Local(s) => s.send(id, cmd).await.map_err(FrontSendError::from),
            FrontInner::Remote(r) => r.send(id, cmd).await,
        }
    }

    pub async fn subscribe(
        &self,
        id: BotId,
    ) -> Result<Option<broadcast::Receiver<BotEvent>>, MusicRuntimeError> {
        match &self.inner {
            FrontInner::Local(s) => Ok(s.subscribe(id).await),
            FrontInner::Remote(r) => r.subscribe(id).await,
        }
    }

    pub async fn shutdown_bot(&self, id: BotId) -> Result<(), FrontSendError> {
        match &self.inner {
            FrontInner::Local(s) => s.shutdown_bot(id).await.map_err(FrontSendError::from),
            FrontInner::Remote(r) => r.shutdown_bot(id).await,
        }
    }

    #[cfg(test)]
    pub fn store(&self) -> Arc<dyn MusicBotStore> {
        match &self.inner {
            FrontInner::Local(s) => Arc::clone(s.store()),
            FrontInner::Remote(r) => Arc::new(r.clone()),
        }
    }

    pub async fn queue_peek(&self, bot: BotId) -> Result<Vec<Track>, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.store().queue_peek(bot).await),
            FrontInner::Remote(r) => r.store_value(StoreOp::QueuePeek { bot: bot.0 }).await,
        }
    }

    pub async fn playlist_create(
        &self,
        bot: BotId,
        name: PlaylistName,
    ) -> Result<(), FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_create(bot, name).await),
            FrontInner::Remote(r) => {
                r.mutate(MutateOp::PlaylistCreate {
                    bot: bot.0,
                    name: name.0,
                })
                .await
            }
        }
    }

    pub async fn playlist_rename(
        &self,
        bot: BotId,
        old: PlaylistName,
        new: PlaylistName,
    ) -> Result<(), FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_rename(bot, old, new).await),
            FrontInner::Remote(r) => {
                r.mutate(MutateOp::PlaylistRename {
                    bot: bot.0,
                    old: old.0,
                    new: new.0,
                })
                .await
            }
        }
    }

    pub async fn playlist_delete(
        &self,
        bot: BotId,
        name: PlaylistName,
    ) -> Result<(), FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_delete(bot, name).await),
            FrontInner::Remote(r) => {
                r.mutate(MutateOp::PlaylistDelete {
                    bot: bot.0,
                    name: name.0,
                })
                .await
            }
        }
    }

    pub async fn playlist_add_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        track: NewTrack,
    ) -> Result<Track, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_add_track(bot, name, track).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::PlaylistAddTrack {
                    bot: bot.0,
                    name: name.0.clone(),
                    track,
                })
                .await
            }
        }
    }

    pub async fn playlist_remove_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        id: TrackId,
    ) -> Result<bool, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_remove_track(bot, name, id).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::PlaylistRemoveTrack {
                    bot: bot.0,
                    name: name.0.clone(),
                    id: id.0,
                })
                .await
            }
        }
    }

    pub async fn playlist_list(&self, bot: BotId) -> Result<Vec<PlaylistName>, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_list(bot).await),
            FrontInner::Remote(r) => r.mutate_value(MutateOp::PlaylistList { bot: bot.0 }).await,
        }
    }

    pub async fn playlist_list_tracks(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> Result<Vec<Track>, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.playlist_list_tracks(bot, name).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::PlaylistListTracks {
                    bot: bot.0,
                    name: name.0.clone(),
                })
                .await
            }
        }
    }

    pub async fn library_add(
        &self,
        bot: BotId,
        entry: NewLibraryEntry,
    ) -> Result<LibraryEntry, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.library_add(bot, entry).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::LibraryAdd { bot: bot.0, entry })
                    .await
            }
        }
    }

    pub async fn library_remove(
        &self,
        bot: BotId,
        id: LibraryEntryId,
    ) -> Result<bool, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.library_remove(bot, id).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::LibraryRemove {
                    bot: bot.0,
                    id: id.0,
                })
                .await
            }
        }
    }

    pub async fn library_lookup(
        &self,
        bot: BotId,
        id: LibraryEntryId,
    ) -> Result<Option<LibraryEntry>, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.library_lookup(bot, id).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::LibraryLookup {
                    bot: bot.0,
                    id: id.0,
                })
                .await
            }
        }
    }

    pub async fn library_list(
        &self,
        bot: BotId,
        tag: Option<&str>,
    ) -> Result<Vec<LibraryEntry>, FrontStoreError> {
        match &self.inner {
            FrontInner::Local(s) => lift_store(s.library_list(bot, tag).await),
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::LibraryList {
                    bot: bot.0,
                    tag: tag.map(str::to_string),
                })
                .await
            }
        }
    }

    pub async fn sync_settings(
        &self,
        yt_cookie: Option<Option<PathBuf>>,
        yt_api_key: Option<Option<String>>,
    ) {
        if let FrontInner::Remote(r) = &self.inner
            && let Err(err) = r
                .push_settings(SettingsRequest {
                    yt_cookie,
                    yt_api_key,
                })
                .await
            && !err.is_auth()
        {
            warn!(error = %err, "failed to push yt settings to music runtime");
        }
    }

    /// `Err(Auth)` when the runtime rejects the bearer. Other remote
    /// failures fall back to the in-process snapshot so a down unit
    /// does not blank the bug-report context.
    pub async fn bug_report_snapshot(
        &self,
    ) -> Result<music_bot::bug_report::BugReportSnapshot, MusicRuntimeError> {
        match &self.inner {
            FrontInner::Local(_) => Ok(music_bot::bug_report::snapshot()),
            FrontInner::Remote(r) => r.bug_report_snapshot().await,
        }
    }

    pub async fn wait_until_healthy(&self, attempts: u32) -> bool {
        match &self.inner {
            FrontInner::Local(_) => true,
            FrontInner::Remote(r) => r.wait_until_healthy(attempts).await,
        }
    }
}

#[derive(Clone)]
struct RemoteMusicRuntime {
    base: String,
    http: reqwest::Client,
    /// No request timeout — `/v1/bots/{id}/events` is a long-lived SSE.
    sse: reqwest::Client,
    /// `None` when `MUSIC_RUNTIME_TOKEN` is unset. Not logged.
    token: Option<MusicRuntimeToken>,
    pumps: Arc<Mutex<HashMap<BotId, broadcast::Sender<BotEvent>>>>,
    /// One latch for every bot on this runtime, not one per bot.
    /// Any music-runtime HTTP 401 (list, command, store, settings, or
    /// one bot's event stream) sets it. Panel routes map that 401 to
    /// 502 `music_runtime_auth`. While it is set, `subscribe` refuses
    /// every bot, and every other bot's event pump stops at its next
    /// loop check instead of reconnecting. A later 2xx clears it for
    /// all of them together. A 401 itself waits out
    /// [`Self::auth_backoff`] and does not loop, so a browser
    /// `EventSource` retry cannot hammer the control API.
    sse_auth_failed: Arc<AtomicBool>,
    /// Wakes a waiter when [`Self::sse_auth_failed`] becomes true.
    /// Nothing in production waits on it.
    auth_latched: Arc<Notify>,
    /// Wakes a waiter when an event pump stops because of that latch
    /// and is not restarted.
    auth_pump_stopped: Arc<Notify>,
    /// Delay after a 401 before the event pump stops. Production is
    /// one second. Latch tests set this to zero so the stop signal is
    /// not tied to a wall-clock sleep.
    auth_backoff: Duration,
}

impl RemoteMusicRuntime {
    fn new(base_url: impl Into<String>, token: Option<MusicRuntimeToken>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_string();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest::Client default build succeeds");
        // reqwest 0.12 `timeout` is `Duration` (cannot disable). The
        // events pump streams `chunk()`s, so this is only a reconnect
        // cap — not a `text().await` body wait.
        let sse = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60 * 60 * 24))
            .build()
            .expect("reqwest::Client SSE build succeeds");
        Self {
            base,
            http,
            sse,
            token,
            pumps: Arc::new(Mutex::new(HashMap::new())),
            sse_auth_failed: Arc::new(AtomicBool::new(false)),
            auth_latched: Arc::new(Notify::new()),
            auth_pump_stopped: Arc::new(Notify::new()),
            auth_backoff: Duration::from_secs(1),
        }
    }

    /// Attach `Authorization` when a token is configured. Header
    /// failure is treated as auth failure and does not log the token.
    fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, MusicRuntimeError> {
        let Some(token) = &self.token else {
            return Ok(builder);
        };
        match token.authorization_header() {
            Ok(value) => Ok(builder.header(reqwest::header::AUTHORIZATION, value)),
            Err(()) => {
                warn!("music runtime token cannot be encoded as an Authorization header");
                Err(MusicRuntimeError::unauthorized())
            }
        }
    }

    /// One warn per rejected call. The message does not include the
    /// token, the header, or the response body. The SSE latch stays set
    /// until a later authenticated call succeeds.
    fn log_unauthorized(&self, op: &'static str) {
        self.sse_auth_failed.store(true, Ordering::SeqCst);
        // Shared across bots: this 401 pauses every event stream.
        self.auth_latched.notify_waiters();
        warn!(
            op,
            "music runtime returned 401; panel routes answer 502 music_runtime_auth; other bots' event streams pause on the shared latch"
        );
    }

    /// A 2xx on an authenticated route means the bearer is accepted.
    fn clear_auth_latch(&self) {
        self.sse_auth_failed.store(false, Ordering::SeqCst);
    }

    fn reject_unauthorized(
        &self,
        op: &'static str,
        status: reqwest::StatusCode,
    ) -> Result<(), MusicRuntimeError> {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized(op);
            Err(MusicRuntimeError::Unauthorized(status))
        } else {
            Ok(())
        }
    }

    async fn wait_until_healthy(&self, attempts: u32) -> bool {
        for i in 0..attempts {
            let req = match self.authorize(self.http.get(format!("{}/health", self.base))) {
                Ok(req) => req,
                // A token that cannot be encoded will not become valid
                // on retry. `authorize` already logged once.
                Err(_) => return false,
            };
            match req.send().await {
                Ok(resp) if resp.status().is_success() => {
                    info!(url = %self.base, "music runtime healthy");
                    return true;
                }
                Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                    self.log_unauthorized("health");
                    return false;
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(400 + u64::from(i) * 100)).await;
        }
        warn!(url = %self.base, "music runtime not healthy after retries");
        false
    }

    async fn list(&self) -> Result<Vec<BotInfo>, MusicRuntimeError> {
        let resp = self
            .authorize(self.http.get(format!("{}/v1/bots", self.base)))?
            .send()
            .await
            .map_err(|e| runtime_err(format!("list bots: {e}")))?;
        self.reject_unauthorized("list", resp.status())?;
        if !resp.status().is_success() {
            return Err(runtime_err(format!("list bots: {}", resp.status())));
        }
        self.clear_auth_latch();
        let body: ListResponse = resp
            .json()
            .await
            .map_err(|e| runtime_err(format!("list bots: {e}")))?;
        Ok(body.bots)
    }

    async fn spawn(&self, req: SpawnRequest) -> Result<BotId, MusicRuntimeError> {
        let resp = self
            .authorize(self.http.post(format!("{}/v1/bots", self.base)).json(&req))?
            .send()
            .await
            .map_err(|e| runtime_err(format!("spawn: {e}")))?;
        self.reject_unauthorized("spawn", resp.status())?;
        if !resp.status().is_success() {
            return Err(runtime_err(format!("spawn: {}", resp.status())));
        }
        self.clear_auth_latch();
        let body: SpawnResponse = resp
            .json()
            .await
            .map_err(|e| runtime_err(format!("spawn: {e}")))?;
        Ok(body.id)
    }

    async fn send(&self, id: BotId, command: BotCommand) -> Result<(), FrontSendError> {
        let resp = match self.authorize(
            self.http
                .post(format!("{}/v1/bots/{}/command", self.base, id.0))
                .json(&SendRequest { command }),
        ) {
            Ok(req) => req.send().await.map_err(|_| FrontSendError::ActorGone)?,
            Err(_) => {
                return Err(FrontSendError::Unauthorized(
                    reqwest::StatusCode::UNAUTHORIZED,
                ));
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized("command");
            return Err(FrontSendError::Unauthorized(resp.status()));
        }
        if resp.status() == reqwest::StatusCode::NOT_FOUND || !resp.status().is_success() {
            return Err(FrontSendError::ActorGone);
        }
        self.clear_auth_latch();
        Ok(())
    }

    async fn shutdown_bot(&self, id: BotId) -> Result<(), FrontSendError> {
        let resp = match self.authorize(self.http.delete(format!("{}/v1/bots/{}", self.base, id.0)))
        {
            Ok(req) => req.send().await.map_err(|_| FrontSendError::ActorGone)?,
            Err(_) => {
                return Err(FrontSendError::Unauthorized(
                    reqwest::StatusCode::UNAUTHORIZED,
                ));
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized("shutdown");
            return Err(FrontSendError::Unauthorized(resp.status()));
        }
        if resp.status() == reqwest::StatusCode::NOT_FOUND || !resp.status().is_success() {
            return Err(FrontSendError::ActorGone);
        }
        self.clear_auth_latch();
        Ok(())
    }

    async fn subscribe(
        &self,
        id: BotId,
    ) -> Result<Option<broadcast::Receiver<BotEvent>>, MusicRuntimeError> {
        if self.sse_auth_failed.load(Ordering::SeqCst) {
            // Shared latch: a 401 for any bot pauses this subscribe too.
            return Err(MusicRuntimeError::unauthorized());
        }
        let bots = self.list().await?;
        if !bots.iter().any(|b| b.id == id) {
            return Ok(None);
        }
        if self.sse_auth_failed.load(Ordering::SeqCst) {
            return Err(MusicRuntimeError::unauthorized());
        }
        let mut pumps = self.pumps.lock().await;
        if let Some(tx) = pumps.get(&id) {
            return Ok(Some(tx.subscribe()));
        }
        let (tx, rx) = broadcast::channel(64);
        start_event_pump(EventPump {
            sse: self.sse.clone(),
            base: self.base.clone(),
            token: self.token.clone(),
            id,
            tx: tx.clone(),
            pumps: Arc::clone(&self.pumps),
            sse_auth_failed: Arc::clone(&self.sse_auth_failed),
            auth_latched: Arc::clone(&self.auth_latched),
            auth_pump_stopped: Arc::clone(&self.auth_pump_stopped),
            auth_backoff: self.auth_backoff,
        });
        pumps.insert(id, tx);
        Ok(Some(rx))
    }

    async fn push_settings(&self, req: SettingsRequest) -> Result<(), MusicRuntimeError> {
        let resp = self
            .authorize(
                self.http
                    .post(format!("{}/v1/settings", self.base))
                    .json(&req),
            )?
            .send()
            .await
            .map_err(|e| runtime_err(e.to_string()))?;
        self.reject_unauthorized("settings", resp.status())?;
        if !resp.status().is_success() {
            return Err(runtime_err(format!("settings: {}", resp.status())));
        }
        self.clear_auth_latch();
        Ok(())
    }

    async fn bug_report_snapshot(
        &self,
    ) -> Result<music_bot::bug_report::BugReportSnapshot, MusicRuntimeError> {
        let resp = match self
            .authorize(
                self.http
                    .get(format!("{}/v1/bug-report-context", self.base)),
            )?
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                warn!(error = %err, "music-runtime bug-report context transport failed");
                return Ok(music_bot::bug_report::snapshot());
            }
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized("bug-report-context");
            return Err(MusicRuntimeError::Unauthorized(resp.status()));
        }
        if !resp.status().is_success() {
            warn!(status = %resp.status(), "music-runtime bug-report context failed");
            return Ok(music_bot::bug_report::snapshot());
        }
        self.clear_auth_latch();
        match resp.json::<BugReportContextResponse>().await {
            Ok(body) => Ok(music_bot::bug_report::BugReportSnapshot {
                music_bot_latency: body.music_bot_latency,
                log_tail: body.log_tail,
            }),
            Err(err) => {
                warn!(error = %err, "malformed music-runtime bug-report context");
                Ok(music_bot::bug_report::snapshot())
            }
        }
    }

    async fn mutate(&self, op: MutateOp) -> Result<(), FrontStoreError> {
        let _: serde_json::Value = self.mutate_value(op).await?;
        Ok(())
    }

    async fn mutate_value<T: serde::de::DeserializeOwned>(
        &self,
        op: MutateOp,
    ) -> Result<T, FrontStoreError> {
        self.post_value("/v1/mutate", "mutate", &op).await
    }

    async fn store_value<T: serde::de::DeserializeOwned>(
        &self,
        op: StoreOp,
    ) -> Result<T, FrontStoreError> {
        self.post_value("/v1/store", "store", &op).await
    }

    async fn store_op<T: serde::de::DeserializeOwned>(&self, op: StoreOp) -> StoreResult<T> {
        self.store_value(op)
            .await
            .map_err(FrontStoreError::into_store)
    }

    async fn post_value<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        op: &'static str,
        body: &impl serde::Serialize,
    ) -> Result<T, FrontStoreError> {
        let builder = self
            .authorize(self.http.post(format!("{}{path}", self.base)).json(body))
            .map_err(|_| FrontStoreError::unauthorized())?;
        let resp = builder
            .send()
            .await
            .map_err(|e| FrontStoreError::Store(StoreError::Backend(e.to_string())))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized(op);
            return Err(FrontStoreError::Unauthorized(status));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| FrontStoreError::Store(StoreError::Backend(e.to_string())))?;
        if !status.is_success() {
            let wire: WireError = serde_json::from_slice(&bytes).unwrap_or(WireError {
                error: format!("{status}"),
            });
            return Err(FrontStoreError::Store(store_err_from_wire(&wire.error)));
        }
        self.clear_auth_latch();
        if bytes.is_empty() || bytes.as_ref() == b"null" {
            return serde_json::from_value(serde_json::Value::Null)
                .map_err(|e| FrontStoreError::Store(StoreError::Backend(e.to_string())));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| FrontStoreError::Store(StoreError::Backend(e.to_string())))
    }
}

struct EventPump {
    sse: reqwest::Client,
    base: String,
    token: Option<MusicRuntimeToken>,
    id: BotId,
    tx: broadcast::Sender<BotEvent>,
    pumps: Arc<Mutex<HashMap<BotId, broadcast::Sender<BotEvent>>>>,
    sse_auth_failed: Arc<AtomicBool>,
    auth_latched: Arc<Notify>,
    auth_pump_stopped: Arc<Notify>,
    auth_backoff: Duration,
}

fn start_event_pump(pump: EventPump) {
    tokio::spawn(async move {
        let end = pump_events(&pump).await;
        let mut map = pump.pumps.lock().await;
        let Some(existing) = map.get(&pump.id) else {
            return;
        };
        if !existing.same_channel(&pump.tx) {
            return;
        }
        // Auth failure stops this pump after the reconnect backoff.
        // Do not restart: a tight loop would hammer the runtime. The
        // browser's EventSource retry is refused while the latch is
        // set. A later 2xx clears it.
        if matches!(end, PumpEnd::Auth) || existing.receiver_count() == 0 {
            map.remove(&pump.id);
            if matches!(end, PumpEnd::Auth) {
                // The shared latch already paused every other bot. This
                // signal is only so a test can observe the stop without
                // sleeping through the backoff.
                pump.auth_pump_stopped.notify_waiters();
            }
            return;
        }
        let restart = EventPump {
            sse: pump.sse.clone(),
            base: pump.base.clone(),
            token: pump.token.clone(),
            id: pump.id,
            tx: existing.clone(),
            pumps: Arc::clone(&pump.pumps),
            sse_auth_failed: Arc::clone(&pump.sse_auth_failed),
            auth_latched: Arc::clone(&pump.auth_latched),
            auth_pump_stopped: Arc::clone(&pump.auth_pump_stopped),
            auth_backoff: pump.auth_backoff,
        };
        drop(map);
        start_event_pump(restart);
    });
}

/// Incremental SSE frame parser. Leaves a partial tail in `buf`.
fn take_sse_events(buf: &mut String) -> Vec<BotEvent> {
    let mut out = Vec::new();
    loop {
        let crlf = buf.find("\r\n\r\n");
        let lf = buf.find("\n\n");
        let (idx, sep_len) = match (crlf, lf) {
            (Some(c), Some(l)) if c <= l => (c, 4),
            (Some(c), None) => (c, 4),
            (_, Some(l)) => (l, 2),
            (None, None) => break,
        };
        let frame = buf[..idx].to_string();
        buf.drain(..idx + sep_len);
        for line in frame.lines() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            if let Ok(ev) = serde_json::from_str::<BotEvent>(data.trim()) {
                out.push(ev);
            }
        }
    }
    out
}

enum PumpEnd {
    Idle,
    Auth,
}

async fn pump_events(pump: &EventPump) -> PumpEnd {
    let url = format!("{}/v1/bots/{}/events", pump.base, pump.id.0);
    loop {
        if pump.sse_auth_failed.load(Ordering::SeqCst) {
            // A 401 on any runtime call (mapped to 502 music_runtime_auth)
            // set the shared latch. This bot's stream pauses with the others.
            return PumpEnd::Auth;
        }
        if pump.tx.receiver_count() == 0 {
            return PumpEnd::Idle;
        }
        let mut builder = pump.sse.get(&url);
        if let Some(token) = &pump.token {
            match token.authorization_header() {
                Ok(value) => builder = builder.header(reqwest::header::AUTHORIZATION, value),
                Err(()) => {
                    pump.sse_auth_failed.store(true, Ordering::SeqCst);
                    warn!(
                        bot = %pump.id,
                        "music runtime token cannot be encoded as an Authorization header; event stream stopped"
                    );
                    return PumpEnd::Auth;
                }
            }
        }
        match builder.send().await {
            Ok(mut resp) if resp.status().is_success() => {
                pump.sse_auth_failed.store(false, Ordering::SeqCst);
                let mut buf = String::new();
                loop {
                    if pump.tx.receiver_count() == 0 {
                        return PumpEnd::Idle;
                    }
                    match resp.chunk().await {
                        Ok(Some(bytes)) => {
                            buf.push_str(&String::from_utf8_lossy(&bytes));
                            for ev in take_sse_events(&mut buf) {
                                let _ = pump.tx.send(ev);
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!(error = %err, bot = %pump.id, "music-runtime SSE chunk failed");
                            break;
                        }
                    }
                }
            }
            Ok(resp) if resp.status() == reqwest::StatusCode::UNAUTHORIZED => {
                pump.sse_auth_failed.store(true, Ordering::SeqCst);
                // One latch for every bot. Panel routes map this 401 to
                // 502 music_runtime_auth, and the other pumps see the
                // flag at the top of their loop.
                pump.auth_latched.notify_waiters();
                warn!(
                    bot = %pump.id,
                    "music runtime returned 401 on the event stream; backing off and not reconnecting; other bots' event streams pause on the shared latch"
                );
                // Same backoff as a transient SSE error, then stop.
                // The next subscribe stays closed until a 2xx clears
                // the latch, so this does not become a retry storm.
                tokio::time::sleep(pump.auth_backoff).await;
                return PumpEnd::Auth;
            }
            Ok(resp) => {
                warn!(status = %resp.status(), bot = %pump.id, "music-runtime SSE HTTP error");
            }
            Err(err) => {
                warn!(error = %err, bot = %pump.id, "music-runtime SSE connect failed");
            }
        }
        if pump.sse_auth_failed.load(Ordering::SeqCst) {
            // Shared latch, including a 401 that landed on another bot
            // while this pump was in the transient-error backoff.
            return PumpEnd::Auth;
        }
        if pump.tx.receiver_count() == 0 {
            return PumpEnd::Idle;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[async_trait]
impl MusicBotStore for RemoteMusicRuntime {
    async fn queue_enqueue(&self, bot: BotId, track: NewTrack) -> StoreResult<Track> {
        self.store_op(StoreOp::QueueEnqueue { bot: bot.0, track })
            .await
    }
    async fn queue_dequeue_head(&self, bot: BotId) -> StoreResult<Option<Track>> {
        self.store_op(StoreOp::QueueDequeueHead { bot: bot.0 })
            .await
    }
    async fn queue_peek(&self, bot: BotId) -> StoreResult<Vec<Track>> {
        self.store_op(StoreOp::QueuePeek { bot: bot.0 }).await
    }
    async fn queue_clear(&self, bot: BotId) -> StoreResult<()> {
        self.store_op::<serde_json::Value>(StoreOp::QueueClear { bot: bot.0 })
            .await
            .map(|_| ())
    }
    async fn queue_reorder(&self, bot: BotId, order: Vec<TrackId>) -> StoreResult<()> {
        self.store_op::<serde_json::Value>(StoreOp::QueueReorder {
            bot: bot.0,
            order: order.into_iter().map(|t| t.0).collect(),
        })
        .await
        .map(|_| ())
    }
    async fn queue_remove(&self, bot: BotId, id: TrackId) -> StoreResult<bool> {
        self.store_op(StoreOp::QueueRemove {
            bot: bot.0,
            id: id.0,
        })
        .await
    }
    async fn queue_current(&self, bot: BotId) -> StoreResult<Option<Track>> {
        self.store_op(StoreOp::QueueCurrent { bot: bot.0 }).await
    }
    async fn queue_set_head_title(&self, bot: BotId, title: String) -> StoreResult<Option<Track>> {
        self.store_op(StoreOp::QueueSetHeadTitle { bot: bot.0, title })
            .await
    }
    async fn playlist_create(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        self.store_op::<serde_json::Value>(StoreOp::PlaylistCreate {
            bot: bot.0,
            name: name.0,
        })
        .await
        .map(|_| ())
    }
    async fn playlist_rename(
        &self,
        bot: BotId,
        old: PlaylistName,
        new: PlaylistName,
    ) -> StoreResult<()> {
        self.store_op::<serde_json::Value>(StoreOp::PlaylistRename {
            bot: bot.0,
            old: old.0,
            new: new.0,
        })
        .await
        .map(|_| ())
    }
    async fn playlist_delete(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        self.store_op::<serde_json::Value>(StoreOp::PlaylistDelete {
            bot: bot.0,
            name: name.0,
        })
        .await
        .map(|_| ())
    }
    async fn playlist_add_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        track: NewTrack,
    ) -> StoreResult<Track> {
        self.store_op(StoreOp::PlaylistAddTrack {
            bot: bot.0,
            name: name.0.clone(),
            track,
        })
        .await
    }
    async fn playlist_remove_track(
        &self,
        bot: BotId,
        name: &PlaylistName,
        id: TrackId,
    ) -> StoreResult<bool> {
        self.store_op(StoreOp::PlaylistRemoveTrack {
            bot: bot.0,
            name: name.0.clone(),
            id: id.0,
        })
        .await
    }
    async fn playlist_list_tracks(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>> {
        self.store_op(StoreOp::PlaylistListTracks {
            bot: bot.0,
            name: name.0.clone(),
        })
        .await
    }
    async fn playlist_list(&self, bot: BotId) -> StoreResult<Vec<PlaylistName>> {
        self.store_op(StoreOp::PlaylistList { bot: bot.0 }).await
    }
    async fn enqueue_playlist(&self, bot: BotId, name: &PlaylistName) -> StoreResult<Vec<Track>> {
        self.store_op(StoreOp::EnqueuePlaylist {
            bot: bot.0,
            name: name.0.clone(),
        })
        .await
    }
    async fn library_add(&self, bot: BotId, entry: NewLibraryEntry) -> StoreResult<LibraryEntry> {
        self.store_op(StoreOp::LibraryAdd { bot: bot.0, entry })
            .await
    }
    async fn library_remove(&self, bot: BotId, id: LibraryEntryId) -> StoreResult<bool> {
        self.store_op(StoreOp::LibraryRemove {
            bot: bot.0,
            id: id.0,
        })
        .await
    }
    async fn library_lookup(
        &self,
        bot: BotId,
        id: LibraryEntryId,
    ) -> StoreResult<Option<LibraryEntry>> {
        self.store_op(StoreOp::LibraryLookup {
            bot: bot.0,
            id: id.0,
        })
        .await
    }
    async fn library_list(&self, bot: BotId, tag: Option<&str>) -> StoreResult<Vec<LibraryEntry>> {
        self.store_op(StoreOp::LibraryList {
            bot: bot.0,
            tag: tag.map(str::to_string),
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::convert::Infallible;

    use tracing_subscriber::layer::SubscriberExt;

    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use axum::routing::get;
    use futures::stream::StreamExt;
    use music_bot::runtime_api::ListResponse;
    use music_bot::{BotConfig, BotEvent, BotId, BotInfo};
    use tokio::net::TcpListener;
    use tokio_stream::wrappers::BroadcastStream;

    #[tokio::test]
    async fn remote_front_roundtrip_does_not_own_a_local_send_loop() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = music_bot::runtime_http::router(music_bot::runtime_http::RuntimeState::new());
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let front = MusicBotFront::remote(format!("http://{addr}"));
        assert!(front.is_remote());
        assert!(front.wait_until_healthy(20).await);

        let cfg = BotConfig::new(
            "remote-front",
            std::env::temp_dir().join("music-runtime-remote-front.identity"),
        )
        .with_auto_connect(false);
        let id = front
            .spawn(
                cfg,
                Arc::new(RwLock::new(None)),
                Arc::new(RwLock::new(None)),
            )
            .await
            .expect("spawn must surface a real id");
        assert_ne!(id.0, 0);
        assert_eq!(front.list().await.expect("list").len(), 1);
        assert!(front.send(id, BotCommand::Disconnect).await.is_ok());
    }

    #[tokio::test]
    async fn remote_spawn_and_list_propagate_transport_errors() {
        let front = MusicBotFront::remote("http://127.0.0.1:1");
        let cfg = BotConfig::new(
            "poison",
            std::env::temp_dir().join("music-runtime-poison.identity"),
        )
        .with_auto_connect(false);
        let spawn = front
            .spawn(
                cfg,
                Arc::new(RwLock::new(None)),
                Arc::new(RwLock::new(None)),
            )
            .await;
        assert!(
            spawn.is_err(),
            "failed hop must not mint BotId(0), got {spawn:?}"
        );
        assert!(
            front.list().await.is_err(),
            "music-unit-down must not look like an empty bot list"
        );
    }

    #[test]
    fn take_sse_events_parses_incremental_frames_and_keeps_tail() {
        let first = serde_json::to_string(&BotEvent::QueueEmpty).unwrap();
        let second = serde_json::to_string(&BotEvent::LeftChannel).unwrap();
        let mut buf = format!("data: {first}\n\ndata: {}", &second[..second.len() / 2]);
        let evs = take_sse_events(&mut buf);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], BotEvent::QueueEmpty));
        assert!(buf.starts_with("data: "));
        buf.push_str(&second[second.len() / 2..]);
        buf.push_str("\n\n");
        let evs = take_sse_events(&mut buf);
        assert_eq!(evs.len(), 1);
        assert!(matches!(evs[0], BotEvent::LeftChannel));
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn remote_sse_pump_delivers_while_stream_stays_open() {
        let (event_tx, _) = broadcast::channel::<BotEvent>(8);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let live = event_tx.clone();
        let app = Router::new()
            .route(
                "/v1/bots",
                get(|| async {
                    Json(ListResponse {
                        bots: vec![BotInfo {
                            id: BotId(1),
                            name: "sse".into(),
                            server_addr: "x".into(),
                        }],
                    })
                }),
            )
            .route(
                "/v1/bots/{id}/events",
                get(move || {
                    let rx = live.subscribe();
                    async move {
                        let stream = BroadcastStream::new(rx).filter_map(|item| async move {
                            match item {
                                Ok(ev) => Some(Ok::<_, Infallible>(
                                    Event::default().data(serde_json::to_string(&ev).unwrap()),
                                )),
                                Err(_) => None,
                            }
                        });
                        Sse::new(stream)
                            .keep_alive(KeepAlive::new().interval(Duration::from_secs(60)))
                    }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let front = MusicBotFront::remote(format!("http://{addr}"));
        let mut rx = front
            .subscribe(BotId(1))
            .await
            .expect("list must succeed")
            .expect("bot 1 exists");

        tokio::time::sleep(Duration::from_millis(150)).await;
        event_tx.send(BotEvent::QueueEmpty).unwrap();
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("SSE pump must deliver while the stream stays open")
            .expect("event");
        assert!(matches!(ev, BotEvent::QueueEmpty));
    }

    #[derive(Clone)]
    struct HitLog {
        hits: Arc<Mutex<Vec<Hit>>>,
        /// Wakes a waiter after a hit is recorded. Tests enable it
        /// before the call so they do not sleep for the request.
        hit: Arc<Notify>,
    }

    impl HitLog {
        fn new() -> Self {
            Self {
                hits: Arc::new(Mutex::new(Vec::new())),
                hit: Arc::new(Notify::new()),
            }
        }

        async fn wait_until(&self, pred: impl Fn(&[Hit]) -> bool) {
            loop {
                let notified = self.hit.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if pred(&self.hits.lock().await.clone()) {
                    return;
                }
                notified.await;
            }
        }

        fn event_count(hits: &[Hit]) -> usize {
            hits.iter()
                .filter(|hit| hit.path.ends_with("/events"))
                .count()
        }
    }

    #[derive(Debug, Clone)]
    struct Hit {
        method: String,
        path: String,
        authorization: Option<String>,
    }

    #[derive(Clone)]
    enum MockMode {
        Allow,
        /// `GET /v1/bots` stays open so subscribe can start the pump.
        /// The event stream answers 401.
        EventsUnauthorized,
        /// `GET /v1/bots` answers 401. Event streams stay open so a
        /// test can see that the shared latch never opens one.
        ListUnauthorized,
        /// When the flag is true, `/events` answers 401. A test clears
        /// it to simulate the runtime accepting the bearer again.
        EventsGate(Arc<AtomicBool>),
    }

    async fn serve_mock(mode: MockMode) -> (String, HitLog) {
        let log = HitLog::new();
        let state = (log.clone(), mode);
        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route(
                "/v1/bots",
                get(|| async {
                    Json(ListResponse {
                        bots: vec![BotInfo {
                            id: BotId(1),
                            name: "t".into(),
                            server_addr: "127.0.0.1:9987".into(),
                        }],
                    })
                })
                .post(|Json(body): Json<serde_json::Value>| async move {
                    let id = body.get("id").and_then(|v| v.as_u64()).unwrap_or(1);
                    Json(serde_json::json!({ "id": id }))
                }),
            )
            .route(
                "/v1/bots/{id}",
                axum::routing::delete(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/bots/{id}/command",
                axum::routing::post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/bots/{id}/events",
                get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        "data: {}\n\n",
                    )
                }),
            )
            .route(
                "/v1/settings",
                axum::routing::post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/v1/bug-report-context",
                get(|| async {
                    Json(serde_json::json!({
                        "music_bot_latency": "",
                        "log_tail": ""
                    }))
                }),
            )
            .route(
                "/v1/mutate",
                axum::routing::post(|| async { Json(serde_json::json!([])) }),
            )
            .route(
                "/v1/store",
                axum::routing::post(|Json(body): Json<serde_json::Value>| async move {
                    if body.get("op").and_then(|op| op.as_str()) == Some("queue_current") {
                        Json(serde_json::Value::Null)
                    } else {
                        Json(serde_json::json!([]))
                    }
                }),
            )
            .layer(axum::middleware::from_fn_with_state(state, record_hit));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        (format!("http://{addr}"), log)
    }

    async fn record_hit(
        axum::extract::State((log, mode)): axum::extract::State<(HitLog, MockMode)>,
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let authorization = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let method = req.method().to_string();
        let path = req.uri().path().to_string();
        let deny_list =
            path == "/v1/bots" && method == "GET" && matches!(mode, MockMode::ListUnauthorized);
        log.hits.lock().await.push(Hit {
            method,
            path: path.clone(),
            authorization,
        });
        log.hit.notify_waiters();
        if deny_list {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        let deny_events = path.ends_with("/events")
            && match &mode {
                MockMode::EventsUnauthorized => true,
                MockMode::EventsGate(deny) => deny.load(Ordering::SeqCst),
                MockMode::Allow | MockMode::ListUnauthorized => false,
            };
        if deny_events {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        next.run(req).await
    }

    fn bearer_token() -> MusicRuntimeToken {
        MusicRuntimeToken::parse("panel-runtime-token-test").unwrap()
    }

    async fn exercise_remote_calls(front: &MusicBotFront, log: &HitLog) {
        assert!(front.wait_until_healthy(3).await);
        front.list().await.expect("list");
        let cfg = BotConfig::new(
            "rehydrate",
            std::env::temp_dir().join("music-runtime-token-rehydrate.identity"),
        )
        .with_auto_connect(false);
        front
            .spawn_with_id(
                BotId(4),
                cfg,
                Arc::new(RwLock::new(None)),
                Arc::new(RwLock::new(None)),
            )
            .await
            .expect("rehydrate spawn");
        front
            .send(BotId(1), BotCommand::Disconnect)
            .await
            .expect("command");
        front.shutdown_bot(BotId(1)).await.expect("shutdown");
        front
            .store()
            .queue_current(BotId(1))
            .await
            .expect("now playing");
        front.playlist_list(BotId(1)).await.expect("mutate");
        front.sync_settings(Some(None), Some(None)).await;
        front.bug_report_snapshot().await.expect("bug report");
        let rx = front
            .subscribe(BotId(1))
            .await
            .expect("subscribe list")
            .expect("bot exists");
        log.wait_until(|hits| HitLog::event_count(hits) >= 1).await;
        drop(rx);
    }

    fn assert_paths_covered(hits: &[Hit]) {
        let has = |method: &str, path: &str| {
            hits.iter()
                .any(|hit| hit.method == method && hit.path == path)
        };
        assert!(has("GET", "/health"), "health: {hits:?}");
        assert!(has("GET", "/v1/bots"), "list: {hits:?}");
        assert!(has("POST", "/v1/bots"), "rehydrate: {hits:?}");
        assert!(has("POST", "/v1/bots/1/command"), "command: {hits:?}");
        assert!(has("DELETE", "/v1/bots/1"), "shutdown: {hits:?}");
        assert!(has("POST", "/v1/store"), "now playing: {hits:?}");
        assert!(has("POST", "/v1/mutate"), "mutate: {hits:?}");
        assert!(has("POST", "/v1/settings"), "settings: {hits:?}");
        assert!(has("GET", "/v1/bug-report-context"), "bug report: {hits:?}");
        assert!(has("GET", "/v1/bots/1/events"), "event stream: {hits:?}");
    }

    #[tokio::test]
    async fn bearer_header_is_sent_on_every_runtime_call_when_token_is_set() {
        let (url, log) = serve_mock(MockMode::Allow).await;
        let front = MusicBotFront::remote_with_token(url, Some(bearer_token()));
        exercise_remote_calls(&front, &log).await;
        let hits = log.hits.lock().await.clone();
        assert_paths_covered(&hits);
        for hit in &hits {
            assert_eq!(
                hit.authorization.as_deref(),
                Some("Bearer panel-runtime-token-test"),
                "{} {} sent {:?}",
                hit.method,
                hit.path,
                hit.authorization
            );
        }
    }

    #[tokio::test]
    async fn unset_token_sends_no_authorization_header() {
        let (url, log) = serve_mock(MockMode::Allow).await;
        let front = MusicBotFront::remote(url);
        exercise_remote_calls(&front, &log).await;
        let hits = log.hits.lock().await.clone();
        assert_paths_covered(&hits);
        for hit in &hits {
            assert!(
                hit.authorization.is_none(),
                "{} {} sent {:?}",
                hit.method,
                hit.path,
                hit.authorization
            );
        }
    }

    /// Arm `notify` before the call that sets it. `notify_waiters` does
    /// not store a permit, so a waiter registered afterwards misses it.
    fn arm_notify(notify: &Notify) -> tokio::sync::futures::Notified<'_> {
        let wait = notify.notified();
        // enable is on the pinned future; callers pin the return value.
        wait
    }

    #[tokio::test]
    async fn event_stream_does_not_reconnect_after_401() {
        let (url, log) = serve_mock(MockMode::EventsUnauthorized).await;
        let front = MusicBotFront::remote_for_latch_tests(url, Some(bearer_token()));
        let (latched, stopped) = front.auth_notifies();
        let latched_wait = arm_notify(&latched);
        let stopped_wait = arm_notify(&stopped);
        tokio::pin!(latched_wait);
        tokio::pin!(stopped_wait);
        latched_wait.as_mut().enable();
        stopped_wait.as_mut().enable();

        let rx = front
            .subscribe(BotId(1))
            .await
            .expect("list is open")
            .expect("bot exists");
        latched_wait.await;
        let other = front.subscribe(BotId(2)).await;
        assert!(
            other.expect_err("shared latch pauses every bot").is_auth(),
            "a 401 on one event stream must pause the other bots"
        );
        stopped_wait.await;
        drop(rx);
        let hits = log.hits.lock().await.clone();
        let events: Vec<_> = hits
            .iter()
            .filter(|hit| hit.path.ends_with("/events"))
            .collect();
        assert_eq!(
            events.len(),
            1,
            "401 must not retry the event stream, saw {events:?}"
        );
        assert_eq!(
            events[0].authorization.as_deref(),
            Some("Bearer panel-runtime-token-test")
        );
        assert!(
            events.iter().all(|hit| hit.path.ends_with("/1/events")),
            "the paused bot must not open its own stream: {events:?}"
        );
    }

    #[tokio::test]
    async fn any_runtime_401_pauses_every_bots_event_stream() {
        let (url, log) = serve_mock(MockMode::ListUnauthorized).await;
        let front = MusicBotFront::remote_with_token(url, Some(bearer_token()));
        let err = front.list().await.expect_err("list 401");
        assert!(err.is_auth());
        let paused = front.subscribe(BotId(2)).await;
        assert!(
            paused
                .expect_err("list 401 pauses every bot's event stream")
                .is_auth()
        );
        let hits = log.hits.lock().await.clone();
        assert_eq!(
            HitLog::event_count(&hits),
            0,
            "a latched 401 must not open an event stream: {hits:?}"
        );
    }

    #[tokio::test]
    async fn auth_latch_clears_after_successful_runtime_response() {
        let deny_events = Arc::new(AtomicBool::new(true));
        let (url, log) = serve_mock(MockMode::EventsGate(Arc::clone(&deny_events))).await;
        let front = MusicBotFront::remote_for_latch_tests(url, Some(bearer_token()));
        let (latched, stopped) = front.auth_notifies();
        let latched_wait = arm_notify(&latched);
        let stopped_wait = arm_notify(&stopped);
        tokio::pin!(latched_wait);
        tokio::pin!(stopped_wait);
        latched_wait.as_mut().enable();
        stopped_wait.as_mut().enable();

        let rx = front
            .subscribe(BotId(1))
            .await
            .expect("list is open")
            .expect("bot exists");
        latched_wait.await;
        assert!(
            front
                .subscribe(BotId(2))
                .await
                .expect_err("401 latches every bot's event stream")
                .is_auth()
        );
        stopped_wait.await;
        let events_after_stop = HitLog::event_count(&log.hits.lock().await.clone());
        assert_eq!(events_after_stop, 1, "401 backoff must not retry");
        drop(rx);

        deny_events.store(false, Ordering::SeqCst);
        front.list().await.expect("200 clears the auth latch");
        let again = front
            .subscribe(BotId(1))
            .await
            .expect("latch cleared")
            .expect("bot exists");
        log.wait_until(|hits| HitLog::event_count(hits) >= 2).await;
        front
            .subscribe(BotId(1))
            .await
            .expect("successful event stream keeps the latch clear")
            .expect("bot exists");
        let events_after_recovery = HitLog::event_count(&log.hits.lock().await.clone());
        assert!(
            events_after_recovery >= 2,
            "recovery must open the event stream again, saw {events_after_recovery}"
        );
        drop(again);
    }

    struct LogCapture(Arc<std::sync::Mutex<String>>);

    impl<S> tracing_subscriber::Layer<S> for LogCapture
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut buf = self.0.lock().expect("log buffer");
            use std::fmt::Write;
            let _ = write!(
                buf,
                "{} {}",
                event.metadata().target(),
                event.metadata().level()
            );
            event.record(&mut FieldCapture(&mut buf));
            buf.push('\n');
        }
    }

    struct FieldCapture<'a>(&'a mut String);

    impl tracing::field::Visit for FieldCapture<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={value:?}", field.name());
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            use std::fmt::Write;
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn token_is_absent_from_debug_display_and_auth_logs() {
        const SECRET: &str = "super-secret-music-runtime-token";
        let token = MusicRuntimeToken::parse(SECRET).unwrap();
        let rendered = format!("{token:?} {token}");
        assert!(!rendered.contains(SECRET), "{rendered}");
        #[derive(Debug)]
        struct Probe {
            music_runtime_token: Option<MusicRuntimeToken>,
        }
        let probe_value = Probe {
            music_runtime_token: Some(token.clone()),
        };
        assert!(probe_value.music_runtime_token.is_some());
        let probe = format!("{probe_value:?}");
        assert!(!probe.contains(SECRET), "{probe}");

        let logs = Arc::new(std::sync::Mutex::new(String::new()));
        let subscriber = tracing_subscriber::registry().with(LogCapture(Arc::clone(&logs)));
        let _guard = tracing::subscriber::set_default(subscriber);

        let log = HitLog::new();
        let app = Router::new().route("/v1/bots", get(|| async { StatusCode::UNAUTHORIZED }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let front = MusicBotFront::remote_with_token(format!("http://{addr}"), Some(token));
        let err = front.list().await.expect_err("401");
        let shown = format!("{err} {err:?}");
        assert!(err.is_auth());
        assert!(!shown.contains(SECRET), "{shown}");
        let text = logs.lock().expect("log buffer").clone();
        assert!(
            text.contains("music_runtime_auth"),
            "expected one auth warn, got {text}"
        );
        assert!(!text.contains(SECRET), "{text}");
        drop(log);
    }

    #[test]
    fn debug_format_of_token_wrapper_does_not_contain_the_raw_token() {
        const SECRET: &str = "super-secret-runtime-token";
        let token = MusicRuntimeToken::parse(SECRET).unwrap();
        let rendered = format!("{token:?}");
        assert_eq!(rendered, "MusicRuntimeToken([redacted])");
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(!format!("{token}").contains(SECRET));
    }

    #[tokio::test]
    async fn whitespace_only_token_sends_no_authorization_header() {
        for raw in ["", " ", "\t", "\n", " \t\r\n "] {
            assert!(
                MusicRuntimeToken::parse(raw).is_none(),
                "whitespace-only must be unset, matching the runtime: {raw:?}"
            );
        }
        let (url, log) = serve_mock(MockMode::Allow).await;
        let front = MusicBotFront::remote_with_token(url, MusicRuntimeToken::parse(" \t "));
        exercise_remote_calls(&front, &log).await;
        let hits = log.hits.lock().await.clone();
        assert_paths_covered(&hits);
        for hit in &hits {
            assert!(
                hit.authorization.is_none(),
                "{} {} sent {:?}",
                hit.method,
                hit.path,
                hit.authorization
            );
        }
        let events: Vec<_> = hits
            .iter()
            .filter(|hit| hit.path.ends_with("/events"))
            .collect();
        assert!(!events.is_empty(), "event stream was not called");
        assert!(events.iter().all(|hit| hit.authorization.is_none()));
    }

    #[tokio::test]
    async fn padded_token_matches_runtime_compare_on_list_and_event_stream() {
        const RAW: &str = "  padded-runtime-token  ";
        const TRIMMED: &str = "padded-runtime-token";
        let panel = MusicRuntimeToken::parse(RAW).expect("padded token");
        assert_eq!(
            panel.authorization_header().unwrap().to_str().unwrap(),
            format!("Bearer {TRIMMED}"),
            "the runtime hashes the trimmed value and does not trim the presented bearer"
        );
        assert!(!format!("{panel:?}").contains(TRIMMED));

        let auth = music_bot::runtime_http::ControlAuth::parse(Some(RAW));
        assert!(!auth.is_open());
        let state = music_bot::runtime_http::RuntimeState::new();
        let id = state
            .supervisor
            .spawn(
                BotConfig::new(
                    "padded",
                    std::env::temp_dir().join("music-runtime-padded-token.identity"),
                )
                .with_auto_connect(false),
                Arc::new(RwLock::new(None)),
                Arc::new(RwLock::new(None)),
            )
            .await;
        let app = music_bot::runtime_http::router_with_auth(state, auth);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let front = MusicBotFront::remote_with_token(format!("http://{addr}"), Some(panel));
        let bots = front
            .list()
            .await
            .expect("runtime accepted the trimmed bearer");
        assert!(bots.iter().any(|bot| bot.id == id));
        let rx = front
            .subscribe(id)
            .await
            .expect("event stream accepted the trimmed bearer")
            .expect("spawned bot");
        tokio::time::sleep(Duration::from_millis(400)).await;
        front
            .subscribe(id)
            .await
            .expect("event-stream 401 would stop further subscriptions");
        drop(rx);
    }

    #[test]
    fn blank_env_token_is_unset_and_trimmed_value_is_redacted() {
        assert!(MusicRuntimeToken::from_os_value(None).unwrap().is_none());
        assert!(MusicRuntimeToken::parse("").is_none());
        assert!(MusicRuntimeToken::parse(" \t\n").is_none());
        assert!(
            MusicRuntimeToken::from_os_value(Some(std::ffi::OsStr::new(" \t\n ")))
                .unwrap()
                .is_none(),
            "whitespace-only value is unset"
        );
        let token =
            MusicRuntimeToken::from_os_value(Some(std::ffi::OsStr::new("  trimmed-secret  ")))
                .unwrap()
                .expect("trimmed token");
        assert_eq!(
            token.authorization_header().unwrap().to_str().unwrap(),
            "Bearer trimmed-secret"
        );
        let runtime = music_bot::runtime_http::ControlAuth::parse(Some("  trimmed-secret  "));
        assert!(!runtime.is_open());
        let rendered = format!("{token:?} {token}");
        assert!(!rendered.contains("trimmed-secret"), "{rendered}");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_token_fails_fullstack_startup_without_printing_the_value() {
        use std::os::unix::ffi::OsStrExt;

        let raw = std::ffi::OsStr::from_bytes(b"not-utf8-\xff-token");
        let err = MusicRuntimeToken::from_os_value(Some(raw)).expect_err("non-utf8 token");
        let msg = err.to_string();
        assert!(msg.contains("Refusing to start"), "{msg}");
        assert!(msg.to_lowercase().contains("utf-8"), "{msg}");
        assert!(!msg.contains("not-utf8"), "{msg}");
        assert!(!format!("{err:?}").contains("not-utf8"), "{err:?}");
        assert!(
            MusicRuntimeToken::from_os_value(Some(std::ffi::OsStr::new("  \t  ")))
                .unwrap()
                .is_none()
        );
        // `Config::load` is `var_os` into `from_os_value` (same as
        // `from_env`). The `Config::load` refusal test lives next to
        // `load`. This one does not mutate the process environment.
        let missing = MusicRuntimeToken::from_os_value(None).unwrap();
        assert!(missing.is_none());
    }
}
