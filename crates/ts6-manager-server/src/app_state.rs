//! Shared application state for axum handlers.
//!
//! Phase 1 SECURITY (slice 3): carries the SurrealDB handle, the JWT secret,
//! and the configured access/refresh lifetimes so [`auth::routes`] can mint
//! tokens without re-reading env vars on every request. Future workstreams
//! (REST, WS, FLOW) extend this struct in their own slices.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::config::Config;
use crate::control::ControlBackendPool;
use crate::control::sidecar::SidecarClient;
use crate::db::Database;
use crate::music_bots::MusicBotService;
use crate::webquery::WebQueryPool;
use crate::widgets::WidgetCache;
use crate::ws::Hub;
use ts6_ssrf::{HickoryResolver, Resolver};

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    /// HS256 signing secret for access tokens. `Arc<Vec<u8>>` keeps cloning
    /// cheap (axum hands the state to every handler by value).
    pub jwt_secret: Arc<Vec<u8>>,
    pub jwt_access_expiry: Duration,
    pub jwt_refresh_expiry: Duration,
    /// Mutex held by `POST /api/setup/init` to serialise concurrent
    /// one-shot initialisation attempts (PURA-22 acceptance: concurrent
    /// inits resolve to one success + one `409`). The lock is process-
    /// scoped — Phase 1 deploys a single process, so the in-memory
    /// mutex is sufficient. The handler still re-reads `user_count`
    /// inside the lock as a defence-in-depth check.
    pub setup_lock: Arc<Mutex<()>>,
    /// PURA-23 → PURA-99: pool of WebQuery clients keyed by
    /// `server_connection.id`. Production code now reaches for
    /// [`Self::control`] for every typed dispatch — this field stays
    /// alive only as a no-op slot for the existing test fixtures (each
    /// constructs an `AppState` literal). Removing it would touch every
    /// test file across the crate without functional benefit; a
    /// dedicated cleanup ticket can rip it out once the test fixture
    /// helper consolidates.
    #[allow(dead_code)]
    pub webquery: WebQueryPool,
    /// PURA-78: backend-agnostic control plane. Lazy-built per
    /// `server_connection.id`; the per-server `controlPath` flag picks
    /// WebQuery vs. SSHBridge at first use. Consumed by the dashboard
    /// route, the widget data path, and (since PURA-99) every
    /// `routes::control::*` REST handler.
    pub control: ControlBackendPool,
    /// PURA-70: live event bus. Per-server fan-out channels + ring
    /// buffer + metrics. Cheap to clone (Arc-shared internals).
    pub ws_hub: Hub,
    /// PURA-72: token-keyed 45 s widget data cache (spec §7.29). Public
    /// widget JSON / SVG / PNG endpoints all read through this cache so
    /// the upstream WebQuery client sees one fan-out per TTL window.
    pub widget_cache: WidgetCache,
    /// PURA-123 WS-5: music-bot supervisor + per-bot store + request log.
    /// One process-wide instance; the route layer in
    /// [`crate::routes::music_bots`] is the only consumer. Persistence
    /// is in-memory for WS-5; the SurrealDB-backed swap is a follow-up.
    pub music_bots: MusicBotService,
    /// PURA-144 (WS-6): HTTP client for `ts6-media-sidecar`. `None` when
    /// the operator has not set `SIDECAR_URL` — the `/api/video-sources`
    /// route returns 503 in that case so the FE can surface a sensible
    /// "configure sidecar first" error.
    pub sidecar: Option<SidecarClient>,
    /// PURA-144 (WS-6): shared SSRF DNS resolver. Reused for the
    /// pre-flight check on operator-supplied video stream URLs (defence
    /// in depth — the sidecar runs its own validator too). The
    /// `Arc<dyn Resolver>` shape matches the ts6-ssrf API and keeps the
    /// resolver cheap to clone into the polling task.
    pub ssrf_resolver: Arc<dyn Resolver>,
    /// PURA-146 (WS-8): publicly-reachable WebTransport endpoint of the
    /// sidecar (e.g. `https://stream.example.com:4443/anon`). The public
    /// widget viewer's `/api/widget/{token}/video-sources` route surfaces
    /// this so embedded iframes can subscribe. `None` means the operator
    /// did not configure a public relay URL — the viewer falls back to
    /// "No live video".
    pub moq_public_url: Option<String>,
    /// PURA-223 — live-updated yt-dlp cookie file path. Written by
    /// `PUT /api/settings/youtube-cookies`; read by each bot actor at
    /// track-play time. `Arc<RwLock<…>>` so the route handler can update
    /// it without touching the actors directly.
    pub yt_cookie: Arc<RwLock<Option<PathBuf>>>,
    /// PURA-223 — on-disk directory for operator-uploaded files (e.g.
    /// `yt-cookies.txt`). Defaults to `./data`; override with `DATA_DIR`.
    pub data_dir: PathBuf,
}

