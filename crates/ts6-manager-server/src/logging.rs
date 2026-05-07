use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;

/// Initialise tracing per spec Chapter 35.
///
/// - Production (NODE_ENV=production, LOG_PRETTY unset/false): JSON-per-line.
/// - Otherwise (development, or LOG_PRETTY=true in any env): pretty colourised.
/// - Level filter from `LOG_LEVEL` env (or the default carried in `Config`).
pub fn init(cfg: &Config) {
    let pretty = cfg.log_pretty.unwrap_or(!cfg.node_env.is_production());

    let env_filter = EnvFilter::try_from_env("LOG_LEVEL")
        .unwrap_or_else(|_| EnvFilter::new(cfg.log_level.clone()));

    let registry = tracing_subscriber::registry().with(env_filter);

    if pretty {
        registry
            .with(fmt::layer().with_target(true).with_line_number(true))
            .init();
    } else {
        registry
            .with(
                fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_target(true)
                    .with_current_span(false)
                    .with_span_list(false),
            )
            .init();
    }
}
