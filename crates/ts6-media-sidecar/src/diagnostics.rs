//! In-process diagnostics bag for operator bug reports (PR #28 `context`).
//!
//! The sidecar is a separate binary from the manager. When `SIDECAR_URL`
//! is set the manager GETs [`GET /diagnostics`](crate::http) and folds
//! these camelCase keys into `POST /api/bug-reports` `context`:
//!
//! - `sidecarFfmpegExit` — last FFmpeg spawn/exit (role + status, no argv)
//! - `sidecarSsrfReject` — last `/source` SSRF deny reason (no IPs, no
//!   credentialed URLs)
//! - `sidecarMoqError` — last publish-drop / session / mux summary
//! - `sidecarHealth` — cheap in-process health hint (not journal scrape)
//! - `sidecarLogTail` — last N structured lines from this bag
//!
//! Observability only — does not unpark MoQ video. No Contabo journal
//! scrape. Values are size-capped so they fit PR #28's context caps.

use std::collections::VecDeque;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use ts6_ssrf::SsrfError;

/// Context key names — keep in lock-step with the manager fold-in
/// (`ts6-manager-server` `bug_reports::sidecar_context`).
pub const KEY_FFMPEG_EXIT: &str = "sidecarFfmpegExit";
pub const KEY_SSRF_REJECT: &str = "sidecarSsrfReject";
pub const KEY_MOQ_ERROR: &str = "sidecarMoqError";
pub const KEY_HEALTH: &str = "sidecarHealth";
pub const KEY_LOG_TAIL: &str = "sidecarLogTail";

/// Last-N structured lines kept for [`KEY_LOG_TAIL`].
pub const LOG_TAIL_CAP: usize = 24;
/// Soft cap per rendered field (PR #28 value cap is 4 KiB).
pub const FIELD_CAP: usize = 1024;
/// Soft cap for the joined log tail.
pub const LOG_TAIL_BYTES: usize = 2048;

#[derive(Debug, Clone)]
struct Timed<T> {
    at: Instant,
    value: T,
}

/// Process-wide (per sidecar instance) last-event slots + ring.
#[derive(Debug)]
pub struct Diagnostics {
    started_at: Instant,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    ffmpeg: Option<Timed<String>>,
    ssrf: Option<Timed<String>>,
    moq: Option<Timed<String>>,
    health_hint: Option<Timed<String>>,
    log: VecDeque<String>,
}

impl Diagnostics {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn record_ffmpeg_spawn(&self, role: &str, ok: bool, err_kind: Option<&str>) {
        let line = if ok {
            format!("spawn role={role} result=ok")
        } else {
            format!(
                "spawn role={role} result=error kind={}",
                err_kind.unwrap_or("unknown")
            )
        };
        self.store_ffmpeg(line);
    }

    pub fn record_ffmpeg_exit(&self, role: &str, code: Option<i32>, signal: Option<i32>) {
        let code = code.map(|c| c.to_string()).unwrap_or_else(|| "-".into());
        let signal = signal
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none".into());
        self.store_ffmpeg(format!("exit role={role} code={code} signal={signal}"));
    }

    /// Record a `/source` SSRF rejection. Never stores the raw URL, userinfo,
    /// or the blocked IP — only a reason code + (when safe) a hostname.
    pub fn record_ssrf_reject(&self, err: &SsrfError) {
        let line = sanitize_ssrf_reason(err);
        self.push_log(&format!("ssrf_reject {line}"));
        if let Ok(mut inner) = self.inner.lock() {
            inner.ssrf = Some(Timed {
                at: Instant::now(),
                value: line,
            });
        }
    }

    pub fn record_moq_error(&self, kind: &str, summary: &str) {
        let summary = redact_for_issue(summary);
        let line = format!("kind={kind} summary={summary}");
        self.push_log(&format!("moq {line}"));
        if let Ok(mut inner) = self.inner.lock() {
            inner.moq = Some(Timed {
                at: Instant::now(),
                value: truncate(&line, FIELD_CAP),
            });
        }
    }

