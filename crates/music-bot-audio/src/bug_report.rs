//! In-process Music Bot ring for operator bug-report `context`.
//!
//! API PR #28 (`POST /api/bug-reports`) has no server-side tracing ring.
//! Seats attach short tails via `context` keys. Music Bot fills:
//!
//! - `musicBotLatency` — recent resolve / retry / fallback stages
//!   (`stage`, `elapsed_ms`, `retry`)
//! - `logTail` — short structured `music_bot_latency` (and related) lines
//!
//! Caps stay well under the shared 4k-per-value / 32 KiB total limits in
//! `ts6_manager_shared::bug_reports`. Secret-shaped tokens are redacted
//! before a line enters the ring.
//!
//! The [`layer`] is a `tracing` `Layer` that records every
//! `music_bot_latency` event (the same stages the SSE / dashboard path
//! already emits: `resolver_warm_retry`, `resolver_resolved`,
//! `first_frame_on_wire`, …). Install it on the process subscriber.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Suggested `context` key for the stage list. Matches API PR #28.
pub const CONTEXT_KEY_MUSIC_BOT_LATENCY: &str = "musicBotLatency";
/// Suggested `context` key for the structured log tail. Matches API PR #28.
pub const CONTEXT_KEY_LOG_TAIL: &str = "logTail";

/// Keep each rendered value well under the 4k-char `context` cap.
pub const MAX_SNAPSHOT_VALUE_CHARS: usize = 2048;
/// Oldest stages drop first.
const MAX_STAGES: usize = 16;
/// Oldest log lines drop first.
const MAX_LOG_LINES: usize = 16;
const MAX_LINE_CHARS: usize = 180;

/// One resolve / retry / fallback (or later wire) latency stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LatencyStage {
    pub stage: String,
    pub elapsed_ms: Option<u64>,
    pub retry: bool,
}

/// Rendered bag ready to merge into `POST /api/bug-reports` `context`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BugReportSnapshot {
    pub music_bot_latency: String,
    pub log_tail: String,
}

impl BugReportSnapshot {
    /// Insert `musicBotLatency` / `logTail` only when the key is absent
    /// and the snapshot has something to say. Does not overwrite Panel-
    /// supplied values.
    pub fn merge_absent(&self, context: &mut Map<String, Value>) {
        if !context.contains_key(CONTEXT_KEY_MUSIC_BOT_LATENCY)
            && !self.music_bot_latency.is_empty()
        {
            context.insert(
                CONTEXT_KEY_MUSIC_BOT_LATENCY.to_string(),
                Value::String(self.music_bot_latency.clone()),
            );
        }
        if !context.contains_key(CONTEXT_KEY_LOG_TAIL) && !self.log_tail.is_empty() {
            context.insert(
                CONTEXT_KEY_LOG_TAIL.to_string(),
                Value::String(self.log_tail.clone()),
            );
        }
    }
}

/// Bounded in-process ring. Cheap to clone (`Arc`).
#[derive(Debug, Clone)]
pub struct LatencyRing {
    inner: Arc<Mutex<RingInner>>,
}

#[derive(Debug, Default)]
struct RingInner {
    stages: VecDeque<LatencyStage>,
    lines: VecDeque<String>,
}

impl LatencyRing {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RingInner::default())),
        }
    }

    /// Tracing layer that writes into this ring.
    pub fn layer(&self) -> LatencyRingLayer {
        LatencyRingLayer { ring: self.clone() }
    }

    /// Record a stage + a structured line (tests and the tracing layer).
    pub fn record_stage(&self, stage: LatencyStage, line: impl Into<String>) {
        let mut inner = lock(&self.inner);
        push_stage(&mut inner.stages, stage);
        push_line(&mut inner.lines, &redact(&line.into()));
    }

    /// Record a related structured line that is not a latency stage
    /// (e.g. a `yt_dlp` WARN).
    pub fn record_line(&self, line: impl Into<String>) {
        let mut inner = lock(&self.inner);
        push_line(&mut inner.lines, &redact(&line.into()));
    }

    pub fn snapshot(&self) -> BugReportSnapshot {
        let inner = lock(&self.inner);
        BugReportSnapshot {
            music_bot_latency: render_stages(&inner.stages),
            log_tail: render_lines(&inner.lines),
        }
    }

    pub fn clear(&self) {
        let mut inner = lock(&self.inner);
        inner.stages.clear();
        inner.lines.clear();
    }
}

impl Default for LatencyRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide ring the server subscriber writes into.
pub fn global_ring() -> &'static LatencyRing {
    static GLOBAL: OnceLock<LatencyRing> = OnceLock::new();
    GLOBAL.get_or_init(LatencyRing::new)
}

/// Layer for the process subscriber (`logging::init`).
pub fn layer() -> LatencyRingLayer {
    global_ring().layer()
}

