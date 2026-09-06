//! Voice seat bag for `POST /api/bug-reports` `context`.
//!
//! Builds a camelCase `serde_json::Map` that matches the API contract in
//! `ts6_manager_shared::bug_reports` (PR #28): string values preferred,
//! keys ≤ 64 chars, each value ≤ 4 KiB. This crate does **not** own the
//! HTTP route or the GitHub Issues sink — callers (the API handler, or a
//! panic / error path that already posts to that sink) merge the map in.
//!
//! Music Bot owns `musicBotLatency` + `logTail` on the same object. Voice
//! uses seat-scoped keys so the two seats do not clobber each other.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use serde_json::{Map, Value};

/// Last-N in-process Voice / wire lines. Sized so a full tail stays under
/// the API's 4 KiB per-value cap (`MAX_CONTEXT_VALUE_LEN`).
const LOG_TAIL_CAP: usize = 24;
const LOG_LINE_MAX: usize = 160;

/// camelCase keys written into `CreateBugReportRequest.context`.
pub const KEY_FIRST_FRAME_ON_WIRE_MS: &str = "firstFrameOnWireMs";
pub const KEY_SEND_AUDIO_ERROR: &str = "sendAudioError";
pub const KEY_ENCODE_ERROR: &str = "encodeError";
pub const KEY_HANDSHAKE_DROPPED: &str = "handshakeDropped";
pub const KEY_CONNECTED_LOOP_STALL: &str = "connectedLoopStall";
pub const KEY_FRAME_UNDERRUN: &str = "frameUnderrun";
pub const KEY_VOICE_STATE: &str = "voiceState";
pub const KEY_VOICE_LOG_TAIL: &str = "voiceLogTail";

#[derive(Default)]
struct VoiceBugSnapshot {
    first_frame_on_wire_ms: Option<u64>,
    last_send_audio_error: Option<String>,
    last_encode_error: Option<String>,
    last_handshake_drop: Option<String>,
    last_loop_stall: Option<String>,
    last_frame_underrun: Option<String>,
    log_tail: VecDeque<String>,
}

impl VoiceBugSnapshot {
    fn push_line(&mut self, line: impl Into<String>) {
        let mut line = line.into();
        if line.chars().count() > LOG_LINE_MAX {
            line = line.chars().take(LOG_LINE_MAX).collect();
        }
        if self.log_tail.len() >= LOG_TAIL_CAP {
            self.log_tail.pop_front();
        }
        self.log_tail.push_back(line);
    }

    fn voice_state_summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(ms) = self.first_frame_on_wire_ms {
            parts.push(format!("firstFrameOnWireMs={ms}"));
        }
        if self.last_send_audio_error.is_some() {
            parts.push("sendAudioError=yes".into());
        }
        if self.last_encode_error.is_some() {
            parts.push("encodeError=yes".into());
        }
        if self.last_handshake_drop.is_some() {
            parts.push("handshakeDropped=yes".into());
        }
        if self.last_loop_stall.is_some() {
            parts.push("connectedLoopStall=yes".into());
        }
        if self.last_frame_underrun.is_some() {
            parts.push("frameUnderrun=yes".into());
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("; "))
        }
    }
}

fn snapshot() -> &'static Mutex<VoiceBugSnapshot> {
    static SNAPSHOT: OnceLock<Mutex<VoiceBugSnapshot>> = OnceLock::new();
    SNAPSHOT.get_or_init(|| Mutex::new(VoiceBugSnapshot::default()))
}

fn lock_snapshot() -> std::sync::MutexGuard<'static, VoiceBugSnapshot> {
    snapshot()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Snapshot the current Voice wire marks + short in-process log tail as
/// a `context` map ready to merge into `CreateBugReportRequest.context`.
///
/// Empty when nothing has been recorded this process (fresh boot, no
/// bot has touched the audible path yet).
pub fn bug_report_context() -> Map<String, Value> {
    let snap = lock_snapshot();
    let mut out = Map::new();

    if let Some(ms) = snap.first_frame_on_wire_ms {
        out.insert(
            KEY_FIRST_FRAME_ON_WIRE_MS.into(),
            Value::String(ms.to_string()),
        );
    }
    insert_opt(
        &mut out,
        KEY_SEND_AUDIO_ERROR,
        snap.last_send_audio_error.as_deref(),
    );
    insert_opt(
        &mut out,
        KEY_ENCODE_ERROR,
        snap.last_encode_error.as_deref(),
    );
    insert_opt(
        &mut out,
        KEY_HANDSHAKE_DROPPED,
        snap.last_handshake_drop.as_deref(),
    );
    insert_opt(
        &mut out,
        KEY_CONNECTED_LOOP_STALL,
        snap.last_loop_stall.as_deref(),
    );
    insert_opt(
        &mut out,
        KEY_FRAME_UNDERRUN,
        snap.last_frame_underrun.as_deref(),
    );
    insert_opt(
        &mut out,
        KEY_VOICE_STATE,
        snap.voice_state_summary().as_deref(),
    );

    if !snap.log_tail.is_empty() {
        out.insert(
            KEY_VOICE_LOG_TAIL.into(),
            Value::String(snap.log_tail.iter().cloned().collect::<Vec<_>>().join("\n")),
        );
    }
    out
}