impl AppState {
    pub fn from_config(cfg: &Config, db: Arc<Database>) -> Self {
        // PURA-100 — opt-in TOFU. Spawning the worker only when the
        // operator set `TS_SSH_TOFU=1` keeps the default boot path
        // free of an idle background task and means the Reject
        // posture from ADR-0002 is preserved bit-for-bit for every
        // operator who has not actively chosen the tradeoff.
        let tofu_sink = if cfg.ssh_tofu {
            Some(crate::sshbridge::tofu::spawn_capture_worker(db.clone()))
        } else {
            None
        };
        let control = ControlBackendPool::new(cfg.ts_allow_self_signed, db.clone())
            .with_known_hosts(cfg.ssh_known_hosts_path.clone())
            .with_tofu(tofu_sink);
        // PURA-123 — bots without an explicit `identityPath` get a file
        // under `<config-dir>/music-bots/`. Default uses the system temp
        // dir so unit tests don't collide on a shared on-disk file; the
        // real binary swap-points to `~/.config/ts6-manager/music-bots`
        // can land alongside the SurrealDB persistence ticket.
        let music_bot_identity_dir: PathBuf = std::env::temp_dir().join("ts6-manager-music-bots");
        let sidecar = cfg
            .sidecar_url
            .as_deref()
            .map(|u| SidecarClient::new(u.to_string()));
        // PURA-144 — production resolver uses hickory (already enabled
        // via the `ts6-ssrf = { features = ["hickory"] }` feature flag
        // in this crate's Cargo.toml). Construction is fallible — fall
        // back to a no-op resolver that returns an empty IP set on
        // error so the manager still boots; ts6-ssrf's spec §9.3 path
        // allows the request when resolution fails, which keeps the
        // sidecar's own SSRF validator as the source of truth.
        let ssrf_resolver: Arc<dyn Resolver> = match HickoryResolver::from_system() {
            Ok(r) => Arc::new(r),
            Err(e) => {
                tracing::warn!(error = %e, "ssrf hickory resolver init failed; falling back to no-op");
                Arc::new(ts6_ssrf::MockResolver::new())
            }
        };
        // PURA-223 — boot-time cookie: prefer YT_COOKIE_FILE env var.
        // The settings route will replace this at runtime if an operator
        // uploads a cookie via the UI.
        let yt_cookie = Arc::new(RwLock::new(cfg.yt_cookie_file.clone()));

        Self {
            db,
            jwt_secret: Arc::new(cfg.jwt_secret.as_bytes().to_vec()),
            jwt_access_expiry: cfg.jwt_access_expiry,
            jwt_refresh_expiry: cfg.jwt_refresh_expiry,
            setup_lock: Arc::new(Mutex::new(())),
            webquery: WebQueryPool::new(cfg.ts_allow_self_signed),
            control,
            ws_hub: Hub::new(),
            widget_cache: WidgetCache::new(),
            music_bots: MusicBotService::new(music_bot_identity_dir),
            sidecar,
            ssrf_resolver,
            moq_public_url: cfg.moq_public_url.clone(),
            yt_cookie,
            data_dir: cfg.data_dir.clone(),
        }
    }
}
