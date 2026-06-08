//! PURA-359 — persistent yt-dlp resolver service.
//!
//! `!play` of an `AudioSource::Url` used to spawn a fresh `yt-dlp`
//! subprocess for every track (`source/url.rs`). [PURA-355] measured ~2.0 s
//! of every resolution as pure *process startup* — importing yt-dlp's
//! extractor registry — entirely local CPU/disk, re-paid on each `!play`.
//!
//! This module replaces that per-play cost with a long-lived Python process
//! ([`yt_resolver.py`], embedded via `include_str!`) that imports `yt_dlp`
//! **once** at boot and keeps the extractor registry warm. The manager
//! talks to it over a unix-domain socket: one JSON request per connection,
//! one JSON response. The warm process returns the resolved `bestaudio`
//! direct URL, which [`build_source`](crate::pipeline) then hands straight
//! to `ffmpeg` — no yt-dlp on the `!play` critical path.
//!
//! **Failure posture.** Every error path — service down, mid-restart,
//! malformed reply, or a genuine resolution failure — degrades to the
//! proven `yt-dlp` subprocess in `build_source`. A broken resolver can slow
//! `!play` down but can never break it. The escape hatch `YT_RESOLVER_DISABLE`
//! pins playback to the subprocess path outright.
//!
//! **Supervision.** [`ResolverHandle::spawn`] launches a background task
//! that (re)spawns the Python process and restarts it on exit with a short
//! backoff. After repeated fast crashes it gives up and leaves the
//! subprocess fallback in effect rather than spin-looping. An image upgrade
//! restarts the whole manager, so the resolver re-imports the upgraded
//! yt-dlp zipapp on the next boot for free.
//!
//! [PURA-355]: https://teamspeak-heaven/PURA/issues/PURA-355

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;

/// The resolver script, embedded so it can never drift from the binary that
/// supervises it. Written to a temp file at [`ResolverHandle::spawn`].
const RESOLVER_SCRIPT: &str = include_str!("yt_resolver.py");

/// How long to wait for the unix socket to accept a connection. The service
/// is either up (connect is instant) or down (fall back immediately) — a
/// short timeout keeps a dead resolver from stalling `!play`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

/// Overall budget for a resolve round-trip.
///
/// THE-932: lowered from 40 s to 15 s. Each TCP socket inside yt-dlp is
/// already bounded by `socket_timeout=10` s, so a single network phase
/// cannot exceed 10 s. The total budget of 15 s covers the nsig-solve phase
/// (~1–2 s warm) plus the socket timeout with a small margin, while cutting
/// the worst-case failure-path delay from 40 s to 15 s before the subprocess
/// fallback fires.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);

/// A resolved track — the warm resolver's answer for one URL.
#[derive(Debug, Clone)]
pub struct ResolvedTrack {
    /// Direct, ffmpeg-consumable `bestaudio` media URL.
    pub direct_url: String,
    /// Track title, when the extractor reports one.
    pub title: Option<String>,
    /// Duration in seconds, when known.
    pub duration: Option<f64>,
    /// Per-phase timing from the Python resolver (THE-932). May be empty for
    /// older resolver versions or when timing is unavailable.
    pub phases: Vec<ResolvePhase>,
    /// YouTube video ID, when the resolver can identify it. Present for both
    /// direct watch URLs and search results after THE-932. The subprocess
    /// fallback uses this to resolve the direct URL rather than re-running the
    /// original search query.
    pub video_id: Option<String>,
}

/// Why a resolve attempt did not yield a [`ResolvedTrack`]. Every variant is
/// non-fatal: the caller falls back to the `yt-dlp` subprocess path.
#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    /// The service could not be reached (down, mid-restart, timed out).
    #[error("resolver service unavailable: {0}")]
    Unavailable(String),
    /// The service answered but yt-dlp could not resolve the URL.
    #[error("resolution failed: {0}")]
    Resolution(String),
    /// The service answered with something we could not parse.
    #[error("resolver protocol error: {0}")]
    Protocol(String),
}

/// One timing phase emitted by the Python resolver (THE-932).
#[derive(Debug, Clone, Deserialize)]
pub struct ResolvePhase {
    pub name: String,
    pub ms: u64,
}

