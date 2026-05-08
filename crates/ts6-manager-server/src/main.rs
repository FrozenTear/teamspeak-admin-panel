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
mod crypto;
#[cfg(feature = "server")]
mod db;
#[cfg(feature = "server")]
mod logging;
#[cfg(feature = "server")]
mod repos;
#[cfg(feature = "server")]
mod ssrf;
mod ui;
#[cfg(feature = "server")]
mod web;

#[cfg(feature = "web")]
fn main() {
    // Browser entry point. The dx-CLI bundle wraps this in a wasm-bindgen
    // export so the generated index.html script can call it during hydration.
    dioxus::launch(ui::App);
}

#[cfg(feature = "server")]
mod server_entry {
    use axum::{Json, Router, routing::get};
    use dioxus_server::{DioxusRouterExt, ServeConfig};
    use std::net::SocketAddr;
    use ts6_manager_shared::health::{Health, HealthStatus};

    use crate::config::Config;
    use crate::{app_state, auth, db, logging, ui, web};

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

        let serve_cfg = ServeConfig::new();
        let state = app_state::AppState::from_config(&cfg, database.clone());

        // Phase 1 SECURITY (slice 3 + 4a): build the stateful sub-routers
        // once with state baked in so they compose as `Router<()>` with the
        // rest of the app.
        let auth_router = auth::routes::router().with_state(state.clone());
        let ws_router = auth::routes::ws_router().with_state(state);

        // PURA-17: `serve_dioxus_application` registers static assets +
        // server functions and adds a fallback that serves the dx-CLI
        // bundle's index.html — so `/login` (and every other SPA route)
        // resolves to the WASM shell. Run via `dx serve --web`, or build
        // the bundle once with `dx build --web` and start the binary with
        // `cargo run` to serve the cached artifact.
        let router: Router = Router::new()
            .route("/health", get(health))
            .nest("/api/auth", auth_router)
            // Phase 1 SECURITY (slice 4a): authenticated WebSocket upgrade.
            // Per-message fan-out (TS events, bot logs, voice/video status —
            // spec §8.4) is owned by the future REST/Realtime engineer.
            .merge(ws_router)
            .serve_dioxus_application(serve_cfg, ui::App)
            // CORS + security headers apply globally. Per-route rate-limit
            // middleware will wrap login/refresh paths in the next slice.
            .layer(web::cors_layer(&cfg.frontend_url));
        let router = web::security_headers_stack(cfg.node_env).apply(router);

        let addr: SocketAddr = format!("0.0.0.0:{}", cfg.port).parse()?;
        tracing::info!(%addr, "ts6-manager-server listening");

        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, router).await?;
        Ok(())
    }
}

#[cfg(feature = "server")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    server_entry::run().await
}
