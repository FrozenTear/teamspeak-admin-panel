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
//! This module gives the voice subsystem its own multi-thread runtime.
//! Every bot actor — its connected loop, the audio sibling, the pipeline
//! worker, the seek/resolve tasks, and the `tsclientlib` connection task
//! it spawns internally — runs here, isolated from web/DB load. The
//! `BotCommand` mpsc and `BotEvent` broadcast channels cross the runtime
//! boundary unchanged (tokio channels are runtime-agnostic), and a
//! `JoinHandle` produced here is still awaitable from the main runtime.

use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};
use tracing::info;

/// Worker threads for the dedicated voice runtime.
///
/// Two is deliberately small. The per-bot hot tasks are the connected
/// loop and the `tsclientlib` connection task (incoming-packet decode);
/// those two want to run concurrently, and two threads cover that. The
/// audio sibling spends ~all its time parked in `sleep_until`, the
/// pipeline worker is back-pressured idle once the frame buffer fills,
/// and the resolve task is one-shot — none of them need a reserved core.
/// Keeping the count low leaves the host's remaining cores to the main
/// app runtime instead of oversubscribing them.
const VOICE_WORKER_THREADS: usize = 2;

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
        Builder::new_multi_thread()
            .worker_threads(VOICE_WORKER_THREADS)
            .thread_name("voice-rt")
            .enable_all()
            .build()
            .expect("build dedicated voice runtime")
    })
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
}