/// Wire response shape — see the protocol docs in `yt_resolver.py`.
#[derive(Debug, Deserialize)]
struct WireResponse {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    direct_url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    yt_dlp_version: Option<String>,
    /// Per-phase timing from the Python resolver (THE-932).
    #[serde(default)]
    phases: Vec<ResolvePhase>,
    /// YouTube video ID — present when the resolver can identify it.
    /// Passed back to the caller so a subprocess fallback can resolve the
    /// direct watch URL instead of re-running a search query.
    #[serde(default)]
    video_id: Option<String>,
}

/// State shared between [`ResolverHandle`] and its background supervisor.
///
/// The `dead` flag is set by the supervisor right before it gives up (after
/// [`MAX_FAST_FAILS`] fast crashes). Once set, the handle's [`round_trip`]
/// short-circuits to [`ResolverError::Unavailable`] without paying the
/// [`CONNECT_TIMEOUT`] tax on every subsequent `!play`.
#[derive(Debug, Default)]
struct SupervisorState {
    dead: AtomicBool,
}

/// Handle to the supervised resolver process. Cheap to clone the reference;
/// a process-global instance is shared via [`shared`].
#[derive(Debug)]
pub struct ResolverHandle {
    socket_path: PathBuf,
    state: Arc<SupervisorState>,
}

impl ResolverHandle {
    /// Write the embedded script to a temp file and spawn the supervisor
    /// task that keeps the Python resolver process alive. Returns as soon as
    /// the supervisor is launched — the process warms up (`import yt_dlp`,
    /// ~2 s) in the background, so callers should [`warm_up`] at server boot
    /// well before the first `!play`.
    fn spawn() -> std::io::Result<Self> {
        let pid = std::process::id();
        let dir = std::env::temp_dir();
        let script_path = dir.join(format!("ts6-yt-resolver-{pid}.py"));
        let socket_path = dir.join(format!("ts6-yt-resolver-{pid}.sock"));
        std::fs::write(&script_path, RESOLVER_SCRIPT)?;
        let state = Arc::new(SupervisorState::default());
        tokio::spawn(supervise(script_path, socket_path.clone(), state.clone()));
        Ok(Self { socket_path, state })
    }

    /// Construct a handle bound to an externally-managed socket. Test-only:
    /// lets a unit test point the client at an in-process mock server.
    #[cfg(test)]
    fn for_socket(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            state: Arc::new(SupervisorState::default()),
        }
    }

    /// Construct a handle with an explicit supervisor state. Test-only seam
    /// for verifying that a `dead` flag short-circuits `resolve()` without
    /// touching the socket.
    #[cfg(test)]
    fn for_socket_with_state(socket_path: PathBuf, state: Arc<SupervisorState>) -> Self {
        Self { socket_path, state }
    }

    /// Resolve `url` to a direct `bestaudio` media URL via the warm process.
    ///
    /// `cookie_file` mirrors the subprocess path — the resolved Netscape
    /// `cookies.txt` (or `None`) for age-gated / rate-limited videos.
    pub async fn resolve(
        &self,
        url: &str,
        cookie_file: Option<&Path>,
    ) -> Result<ResolvedTrack, ResolverError> {
        let req = serde_json::json!({
            "op": "resolve",
            "url": url,
            "cookie_file": cookie_file.map(|p| p.to_string_lossy().into_owned()),
        });
        let resp = self.round_trip(&req).await?;
        if !resp.ok {
            return Err(ResolverError::Resolution(
                resp.error.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        let direct_url = resp
            .direct_url
            .ok_or_else(|| ResolverError::Protocol("ok response without direct_url".into()))?;
        Ok(ResolvedTrack {
            direct_url,
            title: resp.title,
            duration: resp.duration,
            phases: resp.phases,
            video_id: resp.video_id,
        })
    }

    /// Liveness probe — returns the resolver's `yt_dlp` version string.
    pub async fn ping(&self) -> Result<String, ResolverError> {
        let resp = self
            .round_trip(&serde_json::json!({ "op": "ping" }))
            .await?;
        if !resp.ok {
            return Err(ResolverError::Resolution(
                resp.error.unwrap_or_else(|| "ping failed".into()),
            ));
        }
        Ok(resp.yt_dlp_version.unwrap_or_else(|| "unknown".into()))
    }

    /// One request → one response over a fresh connection. The Python
    /// server reads a single newline-terminated JSON line then writes one
    /// reply and closes, so the protocol needs no length framing.
    async fn round_trip(&self, req: &serde_json::Value) -> Result<WireResponse, ResolverError> {
        // Supervisor gave up — no server is bound, so connecting would just
        // burn `CONNECT_TIMEOUT` per call. Fail fast straight to subprocess.
        if self.state.dead.load(Ordering::Acquire) {
            return Err(ResolverError::Unavailable(
                "supervisor gave up; subprocess fallback".into(),
            ));
        }

        let mut line =
            serde_json::to_vec(req).map_err(|e| ResolverError::Protocol(e.to_string()))?;
        line.push(b'\n');

        let mut stream =
            tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket_path))
                .await
                .map_err(|_| ResolverError::Unavailable("connect timed out".into()))?
                .map_err(|e| ResolverError::Unavailable(format!("connect: {e}")))?;

        let exchange = async {
            stream.write_all(&line).await?;
            // Half-close the write side so the server sees a clean EOF even
            // if a future protocol revision drops the newline delimiter.
            stream.shutdown().await?;
            let mut buf = Vec::with_capacity(4096);
            stream.read_to_end(&mut buf).await?;
            Ok::<_, std::io::Error>(buf)
        };
        let buf = tokio::time::timeout(RESOLVE_TIMEOUT, exchange)
            .await
            .map_err(|_| ResolverError::Unavailable("resolve timed out".into()))?
            .map_err(|e| ResolverError::Unavailable(format!("io: {e}")))?;

        serde_json::from_slice(&buf)
            .map_err(|e| ResolverError::Protocol(format!("undecodable reply: {e}")))
    }
}