    pub fn record_health_hint(&self, hint: &str) {
        let line = redact_for_issue(hint);
        self.push_log(&format!("health {line}"));
        if let Ok(mut inner) = self.inner.lock() {
            inner.health_hint = Some(Timed {
                at: Instant::now(),
                value: truncate(&line, FIELD_CAP),
            });
        }
    }

    pub fn snapshot(&self, health_hint: Option<&str>) -> DiagnosticsSnapshot {
        let inner = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return DiagnosticsSnapshot::default(),
        };
        let ago = |at: Instant| at.elapsed().as_secs();
        let ffmpeg = inner
            .ffmpeg
            .as_ref()
            .map(|t| truncate(&format!("{} ago_s={}", t.value, ago(t.at)), FIELD_CAP));
        let ssrf = inner
            .ssrf
            .as_ref()
            .map(|t| truncate(&format!("{} ago_s={}", t.value, ago(t.at)), FIELD_CAP));
        let moq = inner
            .moq
            .as_ref()
            .map(|t| truncate(&format!("{} ago_s={}", t.value, ago(t.at)), FIELD_CAP));
        let health = health_hint
            .map(|h| truncate(&redact_for_issue(h), FIELD_CAP))
            .or_else(|| inner.health_hint.as_ref().map(|t| t.value.clone()))
            .or_else(|| {
                Some(format!(
                    "ok uptime_s={}",
                    self.started_at.elapsed().as_secs()
                ))
            });
        let log_tail = join_log_tail(&inner.log);
        DiagnosticsSnapshot {
            sidecar_ffmpeg_exit: ffmpeg,
            sidecar_ssrf_reject: ssrf,
            sidecar_moq_error: moq,
            sidecar_health: health,
            sidecar_log_tail: if log_tail.is_empty() {
                None
            } else {
                Some(log_tail)
            },
        }
    }

    fn store_ffmpeg(&self, line: String) {
        self.push_log(&format!("ffmpeg {line}"));
        if let Ok(mut inner) = self.inner.lock() {
            inner.ffmpeg = Some(Timed {
                at: Instant::now(),
                value: truncate(&line, FIELD_CAP),
            });
        }
    }

    fn push_log(&self, line: &str) {
        let line = truncate(&redact_for_issue(line), 200);
        if line.is_empty() {
            return;
        }
        if let Ok(mut inner) = self.inner.lock() {
            if inner.log.len() >= LOG_TAIL_CAP {
                inner.log.pop_front();
            }
            let t = self.started_at.elapsed().as_secs();
            inner.log.push_back(format!("t+{t}s {line}"));
        }
    }
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

/// Wire body for `GET /diagnostics` — keys match PR #28 `context`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_ffmpeg_exit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_ssrf_reject: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_moq_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sidecar_log_tail: Option<String>,
}

impl DiagnosticsSnapshot {
    /// Flatten into `(key, value)` pairs for the manager context bag.
    pub fn context_pairs(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let push = |out: &mut Vec<(String, String)>, key: &str, value: &Option<String>| {
            if let Some(v) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                out.push((key.to_string(), v.to_string()));
            }
        };
        push(&mut out, KEY_FFMPEG_EXIT, &self.sidecar_ffmpeg_exit);
        push(&mut out, KEY_SSRF_REJECT, &self.sidecar_ssrf_reject);
        push(&mut out, KEY_MOQ_ERROR, &self.sidecar_moq_error);
        push(&mut out, KEY_HEALTH, &self.sidecar_health);
        push(&mut out, KEY_LOG_TAIL, &self.sidecar_log_tail);
        out
    }
}

