// PURA-17 — single crate compiles to two flavours:
//
// - `--features server` (default) → native binary; axum router, SurrealDB,
//   `/api/auth` surface. Boots `tokio::main` and serves the SPA via
//   `serve_dioxus_application`.
// - `--features web`               → `wasm32-unknown-unknown`; `dioxus::launch`
//   hydrates `ui::App` in the browser.
//
// `dx-CLI` toggles these automatically. `cargo run` keeps working because
// `default = ["server"]`.

#[cfg(feature = "server")]
mod app_state;
#[cfg(feature = "server")]
mod auth;
mod client;
#[cfg(feature = "server")]
mod config;
#[cfg(feature = "server")]
mod control;
#[cfg(feature = "server")]
mod crypto;
#[cfg(feature = "server")]
mod db;
#[cfg(feature = "server")]
mod flow;
#[cfg(feature = "server")]
mod logging;
#[cfg(feature = "server")]
mod music_bots;
#[cfg(feature = "server")]
mod repos;
#[cfg(feature = "server")]
mod routes;
#[cfg(feature = "server")]
mod sshbridge;
mod ui;
#[cfg(feature = "server")]
mod web;
#[cfg(feature = "server")]
mod webquery;
#[cfg(feature = "server")]
mod widgets;
#[cfg(feature = "server")]
mod ws;

#[cfg(feature = "web")]
fn main() {
    // Browser entry point. The dx-CLI bundle wraps this in a wasm-bindgen
    // export so the generated index.html script can call it during hydration.
    dioxus::launch(ui::App);
}

#[cfg(feature = "server")]
mod server_entry {
    use anyhow::Context;
    use axum::{Json, Router, routing::get};
    use dioxus_server::{DioxusRouterExt, ServeConfig};
    use std::net::SocketAddr;
    use ts6_manager_shared::health::{Health, HealthStatus};

    use crate::config::Config;
    use crate::{app_state, auth, db, logging, routes, ui, web, webquery, widgets};

    async fn health() -> Json<Health> {
        Json(Health {
            status: HealthStatus::Ok,
        })
    }

    /// CLI subcommands accepted by the `ts6-manager-server` binary. The
    /// default (no args) is `serve`. PURA-10 adds `migrate` so operators
    /// can apply SurrealDB schema migrations without booting the HTTP listener.
    enum Subcommand {
        Serve,
        Migrate,
    }

    fn parse_subcommand() -> anyhow::Result<Subcommand> {
        let mut args = std::env::args().skip(1);
        match args.next().as_deref() {
            None | Some("serve") => Ok(Subcommand::Serve),
            Some("migrate") => Ok(Subcommand::Migrate),
            Some(other) => Err(anyhow::anyhow!(
                "unknown subcommand `{other}`; expected `serve` or `migrate`"
            )),
        }
    }

    pub async fn run() -> anyhow::Result<()> {
        let cfg = Config::load()?;
        logging::init(&cfg);
        cfg.log_hardening_summary();

        // Phase 1 SECURITY: derive the AES-256-GCM key once at boot.
        crate::crypto::init(&cfg.encryption_key);

        match parse_subcommand()? {
            Subcommand::Migrate => run_migrate(&cfg).await,
            Subcommand::Serve => run_serve(cfg).await,
        }
    }

    async fn run_migrate(cfg: &Config) -> anyhow::Result<()> {
        let database = db::connect(cfg).await?;
        let report = db::migrations::run(&database).await?;
        tracing::info!(
            applied = ?report.applied,
            skipped = ?report.skipped,
            "migrations complete"
        );
        Ok(())
    }