/// Background supervisor — keeps the Python resolver process alive.
///
/// Restarts the process on exit with a short backoff. Counts crashes that
/// happen within [`FAST_FAIL_WINDOW`] of spawn; after [`MAX_FAST_FAILS`] of
/// them it gives up so a structurally-broken resolver (no `python3`, no
/// importable `yt_dlp`) cannot spin-loop — the subprocess fallback carries
/// playback in that case.
async fn supervise(script: PathBuf, socket: PathBuf, state: Arc<SupervisorState>) {
    /// Crashes faster than this count against the resolver's fast-fail tally.
    const FAST_FAIL_WINDOW: Duration = Duration::from_secs(5);
    /// Consecutive fast crashes tolerated before the supervisor gives up.
    const MAX_FAST_FAILS: u32 = 5;

    let mut fast_fails = 0u32;
    loop {
        // Clear any stale socket so the server's bind() succeeds.
        let _ = std::fs::remove_file(&socket);

        let started = Instant::now();
        let mut cmd = Command::new("python3");
        cmd.arg(&script)
            .arg(&socket)
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                fast_fails += 1;
                tracing::warn!(
                    error = %e,
                    "yt-resolver: python3 spawn failed — yt-dlp subprocess fallback in effect",
                );
                if fast_fails >= MAX_FAST_FAILS {
                    tracing::error!(
                        "yt-resolver: python3 unspawnable {MAX_FAST_FAILS}x — giving up; \
                         yt-dlp subprocess fallback stays in effect",
                    );
                    state.dead.store(true, Ordering::Release);
                    break;
                }
                tokio::time::sleep(FAST_FAIL_WINDOW).await;
                continue;
            }
        };

        // Forward the resolver's stderr (its readiness line, yt-dlp import
        // errors) into the manager's tracing output for operator visibility.
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(target: "yt_resolver", "{line}");
                }
            });
        }

        let status = child.wait().await;
        let ran = started.elapsed();
        tracing::warn!(
            ?status,
            ran_secs = ran.as_secs(),
            "yt-resolver process exited — restarting",
        );

        if ran < FAST_FAIL_WINDOW {
            fast_fails += 1;
            if fast_fails >= MAX_FAST_FAILS {
                tracing::error!(
                    "yt-resolver crashed {MAX_FAST_FAILS}x within {}s of spawn — giving up; \
                     yt-dlp subprocess fallback stays in effect",
                    FAST_FAIL_WINDOW.as_secs(),
                );
                state.dead.store(true, Ordering::Release);
                break;
            }
        } else {
            // It ran long enough to be useful; a later crash starts fresh.
            fast_fails = 0;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let _ = std::fs::remove_file(&socket);
}

/// Process-global resolver. `None` means the persistent service is off
/// (`YT_RESOLVER_DISABLE` set, or the script could not be written) and the
/// caller must use the `yt-dlp` subprocess path.
static RESOLVER: OnceLock<Option<ResolverHandle>> = OnceLock::new();

