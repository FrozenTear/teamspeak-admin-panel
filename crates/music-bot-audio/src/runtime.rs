//! Decode-side runtime — packing B.
//!
//! `voice-rt` (the voice crate) pins **only** the 20 ms wire-send path
//! to `TS6_BOT_SEND_CPUSET` (0-1). Pipeline workers, ICY fetch, the
//! yt-dlp byte bridge, `yt-dlp -g` resolve, and the warm-resolver
//! supervisor run here instead, pinned to `TS6_BOT_DECODE_CPUSET`
//! (apply-ready packing B: `2-5`, share Axum, never send `0-1`).
//!
//! Music container HostConfig stays unset (or wide `0-5`). Never
//! HostConfig `0-1` — that is packing C and traps ffmpeg on the send
//! cores. Fullstack soft pin stays `2-5` / nice `-5`; this runtime
//! does not shrink it and does not apply `TS6_BOT_NICE` (that nice is
//! the send path).
//!
//! Unset / empty `TS6_BOT_DECODE_CPUSET` leaves these threads unpinned
//! (same no-op as the cpuset helpers).

use std::future::Future;
use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};
use tokio::task::JoinHandle;

/// Pipeline + fetch + bridge + resolve, with a spare for a stderr
/// reader. They share decode cores `2-5`; they must not occupy a
/// send-runtime worker.
const DECODE_WORKER_THREADS: usize = 4;

static DECODE_RT: OnceLock<Runtime> = OnceLock::new();

/// Process-wide decode runtime, built lazily on first use.
pub fn decode_runtime() -> &'static Runtime {
    DECODE_RT.get_or_init(|| {
        tracing::info!(
            worker_threads = DECODE_WORKER_THREADS,
            "starting decode runtime; pipeline/fetch/bridge/resolve pinned \
             off the voice send cores",
        );
        Builder::new_multi_thread()
            .worker_threads(DECODE_WORKER_THREADS)
            .thread_name("decode-rt")
            .on_thread_start(|| {
                // Packing B. No send pin, no TS6_BOT_NICE — those belong
                // to voice-rt only.
                crate::cpuset::pin_current_thread_decode();
            })
            .enable_all()
            .build()
            .expect("build decode runtime")
    })
}

/// Spawn `future` on the decode runtime.
///
/// The returned [`JoinHandle`] is awaitable from any runtime (the voice
/// send runtime, the music control-plane runtime, tests).
pub fn spawn_decode<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    decode_runtime().spawn(future)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn decode_runtime_spawns_and_joins_across_runtimes() {
        let join = spawn_decode(async { 7 * 6 });
        assert_eq!(join.await.expect("decode task join"), 42);
    }

    #[test]
    fn decode_runtime_is_a_singleton() {
        let a = decode_runtime() as *const Runtime;
        let b = decode_runtime() as *const Runtime;
        assert_eq!(a, b, "decode_runtime() must return the one shared runtime");
    }
}
