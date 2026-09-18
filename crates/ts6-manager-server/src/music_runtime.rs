//! Fullstack front for the Contabo music unit.
//!
//! When `MUSIC_RUNTIME_URL` is set, this process does **not** spawn a
//! Voice send loop. Panel/API stay here; decode → Opus → wire send
//! lives in `ts6-manager-music`. Tests and hosts without the env keep
//! the in-process [`BotSupervisor`].

use std::collections::HashMap;
use std::path::PathBuf;
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

/// Remote music-unit hop failed. REST maps this to 5xx so Panel does
/// not treat a down unit as “no bots” or “spawned id 0”.
#[derive(Debug, Clone)]
pub struct MusicRuntimeError(pub String);

impl std::fmt::Display for MusicRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MusicRuntimeError {}

fn runtime_err(msg: impl Into<String>) -> MusicRuntimeError {
    MusicRuntimeError(msg.into())
}

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
        Self {
            inner: FrontInner::Remote(RemoteMusicRuntime::new(base_url)),
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

    pub async fn send(&self, id: BotId, cmd: BotCommand) -> Result<(), SendError> {
        match &self.inner {
            FrontInner::Local(s) => s.send(id, cmd).await,
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

    pub async fn shutdown_bot(&self, id: BotId) -> Result<(), SendError> {
        match &self.inner {
            FrontInner::Local(s) => s.shutdown_bot(id).await,
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
        {
            warn!(error = %err, "failed to push yt settings to music runtime");
        }
    }

    pub async fn bug_report_snapshot(&self) -> music_bot::bug_report::BugReportSnapshot {
        match &self.inner {
            FrontInner::Local(_) => music_bot::bug_report::snapshot(),
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
    pumps: Arc<Mutex<HashMap<BotId, broadcast::Sender<BotEvent>>>>,
}

impl RemoteMusicRuntime {
    fn new(base_url: impl Into<String>) -> Self {
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
            pumps: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn wait_until_healthy(&self, attempts: u32) -> bool {
        for i in 0..attempts {
            if self
                .http
                .get(format!("{}/health", self.base))
                .send()
                .await
                .map(|r| r.status().is_success())
                .unwrap_or(false)
            {
                info!(url = %self.base, "music runtime healthy");
                return true;
            }
            tokio::time::sleep(Duration::from_millis(400 + u64::from(i) * 100)).await;
        }
        warn!(url = %self.base, "music runtime not healthy after retries");
        false
    }

    async fn list(&self) -> Result<Vec<BotInfo>, MusicRuntimeError> {
        let resp = self
            .http
            .get(format!("{}/v1/bots", self.base))
            .send()
            .await
            .map_err(|e| runtime_err(format!("list bots: {e}")))?;
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
            .http
            .post(format!("{}/v1/bots", self.base))
            .json(&req)
            .send()
            .await
            .map_err(|e| runtime_err(format!("spawn: {e}")))?;
        if !resp.status().is_success() {
            return Err(runtime_err(format!("spawn: {}", resp.status())));
        }
        let body: SpawnResponse = resp
            .json()
            .await
            .map_err(|e| runtime_err(format!("spawn: {e}")))?;
        Ok(body.id)
    }

    async fn send(&self, id: BotId, command: BotCommand) -> Result<(), SendError> {
        let resp = self
            .http
            .post(format!("{}/v1/bots/{}/command", self.base, id.0))
            .json(&SendRequest { command })
            .send()
            .await
            .map_err(|_| SendError::ActorGone)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SendError::ActorGone);
        }
        if !resp.status().is_success() {
            return Err(SendError::ActorGone);
        }
        Ok(())
    }

    async fn shutdown_bot(&self, id: BotId) -> Result<(), SendError> {
        let resp = self
            .http
            .delete(format!("{}/v1/bots/{}", self.base, id.0))
            .send()
            .await
            .map_err(|_| SendError::ActorGone)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(SendError::ActorGone);
        }
        if !resp.status().is_success() {
            return Err(SendError::ActorGone);
        }
        Ok(())
    }

    async fn subscribe(
        &self,
        id: BotId,
    ) -> Result<Option<broadcast::Receiver<BotEvent>>, MusicRuntimeError> {
        let bots = self.list().await?;
        if !bots.iter().any(|b| b.id == id) {
            return Ok(None);
        }
        let mut pumps = self.pumps.lock().await;
        if let Some(tx) = pumps.get(&id) {
            return Ok(Some(tx.subscribe()));
        }
        let (tx, rx) = broadcast::channel(64);
        start_event_pump(
            self.sse.clone(),
            self.base.clone(),
            id,
            tx.clone(),
            Arc::clone(&self.pumps),
        );
        pumps.insert(id, tx);
        Ok(Some(rx))
    }

    async fn push_settings(&self, req: SettingsRequest) -> Result<(), String> {
        let resp = self
            .http
            .post(format!("{}/v1/settings", self.base))
            .json(&req)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("settings: {}", resp.status()));
        }
        Ok(())
    }

    async fn bug_report_snapshot(&self) -> music_bot::bug_report::BugReportSnapshot {
        match self
            .http
            .get(format!("{}/v1/bug-report-context", self.base))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<BugReportContextResponse>().await {
                    Ok(body) => music_bot::bug_report::BugReportSnapshot {
                        music_bot_latency: body.music_bot_latency,
                        log_tail: body.log_tail,
                    },
                    Err(err) => {
                        warn!(error = %err, "malformed music-runtime bug-report context");
                        music_bot::bug_report::snapshot()
                    }
                }
            }
            Ok(resp) => {
                warn!(status = %resp.status(), "music-runtime bug-report context failed");
                music_bot::bug_report::snapshot()
            }
            Err(err) => {
                warn!(error = %err, "music-runtime bug-report context transport failed");
                music_bot::bug_report::snapshot()
            }
        }
    }

    async fn mutate(&self, op: MutateOp) -> StoreResult<()> {
        let _: serde_json::Value = self.mutate_value(op).await?;
        Ok(())
    }

    async fn mutate_value<T: serde::de::DeserializeOwned>(&self, op: MutateOp) -> StoreResult<T> {
        self.post_value("/v1/mutate", &op).await
    }

    async fn store_value<T: serde::de::DeserializeOwned>(&self, op: StoreOp) -> StoreResult<T> {
        self.post_value("/v1/store", &op).await
    }

    async fn post_value<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &impl serde::Serialize,
    ) -> StoreResult<T> {
        let resp = self
            .http
            .post(format!("{}{path}", self.base))
            .json(body)
            .send()
            .await
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let status = resp.status();
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

fn start_event_pump(
    sse: reqwest::Client,
    base: String,
    id: BotId,
    tx: broadcast::Sender<BotEvent>,
    pumps: Arc<Mutex<HashMap<BotId, broadcast::Sender<BotEvent>>>>,
) {
    tokio::spawn(async move {
        pump_events(sse.clone(), base.clone(), id, tx.clone()).await;
        let mut map = pumps.lock().await;
        let Some(existing) = map.get(&id) else {
            return;
        };
        if !existing.same_channel(&tx) {
            return;
        }
        if existing.receiver_count() == 0 {
            map.remove(&id);
            return;
        }
        let restart_tx = existing.clone();
        drop(map);
        start_event_pump(sse, base, id, restart_tx, pumps);
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

async fn pump_events(
    sse: reqwest::Client,
    base: String,
    id: BotId,
    tx: broadcast::Sender<BotEvent>,
) {
    let url = format!("{base}/v1/bots/{}/events", id.0);
    loop {
        if tx.receiver_count() == 0 {
            return;
        }
        match sse.get(&url).send().await {
            Ok(mut resp) if resp.status().is_success() => {
                let mut buf = String::new();
                loop {
                    if tx.receiver_count() == 0 {
                        return;
                    }
                    match resp.chunk().await {
                        Ok(Some(bytes)) => {
                            buf.push_str(&String::from_utf8_lossy(&bytes));
                            for ev in take_sse_events(&mut buf) {
                                let _ = tx.send(ev);
                            }
                        }
                        Ok(None) => break,
                        Err(err) => {
                            warn!(error = %err, bot = %id, "music-runtime SSE chunk failed");
                            break;
                        }
                    }
                }
            }
            Ok(resp) => {
                warn!(status = %resp.status(), bot = %id, "music-runtime SSE HTTP error");
            }
            Err(err) => {
                warn!(error = %err, bot = %id, "music-runtime SSE connect failed");
            }
        }
        if tx.receiver_count() == 0 {
            return;
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

    use axum::Json;
    use axum::Router;
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
}
