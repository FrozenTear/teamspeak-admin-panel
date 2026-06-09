use std::env;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

const DEV_JWT_PLACEHOLDER: &str = "dev-secret-change-me-in-production";
const DEFAULT_LOG_LEVEL: &str = "info";
const DEFAULT_PORT: u16 = 3001;
// PURA-197 — fullstack listener bind address. Defaults to `0.0.0.0` so the
// kube manifest (`hostNetwork: true`) and Containerfile-only deployments
// keep working unchanged. Operators on the Quadlet path who flip the pod to
// `Network=host` set `HOST=127.0.0.1` in `ts6-manager.env` so the listener
// stays on loopback while still avoiding the passt/PURA-105 wedge for
// WebQuery egress against a co-located TS6 fixture.
const DEFAULT_HOST: &str = "0.0.0.0";
// PURA-10 / D8 deviation: SurrealDB v3 embedded with the SurrealKV backend.
// `surrealkv://./data/db` resolves relative to the process working directory;
// operators can override with `surrealkv:///var/lib/...` for absolute paths or
// `ws://host:8000` to point at an external `surreal start` server.
// `memory` is accepted for tests / ephemeral runs.
const DEFAULT_DATABASE_URL: &str = "surrealkv://./data/db";
pub const DEFAULT_DB_NAMESPACE: &str = "ts6";
pub const DEFAULT_DB_NAME: &str = "ts6_manager";
const DEFAULT_MUSIC_DIR: &str = "/data/music";
const DEFAULT_DATA_DIR: &str = "./data";
const DEFAULT_FRONTEND_URL_DEV: &str = "http://localhost:5173";
const DEFAULT_FRONTEND_URL_PROD: &str = "http://localhost:3000";
const DEFAULT_ACCESS_EXPIRY: &str = "4h";
const DEFAULT_REFRESH_EXPIRY: &str = "30d";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeEnv {
    Development,
    Production,
}

impl NodeEnv {
    pub fn from_env_string(raw: Option<&str>) -> Self {
        match raw {
            Some("production") => Self::Production,
            _ => Self::Development,
        }
    }