/// Snapshot of the process-wide ring.
pub fn snapshot() -> BugReportSnapshot {
    global_ring().snapshot()
}

/// Serialise tests (this crate and dependents) that mutate the
/// process-wide ring. Always available so server-crate tests can take it.
pub fn test_global_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// `tracing` layer that captures `music_bot_latency` (and related `yt_dlp`
/// warnings) into a [`LatencyRing`].
#[derive(Debug, Clone)]
pub struct LatencyRingLayer {
    ring: LatencyRing,
}

impl<S> Layer<S> for LatencyRingLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let target = meta.target();
        let related = target == "yt_dlp";
        if target != "music_bot_latency" && !related {
            return;
        }
        // Related `yt_dlp` chatter is WARN+ only so the tail stays short.
        if related && *meta.level() > tracing::Level::WARN {
            return;
        }

        let mut grab = FieldGrab::default();
        event.record(&mut grab);

        if target == "music_bot_latency"
            && let Some(stage) = grab.stage.clone()
        {
            let retry = grab.retry || stage_implies_retry(&stage);
            let elapsed_ms = grab.elapsed_ms;
            let line = format_event_line(target, &grab);
            self.ring.record_stage(
                LatencyStage {
                    stage,
                    elapsed_ms,
                    retry,
                },
                line,
            );
            return;
        }
        self.ring.record_line(format_event_line(target, &grab));
    }
}

#[derive(Debug, Default)]
struct FieldGrab {
    stage: Option<String>,
    elapsed_ms: Option<u64>,
    retry: bool,
    message: Option<String>,
    extras: Vec<(String, String)>,
}

impl Visit for FieldGrab {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record_owned(field.name(), format!("{value:?}"));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_owned(field.name(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        match field.name() {
            "elapsed_ms" | "phase_ms" | "lateness_ms" => {
                if self.elapsed_ms.is_none() {
                    self.elapsed_ms = Some(value);
                }
            }
            name => self.extras.push((name.to_string(), value.to_string())),
        }
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        if value >= 0 {
            self.record_u64(field, value as u64);
        } else {
            self.extras
                .push((field.name().to_string(), value.to_string()));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        match field.name() {
            "retry" | "retrying" => self.retry = value,
            name => self.extras.push((name.to_string(), value.to_string())),
        }
    }
}

impl FieldGrab {
    fn record_owned(&mut self, name: &str, value: String) {
        match name {
            "stage" => self.stage = Some(value),
            "message" => self.message = Some(value),
            "elapsed_ms" | "phase_ms" | "lateness_ms" => {
                if self.elapsed_ms.is_none()
                    && let Ok(ms) = value.parse::<u64>()
                {
                    self.elapsed_ms = Some(ms);
                }
            }
            "retry" | "retrying" => {
                self.retry = matches!(value.as_str(), "true" | "1");
            }
            _ => self.extras.push((name.to_string(), value)),
        }
    }
}

fn stage_implies_retry(stage: &str) -> bool {
    stage.contains("retry")
}

fn format_event_line(target: &str, grab: &FieldGrab) -> String {
    let mut parts = vec![target.to_string()];
    if let Some(stage) = &grab.stage {
        parts.push(format!("stage={stage}"));
    }
    if let Some(ms) = grab.elapsed_ms {
        parts.push(format!("elapsed_ms={ms}"));
    }
    if grab.retry {
        parts.push("retry=1".into());
    }
    for (k, v) in &grab.extras {
        if k == "log.target" || k == "log.module_path" {
            continue;
        }
        parts.push(format!("{k}={}", compact_extra(v)));
    }
    if let Some(msg) = &grab.message {
        parts.push(msg.clone());
    }
    parts.join(" ")
}

fn compact_extra(v: &str) -> String {
    truncate_chars(v.trim_matches('"'), 64)
}

fn push_stage(stages: &mut VecDeque<LatencyStage>, stage: LatencyStage) {
    if stages.len() >= MAX_STAGES {
        stages.pop_front();
    }
    stages.push_back(stage);
}

fn push_line(lines: &mut VecDeque<String>, line: &str) {
    let line = truncate_chars(line, MAX_LINE_CHARS);
    if line.is_empty() {
        return;
    }
    if lines.len() >= MAX_LOG_LINES {
        lines.pop_front();
    }
    lines.push_back(line);
}

fn render_stages(stages: &VecDeque<LatencyStage>) -> String {
    let mut out = String::new();
    for stage in stages {
        let ms = stage
            .elapsed_ms
            .map(|v| v.to_string())
            .unwrap_or_else(|| "-".into());
        let retry = if stage.retry { 1 } else { 0 };
        let line = format!("{} elapsed_ms={} retry={}", stage.stage, ms, retry);
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
        if out.chars().count() >= MAX_SNAPSHOT_VALUE_CHARS {
            break;
        }
    }
    truncate_chars(&out, MAX_SNAPSHOT_VALUE_CHARS)
}

fn render_lines(lines: &VecDeque<String>) -> String {
    let mut out = String::new();
    for line in lines {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        if out.chars().count() >= MAX_SNAPSHOT_VALUE_CHARS {
            break;
        }
    }
    truncate_chars(&out, MAX_SNAPSHOT_VALUE_CHARS)
}

fn lock(inner: &Mutex<RingInner>) -> std::sync::MutexGuard<'_, RingInner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

/// Redact JWT / GitHub PAT / Bearer tokens and obvious query secrets.
pub fn redact(s: &str) -> String {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    let mut out = Vec::with_capacity(tokens.len());
    let mut redact_next = false;
    for tok in tokens {
        let trimmed = tok.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';' | ')'));
        let lower = trimmed.to_ascii_lowercase();
        if redact_next || looks_secret(trimmed) {
            out.push("[redacted]".to_string());
            redact_next = false;
            continue;
        }
        if lower == "bearer" || lower == "authorization:" {
            out.push("[redacted]".to_string());
            redact_next = true;
            continue;
        }
        out.push(redact_query_secrets(tok));
    }
    out.join(" ").replace('\0', "")
}

