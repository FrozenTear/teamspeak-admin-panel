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
use tokio::sync::{Mutex, broadcast};
use tracing::{info, warn};

/// Environment variable holding the shared bearer for the music control
/// API. Read at process start only. Never a file, a database row, or a
/// CLI flag. The music process reads the same name.
pub const MUSIC_RUNTIME_TOKEN_ENV: &str = "MUSIC_RUNTIME_TOKEN";

/// `StoreError::Backend` payload when the music runtime returns 401.
/// Browser routes map this to 502 `{"error":"music_runtime_auth"}`.
/// The string is a classifier, not a secret.
pub const MUSIC_RUNTIME_AUTH_STORE: &str = "music_runtime_auth";

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

/// `MUSIC_RUNTIME_TOKEN` is set to bytes that are not UTF-8.
///
/// Fullstack refuses to start. The value is not included in the
/// message or in `Debug`.
#[derive(Debug, thiserror::Error)]
#[error(
    "MUSIC_RUNTIME_TOKEN is set but is not valid UTF-8. Refusing to start. Set a UTF-8 token or unset MUSIC_RUNTIME_TOKEN"
)]
pub struct MusicRuntimeTokenError;

impl MusicRuntimeToken {
    /// Missing, empty, and whitespace-only values are `Ok(None)` (no
    /// `Authorization` header). Surrounding whitespace is stripped,
    /// matching the music process, so a stray space in one container
    /// cannot 401. A present non-UTF-8 value is an error and does not
    /// mean "no auth".
    pub fn from_env() -> Result<Option<Self>, MusicRuntimeTokenError> {
        match std::env::var_os(MUSIC_RUNTIME_TOKEN_ENV) {
            None => Ok(None),
            Some(value) => Self::from_os_value(Some(value.as_os_str())),
        }
    }