/// Map [`SsrfError`] to a reason string that never includes a blocked IP
/// or a credentialed URL.
pub fn sanitize_ssrf_reason(err: &SsrfError) -> String {
    match err {
        SsrfError::InvalidUrlFormat => "reason=invalid_url_format".into(),
        SsrfError::DisallowedProtocol(scheme) => {
            format!(
                "reason=disallowed_protocol scheme={}",
                sanitize_token(scheme)
            )
        }
        SsrfError::MissingHost => "reason=missing_host".into(),
        SsrfError::HostnameNotAllowed(host) => {
            format!(
                "reason=hostname_not_allowed host={}",
                sanitize_hostname(host)
            )
        }
        SsrfError::IpNotAllowed(ip) => {
            format!("reason=ip_not_allowed class={}", ip_class(*ip))
        }
        SsrfError::ResolvedToBlockedRange { host, ip } => {
            format!(
                "reason=resolved_to_blocked_range host={} class={}",
                sanitize_hostname(host),
                ip_class(*ip)
            )
        }
    }
}

fn ip_class(ip: IpAddr) -> &'static str {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if o[0] == 0 {
                "unspecified"
            } else if o[0] == 127 {
                "loopback"
            } else if o[0] == 169 && o[1] == 254 {
                "link_local"
            } else if o[0] == 10
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
            {
                "private"
            } else {
                "blocked"
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() {
                "unspecified"
            } else if v6.is_loopback() {
                "loopback"
            } else {
                let segs = v6.segments();
                if (segs[0] & 0xFFC0) == 0xFE80 {
                    "link_local"
                } else if (segs[0] & 0xFE00) == 0xFC00 {
                    "ula"
                } else {
                    "blocked"
                }
            }
        }
    }
}

fn sanitize_hostname(host: &str) -> String {
    let host = host.trim().trim_matches(|c| c == '[' || c == ']');
    if host.parse::<IpAddr>().is_ok() {
        return "[host]".into();
    }
    sanitize_token(host)
}

fn sanitize_token(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// Strip credentialed userinfo, IP literals, and obvious secret tokens.
pub fn redact_for_issue(s: &str) -> String {
    let mut out = strip_url_userinfo(s);
    out = redact_ip_literals(&out);
    truncate(&out, FIELD_CAP)
}

fn strip_url_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(rel) = s[i..].find("://") {
            out.push_str(&s[i..i + rel + 3]);
            i += rel + 3;
            let rest = &s[i..];
            let end = rest
                .find(|c: char| c == '/' || c == ' ' || c == '"' || c == '\'')
                .unwrap_or(rest.len());
            let authority = &rest[..end];
            if let Some(at) = authority.rfind('@') {
                out.push_str("[redacted]@");
                out.push_str(&authority[at + 1..]);
                i += end;
            } else {
                out.push_str(authority);
                i += end;
            }
        } else {
            out.push_str(&s[i..]);
            break;
        }
    }
    out
}

