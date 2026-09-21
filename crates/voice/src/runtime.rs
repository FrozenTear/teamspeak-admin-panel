//! Dedicated voice runtime — PURA-367.
//!
//! Each bot actor drives its connected loop on a hard 20 ms real-time
//! audio cadence: every frame must reach `Connection::send_audio` within
//! its paced slot or the wire gaps and the listener hears a crackle.
//!
//! Until now every bot actor was `tokio::spawn`ed onto the process-wide
//! runtime it shared with the web server, the SurrealDB query path, and
//! dx-server rendering. tokio's scheduler is cooperative: once a worker
//! thread enters a long poll (a DB round-trip, a request handler), the
//! connected-loop task waits behind it in the shared run queue. Nothing
//! drains the audio channel until the loop task is scheduled again.
//!
//! contabo-dev v1.5.3 measurement (PURA-367): ~110 mid-song
//! `frame_underrun` events over 30 min, every one with `buffered_frames`
//! full (247–249) — the producer was fine, the *consumer* (the connected
//! loop) simply was not on a CPU. Only 5 of them coincided with a logged
//! `connected_loop_stall`; the rest had no slow arm body at all, i.e. the
//! loop task was descheduled, not busy.
//!
//! This module gives the wire-send path its own multi-thread runtime.
//! The bot actor, its connected loop (the task that calls
//! `Connection::send_audio`), the audio sibling, and the `tsclientlib`
//! connection task run here, isolated from web/DB load **and** from
//! decode work. Pipeline workers, ICY fetch, the yt-dlp bridge, and
//! resolve tasks are spawned on `music_bot_audio`'s `decode-rt`
//! (`TS6_BOT_DECODE_CPUSET`, packing B `2-5`) so they cannot starve
//! the 20 ms send loop on cores 0-1. The non-split connected loop stays
//! on this runtime because that task *is* the send path (split wire is
//! still opt-in). The `BotCommand` mpsc and `BotEvent` broadcast
//! channels cross the runtime boundary unchanged (tokio channels are
//! runtime-agnostic), and a `JoinHandle` produced here is still
//! awaitable from the main runtime.

use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};
use tracing::info;

/// Worker threads for the dedicated voice runtime.
///
/// Four covers the per-bot send-path tasks with margin: the connected
/// loop, the tsclientlib connection task (incoming-packet decode), and
/// the audio sibling (parked in `sleep_until`, wakes every 20 ms). The
/// pipeline worker lives on `decode-rt`, not here. The extra headroom
/// matters because `con.send_audio` is a synchronous call (packet
/// framing + encryption + UDP write) that can hold a worker thread for
/// 12–130 ms; with fewer workers the sibling task can stall waiting for
/// a free thread while the connected loop is occupied, producing the
/// same frame-underrun pattern the dedicated runtime was meant to cure.
const VOICE_WORKER_THREADS: usize = 4;

/// Process-wide dedicated voice runtime, built lazily on first use. Held
/// in a `static` so it is never dropped from an async context (dropping a
/// `Runtime` inside a runtime panics) — it lives for the process.
static VOICE_RT: OnceLock<Runtime> = OnceLock::new();

/// Handle to the dedicated voice runtime, building it on first call.
///
/// Safe to call from sync or async context: constructing a `Runtime`
/// neither requires nor forbids an ambient runtime.
pub(crate) fn voice_runtime() -> &'static Runtime {
    VOICE_RT.get_or_init(|| {
        info!(
            worker_threads = VOICE_WORKER_THREADS,
            "PURA-367 — starting dedicated voice runtime; isolates the 20 ms \
             audio-frame cadence from web/DB scheduler latency",
        );
        let rt = Builder::new_multi_thread()
            .worker_threads(VOICE_WORKER_THREADS)
            .thread_name(music_bot_audio::cpuset::VOICE_RT_THREAD_COMM)
            .on_thread_start(|| {
                // Contabo bot unit: pin + nice ONLY this send runtime
                // (`voice-rt` → SEND 0-1). decode-rt
                // (pipeline/fetch/bridge/resolve) pins itself to
                // TS6_BOT_DECODE_CPUSET=2-5 (packing B; share Axum).
                // Decode children inherit that set via
                // install_decode_pre_exec; pin_decode_child is only a
                // leader backup. Never HostConfig cpuset 0-1 (packing C).
                // Per-thread setpriority of TS6_BOT_NICE=-5 is often
                // EPERM as uid 10001. The walk below targets every
                // voice-rt tid; the host script does the same walk
                // with permission to set a negative nice. Do not shrink
                // the fullstack 2-5 pin.
                music_bot_audio::cpuset::pin_current_thread_send();
                music_bot_audio::cpuset::nice_current_thread_from_env(
                    music_bot_audio::cpuset::NICE_ENV,
                );
            })
            .enable_all()
            .build()
            .expect("build dedicated voice runtime");
        // Workers are already named voice-rt. Re-apply nice by comm so
        // a process that can setpriority hits every send tid, not only
        // whichever thread happened to run on_thread_start successfully.
        music_bot_audio::cpuset::nice_voice_rt_from_env();
        rt
    })
}

/// Start the voice runtime if it is not already running.
///
/// The music process calls this before it serves `/health`. Host
/// `apply-fullstack-soft-pin.sh` runs after health and renices tids
/// whose `comm` is `voice-rt`. The runtime is otherwise created on the
/// first bot spawn, which is after that script has already exited.
pub fn ensure_voice_runtime() {
    let _ = voice_runtime();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runtime builds, hosts a task, and the resulting `JoinHandle`
    /// is awaitable from a *different* runtime — the cross-runtime
    /// property `BotHandle::shutdown` relies on.
    #[tokio::test]
    async fn voice_runtime_spawns_and_joins_across_runtimes() {
        let join = voice_runtime().spawn(async { 21 * 2 });
        assert_eq!(join.await.expect("voice task join"), 42);
    }

    /// `voice_runtime()` is idempotent — the `OnceLock` hands back the
    /// same runtime instance on every call.
    #[test]
    fn voice_runtime_is_a_singleton() {
        let a = voice_runtime() as *const Runtime;
        let b = voice_runtime() as *const Runtime;
        assert_eq!(a, b, "voice_runtime() must return the one shared runtime");
    }

    /// Host renice selects tids by this comm. The workers must exist and
    /// be named as soon as the runtime is built — not on the first bot.
    #[cfg(target_os = "linux")]
    #[test]
    fn voice_runtime_workers_are_named_voice_rt() {
        ensure_voice_runtime();
        let mut tids = Vec::new();
        for _ in 0..50 {
            tids = music_bot_audio::cpuset::voice_rt_tids(std::process::id()).unwrap_or_default();
            if tids.len() >= VOICE_WORKER_THREADS {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            tids.len() >= VOICE_WORKER_THREADS,
            "voice-rt workers must be visible under /proc for host renice, got {tids:?}"
        );
    }
}