    pub fn is_production(&self) -> bool {
        matches!(self, Self::Production)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // populated for downstream streams (SECURITY, DATA, WEBQUERY)
pub struct Config {
    pub node_env: NodeEnv,
    /// PURA-197 — fullstack listener bind address. Defaults to `0.0.0.0`.
    /// Operators on the Quadlet path who flip the pod to `Network=host`
    /// set `HOST=127.0.0.1` so the listener stays on loopback only and
    /// fronts behind a reverse proxy.
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub jwt_secret: String,
    /// True when [`Self::jwt_secret`] holds the dev placeholder; only allowed in development.
    pub jwt_secret_is_placeholder: bool,
    pub jwt_secret_short: bool,
    pub encryption_key: String,
    /// True when no `ENCRYPTION_KEY` was set and we fell back to `JWT_SECRET`.
    pub encryption_key_fell_back: bool,
    pub jwt_access_expiry: Duration,
    pub jwt_refresh_expiry: Duration,
    pub frontend_url: String,
    pub music_dir: PathBuf,
    pub sidecar_url: Option<String>,
    pub sidecar_binary_path: Option<PathBuf>,
    /// PURA-146 (WS-8) — publicly-reachable WebTransport endpoint of the
    /// sidecar's moq-lite-04 listener. Surfaced to the public widget viewer
    /// via `GET /api/widget/{token}/video-sources` so an embedded iframe on
    /// a third-party site knows where to subscribe. Distinct from
    /// `sidecar_url` (manager → sidecar HTTP control plane); when unset the
    /// public viewer shows "No live video" instead of attempting a
    /// connection.
    pub moq_public_url: Option<String>,
    pub yt_cookie_file: Option<PathBuf>,
    /// THE-948 — boot-time seed for the YouTube Data API key used by the
    /// music bot's fast-search path (THE-933). Read from `YOUTUBE_API_KEY`
    /// here; once an operator saves a key via `/settings`, the persisted
    /// `app_setting:youtube_api_key` row takes precedence at boot. Held at
    /// runtime in `AppState::yt_api_key` (live-updatable without restart).
    pub youtube_api_key: Option<String>,
    /// PURA-223 — base directory for operator-uploaded files.
    /// Defaults to `./data`. Cookie file is written as `<data_dir>/yt-cookies.txt`.
    pub data_dir: PathBuf,
    pub ts_allow_self_signed: bool,
    /// PURA-100 — opt-in trust-on-first-use for SSHBridge host keys.
    /// Default `false`. When `true`, the SSH transport's host-key
    /// verifier captures the upstream's fingerprint on first connect
    /// (subject to the per-server `sshHostKeyFingerprint` column being
    /// NULL and `TS_SSH_KNOWN_HOSTS` being unset), persists it onto
    /// the row, and falls through to strict-fingerprint enforcement
    /// for every later connect. Documented operator tradeoff: the
    /// first-connect MitM window is the exposure surface.
    pub ssh_tofu: bool,
    /// PURA-100 — operator-supplied path to a shared OpenSSH
    /// `known_hosts` file consumed by the SSHBridge host-key
    /// verifier's `KnownHostsFile` policy. Sourced from
    /// `TS_SSH_KNOWN_HOSTS`; empty/unset stays `None` and the
    /// verifier falls back to per-row fingerprint pinning (or TOFU
    /// when opted in, or `Reject` otherwise).
    pub ssh_known_hosts_path: Option<PathBuf>,
    pub log_level: String,
    pub log_pretty: Option<bool>,
    /// Number of trusted reverse-proxy hops in front of the listener
    /// (spec §6.8). `0` means the listener is exposed directly and
    /// `X-Forwarded-For` is ignored. `1` means a single trusted proxy
    /// rewrote/appended the client IP and the rate limiter trusts the
    /// rightmost XFF entry. Larger values are accepted but discouraged
    /// (spec mandates "exactly one proxy hop").
    pub trusted_proxy_hops: u8,
    /// PURA-72 Slice F — per-token request budget for `/api/widget/*`.
    /// Defaults to 30 req/min; overridable via
    /// `WIDGET_RATE_LIMIT_PER_TOKEN_PER_MINUTE`. Protects upstream
    /// WebQuery from a single token spammer.
    pub widget_rate_limit_per_token_rpm: u32,
    /// PURA-72 Slice F — per-IP request budget for `/api/widget/*`.
    /// Defaults to 30 req/min; overridable via
    /// `WIDGET_RATE_LIMIT_PER_IP_PER_MINUTE`. Protects the box from a
    /// single client iterating tokens.
    pub widget_rate_limit_per_ip_rpm: u32,
    /// PURA-307 — reverse-proxy allow-list for the public moderation
    /// surface (`/api/public/moderation/*`). `X-Forwarded-For` is trusted
    /// for client-IP attribution **only** when the direct peer falls
    /// inside one of these CIDR blocks (PURA-269 §6 hook 2). Sourced from
    /// `MODERATION_TRUSTED_PROXY_CIDRS` (comma-separated); empty by
    /// default, which is default-deny — XFF is ignored and the rate
    /// limiter keys on the direct connection IP.
    pub moderation_trusted_proxy_cidrs: Vec<ipnet::IpNet>,
}

impl Config {
    pub fn load() -> Result<Self> {
        let node_env = NodeEnv::from_env_string(env::var("NODE_ENV").ok().as_deref());

        let raw_jwt_secret = env::var("JWT_SECRET").ok();
        let (jwt_secret, jwt_secret_is_placeholder) = match (raw_jwt_secret, node_env) {
            (Some(s), _) if !s.is_empty() && s != DEV_JWT_PLACEHOLDER => (s, false),
            (_, NodeEnv::Production) => {
                bail!("JWT_SECRET must be set to a non-placeholder value when NODE_ENV=production");
            }
            (Some(s), NodeEnv::Development) if s == DEV_JWT_PLACEHOLDER => (s, true),
            (_, NodeEnv::Development) => (DEV_JWT_PLACEHOLDER.to_string(), true),
        };
        let jwt_secret_short = jwt_secret.len() < 32;

        let (encryption_key, encryption_key_fell_back) = match env::var("ENCRYPTION_KEY") {
            Ok(s) if !s.is_empty() => (s, false),
            _ => (jwt_secret.clone(), true),
        };

        let host = env_or("HOST", DEFAULT_HOST);
        let port = parse_env_u16("PORT", DEFAULT_PORT)?;
        let database_url =
            env::var("DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.to_string());

        let jwt_access_expiry = parse_duration(&env_or("JWT_ACCESS_EXPIRY", DEFAULT_ACCESS_EXPIRY))
            .context("invalid JWT_ACCESS_EXPIRY")?;
        let jwt_refresh_expiry =
            parse_duration(&env_or("JWT_REFRESH_EXPIRY", DEFAULT_REFRESH_EXPIRY))
                .context("invalid JWT_REFRESH_EXPIRY")?;

        let frontend_url = env::var("FRONTEND_URL").unwrap_or_else(|_| match node_env {
            NodeEnv::Production => DEFAULT_FRONTEND_URL_PROD.to_string(),
            NodeEnv::Development => DEFAULT_FRONTEND_URL_DEV.to_string(),
        });

        let music_dir = PathBuf::from(env_or("MUSIC_DIR", DEFAULT_MUSIC_DIR));

        let sidecar_url = optional_env("SIDECAR_URL");
        let sidecar_binary_path = optional_env("SIDECAR_BINARY_PATH").map(PathBuf::from);
        let moq_public_url = optional_env("MOQ_PUBLIC_URL");
        let yt_cookie_file = optional_env("YT_COOKIE_FILE").map(PathBuf::from);
        let youtube_api_key = optional_env("YOUTUBE_API_KEY");
        let data_dir = PathBuf::from(env_or("DATA_DIR", DEFAULT_DATA_DIR));

        let ts_allow_self_signed = parse_bool_flag("TS_ALLOW_SELF_SIGNED");
        let ssh_tofu = parse_bool_flag("TS_SSH_TOFU");
        let ssh_known_hosts_path = optional_env("TS_SSH_KNOWN_HOSTS").map(PathBuf::from);

        let log_level = env_or("LOG_LEVEL", DEFAULT_LOG_LEVEL);
        let log_pretty = optional_env("LOG_PRETTY").map(|raw| matches!(raw.as_str(), "1" | "true"));

        let trusted_proxy_hops = parse_env_u8("TRUSTED_PROXY_HOPS", 0)?;

        // PURA-72 Slice F — widget rate-limit budgets. Default 30/min per
        // token and per IP. The two buckets are independent — see
        // `web::widget_security` for how they compose.
        let widget_rate_limit_per_token_rpm =
            parse_env_u32("WIDGET_RATE_LIMIT_PER_TOKEN_PER_MINUTE", 30)?;
        let widget_rate_limit_per_ip_rpm =
            parse_env_u32("WIDGET_RATE_LIMIT_PER_IP_PER_MINUTE", 30)?;

        // PURA-307 — public moderation surface trusted-proxy allow-list.
        let moderation_trusted_proxy_cidrs = parse_env_cidrs("MODERATION_TRUSTED_PROXY_CIDRS")?;

        Ok(Self {
            node_env,
            host,
            port,
            database_url,
            jwt_secret,
            jwt_secret_is_placeholder,
            jwt_secret_short,
            encryption_key,
            encryption_key_fell_back,
            jwt_access_expiry,
            jwt_refresh_expiry,
            frontend_url,
            music_dir,
            sidecar_url,
            sidecar_binary_path,
            moq_public_url,
            yt_cookie_file,
            youtube_api_key,
            data_dir,
            ts_allow_self_signed,
            ssh_tofu,
            ssh_known_hosts_path,
            log_level,
            log_pretty,
            trusted_proxy_hops,
            widget_rate_limit_per_token_rpm,
            widget_rate_limit_per_ip_rpm,
            moderation_trusted_proxy_cidrs,
        })
    }

    /// Spec §5.6 — log presence/absence of secrets and key flags at boot.
    /// Never log values themselves.
    pub fn log_hardening_summary(&self) {
        tracing::info!(
            node_env = self.node_env.as_str(),
            host = %self.host,
            port = self.port,
            database_url = %self.database_url,
            frontend_url = %self.frontend_url,
            music_dir = %self.music_dir.display(),
            jwt_secret_placeholder = self.jwt_secret_is_placeholder,
            jwt_secret_short = self.jwt_secret_short,
            encryption_key_fell_back = self.encryption_key_fell_back,
            ts_allow_self_signed = self.ts_allow_self_signed,
            ssh_tofu = self.ssh_tofu,
            ssh_known_hosts_set = self.ssh_known_hosts_path.is_some(),
            sidecar_url_set = self.sidecar_url.is_some(),
            sidecar_binary_path_set = self.sidecar_binary_path.is_some(),
            moq_public_url_set = self.moq_public_url.is_some(),
            yt_cookie_file_set = self.yt_cookie_file.is_some(),
            youtube_api_key_set = self.youtube_api_key.is_some(),
            "ts6-manager configuration loaded"
        );

        if self.jwt_secret_is_placeholder {
            tracing::warn!(
                "JWT_SECRET is unset or equals the dev placeholder; rotate before production"
            );
        }
        if self.jwt_secret_short {
            tracing::warn!("JWT_SECRET is shorter than 32 bytes; recommend ≥32 bytes of entropy");
        }
        if self.encryption_key_fell_back {
            tracing::warn!("ENCRYPTION_KEY unset; derived from JWT_SECRET (spec §5.1 fallback)");
        }
        if !self.encryption_key_fell_back && self.encryption_key == self.jwt_secret {
            tracing::warn!(
                "ENCRYPTION_KEY equals JWT_SECRET; use separate values for better security"
            );
        }
        if self.ssh_tofu {
            // PURA-100 — TOFU is a security-weakening tradeoff. Log
            // every boot so the operator (and any reviewer auditing
            // the journal) sees that the manager is in TOFU mode.
            // Only the first-connect window is the exposure surface;
            // every later connect falls through to strict-fingerprint
            // enforcement against the captured key.
            tracing::warn!(
                "TS_SSH_TOFU=1 — SSHBridge will trust the upstream's host key on \
                 first connect for any server_connection row whose \
                 sshHostKeyFingerprint is NULL and where TS_SSH_KNOWN_HOSTS is unset. \
                 The first-connect MitM window is the security exposure. \
                 Operators who can extract the fingerprint out-of-band SHOULD pin \
                 sshHostKeyFingerprint manually instead."
            );
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

fn optional_env(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

fn parse_env_u16(key: &str, default: u16) -> Result<u16> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .with_context(|| format!("env var {key} is not a valid u16: {v}")),
        _ => Ok(default),
    }
}

fn parse_env_u8(key: &str, default: u8) -> Result<u8> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .with_context(|| format!("env var {key} is not a valid u8: {v}")),
        _ => Ok(default),
    }
}

fn parse_env_u32(key: &str, default: u32) -> Result<u32> {
    match env::var(key) {
        Ok(v) if !v.is_empty() => v
            .parse()
            .with_context(|| format!("env var {key} is not a valid u32: {v}")),
        _ => Ok(default),
    }
}

/// Parse a comma-separated list of CIDR blocks (`10.0.0.0/8,192.168.0.0/16`)
/// — the `MODERATION_TRUSTED_PROXY_CIDRS` allow-list of reverse proxies
/// whose `X-Forwarded-For` header the public moderation surface trusts
/// (PURA-269 §6 hook 2). An empty / unset value yields an empty list,
/// which the resolver treats as **default-deny** — XFF is ignored and the
/// direct peer IP is used. A malformed entry fails boot loudly rather than
/// being silently dropped, so a typo cannot quietly weaken IP attribution.
fn parse_env_cidrs(key: &str) -> Result<Vec<ipnet::IpNet>> {
    let raw = match env::var(key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return Ok(Vec::new()),
    };
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<ipnet::IpNet>()
                .with_context(|| format!("env var {key} has an invalid CIDR entry: {s}"))
        })
        .collect()
}

fn parse_bool_flag(key: &str) -> bool {
    matches!(
        env::var(key).as_deref(),
        Ok("true") | Ok("1") | Ok("TRUE") | Ok("True")
    )
}

/// Parse `15m` / `7d` / `30s` / `2h` style durations. Numeric-only values are treated as seconds.
pub fn parse_duration(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty duration"));
    }