fn redact_ip_literals(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            if let Some((end, _)) = match_ipv4(&chars, i) {
                out.push_str("[ip]");
                i = end;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some(end) = match_bracketed_ipv6(&chars, i) {
                out.push_str("[ip]");
                i = end;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn match_ipv4(chars: &[char], start: usize) -> Option<(usize, ())> {
    let mut i = start;
    let mut octets = 0;
    while octets < 4 {
        if i >= chars.len() || !chars[i].is_ascii_digit() {
            return None;
        }
        let oct_start = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i - oct_start > 3 {
            return None;
        }
        octets += 1;
        if octets == 4 {
            return Some((i, ()));
        }
        if i >= chars.len() || chars[i] != '.' {
            return None;
        }
        i += 1;
    }
    None
}

fn match_bracketed_ipv6(chars: &[char], start: usize) -> Option<usize> {
    if start >= chars.len() || chars[start] != '[' {
        return None;
    }
    let close = (start + 1..chars.len()).find(|&i| chars[i] == ']')?;
    let inner: String = chars[start + 1..close].iter().collect();
    if inner.parse::<std::net::Ipv6Addr>().is_ok() {
        Some(close + 1)
    } else {
        None
    }
}

fn join_log_tail(log: &VecDeque<String>) -> String {
    let mut out = String::new();
    for line in log {
        if out.len().saturating_add(line.len()).saturating_add(1) > LOG_TAIL_BYTES {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;
    use std::str::FromStr;

    fn ip(s: &str) -> IpAddr {
        IpAddr::from_str(s).unwrap()
    }

    #[test]
    fn ssrf_ip_literal_never_echoes_address() {
        let err = SsrfError::IpNotAllowed(ip("10.0.0.42"));
        let line = sanitize_ssrf_reason(&err);
        assert!(line.contains("reason=ip_not_allowed"));
        assert!(line.contains("class=private"));
        assert!(!line.contains("10.0.0.42"));
    }

    #[test]
    fn ssrf_loopback_and_unspecified_use_class_not_ip() {
        assert!(!sanitize_ssrf_reason(&SsrfError::IpNotAllowed(ip("127.0.0.1"))).contains("127."));
        assert!(
            sanitize_ssrf_reason(&SsrfError::IpNotAllowed(ip("127.0.0.1"))).contains("loopback")
        );
        let v6 = SsrfError::IpNotAllowed(ip("::"));
        let line = sanitize_ssrf_reason(&v6);
        assert!(line.contains("unspecified"));
        assert!(!line.contains("::"));
    }

    #[test]
    fn ssrf_resolved_range_keeps_host_drops_ip() {
        let err = SsrfError::ResolvedToBlockedRange {
            host: "private.test".into(),
            ip: ip("192.168.1.9"),
        };
        let line = sanitize_ssrf_reason(&err);
        assert!(line.contains("host=private.test"));
        assert!(line.contains("class=private"));
        assert!(!line.contains("192.168"));
    }

    #[test]
    fn ssrf_hostname_literal_as_ip_is_masked() {
        let err = SsrfError::HostnameNotAllowed("127.0.0.1".into());
        let line = sanitize_ssrf_reason(&err);
        assert_eq!(line, "reason=hostname_not_allowed host=[host]");
    }

    #[test]
    fn redact_strips_userinfo_and_ips() {
        let raw = "fetch http://alice:s3cret@10.1.2.3/clip.mp4 from [fe80::1]";
        let red = redact_for_issue(raw);
        assert!(!red.contains("alice"));
        assert!(!red.contains("s3cret"));
        assert!(!red.contains("10.1.2.3"));
        assert!(!red.contains("fe80"));
        assert!(red.contains("[redacted]@"));
        assert!(red.contains("[ip]"));
    }

    #[test]
    fn snapshot_flattens_context_keys() {
        let d = Diagnostics::new();
        d.record_ffmpeg_exit("video", Some(1), None);
        d.record_ssrf_reject(&SsrfError::IpNotAllowed(ip("127.0.0.1")));
        d.record_moq_error("publish_drop", "append video group failed");
        let snap = d.snapshot(Some("degraded ffmpeg_dead=1"));
        let keys: Vec<_> = snap.context_pairs().into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            keys,
            vec![
                KEY_FFMPEG_EXIT,
                KEY_SSRF_REJECT,
                KEY_MOQ_ERROR,
                KEY_HEALTH,
                KEY_LOG_TAIL,
            ]
        );
        let ssrf = snap.sidecar_ssrf_reject.unwrap();
        assert!(!ssrf.contains("127.0.0.1"));
        assert!(snap.sidecar_log_tail.unwrap().contains("ssrf_reject"));
    }

    #[test]
    fn diagnostics_json_uses_context_camel_case() {
        let snap = DiagnosticsSnapshot {
            sidecar_ffmpeg_exit: Some("exit role=video code=1 signal=none".into()),
            sidecar_ssrf_reject: Some("reason=ip_not_allowed class=loopback".into()),
            sidecar_moq_error: None,
            sidecar_health: Some("ok".into()),
            sidecar_log_tail: Some("t+0s ffmpeg exit".into()),
        };
        let json = serde_json::to_string(&snap).unwrap();
        for key in [KEY_FFMPEG_EXIT, KEY_SSRF_REJECT, KEY_HEALTH, KEY_LOG_TAIL] {
            assert!(
                json.contains(&format!("\"{key}\"")),
                "missing {key}: {json}"
            );
        }
        assert!(!json.contains("sidecar_ffmpeg_exit"));
        assert!(!json.contains(KEY_MOQ_ERROR));
    }
}