    /// `None` is unset. UTF-8 values follow [`Self::parse`]. A non-UTF-8
    /// `OsStr` is [`MusicRuntimeTokenError`] and does not echo the bytes.
    pub fn from_os_value(
        raw: Option<&std::ffi::OsStr>,
    ) -> Result<Option<Self>, MusicRuntimeTokenError> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        match raw.to_str() {
            Some(text) => Ok(Self::parse(text)),
            None => Err(MusicRuntimeTokenError),
        }
    }

    /// `None`, `""`, and whitespace-only are unset. Any other value is
    /// the trimmed bearer the music process hashes. The untrimmed
    /// bytes are not what the runtime compares.
    pub fn parse(raw: &str) -> Option<Self> {
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

pub fn is_runtime_auth_store(err: &StoreError) -> bool {
    matches!(err, StoreError::Backend(msg) if msg == MUSIC_RUNTIME_AUTH_STORE)
}

fn store_auth_error() -> StoreError {
    StoreError::Backend(MUSIC_RUNTIME_AUTH_STORE.to_string())
}

/// Remote music-unit hop failed. REST maps this to 5xx so Panel does
/// not treat a down unit as “no bots” or “spawned id 0”.
///
/// [`MusicRuntimeError::Auth`] is a runtime HTTP 401. Browser routes
/// turn that into 502 `{"error":"music_runtime_auth"}` so the browser
/// does not treat it as the panel session expiring. The token is not
/// part of this error.
#[derive(Debug, Clone)]
pub enum MusicRuntimeError {
    Auth,
    Unavailable(String),
}

impl MusicRuntimeError {
    pub fn is_auth(&self) -> bool {
        matches!(self, Self::Auth)
    }
}

impl std::fmt::Display for MusicRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth => f.write_str("music runtime authentication failed"),
            Self::Unavailable(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for MusicRuntimeError {}

fn runtime_err(msg: impl Into<String>) -> MusicRuntimeError {
    MusicRuntimeError::Unavailable(msg.into())
}

/// Command / shutdown failure. [`FrontSendError::Auth`] is a runtime
/// HTTP 401 and must not be reported as "bot not found".
#[derive(Debug)]
pub enum FrontSendError {
    ActorGone,
    Full,
    Auth,
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
            Self::Auth => f.write_str("music runtime authentication failed"),
        }
    }
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

    pub fn store(&self) -> Arc<dyn MusicBotStore> {
        match &self.inner {
            FrontInner::Local(s) => Arc::clone(s.store()),
            FrontInner::Remote(r) => Arc::new(r.clone()),
        }
    }

    pub async fn playlist_create(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_create(bot, name).await,
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
    ) -> StoreResult<()> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_rename(bot, old, new).await,
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

    pub async fn playlist_delete(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_delete(bot, name).await,
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
    ) -> StoreResult<Track> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_add_track(bot, name, track).await,
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
    ) -> StoreResult<bool> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_remove_track(bot, name, id).await,
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

    pub async fn playlist_list(&self, bot: BotId) -> StoreResult<Vec<PlaylistName>> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_list(bot).await,
            FrontInner::Remote(r) => r.mutate_value(MutateOp::PlaylistList { bot: bot.0 }).await,
        }
    }

    pub async fn playlist_list_tracks(
        &self,
        bot: BotId,
        name: &PlaylistName,
    ) -> StoreResult<Vec<Track>> {
        match &self.inner {
            FrontInner::Local(s) => s.playlist_list_tracks(bot, name).await,
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
    ) -> StoreResult<LibraryEntry> {
        match &self.inner {
            FrontInner::Local(s) => s.library_add(bot, entry).await,
            FrontInner::Remote(r) => {
                r.mutate_value(MutateOp::LibraryAdd { bot: bot.0, entry })
                    .await
            }
        }
    }

    pub async fn library_remove(&self, bot: BotId, id: LibraryEntryId) -> StoreResult<bool> {
        match &self.inner {
            FrontInner::Local(s) => s.library_remove(bot, id).await,
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
    ) -> StoreResult<Option<LibraryEntry>> {
        match &self.inner {
            FrontInner::Local(s) => s.library_lookup(bot, id).await,
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
    ) -> StoreResult<Vec<LibraryEntry>> {
        match &self.inner {
            FrontInner::Local(s) => s.library_list(bot, tag).await,
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
    /// Set when the event stream is rejected with 401. Further
    /// subscriptions fail closed without opening another stream, so a
    /// browser `EventSource` reconnect cannot hammer the runtime.
    sse_auth_failed: Arc<AtomicBool>,
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
                Err(MusicRuntimeError::Auth)
            }
        }
    }

    /// One warn per rejected call. The message does not include the
    /// token, the header, or the response body.
    fn log_unauthorized(&self, op: &'static str) {
        warn!(
            op,
            "music runtime returned 401; panel routes answer 502 music_runtime_auth"
        );
    }

    fn reject_unauthorized(
        &self,
        op: &'static str,
        status: reqwest::StatusCode,
    ) -> Result<(), MusicRuntimeError> {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized(op);
            Err(MusicRuntimeError::Auth)
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
            Err(_) => return Err(FrontSendError::Auth),
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized("command");
            return Err(FrontSendError::Auth);
        }
        if resp.status() == reqwest::StatusCode::NOT_FOUND || !resp.status().is_success() {
            return Err(FrontSendError::ActorGone);
        }
        Ok(())
    }

    async fn shutdown_bot(&self, id: BotId) -> Result<(), FrontSendError> {
        let resp = match self.authorize(self.http.delete(format!("{}/v1/bots/{}", self.base, id.0)))
        {
            Ok(req) => req.send().await.map_err(|_| FrontSendError::ActorGone)?,
            Err(_) => return Err(FrontSendError::Auth),
        };
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized("shutdown");
            return Err(FrontSendError::Auth);
        }
        if resp.status() == reqwest::StatusCode::NOT_FOUND || !resp.status().is_success() {
            return Err(FrontSendError::ActorGone);
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        id: BotId,
    ) -> Result<Option<broadcast::Receiver<BotEvent>>, MusicRuntimeError> {
        if self.sse_auth_failed.load(Ordering::SeqCst) {
            return Err(MusicRuntimeError::Auth);
        }
        let bots = self.list().await?;
        if !bots.iter().any(|b| b.id == id) {
            return Ok(None);
        }
        if self.sse_auth_failed.load(Ordering::SeqCst) {
            return Err(MusicRuntimeError::Auth);
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
            return Err(MusicRuntimeError::Auth);
        }
        if !resp.status().is_success() {
            warn!(status = %resp.status(), "music-runtime bug-report context failed");
            return Ok(music_bot::bug_report::snapshot());
        }
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

    async fn mutate(&self, op: MutateOp) -> StoreResult<()> {
        let _: serde_json::Value = self.mutate_value(op).await?;
        Ok(())
    }

    async fn mutate_value<T: serde::de::DeserializeOwned>(&self, op: MutateOp) -> StoreResult<T> {
        self.post_value("/v1/mutate", "mutate", &op).await
    }

    async fn store_value<T: serde::de::DeserializeOwned>(&self, op: StoreOp) -> StoreResult<T> {
        self.post_value("/v1/store", "store", &op).await
    }

    async fn post_value<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        op: &'static str,
        body: &impl serde::Serialize,
    ) -> StoreResult<T> {
        let builder = self
            .authorize(self.http.post(format!("{}{path}", self.base)).json(body))
            .map_err(|_| store_auth_error())?;
        let resp = builder
            .send()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.log_unauthorized(op);
            return Err(store_auth_error());
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        if !status.is_success() {
            let wire: WireError = serde_json::from_slice(&bytes).unwrap_or(WireError {
                error: format!("{status}"),
            });
            return Err(store_err_from_wire(&wire.error));
        }
        if bytes.is_empty() || bytes.as_ref() == b"null" {
            return serde_json::from_value(serde_json::Value::Null)
                .map_err(|e| StoreError::Backend(e.to_string()));
        }
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Backend(e.to_string()))
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
        // Auth failure stops the pump. Do not restart: a 1s reconnect
        // loop would hammer the runtime, and the browser's own
        // EventSource retry is refused by `sse_auth_failed`.
        if matches!(end, PumpEnd::Auth) || existing.receiver_count() == 0 {
            map.remove(&pump.id);
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
                warn!(
                    bot = %pump.id,
                    "music runtime returned 401 on the event stream; not reconnecting"
                );
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
        self.store_value(StoreOp::QueueEnqueue { bot: bot.0, track })
            .await
    }
    async fn queue_dequeue_head(&self, bot: BotId) -> StoreResult<Option<Track>> {
        self.store_value(StoreOp::QueueDequeueHead { bot: bot.0 })
            .await
    }
    async fn queue_peek(&self, bot: BotId) -> StoreResult<Vec<Track>> {
        self.store_value(StoreOp::QueuePeek { bot: bot.0 }).await
    }
    async fn queue_clear(&self, bot: BotId) -> StoreResult<()> {
        self.store_value::<serde_json::Value>(StoreOp::QueueClear { bot: bot.0 })
            .await
            .map(|_| ())
    }
    async fn queue_reorder(&self, bot: BotId, order: Vec<TrackId>) -> StoreResult<()> {
        self.store_value::<serde_json::Value>(StoreOp::QueueReorder {
            bot: bot.0,
            order: order.into_iter().map(|t| t.0).collect(),
        })
        .await
        .map(|_| ())
    }
    async fn queue_remove(&self, bot: BotId, id: TrackId) -> StoreResult<bool> {
        self.store_value(StoreOp::QueueRemove {
            bot: bot.0,
            id: id.0,
        })
        .await
    }
    async fn queue_current(&self, bot: BotId) -> StoreResult<Option<Track>> {
        self.store_value(StoreOp::QueueCurrent { bot: bot.0 }).await
    }
    async fn queue_set_head_title(&self, bot: BotId, title: String) -> StoreResult<Option<Track>> {
        self.store_value(StoreOp::QueueSetHeadTitle { bot: bot.0, title })
            .await
    }
    async fn playlist_create(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        self.store_value::<serde_json::Value>(StoreOp::PlaylistCreate {
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
        self.store_value::<serde_json::Value>(StoreOp::PlaylistRename {
            bot: bot.0,
            old: old.0,
            new: new.0,
        })
        .await
        .map(|_| ())
    }
    async fn playlist_delete(&self, bot: BotId, name: PlaylistName) -> StoreResult<()> {
        self.store_value::<serde_json::Value>(StoreOp::PlaylistDelete {
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
        self.store_value(StoreOp::PlaylistAddTrack {
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
        self.store_value(StoreOp::PlaylistRemoveTrack {
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
        self.store_value(StoreOp::PlaylistListTracks {
            bot: bot.0,
            name: name.0.clone(),
        })
        .await
    }
    async fn playlist_list(&self, bot: BotId) -> StoreResult<Vec<PlaylistName>> {
        self.store_value(StoreOp::PlaylistList { bot: bot.0 }).await
    }
    async fn enqueue_playlist(&self, bot: BotId, name: &PlaylistName) -> StoreResult<Vec<Track>> {
        self.store_value(StoreOp::EnqueuePlaylist {
            bot: bot.0,
            name: name.0.clone(),
        })
        .await
    }
    async fn library_add(&self, bot: BotId, entry: NewLibraryEntry) -> StoreResult<LibraryEntry> {
        self.store_value(StoreOp::LibraryAdd { bot: bot.0, entry })
            .await
    }
    async fn library_remove(&self, bot: BotId, id: LibraryEntryId) -> StoreResult<bool> {
        self.store_value(StoreOp::LibraryRemove {
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
        self.store_value(StoreOp::LibraryLookup {
            bot: bot.0,
            id: id.0,
        })
        .await
    }
    async fn library_list(&self, bot: BotId, tag: Option<&str>) -> StoreResult<Vec<LibraryEntry>> {
        self.store_value(StoreOp::LibraryList {
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
    }

    #[derive(Debug, Clone)]
    struct Hit {
        method: String,
        path: String,
        authorization: Option<String>,
    }

    #[derive(Clone, Copy)]
    enum MockMode {
        Allow,
        /// `GET /v1/bots` stays open so subscribe can start the pump.
        /// The event stream answers 401.
        EventsUnauthorized,
    }

    async fn serve_mock(mode: MockMode) -> (String, HitLog) {
        let log = HitLog {
            hits: Arc::new(Mutex::new(Vec::new())),
        };
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
        log.hits.lock().await.push(Hit {
            method,
            path: path.clone(),
            authorization,
        });
        if matches!(mode, MockMode::EventsUnauthorized) && path.ends_with("/events") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        next.run(req).await
    }

    fn bearer_token() -> MusicRuntimeToken {
        MusicRuntimeToken::parse("panel-runtime-token-test").unwrap()
    }

    async fn exercise_remote_calls(front: &MusicBotFront) {
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
        tokio::time::sleep(Duration::from_millis(200)).await;
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
        exercise_remote_calls(&front).await;
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
        exercise_remote_calls(&front).await;
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

    #[tokio::test]
    async fn event_stream_does_not_reconnect_after_401() {
        let (url, log) = serve_mock(MockMode::EventsUnauthorized).await;
        let front = MusicBotFront::remote_with_token(url, Some(bearer_token()));
        let rx = front
            .subscribe(BotId(1))
            .await
            .expect("list is open")
            .expect("bot exists");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let again = front.subscribe(BotId(1)).await;
        assert!(
            again.expect_err("auth latch").is_auth(),
            "a second subscribe must not open another event stream"
        );
        drop(rx);
        tokio::time::sleep(Duration::from_secs(2)).await;
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

        let log = HitLog {
            hits: Arc::new(Mutex::new(Vec::new())),
        };
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

    fn token_env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|err| err.into_inner())
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
        exercise_remote_calls(&front).await;
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
        let _lock = token_env_lock();
        struct ClearEnv;
        impl Drop for ClearEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var(MUSIC_RUNTIME_TOKEN_ENV);
                }
            }
        }
        let _clear = ClearEnv;
        unsafe {
            std::env::remove_var(MUSIC_RUNTIME_TOKEN_ENV);
        }
        assert!(MusicRuntimeToken::from_env().unwrap().is_none());
        assert!(MusicRuntimeToken::parse("").is_none());
        assert!(MusicRuntimeToken::parse(" \t\n").is_none());
        unsafe {
            std::env::set_var(MUSIC_RUNTIME_TOKEN_ENV, " \t\n ");
        }
        assert!(
            MusicRuntimeToken::from_env().unwrap().is_none(),
            "whitespace-only env is unset"
        );
        unsafe {
            std::env::set_var(MUSIC_RUNTIME_TOKEN_ENV, "  trimmed-secret  ");
        }
        let token = MusicRuntimeToken::from_env()
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
        assert_eq!(
            MUSIC_RUNTIME_TOKEN_ENV,
            music_bot::runtime_http::MUSIC_RUNTIME_TOKEN_ENV
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_token_fails_fullstack_startup_without_printing_the_value() {
        use std::os::unix::ffi::OsStrExt;

        let _lock = token_env_lock();
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

        struct Restore {
            jwt: Option<String>,
        }
        impl Drop for Restore {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var(MUSIC_RUNTIME_TOKEN_ENV);
                    match &self.jwt {
                        Some(value) => std::env::set_var("JWT_SECRET", value),
                        None => std::env::remove_var("JWT_SECRET"),
                    }
                }
            }
        }
        let _restore = Restore {
            jwt: std::env::var("JWT_SECRET").ok(),
        };
        unsafe {
            std::env::set_var(
                "JWT_SECRET",
                "config-load-test-jwt-secret-not-a-music-token",
            );
            std::env::set_var(MUSIC_RUNTIME_TOKEN_ENV, raw);
        }
        let err = crate::config::Config::load().expect_err("startup must refuse a non-utf8 token");
        let msg = err.to_string();
        assert!(msg.contains("Refusing to start"), "{msg}");
        assert!(msg.to_lowercase().contains("utf-8"), "{msg}");
        assert!(!msg.contains("not-utf8"), "{msg}");
        assert!(!format!("{err:?}").contains("not-utf8"), "{err:?}");
    }
}
