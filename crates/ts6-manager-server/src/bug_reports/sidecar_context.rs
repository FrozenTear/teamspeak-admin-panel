//! Fold sidecar diagnostics into a bug-report `context` bag.
//!
//! The sidecar is a separate binary (`ts6-media-sidecar`). When
//! `SIDECAR_URL` is set the manager GETs `GET /diagnostics` (short
//! timeout) and merges prefixed keys into the PR #28 `context` map
//! **before** validate, matching Voice (#29): existing keys (Panel /
//! Music Bot `logTail` / Voice `voiceLogTail`) are never overwritten.
//!
//! No second SaaS: we do not file Issues from the sidecar process. The
//! only sink is `POST /api/bug-reports` on this manager.

use std::time::Duration;

use serde_json::{Map, Value};

use crate::control::sidecar::{DiagnosticsResponse, SidecarClient};

const PULL_TIMEOUT: Duration = Duration::from_secs(2);

pub const KEY_FFMPEG_EXIT: &str = "sidecarFfmpegExit";
pub const KEY_SSRF_REJECT: &str = "sidecarSsrfReject";
pub const KEY_MOQ_ERROR: &str = "sidecarMoqError";
pub const KEY_HEALTH: &str = "sidecarHealth";
pub const KEY_LOG_TAIL: &str = "sidecarLogTail";
pub const KEY_STATUS: &str = "sidecarStatus";

/// Merge sidecar diagnostics into the request `context` map. Best-effort:
/// a down / unconfigured sidecar must not fail the bug report.
pub async fn fold_sidecar_context(sidecar: Option<&SidecarClient>, dest: &mut Map<String, Value>) {
    let extra = match sidecar {
        None => vec![(KEY_STATUS, "unconfigured")],
        Some(client) => match pull(client).await {
            Ok(pairs) => {
                merge_owned(dest, pairs);
                return;
            }
            Err(()) => vec![(KEY_STATUS, "unreachable")],
        },
    };
    for (key, value) in extra {
        dest.entry(key.to_string())
            .or_insert_with(|| Value::String(value.to_string()));
    }
}

async fn pull(client: &SidecarClient) -> Result<Vec<(String, String)>, ()> {
    let fetched = tokio::time::timeout(PULL_TIMEOUT, client.get_diagnostics()).await;
    match fetched {
        Ok(Ok(diag)) => Ok(pairs_from_diagnostics(&diag)),
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "bug-reports: sidecar /diagnostics failed");
            Err(())
        }
        Err(_) => {
            tracing::debug!("bug-reports: sidecar /diagnostics timed out");
            Err(())
        }
    }
}

fn pairs_from_diagnostics(diag: &DiagnosticsResponse) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let push = |out: &mut Vec<(String, String)>, key: &str, value: &Option<String>| {
        if let Some(v) = value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            out.push((key.to_string(), v.to_string()));
        }
    };
    push(&mut out, KEY_FFMPEG_EXIT, &diag.sidecar_ffmpeg_exit);
    push(&mut out, KEY_SSRF_REJECT, &diag.sidecar_ssrf_reject);
    push(&mut out, KEY_MOQ_ERROR, &diag.sidecar_moq_error);
    push(&mut out, KEY_HEALTH, &diag.sidecar_health);
    push(&mut out, KEY_LOG_TAIL, &diag.sidecar_log_tail);
    out
}

fn merge_owned(dest: &mut Map<String, Value>, extra: Vec<(String, String)>) {
    for (key, value) in extra {
        dest.entry(key).or_insert_with(|| Value::String(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_skips_existing_keys() {
        let mut dest = Map::new();
        dest.insert(KEY_HEALTH.into(), Value::String("from-panel".into()));
        merge_owned(
            &mut dest,
            vec![
                (KEY_HEALTH.into(), "from-sidecar".into()),
                (KEY_FFMPEG_EXIT.into(), "exit role=video".into()),
            ],
        );
        assert_eq!(
            dest.get(KEY_HEALTH).and_then(Value::as_str),
            Some("from-panel")
        );
        assert_eq!(
            dest.get(KEY_FFMPEG_EXIT).and_then(Value::as_str),
            Some("exit role=video")
        );
    }

    #[test]
    fn pairs_skip_empty_fields() {
        let diag = DiagnosticsResponse {
            sidecar_ffmpeg_exit: Some("exit role=audio code=1 signal=none".into()),
            sidecar_ssrf_reject: Some("".into()),
            sidecar_moq_error: None,
            sidecar_health: Some("ok sources=0".into()),
            sidecar_log_tail: None,
        };
        let pairs = pairs_from_diagnostics(&diag);
        let keys: Vec<_> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, [KEY_FFMPEG_EXIT, KEY_HEALTH]);
    }
}