    async fn run_serve(cfg: Config) -> anyhow::Result<()> {
        // Apply migrations before opening the REST listener (spec §4.4: refuse
        // to start if any migration fails).
        let database = db::connect(&cfg).await?;
        let report = db::migrations::run(&database).await?;
        tracing::info!(
            applied = ?report.applied,
            skipped = ?report.skipped,
            "migrations applied at boot"
        );

        // PURA-226 — boot-time refresh-token volume sanity check. The board
        // reports repeat involuntary sign-outs in the deployed image; one
        // of the four candidate failure modes is "the panel container lost
        // its DB volume on restart, so every operator's still-valid refresh
        // row is gone." We can't tell whose refresh rows ought to be alive
        // (the SPA holds the bearer client-side), but we CAN warn when
        // enabled users > 0 and live refresh rows = 0 — that combination
        // means every authenticated operator will bounce to `/login` on
        // their next call. Best-effort: failure here doesn't gate startup.
        match crate::repos::refresh_tokens::boot_snapshot(&database).await {
            Ok(snap) => match crate::repos::users::count(&database).await {
                Ok(user_count) => {
                    if user_count > 0 && snap.total == 0 {
                        tracing::warn!(
                            enabled_users = user_count,
                            refresh_tokens = snap.total,
                            "no live refresh tokens at boot \u{2014} if operators report \
                             repeat involuntary sign-outs, suspect ephemeral DB volume; \
                             see docs/auth.md §debug-knob"
                        );
                    } else {
                        tracing::info!(
                            users = user_count,
                            refresh_tokens = snap.total,
                            distinct_users_with_tokens = snap.distinct_users,
                            "refresh-token volume snapshot at boot"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "boot refresh-token sanity check: user count query failed");
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "boot refresh-token sanity check: snapshot query failed");
            }
        }

        // PURA-79 R1/R6: hourly best-effort sweep that prunes
        // `ssh_audit_log` per the operator-set retention policy. Spawned
        // after migrations so the seeded `app_setting:ssh_audit_retention_days`
        // row exists; bound to the same `Database` arc as the rest of the
        // app so a shutdown of the runtime tears it down with everything else.
        crate::sshbridge::retention::spawn_sweep(database.clone());

        let serve_cfg = ServeConfig::new();
        let state = app_state::AppState::from_config(&cfg, database.clone());

        // PURA-223 — if a previous run uploaded a cookie via the UI, the
        // path was persisted to `app_setting:yt_cookie_path`. Prefer that
        // over the env-var-sourced boot value so UI uploads survive restarts.
        if let Ok(Some(row)) = crate::repos::app_settings::get(&database, "yt_cookie_path").await {
            let db_path = std::path::PathBuf::from(&row.value);
            if db_path.exists() {
                *state.yt_cookie.write().unwrap() = Some(db_path);
            }
        }

        // PURA-81 — periodic dashboard tick republisher. Spawns one
        // worker per enabled `server_connection`; each pushes a
        // `dashboard:tick` envelope onto the WS hub every 5 s. The
        // handle is held in a `_`-prefixed binding so it lives for
        // the lifetime of `run_serve` — a future graceful-shutdown
        // path can take ownership and call `.shutdown().await`.
        let _dashboard_ticks =
            crate::ws::dashboard_tick::spawn(crate::ws::dashboard_tick::TickerDeps {
                db: database.clone(),
                hub: state.ws_hub.clone(),
                control: state.control.clone(),
            });

        // PURA-80 — TS server-notify event source. Spawns one worker
        // per enabled SSH-controlled `server_connection`; each
        // subscribes to the SSH transport's `notify*` broadcast,
        // registers for server/channel/textserver/textchannel events,
        // and re-publishes them onto the matching WS hub topic. Held
        // as a drop-guard alongside the dashboard tick.
        let _server_notify =
            crate::ws::server_notify::spawn(crate::ws::server_notify::EventSourceDeps {
                db: database.clone(),
                hub: state.ws_hub.clone(),
                control: state.control.clone(),
            });

        // PURA-144 (WS-6) — sidecar `/stats` poller + per-server
        // `video_sources` WS topic. Only spawned when SIDECAR_URL is
        // configured; the rest of the manager still boots when no
        // sidecar is wired up so operators can adopt video streaming
        // incrementally.
        let _video_tick = state.sidecar.as_ref().map(|sidecar| {
            crate::ws::video_source_tick::spawn(crate::ws::video_source_tick::VideoTickDeps {
                db: database.clone(),
                hub: state.ws_hub.clone(),
                sidecar: sidecar.clone(),
            })
        });

        // PURA-242 / PURA-249 — v1.1 flow engine + REST surface. The
        // engine boots before the listener opens so the boot-time
        // in-flight-run sweep and cron registration (PURA-241 engine
        // §6.1) complete first. PURA-249 swaps the `BasicDispatcher`
        // stand-in for `ProductionDispatcher`: `ts6Command` lowers onto
        // a typed `ControlBackend` call, `musicBotCommand` onto a
        // `BotCommand`, `webhookOut` onto an SSRF-gated `reqwest` POST,
        // and `logLine` keeps its behaviour. The handle is bound
        // `_flow_engine` so the `FlowEngine`'s cron + TTL tasks live for
        // the whole serve scope and abort on shutdown via its `Drop`.
        let _flow_engine = crate::flow::FlowEngine::start(crate::flow::EngineDeps::new(
            database.clone(),
            std::sync::Arc::new(crate::flow::dispatch::ProductionDispatcher::new(&state)),
        ))
        .await
        .context("flow engine boot")?;
        let flow_api_state =
            crate::flow::routes::FlowApiState::new(state.clone(), _flow_engine.handle());

        // Phase 1 SECURITY (slice 4b): per-IP rate limit on the auth
        // surface. One bucket shared across `/login` and `/refresh` per
        // spec §6.8; `trusted_proxy_hops` decides whether the limiter
        // keys by ConnectInfo (direct listener) or by the rightmost XFF
        // entry (single trusted proxy in front).
        let auth_rate_limit_state = web::rate_limit::RateLimitState {
            limiter: web::rate_limit::make_auth_limiter(),
            trusted_hops: cfg.trusted_proxy_hops,
        };
        // PURA-35: dedicated limiter for `POST /api/setup/init`. Same
        // 15-req / 15-min spec §6.8 quota, but its OWN GCRA bucket map
        // — login spam can't DoS the bootstrap wizard and a stuck setup
        // retry can't DoS login (R-S5.1 from PURA-22 review).
        let setup_rate_limit_state = web::rate_limit::RateLimitState {
            limiter: web::rate_limit::make_setup_limiter(),
            trusted_hops: cfg.trusted_proxy_hops,
        };

        // Phase 1 SECURITY (slice 3 + 4a + 4b): build the stateful sub-routers
        // once with state baked in so they compose as `Router<()>` with the
        // rest of the app.
        let auth_router = auth::routes::router(auth_rate_limit_state).with_state(state.clone());
        let ws_router = auth::routes::ws_router().with_state(state.clone());
        // PURA-22 SECURITY slice 5 — `/api/setup` (one-shot bootstrap) and
        // `/api/servers` (list / create with sealed-at-rest credentials). Both
        // sub-routers live under `crate::routes`; auth + RBAC checks happen
        // inside the handlers via the `RequireAuth` / `RequireAdmin`
        // extractors so we don't need a separate middleware layer here.
        let setup_router = routes::setup::router(setup_rate_limit_state).with_state(state.clone());
        let servers_router = routes::servers::router().with_state(state.clone());
        // PURA-23 — Phase 1 dashboard route. Lives at an absolute path under
        // `/api/servers/:configId/vs/:sid/dashboard` (spec §7.19); the rest of
        // the `/api/servers/...` surface is owned by SecurityEngineer's
        // PURA-22 routes.
        let dashboard_router = webquery::dashboard::router().with_state(state.clone());
        // PURA-71 — Phase 2 control surface (clients/channels/bans/info/logs).
        // Mounts every `/api/servers/:configId/vs/:sid/...` action route; auth
        // and per-server access checks live inside each handler.
        let control_router = routes::control::router().with_state(state.clone());
        // PURA-144 (WS-6) — `/api/video-sources` CRUD for the MoQ sidecar
        // integration. Mounted at top-level paths (not under
        // `/api/servers/.../vs/...`) so the FE-PAGES (WS-7) can list
        // every video source the operator owns in a single request.
        let video_sources_router =
            routes::control::video_sources::router().with_state(state.clone());
        // PURA-82 — `/metrics` Prometheus exposition for the WS hub.
        // Admin-JWT gated; see the route module for the auth-gate rationale.
        let metrics_router = routes::metrics::router().with_state(state.clone());
        // PURA-223 — `/api/settings/youtube-cookies` cookie-file management.
        // Admin-JWT gated so only operators can upload or delete the file.
        let settings_router = routes::settings::router().with_state(state.clone());
        // PURA-123 WS-5 — music-bot REST surface (`/api/music-bots`,
        // `/api/music-library`, `/api/playlists`, `/api/radio-stations`,
        // `/api/music-requests`). Auth via the same `RequireAuth`
        // extractor as the rest of the panel.
        let music_bots_router = routes::music_bots::router().with_state(state.clone());
        // PURA-72 (Slice A) — public widget JSON endpoint
        // (`/api/widget/{token}/data`). No authentication.
        //
        // PURA-72 Slice F adds the per-token + per-IP `governor` bucket via
        // `widget_rate_limit`, mounted as a route-layer so it lands only on
        // the API surface — the SPA `/widget/*` HTML page doesn't hit
        // upstream WebQuery and only needs the relaxed CORS / frame
        // headers, which are applied globally further down.
        let widget_rl_state = web::make_widget_rate_limit_state(
            cfg.widget_rate_limit_per_token_rpm,
            cfg.widget_rate_limit_per_ip_rpm,
            cfg.trusted_proxy_hops,
        );
        let widget_router = widgets::routes::router().with_state(state.clone()).layer(
            axum::middleware::from_fn_with_state(widget_rl_state, web::widget_rate_limit),
        );
        // PURA-72 Slice D ([PURA-89]) — operator widget CRUD `/api/widgets`.
        // RequireAuth for reads; RequireModerator (admin OR moderator) for
        // writes. PATCH / DELETE / regenerate-token invalidate the public
        // `widget_cache` so a rotated or deleted token 404s on the next
        // public-route call (spec §7.29 / §26.4).
        let widget_admin_router = widgets::admin::router().with_state(state);
        // PURA-242 — v1.1 flow-engine REST surface (`/api/flows`, `/fire`,
        // `/runs`). Carries its own `FlowApiState` (wraps `AppState` +
        // the engine handle); `RequireAuth` / `RequireAdmin` still apply
        // via the `FromRef<FlowApiState> for AppState` impl.
        let flows_router = crate::flow::routes::router().with_state(flow_api_state);

        // PURA-17: `serve_dioxus_application` registers static assets +
        // server functions and adds a fallback that serves the dx-CLI
        // bundle's index.html — so `/login` (and every other SPA route)
        // resolves to the WASM shell. Run via `dx serve --web`, or build
        // the bundle once with `dx build --web` and start the binary with
        // `cargo run` to serve the cached artifact.
        let router: Router = Router::new()
            .route("/health", get(health))
            .nest("/api/auth", auth_router)
            // PURA-22 — first-run wizard + server CRUD (list / create).
            // Both sub-routers use absolute paths so we `merge` rather than
            // `nest` (avoids axum 0.8's strict-trailing-slash interaction
            // with the no-slash spec URIs).
            .merge(setup_router)
            .merge(servers_router)
            // Phase 1 SECURITY (slice 4a): authenticated WebSocket upgrade.
            // Per-message fan-out (TS events, bot logs, voice/video status —
            // spec §8.4) is owned by the future REST/Realtime engineer.
            .merge(ws_router)
            // PURA-23 dashboard route (spec §7.19). The handler enforces JWT
            // auth itself via the `RequireAuth` extractor.
            .merge(dashboard_router)
            // PURA-71 — Phase 2 control surface.
            .merge(control_router)
            // PURA-144 (WS-6) — video-source CRUD.
            .merge(video_sources_router)
            // PURA-82 — Prometheus metrics endpoint for the WS hub.
            .merge(metrics_router)
            // PURA-223 — YouTube cookie file management.
            .merge(settings_router)
            // PURA-123 — music-bot REST surface.
            .merge(music_bots_router)
            // PURA-72 — public widget endpoints (`/api/widget/{token}/...`).
            .merge(widget_router)
            // PURA-72 Slice D ([PURA-89]) — operator widget CRUD `/api/widgets`.
            .merge(widget_admin_router)
            // PURA-242 — v1.1 flow-engine REST surface.
            .merge(flows_router)
            .serve_dioxus_application(serve_cfg, ui::App)
            .layer(web::cors_layer(&cfg.frontend_url));
        let router = web::security_headers_stack(cfg.node_env).apply(router);
        // PURA-48 — per-request nonce-based CSP. Layered LAST so it sits
        // outermost: on the response path it runs after every inner layer,
        // and `headers_mut().insert(CSP, …)` overrides any pre-existing CSP
        // value (eg. the static one set by `security_headers_stack`).
        // Cleanup of the now-redundant static CSP is deferred while
        // PURA-49's predicate sanity-check runs on `web/headers.rs`.
        let router = router.layer(axum::middleware::from_fn(web::nonce_csp_middleware));
        // PURA-72 Slice F — widget-route response-header override. Layered
        // OUTSIDE the nonce-CSP middleware so its CSP rewrite (`frame-ancestors *`
        // on `/api/widget/*` and `/widget/*`) and `X-Frame-Options` removal
        // run last on the response path and win over the strict defaults.
        // PURA-146 WS-8 — the state carries the public MoQ relay origin so
        // the widget CSP can extend `connect-src` to allow WebTransport
        // dialing from the embedded viewer.
        let widget_csp_state = web::widget_security::WidgetCspState::from_moq_public_url(
            cfg.moq_public_url.as_deref(),
        );
        let router = router.layer(axum::middleware::from_fn_with_state(
            widget_csp_state,
            web::widget_response_headers,
        ));

        let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
            .parse()
            .with_context(|| {
                format!(
                    "HOST/PORT does not form a valid socket address: {}:{}",
                    cfg.host, cfg.port
                )
            })?;
        tracing::info!(
            %addr,
            trusted_proxy_hops = cfg.trusted_proxy_hops,
            "ts6-manager-server listening"
        );

        // `into_make_service_with_connect_info::<SocketAddr>()` makes the
        // peer socket address available via `ConnectInfo<SocketAddr>` — the
        // rate-limit middleware uses it as the per-IP bucket key when
        // `TRUSTED_PROXY_HOPS=0`, and as the fallback when XFF is missing
        // / malformed at higher hop counts.
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
        Ok(())
    }
}

#[cfg(feature = "server")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    server_entry::run().await
}