/// Merge Voice keys into an existing `context` map **without** overwriting
/// keys already set by Panel or Music Bot (`musicBotLatency`, `logTail`, …).
pub fn merge_voice_bug_context(dest: &mut Map<String, Value>) {
    for (key, value) in bug_report_context() {
        dest.entry(key).or_insert(value);
    }
}

fn insert_opt(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|s| !s.is_empty()) {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// First Opus frame for the current play just hit `Connection::send_audio`.
pub(crate) fn record_first_frame_on_wire(elapsed_ms: u64) {
    let mut snap = lock_snapshot();
    snap.first_frame_on_wire_ms = Some(elapsed_ms);
    snap.push_line(format!("first_frame_on_wire elapsed_ms={elapsed_ms}"));
}

/// `Connection::send_audio` returned an error on the wire path.
pub(crate) fn record_send_audio_error(error: impl ToString) {
    let error = truncate_mark(error.to_string());
    let mut snap = lock_snapshot();
    snap.push_line(format!("send_audio_error {error}"));
    snap.last_send_audio_error = Some(error);
}

/// Consumer-side Opus encode skipped a frame.
pub(crate) fn record_encode_error(error: impl ToString) {
    let error = truncate_mark(error.to_string());
    let mut snap = lock_snapshot();
    snap.push_line(format!("encode_error {error}"));
    snap.last_encode_error = Some(error);
}

/// Handshake failed, timed out, or the connected loop dropped.
pub(crate) fn record_handshake_drop(reason: impl ToString) {
    let reason = truncate_mark(reason.to_string());
    let mut snap = lock_snapshot();
    snap.push_line(format!("handshake_dropped {reason}"));
    snap.last_handshake_drop = Some(reason);
}

/// Connected-loop select-arm body outran the 20 ms cadence.
pub(crate) fn record_connected_loop_stall(arm: &str, elapsed_ms: u64, detail: &str) {
    let summary = truncate_mark(format!("arm={arm} elapsed_ms={elapsed_ms} {detail}"));
    let mut snap = lock_snapshot();
    snap.push_line(format!("connected_loop_stall {summary}"));
    snap.last_loop_stall = Some(summary);
}

/// Frame-buffer underrun (audible crackle) on the send path.
pub(crate) fn record_frame_underrun(
    regime: &str,
    frame_index: u64,
    lateness_ms: u64,
    buffered_frames: usize,
) {
    let summary = truncate_mark(format!(
        "regime={regime} frame={frame_index} lateness_ms={lateness_ms} buffered={buffered_frames}"
    ));
    let mut snap = lock_snapshot();
    snap.push_line(format!("frame_underrun {summary}"));
    snap.last_frame_underrun = Some(summary);
}

/// Bot actor task panicked (supervisor join path).
pub(crate) fn record_actor_panic(detail: impl ToString) {
    let detail = truncate_mark(detail.to_string());
    lock_snapshot().push_line(format!("actor_panic {detail}"));
}

fn truncate_mark(s: String) -> String {
    if s.chars().count() <= LOG_LINE_MAX {
        s
    } else {
        s.chars().take(LOG_LINE_MAX).collect()
    }
}

static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Hold this across a test that reads or writes the process-wide snapshot
/// so parallel `cargo test` threads do not clobber each other.
#[doc(hidden)]
pub fn acquire_test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Reset the process-wide snapshot. Tests only — the production path
/// never clears marks (a report should see the last-known bag).
#[doc(hidden)]
pub fn reset_for_tests() {
    *lock_snapshot() = VoiceBugSnapshot::default();
}

/// Seed a representative Voice bag so API / DTO tests can assert the
/// camelCase keys survive `CreateBugReportRequest` validate + Issue body.
#[doc(hidden)]
pub fn seed_for_tests() {
    reset_for_tests();
    record_first_frame_on_wire(1842);
    record_send_audio_error("connection reset");
    record_handshake_drop("handshake did not complete within 30s");
    record_connected_loop_stall("audio", 14, "audio_msg=frame");
    record_frame_underrun("midsong", 900, 18, 248);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn isolated<F: FnOnce()>(f: F) {
        let _guard = acquire_test_lock();
        reset_for_tests();
        f();
        reset_for_tests();
    }

    #[test]
    fn empty_snapshot_is_an_empty_map() {
        isolated(|| {
            assert!(bug_report_context().is_empty());
        });
    }

    #[test]
    fn records_wire_marks_as_camel_case_string_values() {
        isolated(|| {
            record_first_frame_on_wire(1842);
            record_send_audio_error("connection reset");
            record_encode_error("opus: buffer too small");
            record_handshake_drop("handshake did not complete within 30s");
            record_connected_loop_stall("audio", 14, "audio_msg=frame");
            record_frame_underrun("midsong", 900, 18, 248);

            let ctx = bug_report_context();
            assert_eq!(
                ctx.get(KEY_FIRST_FRAME_ON_WIRE_MS).and_then(Value::as_str),
                Some("1842")
            );
            assert_eq!(
                ctx.get(KEY_SEND_AUDIO_ERROR).and_then(Value::as_str),
                Some("connection reset")
            );
            assert_eq!(
                ctx.get(KEY_ENCODE_ERROR).and_then(Value::as_str),
                Some("opus: buffer too small")
            );
            assert!(
                ctx.get(KEY_HANDSHAKE_DROPPED)
                    .and_then(Value::as_str)
                    .unwrap()
                    .contains("30s")
            );
            assert_eq!(
                ctx.get(KEY_CONNECTED_LOOP_STALL).and_then(Value::as_str),
                Some("arm=audio elapsed_ms=14 audio_msg=frame")
            );
            assert!(
                ctx.get(KEY_FRAME_UNDERRUN)
                    .and_then(Value::as_str)
                    .unwrap()
                    .contains("regime=midsong")
            );
            let state = ctx.get(KEY_VOICE_STATE).and_then(Value::as_str).unwrap();
            assert!(state.contains("firstFrameOnWireMs=1842"));
            assert!(state.contains("handshakeDropped=yes"));
            let tail = ctx.get(KEY_VOICE_LOG_TAIL).and_then(Value::as_str).unwrap();
            assert!(tail.contains("first_frame_on_wire elapsed_ms=1842"));
            assert!(tail.contains("frame_underrun"));
            // Seat-scoped tail — Music Bot owns the generic `logTail` key.
            assert!(!ctx.contains_key("logTail"));
            assert!(!ctx.contains_key("musicBotLatency"));
        });
    }

    #[test]
    fn log_tail_keeps_last_n_lines() {
        isolated(|| {
            for i in 0..(LOG_TAIL_CAP + 5) {
                record_first_frame_on_wire(i as u64);
            }
            let tail = bug_report_context().remove(KEY_VOICE_LOG_TAIL).unwrap();
            let lines: Vec<_> = tail.as_str().unwrap().lines().collect();
            assert_eq!(lines.len(), LOG_TAIL_CAP);
            assert!(lines[0].contains("elapsed_ms=5"));
            assert!(lines.last().unwrap().contains("elapsed_ms=28"));
        });
    }

    #[test]
    fn merge_does_not_overwrite_existing_keys() {
        isolated(|| {
            record_first_frame_on_wire(99);
            let mut dest = Map::from_iter([
                (
                    KEY_FIRST_FRAME_ON_WIRE_MS.into(),
                    Value::String("panel".into()),
                ),
                (
                    "musicBotLatency".into(),
                    Value::String("resolve=20s".into()),
                ),
                ("logTail".into(), Value::String("music-bot lines".into())),
            ]);
            merge_voice_bug_context(&mut dest);
            assert_eq!(
                dest.get(KEY_FIRST_FRAME_ON_WIRE_MS).and_then(Value::as_str),
                Some("panel"),
                "Panel / Music Bot values win on collision"
            );
            assert_eq!(
                dest.get("musicBotLatency").and_then(Value::as_str),
                Some("resolve=20s")
            );
            assert_eq!(
                dest.get("logTail").and_then(Value::as_str),
                Some("music-bot lines")
            );
            assert!(dest.contains_key(KEY_VOICE_LOG_TAIL));
            assert!(dest.contains_key(KEY_VOICE_STATE));
        });
    }
}