fn looks_secret(tok: &str) -> bool {
    let lower = tok.to_ascii_lowercase();
    if lower.starts_with("bearer ")
        || lower.contains("authorization:")
        || tok.starts_with("ghp_")
        || tok.starts_with("github_pat_")
        || tok.starts_with("gho_")
        || tok.starts_with("ghu_")
        || tok.starts_with("ghs_")
        || tok.starts_with("ghr_")
    {
        return true;
    }
    let parts: Vec<&str> = tok.split('.').collect();
    parts.len() == 3 && parts[0].starts_with("eyJ") && parts.iter().all(|p| !p.is_empty())
}

fn redact_query_secrets(tok: &str) -> String {
    let lower = tok.to_ascii_lowercase();
    let mut out = tok.to_string();
    for key in [
        "token=",
        "api_key=",
        "apikey=",
        "cookie=",
        "password=",
        "authorization=",
    ] {
        if let Some(idx) = lower.find(key) {
            let start = idx + key.len();
            if start > out.len() {
                continue;
            }
            let rest = &out[start..];
            let end = rest.find(['&', '?', '#', '"']).unwrap_or(rest.len());
            out.replace_range(start..start + end, "[redacted]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::Level;
    use tracing_subscriber::layer::SubscriberExt;

    fn stage(name: &str, ms: Option<u64>, retry: bool) -> LatencyStage {
        LatencyStage {
            stage: name.into(),
            elapsed_ms: ms,
            retry,
        }
    }

    #[test]
    fn ring_keeps_recent_stages_and_renders_compact_lines() {
        let ring = LatencyRing::new();
        ring.record_stage(stage("resolver_warm_retry", None, true), "warm retry");
        ring.record_stage(
            stage("resolver_resolved", Some(1840), false),
            "resolver_resolved elapsed_ms=1840",
        );
        ring.record_stage(
            stage("first_frame_on_wire", Some(2105), false),
            "first_frame_on_wire elapsed_ms=2105",
        );

        let snap = ring.snapshot();
        assert!(
            snap.music_bot_latency
                .contains("resolver_warm_retry elapsed_ms=- retry=1"),
            "{}",
            snap.music_bot_latency
        );
        assert!(
            snap.music_bot_latency
                .contains("resolver_resolved elapsed_ms=1840 retry=0")
        );
        assert!(
            snap.music_bot_latency
                .contains("first_frame_on_wire elapsed_ms=2105 retry=0")
        );
        assert!(snap.log_tail.contains("resolver_resolved elapsed_ms=1840"));
        assert!(snap.music_bot_latency.chars().count() <= MAX_SNAPSHOT_VALUE_CHARS);
        assert!(snap.log_tail.chars().count() <= MAX_SNAPSHOT_VALUE_CHARS);
    }

    #[test]
    fn ring_drops_oldest_when_full() {
        let ring = LatencyRing::new();
        for i in 0..(MAX_STAGES + 4) {
            ring.record_stage(
                stage(&format!("stage_{i}"), Some(i as u64), false),
                format!("line {i}"),
            );
        }
        let snap = ring.snapshot();
        assert!(!snap.music_bot_latency.contains("stage_0"));
        assert!(
            snap.music_bot_latency
                .contains(&format!("stage_{}", MAX_STAGES + 3))
        );
        assert!(!snap.log_tail.contains("line 0"));
        assert!(
            snap.log_tail
                .contains(&format!("line {}", MAX_LOG_LINES + 3))
        );
    }

    #[test]
    fn merge_absent_fills_only_missing_keys() {
        let ring = LatencyRing::new();
        ring.record_stage(stage("resolver_resolved", Some(20), false), "ok");
        let snap = ring.snapshot();

        let mut ctx = Map::new();
        snap.merge_absent(&mut ctx);
        assert_eq!(
            ctx.get(CONTEXT_KEY_MUSIC_BOT_LATENCY)
                .and_then(|v| v.as_str()),
            Some(snap.music_bot_latency.as_str())
        );
        assert_eq!(
            ctx.get(CONTEXT_KEY_LOG_TAIL).and_then(|v| v.as_str()),
            Some(snap.log_tail.as_str())
        );

        ctx.insert(
            CONTEXT_KEY_MUSIC_BOT_LATENCY.into(),
            Value::String("panel-supplied".into()),
        );
        let again = ring.snapshot();
        again.merge_absent(&mut ctx);
        assert_eq!(
            ctx.get(CONTEXT_KEY_MUSIC_BOT_LATENCY)
                .and_then(|v| v.as_str()),
            Some("panel-supplied")
        );
    }

    #[test]
    fn merge_absent_skips_empty_snapshot() {
        let snap = BugReportSnapshot::default();
        let mut ctx = Map::new();
        snap.merge_absent(&mut ctx);
        assert!(ctx.is_empty());
    }

    #[test]
    fn redact_strips_jwt_github_and_query_secrets() {
        let raw = "got eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sig and ghp_abcdefghijklmnopqrstuvwxyz012345 url=https://x/?token=abc&ok=1 Bearer hunter2";
        let cleaned = redact(raw);
        assert!(!cleaned.contains("eyJ"));
        assert!(!cleaned.contains("ghp_"));
        assert!(!cleaned.contains("hunter2"));
        assert!(!cleaned.contains("token=abc"));
        assert!(cleaned.contains("[redacted]"));
        assert!(cleaned.contains("ok=1"));
    }

    #[test]
    fn tracing_layer_captures_music_bot_latency_stages() {
        let ring = LatencyRing::new();
        let subscriber = tracing_subscriber::registry()
            .with(ring.layer())
            .with(tracing_subscriber::filter::LevelFilter::INFO);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "music_bot_latency",
                stage = "resolver_warm_retry",
                error = "timeout",
                "warm resolve blipped"
            );
            tracing::info!(
                target: "music_bot_latency",
                stage = "resolver_resolved",
                elapsed_ms = 20u64,
                "warm yt-dlp resolver returned direct media URL"
            );
            tracing::info!(
                target: "music_bot_latency",
                stage = "first_frame_on_wire",
                elapsed_ms = 105u64,
                "first Opus frame sent on the wire — playback audible"
            );
            tracing::info!(target: "unrelated", stage = "nope", "ignored");
            tracing::info!(target: "yt_dlp", "too chatty");
            tracing::event!(
                target: "yt_dlp",
                Level::WARN,
                "ERROR: Sign in to confirm you’re not a bot"
            );
        });

        let snap = ring.snapshot();
        assert!(
            snap.music_bot_latency.contains("resolver_warm_retry")
                && snap.music_bot_latency.contains("retry=1"),
            "{}",
            snap.music_bot_latency
        );
        assert!(
            snap.music_bot_latency
                .contains("resolver_resolved elapsed_ms=20 retry=0")
        );
        assert!(
            snap.music_bot_latency
                .contains("first_frame_on_wire elapsed_ms=105 retry=0")
        );
        assert!(!snap.music_bot_latency.contains("nope"));
        assert!(snap.log_tail.contains("stage=resolver_resolved"));
        assert!(snap.log_tail.contains("yt_dlp"));
        assert!(!snap.log_tail.contains("too chatty"));
    }

    #[test]
    fn snapshot_values_stay_under_shared_context_cap() {
        let ring = LatencyRing::new();
        let huge = "x".repeat(400);
        for i in 0..MAX_STAGES {
            ring.record_stage(
                stage(
                    &format!("resolver_phase_{i}"),
                    Some(1_000 + i as u64),
                    i == 0,
                ),
                format!("{huge} line {i} ghp_shouldnotsurvive"),
            );
        }
        let snap = ring.snapshot();
        assert!(snap.music_bot_latency.chars().count() <= MAX_SNAPSHOT_VALUE_CHARS);
        assert!(snap.log_tail.chars().count() <= MAX_SNAPSHOT_VALUE_CHARS);
        assert!(snap.music_bot_latency.chars().count() < 4096);
        assert!(snap.log_tail.chars().count() < 4096);
        assert!(!snap.log_tail.contains("ghp_"));
    }
}