fn init() -> Option<ResolverHandle> {
    if std::env::var_os("YT_RESOLVER_DISABLE").is_some() {
        tracing::info!(
            "YT_RESOLVER_DISABLE set — persistent yt-dlp resolver disabled; \
             subprocess path in use",
        );
        return None;
    }
    match ResolverHandle::spawn() {
        Ok(handle) => {
            tracing::info!(
                socket = %handle.socket_path.display(),
                "persistent yt-dlp resolver service starting",
            );
            Some(handle)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "could not start persistent yt-dlp resolver — subprocess path in use",
            );
            None
        }
    }
}

/// The shared resolver handle, or `None` when the persistent service is off.
///
/// First call spawns the supervisor; [`warm_up`] should be invoked at server
/// boot so the `import yt_dlp` cost is paid before the first `!play`.
pub fn shared() -> Option<&'static ResolverHandle> {
    RESOLVER.get_or_init(init).as_ref()
}

/// Start the resolver service early so it is warm by the first `!play`.
/// Idempotent — safe to call once at server boot.
pub fn warm_up() {
    let _ = shared();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// Spawn a one-shot mock resolver: bind `path`, accept one connection,
    /// read the request line, reply with `reply`, close.
    fn mock_server(path: PathBuf, reply: &'static str) {
        tokio::spawn(async move {
            let listener = UnixListener::bind(&path).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            // Read just the request line (client half-closes its write side).
            let mut byte = [0u8; 1];
            loop {
                match stream.read(&mut byte).await {
                    Ok(0) => break,
                    Ok(_) => {
                        req.push(byte[0]);
                        if byte[0] == b'\n' {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            stream.write_all(reply.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        });
    }

    fn sock(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ts6-yt-resolver-test-{}-{}.sock",
            std::process::id(),
            name
        ))
    }

    #[tokio::test]
    async fn resolve_parses_a_successful_reply() {
        let path = sock("ok");
        let _ = std::fs::remove_file(&path);
        mock_server(
            path.clone(),
            "{\"ok\":true,\"direct_url\":\"https://cdn/x.webm\",\"title\":\"Song\",\"duration\":210.5}\n",
        );
        // Give the listener a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let handle = ResolverHandle::for_socket(path.clone());
        let track = handle.resolve("https://youtu.be/x", None).await.unwrap();
        assert_eq!(track.direct_url, "https://cdn/x.webm");
        assert_eq!(track.title.as_deref(), Some("Song"));
        assert_eq!(track.duration, Some(210.5));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn resolve_surfaces_a_resolution_error() {
        let path = sock("err");
        let _ = std::fs::remove_file(&path);
        mock_server(path.clone(), "{\"ok\":false,\"error\":\"Private video\"}\n");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let handle = ResolverHandle::for_socket(path.clone());
        let err = handle
            .resolve("https://youtu.be/x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolverError::Resolution(m) if m.contains("Private video")));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn missing_socket_is_unavailable_not_a_panic() {
        // No server bound — the client must report Unavailable so the
        // caller falls back to the yt-dlp subprocess.
        let handle = ResolverHandle::for_socket(sock("absent"));
        let err = handle
            .resolve("https://youtu.be/x", None)
            .await
            .unwrap_err();
        assert!(matches!(err, ResolverError::Unavailable(_)), "got: {err:?}");
    }

    #[tokio::test]
    async fn dead_supervisor_short_circuits_without_connecting() {
        // After the supervisor gives up, the handle still exists in
        // RESOLVER. The dead flag must short-circuit `resolve` synchronously
        // instead of paying CONNECT_TIMEOUT on every !play.
        let state = Arc::new(SupervisorState::default());
        state.dead.store(true, Ordering::Release);
        // Point at a path no listener will ever bind to — proves we never
        // actually attempt a connect.
        let handle = ResolverHandle::for_socket_with_state(sock("dead"), state);

        let start = Instant::now();
        let err = handle
            .resolve("https://youtu.be/x", None)
            .await
            .unwrap_err();
        let elapsed = start.elapsed();

        assert!(matches!(err, ResolverError::Unavailable(_)), "got: {err:?}");
        // CONNECT_TIMEOUT is 2 s; the short-circuit should fire instantly.
        assert!(
            elapsed < Duration::from_millis(50),
            "dead-flag short-circuit must not block on connect: took {elapsed:?}",
        );
    }
}