    let last = trimmed.chars().last().unwrap();
    let (digits, multiplier) = match last {
        's' => (&trimmed[..trimmed.len() - 1], 1u64),
        'm' => (&trimmed[..trimmed.len() - 1], 60),
        'h' => (&trimmed[..trimmed.len() - 1], 60 * 60),
        'd' => (&trimmed[..trimmed.len() - 1], 60 * 60 * 24),
        _ if last.is_ascii_digit() => (trimmed, 1),
        _ => return Err(anyhow!("unknown duration suffix in '{input}'")),
    };

    let n: u64 = digits
        .parse()
        .with_context(|| format!("invalid duration number in '{input}'"))?;
    Ok(Duration::from_secs(n * multiplier))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_parses_suffix_units() {
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_secs(900));
        assert_eq!(parse_duration("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("60").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn duration_rejects_garbage() {
        assert!(parse_duration("").is_err());
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("15x").is_err());
    }

    #[test]
    fn node_env_defaults_to_development() {
        assert_eq!(NodeEnv::from_env_string(None), NodeEnv::Development);
        assert_eq!(
            NodeEnv::from_env_string(Some("anything-else")),
            NodeEnv::Development
        );
        assert_eq!(
            NodeEnv::from_env_string(Some("production")),
            NodeEnv::Production
        );
    }
}
