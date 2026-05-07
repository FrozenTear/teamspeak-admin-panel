mod auth;
mod config;
mod crypto;
mod logging;
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    logging::init(&cfg);
    cfg.log_hardening_summary();

    // Phase 1 SECURITY: derive the AES-256-GCM key once at boot. Subsequent
    // crypto::seal / crypto::unseal calls reuse this cached key.
    crypto::init(&cfg.encryption_key);

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
