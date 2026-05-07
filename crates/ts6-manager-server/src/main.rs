mod auth;
mod config;
mod crypto;
mod db;
mod logging;
mod repos;
mod ssrf;
mod ui;
mod web;

use axum::{Json, Router, response::Html, routing::get};
use dioxus_server::{DioxusRouterExt, ServeConfig};
use std::net::SocketAddr;
use ts6_manager_shared::health::{Health, HealthStatus};

use crate::config::Config;

const PLACEHOLDER_HTML: &str = include_str!("../assets/placeholder.html");

async fn health() -> Json<Health> {
    Json(Health {
        status: HealthStatus::Ok,
    })
}

async fn placeholder_page() -> Html<&'static str> {
    Html(PLACEHOLDER_HTML)
}

/// CLI subcommands accepted by the `ts6-manager-server` binary. The default
/// (no args) is `serve`. PURA-10 adds `migrate` so operators can apply
/// SurrealDB schema migrations without booting the HTTP listener.
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    logging::init(&cfg);
    cfg.log_hardening_summary();

    // Phase 1 SECURITY: derive the AES-256-GCM key once at boot. Subsequent
    // crypto::seal / crypto::unseal calls reuse this cached key.
    crypto::init(&cfg.encryption_key);

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
    // Phase 0: serve_api_application registers Dioxus server-functions without scanning a
    // dx-CLI public/ bundle. The placeholder root page is a static HTML asset so the Phase 0
    // gate ("placeholder page renders") passes without the WASM build pipeline. The Dioxus
    // frontend crate (PURA-5) replaces this root with a real SPA bundle.
    let router: Router = Router::new()
        .serve_api_application(serve_cfg, ui::App)
        .route("/", get(placeholder_page))
        .route("/health", get(health))
        // Phase 1 SECURITY (slice 1.5): CORS allowlist + security headers
        // applied globally. Per-route auth/rate-limit middleware will wrap
        // specific paths in slice 2.
        .layer(web::cors_layer(&cfg.frontend_url));
    let router = web::security_headers_stack(cfg.node_env).apply(router);

    let addr: SocketAddr = format!("0.0.0.0:{}", cfg.port).parse()?;
    tracing::info!(%addr, "ts6-manager-server listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}
