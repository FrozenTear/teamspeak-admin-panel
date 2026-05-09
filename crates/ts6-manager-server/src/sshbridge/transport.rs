//! SSHBridge transport state machine — spec §11.3 / §11.5.
//!
//! Owns one [`SshChannel`] per session and serialises every ServerQuery
//! command on a single tokio task. Single-task ownership is the queue's
//! "single permit" (matches `WebQueryClient`'s single-socket invariant).
//!
//! ## Lifecycle
//!
//! 1. **Connect.** A `connect_factory` future yields an open
//!    [`SshChannel`] (post-SSH-handshake, with the shell channel
//!    allocated). The factory is what `russh_channel::connect` resolves
//!    to in production; tests pass a stub factory.
//! 2. **Banner detect** (spec §11.3). Read until `error id=0 msg=ok`.
//!    The intervening lines are the canonical TS6 banner (`TS3` /
//!    `Welcome` / optional `virtualserver_status`); we don't enforce
//!    their presence verbatim — many TS6 builds vary the order — but a
//!    non-zero terminator is a fatal protocol error.
//! 3. **Dispatch loop.** [`dispatch_loop`] selects between a command
//!    receiver (callers' submitted commands) and a keepalive timer. A
//!    single in-flight command at a time; bodies accumulate; the
//!    `error` frame resolves the caller's `oneshot`.
//! 4. **Reconnect** (spec §11.5). Connection-class transport failures
//!    return [`SessionResult::Reconnect`] from the dispatch loop;
//!    [`run_with_reconnect`] applies exponential backoff
//!    `min(initial * 2^attempts, max)` with ±25% jitter and re-invokes
//!    `connect_factory`. Jitter prevents fleet-wide synchronised
//!    reconnect storms after a shared upstream blip. Auth-rejected is
//!    fatal — no reconnect.
//!
//! ## Test seam
//!
//! `dispatch_loop` is generic over [`SshChannel`], so unit tests pass a
//! stub channel that records writes and emits scripted reads. Banner
//! detection, queue ordering, keepalive cadence, and the auth-rejected
//! short-circuit are all verifiable without a real SSH peer.

use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use tokio::time::Instant;

use super::audit::AuditEntry;
use super::channel::{looks_like_auth_failure, SshChannel, TransportError};
use super::wire::{ErrorFrame, Frame, LineBuffer, NotifyFrame};
use super::{SshBridgeError, SshBridgeResult};
use crate::db::Database;

/// Tunable timings. Defaults reflect the PURA-76 acceptance criteria.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub config_id: i64,
    /// Default per-command deadline. Spec §11.4 — 10 s default.
    pub command_timeout: Duration,
    /// Banner deadline. Spec §11.3 — banner usually arrives in
    /// milliseconds; pad generously for high-latency networks.
    pub banner_timeout: Duration,
    /// Idle window for banner-complete detection on lazy upstreams.
    /// Spec §11.3 says the banner ends with `error id=0 msg=ok`, but
    /// `teamspeak6-server:6.0.0-beta9` (libssh-backed) only emits the
    /// `TS3` / `Welcome` body lines and waits for a command before
    /// sending any `error` frame. After the first banner body line
    /// arrives, [`read_banner`] returns `Ok` if no further bytes
    /// arrive within this window — without it, the supervisor sits at
    /// the spec terminator until [`banner_timeout`] fires. PURA-101.
    pub banner_idle_window: Duration,
    /// Wall-clock ceiling on the connect future returned by
    /// `connect_factory`. Wraps the entire pre-banner sequence (TCP
    /// connect, SSH key exchange, password auth, channel open,
    /// `request_shell`). Without this a slow-loris peer or a wedged
    /// MITM can keep the supervisor parked indefinitely with no
    /// observable failure event. PURA-76 — 30 s default.
    pub connect_timeout: Duration,
    /// Keepalive cadence. PURA-76 — `whoami` every 30 s.
    pub keepalive_interval: Duration,
    /// Per-keepalive deadline. PURA-76 — 5 s.
    pub keepalive_timeout: Duration,
    /// Consecutive keepalive failures before forcing a reconnect.
    /// PURA-76 — 3.
    pub keepalive_failure_threshold: u32,
    /// Reconnect backoff: `min(backoff_initial * 2^attempts, backoff_max)`.
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
    /// Capacity for the broadcast channel that fans out `notify*`
    /// events to subscribers.
    pub notify_capacity: usize,
    /// Capacity for the command queue. The queue is the operator-
    /// observable "submission order" buffer — FIFO drain.
    pub command_queue_capacity: usize,
}

impl TransportConfig {
    pub fn for_connection(config_id: i64) -> Self {
        Self {
            config_id,
            command_timeout: Duration::from_secs(10),
            banner_timeout: Duration::from_secs(15),
            banner_idle_window: Duration::from_millis(500),
            connect_timeout: Duration::from_secs(30),
            keepalive_interval: Duration::from_secs(30),
            keepalive_timeout: Duration::from_secs(5),
            keepalive_failure_threshold: 3,
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            notify_capacity: 256,
            command_queue_capacity: 64,
        }
    }
}

/// Deterministic capped backoff base — `min(initial * 2^attempts, max)`.
/// Exposed as a free function so it is unit-testable independently of
/// the jitter applied by [`next_backoff`].
pub fn next_backoff_base(attempts: u32, cfg: &TransportConfig) -> Duration {
    let initial_ms = cfg.backoff_initial.as_millis() as u64;
    let max_ms = cfg.backoff_max.as_millis() as u64;
    let scale = 2u64.checked_pow(attempts).unwrap_or(u64::MAX);
    let scaled = initial_ms.saturating_mul(scale);
    Duration::from_millis(scaled.min(max_ms))
}

/// Reconnect backoff with ±25% jitter applied on top of the capped base.
/// Cap is taken on the deterministic base before jitter, so the returned
/// duration spans `[0.75 * base, 1.25 * base]` for a given `attempts`.
/// Without jitter, fleet-wide network blips would re-converge every
/// supervisor onto the same reconnect schedule and hammer the upstream
/// in lock-step.
pub fn next_backoff(attempts: u32, cfg: &TransportConfig) -> Duration {
    let base_ms = next_backoff_base(attempts, cfg).as_millis() as u64;
    let jitter: f64 = rand::thread_rng().gen_range(0.75..=1.25);
    Duration::from_millis((base_ms as f64 * jitter) as u64)
}

/// One command submitted to the dispatch loop.
struct CommandRequest {
    line: String,
    timeout: Option<Duration>,
    user_id: Option<i64>,
    virtual_server_id: Option<i64>,
    reply: oneshot::Sender<SshBridgeResult<CommandOutcome>>,
}

/// The result the dispatch loop sends back through the `oneshot` reply
/// channel.
#[derive(Debug, Clone)]
pub struct CommandOutcome {
    /// Body lines collected between the command and its `error` frame.
    /// Each entry is one CR-LF-terminated wire line, already
    /// CR-LF-stripped. Typed parsers run on this `Vec<String>`.
    pub body_lines: Vec<String>,
    /// The terminator. `error.id == 0` is success; other ids are
    /// surfaced to the caller as [`SshBridgeError::Upstream`] before
    /// the outcome ever leaves [`execute_one`].
    pub error: ErrorFrame,
    /// Wall-clock duration from command-issue to terminator.
    pub latency: Duration,
}

/// Why the dispatch loop returned. The reconnect supervisor inspects
/// this to decide whether to re-invoke the connect factory.
#[derive(Debug)]
pub enum SessionResult {
    /// Caller dropped the [`TransportHandle`] — graceful shutdown.
    ShuttingDown,
    /// Transport failure, not auth-related — reconnect with backoff.
    Reconnect,
    /// Auth was rejected. Fatal; the supervisor stops without
    /// reconnecting and surfaces [`SshBridgeError::AuthRejected`] to
    /// every queued command.
    AuthRejected,
}

/// Cheap-to-clone handle the rest of the server uses to talk to the
/// SSH bridge. Submitting drops a [`CommandRequest`] onto the dispatch
/// task's mpsc; the task consumes them in submission order.
#[derive(Clone)]
pub struct TransportHandle {
    config_id: i64,
    cmd_tx: mpsc::Sender<CommandRequest>,
    notify_tx: broadcast::Sender<NotifyFrame>,
    /// PURA-80 — broadcast tick fired by [`run_with_reconnect`] each
    /// time a fresh session reaches `error id=0 msg=ok` on the banner.
    /// Subscribers re-issue any session-scoped state the upstream
    /// resets on reconnect (notify subscriptions, sid selection, …).
    /// The first banner-ok after `spawn` also fires this; subscribers
    /// that called `subscribe_session_up()` BEFORE `spawn` returned
    /// receive the bootstrap signal (see `spawn_inner` ordering).
    session_up_tx: broadcast::Sender<()>,
    /// Set once when the dispatch supervisor terminates fatally
    /// (`SessionResult::AuthRejected`). Subsequent submissions
    /// short-circuit to `SshBridgeError::AuthRejected` rather than
    /// blocking on a dead mpsc.
    auth_rejected: Arc<Mutex<bool>>,
    /// Set once when the host-key verifier rejected the server-presented
    /// key. Parallel to [`auth_rejected`](TransportHandle::auth_rejected) —
    /// fail-closed and short-circuit fresh submissions so the operator
    /// sees a typed `HostKeyMismatch` envelope instead of a generic
    /// transport-class error after the supervisor exits.
    host_key_mismatch: Arc<Mutex<bool>>,
}

impl TransportHandle {
    pub fn config_id(&self) -> i64 {
        self.config_id
    }

    /// Subscribe to `notify*` events. Each subscriber gets its own
    /// receiver; a slow subscriber lags but never blocks the dispatch
    /// loop (broadcast channels drop the oldest messages).
    pub fn subscribe_notify(&self) -> broadcast::Receiver<NotifyFrame> {
        self.notify_tx.subscribe()
    }

    /// Subscribe to session-up signals. PURA-80 uses this to re-issue
    /// `servernotifyregister` after every reconnect — the upstream
    /// drops the notify subscription when the SSH session ends.
    pub fn subscribe_session_up(&self) -> broadcast::Receiver<()> {
        self.session_up_tx.subscribe()
    }

    /// Submit a wire ServerQuery line to the bridge. Returns the
    /// terminator + body lines; non-zero `error id` is folded into
    /// [`SshBridgeError::Upstream`] before this returns.
    pub async fn execute(
        &self,
        line: impl Into<String>,
        user_id: Option<i64>,
        virtual_server_id: Option<i64>,
    ) -> SshBridgeResult<CommandOutcome> {
        // Host-key check first — re-keying is a security signal the
        // operator must clear by editing the row, so we short-circuit
        // even ahead of auth-rejected. Both flags are sticky once set;
        // order between them is observably the same to callers.
        if *self.host_key_mismatch.lock().await {
            return Err(SshBridgeError::HostKeyMismatch {
                config_id: self.config_id,
            });
        }
        if *self.auth_rejected.lock().await {
            return Err(SshBridgeError::AuthRejected {
                config_id: self.config_id,
            });
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        let req = CommandRequest {
            line: line.into(),
            timeout: None,
            user_id,
            virtual_server_id,
            reply: reply_tx,
        };
        if self.cmd_tx.send(req).await.is_err() {
            return Err(SshBridgeError::Transport(
                "ssh transport task is no longer running".into(),
            ));
        }
        match reply_rx.await {
            Ok(outcome) => outcome,
            Err(_) => Err(SshBridgeError::Transport(
                "ssh transport reply channel was dropped".into(),
            )),
        }
    }
}

/// Per-session shared state — held by the dispatch loop, surfaced
/// through [`TransportHandle`].
///
/// `db` carries the audit-log persistence handle (PURA-79 BLOCKER fix). When
/// `Some`, [`dispatch_loop`] schedules a fire-and-forget
/// [`AuditEntry::persist`] task after each `.emit()` so operator commands
/// land in `ssh_audit_log` rather than only in the `tracing::info!` stream.
/// When `None`, the loop only emits — the test seam exercises this path
/// without standing up a migrated database.
struct DispatchContext {
    cfg: TransportConfig,
    notify_tx: broadcast::Sender<NotifyFrame>,
    db: Option<Arc<Database>>,
}

/// Schedule a best-effort DB write for `entry` if `db` is wired.
///
/// PURA-79 BLOCKER: detaches the persist call from the dispatch loop so
/// `cmd.reply.send(…)` never blocks on DB latency, and so the issue's
/// hard rule — "a DB-write failure MUST NOT cancel the in-flight operator
/// command" — is satisfied two ways: `persist` already swallows errors
/// internally, *and* spawning detaches the call from the reply path.
fn fire_and_forget_persist(db: Option<&Arc<Database>>, entry: &AuditEntry) {
    if let Some(db) = db {
        let db = db.clone();
        let entry = entry.clone();
        tokio::spawn(async move { entry.persist(&db).await });
    }
}

/// Read until either the banner terminator `error id=0 msg=ok` arrives
/// (spec §11.3, canonical TS3 form) or the upstream goes idle for
/// [`TransportConfig::banner_idle_window`] after at least one body
/// line (PURA-101 — `teamspeak6-server:6.0.0-beta9` emits the
/// `TS3` / `Welcome` body lines and stops, never sending the spec
/// terminator until a command lands). Surfaces `error id != 0` as
/// [`SshBridgeError::Upstream`] and any transport failure as
/// [`SshBridgeError::Transport`] (mapping auth-related ones to
/// [`SshBridgeError::AuthRejected`]).
pub(crate) async fn read_banner<C: SshChannel>(
    channel: &mut C,
    parser: &mut LineBuffer,
    cfg: &TransportConfig,
) -> SshBridgeResult<Vec<String>> {
    let banner_deadline = Instant::now() + cfg.banner_timeout;
    let mut banner_lines: Vec<String> = Vec::new();
    let mut last_byte_at: Option<Instant> = None;
    loop {
        // While we haven't seen any banner body lines, hold the strict
        // banner_timeout deadline — a server that's totally silent is
        // still a fail. Once at least one body line has arrived, accept
        // a banner_idle_window of silence as "lazy banner complete".
        let next_deadline = if banner_lines.is_empty() {
            banner_deadline
        } else if let Some(t) = last_byte_at {
            banner_deadline.min(t + cfg.banner_idle_window)
        } else {
            banner_deadline
        };
        let chunk = match tokio::time::timeout_at(next_deadline, channel.recv()).await {
            Err(_) => {
                if !banner_lines.is_empty() && Instant::now() < banner_deadline {
                    tracing::debug!(
                        target: "sshbridge::transport",
                        config_id = cfg.config_id,
                        idle_window_ms = cfg.banner_idle_window.as_millis() as u64,
                        body_lines = banner_lines.len(),
                        "ssh banner idle-window settled (lazy banner — no `error id=0` terminator from upstream)"
                    );
                    return Ok(banner_lines);
                }
                return Err(SshBridgeError::Transport(format!(
                    "banner timeout after {:?}",
                    cfg.banner_timeout
                )));
            }
            Ok(Ok(Some(bytes))) => {
                last_byte_at = Some(Instant::now());
                bytes
            }
            Ok(Ok(None)) => {
                return Err(SshBridgeError::Transport(
                    "channel closed before banner terminator".into(),
                ));
            }
            Ok(Err(TransportError::AuthRejected)) => {
                return Err(SshBridgeError::AuthRejected {
                    config_id: cfg.config_id,
                });
            }
            Ok(Err(e)) => {
                if let TransportError::Closed(s) | TransportError::Io(s) = &e {
                    if looks_like_auth_failure(s) {
                        return Err(SshBridgeError::AuthRejected {
                            config_id: cfg.config_id,
                        });
                    }
                }
                return Err(SshBridgeError::Transport(e.to_string()));
            }
        };
        if let Err(e) = parser.push(&chunk) {
            // F1: line-buffer overflow surfaces as a transport-class
            // failure. The buffer cleared itself on rejection so the
            // reconnect that follows starts clean.
            return Err(SshBridgeError::Transport(e.to_string()));
        }
        for line in parser.drain_lines() {
            match Frame::classify(&line) {
                Frame::Notify(_) => {
                    // Notify lines before the banner terminator are
                    // unusual but legal; ignore them — there are no
                    // subscribers yet.
                }
                Frame::Body(body) => banner_lines.push(body),
                Frame::Error(error) => {
                    if error.id == 0 {
                        return Ok(banner_lines);
                    } else {
                        return Err(SshBridgeError::Upstream {
                            code: error.id,
                            message: error.msg,
                        });
                    }
                }
            }
        }
    }
}

/// Issue one command and read until its `error` terminator. `Ok` means
/// the channel handed us a terminator (caller maps `id != 0` into the
/// public [`SshBridgeError::Upstream`] variant via [`super::frame_to_result`]).
async fn execute_one<C: SshChannel>(
    channel: &mut C,
    parser: &mut LineBuffer,
    line: &str,
    timeout: Duration,
    notify_tx: &broadcast::Sender<NotifyFrame>,
) -> Result<CommandOutcome, TransportError> {
    let started = std::time::Instant::now();
    channel.write(line.as_bytes()).await?;
    channel.write(b"\r\n").await?;

    let deadline = Instant::now() + timeout;
    let mut body_lines = Vec::new();
    loop {
        let chunk = match tokio::time::timeout_at(deadline, channel.recv()).await {
            Err(_) => return Err(TransportError::Timeout),
            Ok(Ok(Some(bytes))) => bytes,
            Ok(Ok(None)) => {
                return Err(TransportError::Closed(
                    "channel closed mid-command".into(),
                ));
            }
            Ok(Err(e)) => return Err(e),
        };
        // F1: bound the in-flight line accumulator. A peer that streams
        // bytes without CR-LF would otherwise drive the bridge process
        // to OOM; on overflow `push` clears `pending` and returns
        // TransportError::Io so this command resolves as a transport
        // failure and the supervisor reconnects.
        parser.push(&chunk)?;
        for ln in parser.drain_lines() {
            match Frame::classify(&ln) {
                Frame::Notify(n) => {
                    let _ = notify_tx.send(n);
                }
                Frame::Body(s) => body_lines.push(s),
                Frame::Error(error) => {
                    return Ok(CommandOutcome {
                        body_lines,
                        error,
                        latency: started.elapsed(),
                    });
                }
            }
        }
    }
}

/// Drive one connected session — pull commands off `cmd_rx`, issue them
/// over `channel`, fan out notify events, and run the keepalive timer.
/// Returns the reason the session ended.
async fn dispatch_loop<C: SshChannel>(
    mut channel: C,
    mut cmd_rx: mpsc::Receiver<CommandRequest>,
    ctx: DispatchContext,
) -> SessionResult {
    let mut parser = LineBuffer::new();
    let mut keepalive = tokio::time::interval(ctx.cfg.keepalive_interval);
    // Skip the immediate first tick — `interval` fires at t=0; we
    // don't want a keepalive racing with caller commands at startup.
    keepalive.tick().await;
    let mut consecutive_keepalive_failures: u32 = 0;

    loop {
        tokio::select! {
            biased;

            // Caller commands always preferred over keepalives — keeps
            // submission-order draining intact and means a backed-up
            // queue defers keepalive ticks naturally.
            cmd = cmd_rx.recv() => {
                let Some(cmd) = cmd else { return SessionResult::ShuttingDown; };
                let timeout = cmd.timeout.unwrap_or(ctx.cfg.command_timeout);
                let result = execute_one(
                    &mut channel,
                    &mut parser,
                    &cmd.line,
                    timeout,
                    &ctx.notify_tx,
                ).await;
                match result {
                    Ok(outcome) => {
                        let public = match super::frame_to_result(outcome.error.clone()) {
                            Ok(()) => {
                                let entry = AuditEntry::success(
                                    ctx.cfg.config_id,
                                    cmd.virtual_server_id,
                                    cmd.user_id,
                                    cmd.line.clone(),
                                    outcome.latency,
                                );
                                entry.emit();
                                fire_and_forget_persist(ctx.db.as_ref(), &entry);
                                Ok(outcome)
                            }
                            Err(SshBridgeError::Upstream { code, message }) => {
                                let entry = AuditEntry::upstream_error(
                                    ctx.cfg.config_id,
                                    cmd.virtual_server_id,
                                    cmd.user_id,
                                    cmd.line.clone(),
                                    code,
                                    message.clone(),
                                    outcome.latency,
                                );
                                entry.emit();
                                fire_and_forget_persist(ctx.db.as_ref(), &entry);
                                Err(SshBridgeError::Upstream { code, message })
                            }
                            Err(other) => Err(other),
                        };
                        let _ = cmd.reply.send(public);
                    }
                    Err(TransportError::AuthRejected) => {
                        let _ = cmd.reply.send(Err(SshBridgeError::AuthRejected {
                            config_id: ctx.cfg.config_id,
                        }));
                        return SessionResult::AuthRejected;
                    }
                    Err(e) => {
                        let is_auth = matches!(&e, TransportError::Closed(s) | TransportError::Io(s) if looks_like_auth_failure(s));
                        let entry = AuditEntry::transport(
                            ctx.cfg.config_id,
                            cmd.virtual_server_id,
                            cmd.user_id,
                            cmd.line.clone(),
                            e.to_string(),
                            Duration::from_millis(0),
                        );
                        entry.emit();
                        fire_and_forget_persist(ctx.db.as_ref(), &entry);
                        let public_err = if is_auth {
                            SshBridgeError::AuthRejected { config_id: ctx.cfg.config_id }
                        } else {
                            SshBridgeError::Transport(e.to_string())
                        };
                        let _ = cmd.reply.send(Err(public_err));
                        return if is_auth { SessionResult::AuthRejected } else { SessionResult::Reconnect };
                    }
                }
            }

            // PURA-101 — watch the SSH channel between commands so an
            // upstream that closes the session asynchronously gets
            // detected promptly. TS6 (`teamspeak6-server:6.0.0-beta9`)
            // closes the shell channel *after* sending the
            // `error id=0 msg=ok` response for `quit`, not mid-response.
            // Without this branch the supervisor sees `quit` succeed,
            // idles waiting for the next command, and only learns
            // about the close when the next user command's write hits
            // a dead russh channel — surfacing as a transport error
            // to the caller rather than triggering a transparent
            // reconnect.
            //
            // Stray notify frames between commands also get routed to
            // subscribers here. Stray Body / Error frames are
            // unsolicited (the single-in-flight invariant means no
            // command is awaiting them) and are logged-and-dropped.
            chunk = channel.recv() => {
                match chunk {
                    Ok(Some(bytes)) => {
                        if let Err(e) = parser.push(&bytes) {
                            tracing::warn!(
                                target: "sshbridge::transport",
                                config_id = ctx.cfg.config_id,
                                error = %e,
                                "line buffer overflow on idle channel data — reconnecting"
                            );
                            return SessionResult::Reconnect;
                        }
                        for line in parser.drain_lines() {
                            match Frame::classify(&line) {
                                Frame::Notify(n) => {
                                    let _ = ctx.notify_tx.send(n);
                                }
                                Frame::Body(s) => {
                                    tracing::debug!(
                                        target: "sshbridge::transport",
                                        config_id = ctx.cfg.config_id,
                                        line = %s,
                                        "stray body line between commands — discarding"
                                    );
                                }
                                Frame::Error(e) => {
                                    tracing::debug!(
                                        target: "sshbridge::transport",
                                        config_id = ctx.cfg.config_id,
                                        id = e.id,
                                        msg = %e.msg,
                                        "stray error frame between commands — discarding"
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::warn!(
                            target: "sshbridge::transport",
                            config_id = ctx.cfg.config_id,
                            "ssh channel closed by peer between commands — reconnecting"
                        );
                        return SessionResult::Reconnect;
                    }
                    Err(TransportError::AuthRejected) => return SessionResult::AuthRejected,
                    Err(e) => {
                        let is_auth = matches!(&e, TransportError::Closed(s) | TransportError::Io(s) if looks_like_auth_failure(s));
                        if is_auth {
                            return SessionResult::AuthRejected;
                        }
                        tracing::warn!(
                            target: "sshbridge::transport",
                            config_id = ctx.cfg.config_id,
                            error = %e,
                            "ssh channel error between commands — reconnecting"
                        );
                        return SessionResult::Reconnect;
                    }
                }
            }

            _ = keepalive.tick() => {
                let r = execute_one(
                    &mut channel,
                    &mut parser,
                    "whoami",
                    ctx.cfg.keepalive_timeout,
                    &ctx.notify_tx,
                ).await;
                match r {
                    Ok(outcome) => {
                        // `whoami` should always resolve `error id=0`; any
                        // non-zero terminator is still a "channel is alive"
                        // signal — reset the counter.
                        let _ = outcome;
                        consecutive_keepalive_failures = 0;
                    }
                    Err(TransportError::AuthRejected) => return SessionResult::AuthRejected,
                    Err(e) => {
                        consecutive_keepalive_failures += 1;
                        tracing::warn!(
                            target: "sshbridge::keepalive",
                            config_id = ctx.cfg.config_id,
                            failures = consecutive_keepalive_failures,
                            error = %e,
                            "ssh keepalive failed",
                        );
                        if consecutive_keepalive_failures >= ctx.cfg.keepalive_failure_threshold {
                            tracing::warn!(
                                target: "sshbridge::keepalive",
                                config_id = ctx.cfg.config_id,
                                threshold = ctx.cfg.keepalive_failure_threshold,
                                "keepalive failure threshold exceeded — forcing reconnect"
                            );
                            return SessionResult::Reconnect;
                        }
                    }
                }
            }
        }
    }
}

/// Outer reconnect supervisor. Calls `connect_factory` to obtain a
/// fresh channel + run banner detect, then hands off to
/// [`dispatch_loop`]. On `SessionResult::Reconnect` it sleeps with the
/// formula `min(backoff_initial * 2^attempts, backoff_max)` and loops.
/// `SessionResult::AuthRejected` returns immediately without retry —
/// spec §11.5 fatal-on-auth.
async fn run_with_reconnect<C, F, Fut>(
    cfg: TransportConfig,
    mut connect_factory: F,
    mut cmd_rx: mpsc::Receiver<CommandRequest>,
    notify_tx: broadcast::Sender<NotifyFrame>,
    session_up_tx: broadcast::Sender<()>,
    auth_rejected_flag: Arc<Mutex<bool>>,
    host_key_mismatch_flag: Arc<Mutex<bool>>,
    db: Option<Arc<Database>>,
)
where
    C: SshChannel,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<C, TransportError>>,
{
    let mut attempts: u32 = 0;
    loop {
        // F2: cap the entire connect future (TCP + KEX + auth + channel
        // open + request_shell) at `connect_timeout`. Without this, a
        // slow-loris peer parks the supervisor task forever — the
        // supervisor never reaches `read_banner`, so `banner_timeout`
        // never fires and the operator sees no failure event.
        let connect_attempt = match tokio::time::timeout(
            cfg.connect_timeout,
            connect_factory(),
        )
        .await
        {
            Ok(r) => r,
            Err(_) => Err(TransportError::Io(format!(
                "connect timeout after {:?}",
                cfg.connect_timeout
            ))),
        };
        let mut channel = match connect_attempt {
            Ok(c) => c,
            Err(TransportError::AuthRejected) => {
                *auth_rejected_flag.lock().await = true;
                drain_with_auth_rejected(&mut cmd_rx, cfg.config_id).await;
                return;
            }
            Err(TransportError::HostKeyMismatch) => {
                tracing::warn!(
                    target: "sshbridge::transport",
                    config_id = cfg.config_id,
                    "ssh host-key verifier rejected server key — fatal, no reconnect"
                );
                *host_key_mismatch_flag.lock().await = true;
                drain_with_host_key_mismatch(&mut cmd_rx, cfg.config_id).await;
                return;
            }
            Err(e) => {
                let auth_marker = matches!(&e, TransportError::Closed(s) | TransportError::Io(s) if looks_like_auth_failure(s));
                if auth_marker {
                    *auth_rejected_flag.lock().await = true;
                    drain_with_auth_rejected(&mut cmd_rx, cfg.config_id).await;
                    return;
                }
                let backoff = next_backoff(attempts, &cfg);
                tracing::warn!(
                    target: "sshbridge::transport",
                    config_id = cfg.config_id,
                    attempts = attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "ssh connect failed — backing off"
                );
                tokio::time::sleep(backoff).await;
                attempts = attempts.saturating_add(1);
                continue;
            }
        };

        let mut parser = LineBuffer::new();
        match read_banner(&mut channel, &mut parser, &cfg).await {
            Ok(_lines) => {
                tracing::info!(
                    target: "sshbridge::transport",
                    config_id = cfg.config_id,
                    "ssh banner OK — entering dispatch loop"
                );
                // PURA-80 — fan out the session-up tick to subscribers
                // (server-notify event source re-issues
                // `servernotifyregister` here). `send` errors only when
                // there are zero receivers; that's fine — the signal
                // is best-effort and a future first subscriber sees
                // the next reconnect.
                let _ = session_up_tx.send(());
                attempts = 0;
            }
            Err(SshBridgeError::AuthRejected { .. }) => {
                *auth_rejected_flag.lock().await = true;
                drain_with_auth_rejected(&mut cmd_rx, cfg.config_id).await;
                return;
            }
            Err(e) => {
                let backoff = next_backoff(attempts, &cfg);
                tracing::warn!(
                    target: "sshbridge::transport",
                    config_id = cfg.config_id,
                    attempts = attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %e,
                    "ssh banner read failed — backing off"
                );
                let _ = channel.close().await;
                tokio::time::sleep(backoff).await;
                attempts = attempts.saturating_add(1);
                continue;
            }
        }

        // Dispatch loop owns the receiver until the session ends. We
        // can't `move` `cmd_rx` because we may need it for the next
        // reconnect attempt — instead transfer ownership in and out
        // via the helper.
        let (returned_rx, result) =
            dispatch_loop_owning(channel, cmd_rx, &cfg, &notify_tx, db.clone()).await;
        cmd_rx = returned_rx;
        match result {
            SessionResult::ShuttingDown => return,
            SessionResult::AuthRejected => {
                *auth_rejected_flag.lock().await = true;
                drain_with_auth_rejected(&mut cmd_rx, cfg.config_id).await;
                return;
            }
            SessionResult::Reconnect => {
                let backoff = next_backoff(attempts, &cfg);
                tracing::warn!(
                    target: "sshbridge::transport",
                    config_id = cfg.config_id,
                    attempts = attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    "ssh session ended — reconnecting after backoff"
                );
                tokio::time::sleep(backoff).await;
                attempts = attempts.saturating_add(1);
            }
        }
    }
}

/// Wrap `dispatch_loop` so the supervisor can re-take the receiver on
/// reconnect.
///
/// PURA-101: pump exits cleanly when the dispatch loop returns,
/// preserving `outer_rx` across reconnect cycles. The previous
/// implementation called `pump_handle.abort()` after `dispatch_loop`
/// returned — but pump was usually parked on `outer_rx.recv()` at
/// that moment, so the abort cancelled it before it could observe
/// `inner_tx`'s closure and return `outer_rx`. The recovery branch
/// then synthesised a *fresh* (unconnected) receiver, which orphaned
/// the public `cmd_tx` held by [`TransportHandle`] — every
/// post-reconnect submission resolved to "ssh transport task is no
/// longer running", silently destroying the reconnect contract.
async fn dispatch_loop_owning<C: SshChannel>(
    channel: C,
    cmd_rx: mpsc::Receiver<CommandRequest>,
    cfg: &TransportConfig,
    notify_tx: &broadcast::Sender<NotifyFrame>,
    db: Option<Arc<Database>>,
) -> (mpsc::Receiver<CommandRequest>, SessionResult) {
    // Spawn the dispatch loop with a private channel pair; pump
    // commands from the outer receiver into the inner one. When the
    // dispatch loop returns, pump observes `inner_tx.closed()` and
    // returns `outer_rx` intact for the next reconnect cycle.
    let (inner_tx, inner_rx) = mpsc::channel::<CommandRequest>(cfg.command_queue_capacity);
    let pump_handle = tokio::spawn(pump(cmd_rx, inner_tx));
    let result = dispatch_loop(
        channel,
        inner_rx,
        DispatchContext {
            cfg: cfg.clone(),
            notify_tx: notify_tx.clone(),
            db,
        },
    )
    .await;
    // Pump exits within a few microseconds of `inner_rx` drop (the
    // dispatch loop's exit dropped `inner_rx`). Awaiting it returns
    // the original `outer_rx` so the supervisor can resume the
    // reconnect cycle without losing the public `cmd_tx` connection.
    let outer_rx = match pump_handle.await {
        Ok(rx) => rx,
        Err(e) => {
            // Pump panicked (should not happen — its body is a plain
            // mpsc forwarder). Log and synthesise a fresh receiver so
            // the supervisor can at least surface a clean "task no
            // longer running" rather than wedging silently. Any
            // queued user commands on the public cmd_tx will still
            // surface as transport errors — the operator's next
            // submission is the canary.
            tracing::error!(
                target: "sshbridge::transport",
                config_id = cfg.config_id,
                error = %e,
                "pump task ended abnormally — reconnect cycle will surface as transport errors to callers"
            );
            mpsc::channel::<CommandRequest>(cfg.command_queue_capacity).1
        }
    };
    (outer_rx, result)
}

/// Forward commands from the public `outer_rx` (driven by
/// [`TransportHandle::execute`]) into the dispatch loop's private
/// `inner_tx`. Exits cleanly under three conditions:
///
/// 1. The public `cmd_tx` is dropped (`outer_rx.recv()` returns
///    `None`) — supervisor is shutting down.
/// 2. `inner_tx.send(cmd)` fails — the dispatch loop already
///    returned and dropped `inner_rx`, with a command in flight that
///    we couldn't forward.
/// 3. `inner_tx.closed()` fires — the dispatch loop returned while
///    pump was idle on `outer_rx.recv()`. PURA-101 — without this
///    branch, the supervisor's `pump_handle.abort()` cancelled the
///    parked recv and the recovery path synthesised a fresh
///    (unconnected) outer_rx, breaking the reconnect contract.
///
/// In all three cases the function returns `outer_rx` so the
/// supervisor can hand it to the next dispatch loop cycle.
async fn pump(
    mut outer_rx: mpsc::Receiver<CommandRequest>,
    inner_tx: mpsc::Sender<CommandRequest>,
) -> mpsc::Receiver<CommandRequest> {
    loop {
        tokio::select! {
            cmd = outer_rx.recv() => {
                let Some(cmd) = cmd else { break; };
                if inner_tx.send(cmd).await.is_err() {
                    break;
                }
            }
            _ = inner_tx.closed() => break,
        }
    }
    outer_rx
}

async fn drain_with_auth_rejected(rx: &mut mpsc::Receiver<CommandRequest>, config_id: i64) {
    while let Ok(cmd) = rx.try_recv() {
        let _ = cmd
            .reply
            .send(Err(SshBridgeError::AuthRejected { config_id }));
    }
}

async fn drain_with_host_key_mismatch(
    rx: &mut mpsc::Receiver<CommandRequest>,
    config_id: i64,
) {
    while let Ok(cmd) = rx.try_recv() {
        let _ = cmd
            .reply
            .send(Err(SshBridgeError::HostKeyMismatch { config_id }));
    }
}

/// Public constructor — wires the connect factory into the supervisor
/// and returns a [`TransportHandle`] callers use to submit commands.
/// The supervisor task is detached; it runs until the handle (and all
/// clones) are dropped.
///
/// **No audit-DB persistence.** Audit events still emit via
/// [`AuditEntry::emit`], but `ssh_audit_log` rows are not written. Use
/// [`spawn_with_db`] from production paths so the audit table is
/// populated; this thin variant exists for callers (tests, future
/// experiments) that genuinely have no DB to hand over.
pub fn spawn<C, F, Fut>(cfg: TransportConfig, connect_factory: F) -> TransportHandle
where
    C: SshChannel + 'static,
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<C, TransportError>> + Send,
{
    spawn_inner(cfg, connect_factory, None)
}

/// Production constructor — same as [`spawn`] but threads `db` into
/// [`DispatchContext`] so [`dispatch_loop`] schedules a fire-and-forget
/// [`AuditEntry::persist`] after each emission. Operator commands
/// queryable via `ssh_audit_log` (PURA-79 BLOCKER).
pub fn spawn_with_db<C, F, Fut>(
    cfg: TransportConfig,
    connect_factory: F,
    db: Arc<Database>,
) -> TransportHandle
where
    C: SshChannel + 'static,
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<C, TransportError>> + Send,
{
    spawn_inner(cfg, connect_factory, Some(db))
}

fn spawn_inner<C, F, Fut>(
    cfg: TransportConfig,
    connect_factory: F,
    db: Option<Arc<Database>>,
) -> TransportHandle
where
    C: SshChannel + 'static,
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<C, TransportError>> + Send,
{
    let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(cfg.command_queue_capacity);
    let (notify_tx, _) = broadcast::channel::<NotifyFrame>(cfg.notify_capacity);
    // Capacity 8 is plenty — `session_up` fires once per (re)connect,
    // so even a fleet-wide network blip would only emit a handful of
    // signals per server. Slow subscribers see `Lagged` and re-pull;
    // missing a tick just means waiting for the next reconnect.
    let (session_up_tx, _) = broadcast::channel::<()>(8);
    let auth_flag = Arc::new(Mutex::new(false));
    let host_key_flag = Arc::new(Mutex::new(false));

    let handle = TransportHandle {
        config_id: cfg.config_id,
        cmd_tx,
        notify_tx: notify_tx.clone(),
        session_up_tx: session_up_tx.clone(),
        auth_rejected: auth_flag.clone(),
        host_key_mismatch: host_key_flag.clone(),
    };

    let cfg_clone = cfg.clone();
    tokio::spawn(async move {
        run_with_reconnect(
            cfg_clone,
            connect_factory,
            cmd_rx,
            notify_tx,
            session_up_tx,
            auth_flag,
            host_key_flag,
            db,
        )
        .await;
    });

    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::sync::mpsc as tmpsc;

    /// Stub channel — script reads with a sender, capture writes with a
    /// receiver. Each read item is `Ok(Some(...))`, `Ok(None)` (EOF),
    /// or `Err(TransportError)`.
    type ScriptedRead = Result<Option<Vec<u8>>, TransportError>;

    struct StubChannel {
        reads: tmpsc::Receiver<ScriptedRead>,
        writes: tmpsc::UnboundedSender<Vec<u8>>,
    }

    impl StubChannel {
        fn new() -> (Self, tmpsc::Sender<ScriptedRead>, tmpsc::UnboundedReceiver<Vec<u8>>) {
            let (read_tx, read_rx) = tmpsc::channel(64);
            let (write_tx, write_rx) = tmpsc::unbounded_channel();
            (
                Self {
                    reads: read_rx,
                    writes: write_tx,
                },
                read_tx,
                write_rx,
            )
        }
    }

    #[async_trait::async_trait]
    impl SshChannel for StubChannel {
        async fn write(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            let _ = self.writes.send(bytes.to_vec());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
            match self.reads.recv().await {
                Some(item) => item,
                None => Ok(None),
            }
        }

        async fn close(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
    }

    fn collect_writes(rx: &mut tmpsc::UnboundedReceiver<Vec<u8>>) -> String {
        let mut out = Vec::new();
        while let Ok(b) = rx.try_recv() {
            out.extend_from_slice(&b);
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn test_cfg() -> TransportConfig {
        TransportConfig {
            config_id: 7,
            command_timeout: Duration::from_millis(500),
            banner_timeout: Duration::from_millis(500),
            // Tight idle window keeps the lazy-banner regression tests
            // fast; the production default (500ms) only matters on the
            // wire path.
            banner_idle_window: Duration::from_millis(50),
            connect_timeout: Duration::from_millis(500),
            keepalive_interval: Duration::from_secs(3600),
            keepalive_timeout: Duration::from_millis(500),
            keepalive_failure_threshold: 3,
            backoff_initial: Duration::from_millis(10),
            backoff_max: Duration::from_millis(40),
            notify_capacity: 16,
            command_queue_capacity: 16,
        }
    }

    #[test]
    fn backoff_base_doubles_then_caps() {
        let cfg = TransportConfig {
            backoff_initial: Duration::from_millis(1000),
            backoff_max: Duration::from_millis(30000),
            ..test_cfg()
        };
        // 1000, 2000, 4000, 8000, 16000, 30000 (cap), 30000, ...
        assert_eq!(next_backoff_base(0, &cfg), Duration::from_millis(1000));
        assert_eq!(next_backoff_base(1, &cfg), Duration::from_millis(2000));
        assert_eq!(next_backoff_base(2, &cfg), Duration::from_millis(4000));
        assert_eq!(next_backoff_base(3, &cfg), Duration::from_millis(8000));
        assert_eq!(next_backoff_base(4, &cfg), Duration::from_millis(16000));
        // 32_000 ms would exceed the cap → 30_000.
        assert_eq!(next_backoff_base(5, &cfg), Duration::from_millis(30000));
        assert_eq!(next_backoff_base(50, &cfg), Duration::from_millis(30000));
    }

    /// Property test for jitter — every sample lands inside the ±25%
    /// window, the mean is close to `base`, and the spread is positive
    /// (i.e. jitter is actually being applied).
    #[test]
    fn backoff_jitter_within_range_with_positive_spread() {
        let cfg = TransportConfig {
            backoff_initial: Duration::from_millis(1000),
            backoff_max: Duration::from_millis(30000),
            ..test_cfg()
        };
        // attempts=5 hits the cap (30_000 ms) so we are testing the
        // jitter applied to the capped base, which is the worst case
        // for fleet-wide synchronisation.
        let base_ms = next_backoff_base(5, &cfg).as_millis() as f64;
        let lower = base_ms * 0.75;
        let upper = base_ms * 1.25;

        let n = 1000usize;
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let v = next_backoff(5, &cfg).as_millis() as f64;
            assert!(
                v >= lower && v <= upper,
                "sample {v} outside [{lower}, {upper}]"
            );
            samples.push(v);
        }

        let mean: f64 = samples.iter().sum::<f64>() / n as f64;
        // Allow ±2% drift on the mean — a uniform [0.75, 1.25] window
        // has mean 1.0; with 1000 samples the sample mean is well
        // within ±0.02 of `base` in practice.
        assert!(
            (mean - base_ms).abs() < base_ms * 0.02,
            "mean {mean} not within 2% of base {base_ms}",
        );

        let variance: f64 =
            samples.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
        let stddev = variance.sqrt();
        assert!(
            stddev > 0.0,
            "stddev {stddev} should be positive — jitter not applied"
        );
    }

    #[tokio::test]
    async fn read_banner_accepts_canonical_ts6_banner() {
        let (mut channel, read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = test_cfg();

        // Spec §11.3: TS3 / Welcome / virtualserver_status / error id=0 msg=ok
        read_tx.send(Ok(Some(b"TS3\r\n".to_vec()))).await.unwrap();
        read_tx
            .send(Ok(Some(b"Welcome to the TeamSpeak 6 ServerQuery interface\r\n".to_vec())))
            .await
            .unwrap();
        read_tx
            .send(Ok(Some(b"virtualserver_status=online\r\n".to_vec())))
            .await
            .unwrap();
        read_tx.send(Ok(Some(b"error id=0 msg=ok\r\n".to_vec()))).await.unwrap();

        let lines = read_banner(&mut channel, &mut parser, &cfg).await.unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "TS3");
    }

    /// PURA-101 — `teamspeak6-server:6.0.0-beta9` emits the `TS3` /
    /// `Welcome to the TeamSpeak ServerQuery interface` body lines and
    /// then goes idle, **never** sending the spec's `error id=0 msg=ok`
    /// banner terminator until a command lands. `read_banner` must
    /// settle on `banner_idle_window` of silence after at least one
    /// body line; without this, the supervisor blocks until
    /// `banner_timeout` fires and the operator sees a `banner timeout`
    /// transport error.
    #[tokio::test]
    async fn read_banner_settles_on_idle_after_lazy_ts6_banner() {
        let (mut channel, read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = TransportConfig {
            // Plenty of headroom on the absolute timeout; the idle
            // window is what should trigger the return.
            banner_timeout: Duration::from_millis(500),
            banner_idle_window: Duration::from_millis(50),
            ..test_cfg()
        };

        // Lazy banner: TS6 sends only the welcome lines, no terminator.
        // Wire bytes match the captured 4-chunk pattern from the live
        // fixture (russh fragments the banner across small Data msgs).
        read_tx.send(Ok(Some(b"TS3".to_vec()))).await.unwrap();
        read_tx.send(Ok(Some(b"\n\r".to_vec()))).await.unwrap();
        read_tx
            .send(Ok(Some(b"Welcome to the TeamSpeak ServerQuery interface, type \"help\" for a list of commands and \"help <command>\" for information on a specific command.".to_vec())))
            .await
            .unwrap();
        read_tx.send(Ok(Some(b"\n\r".to_vec()))).await.unwrap();
        // No more sends — the channel goes silent. We hold `read_tx`
        // alive so `recv()` blocks rather than EOFing.
        let started = std::time::Instant::now();
        let lines = read_banner(&mut channel, &mut parser, &cfg)
            .await
            .expect("lazy TS6 banner must settle on idle window, not timeout");
        let elapsed = started.elapsed();
        assert_eq!(lines.len(), 2, "expected TS3 + Welcome lines, got {lines:?}");
        assert_eq!(lines[0], "TS3");
        assert!(lines[1].contains("ServerQuery interface"));
        // Sanity: settled via the 50ms idle window, well under the
        // 500ms strict banner_timeout.
        assert!(
            elapsed < Duration::from_millis(300),
            "banner should settle on idle window quickly; took {elapsed:?}"
        );
    }

    /// PURA-101 — silent server (no banner at all) still fails on the
    /// strict `banner_timeout`. The idle-window only kicks in *after*
    /// at least one body line has arrived.
    #[tokio::test]
    async fn read_banner_idle_window_does_not_short_circuit_silent_upstream() {
        let (mut channel, _read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = TransportConfig {
            banner_timeout: Duration::from_millis(80),
            banner_idle_window: Duration::from_millis(20),
            ..test_cfg()
        };
        let r = read_banner(&mut channel, &mut parser, &cfg).await;
        match r {
            Err(SshBridgeError::Transport(s)) => assert!(s.contains("banner timeout")),
            other => panic!("expected banner-timeout Transport, got {other:?}"),
        }
    }

    /// PURA-101 — live TS6 SSH ServerQuery emits LF-CR (`\n\r`), not
    /// the spec's nominal CR-LF. Before the [`super::wire::LineBuffer`]
    /// fix, `error id=0 msg=ok\n\r` arrived after a leading `\r` from
    /// the prior line's terminator and fell through `Frame::classify`
    /// as `Frame::Body`, so `read_banner` hung until `banner_timeout`
    /// fired. This regression wires the exact byte pattern observed
    /// against `teamspeak6-server:6.0.0-beta9` through `read_banner`.
    #[tokio::test]
    async fn read_banner_accepts_ts6_lf_cr_wire() {
        let (mut channel, read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = test_cfg();

        // Single SSH packet carrying the full banner exactly as the
        // live fixture sends it: every line LF-CR terminated.
        read_tx
            .send(Ok(Some(
                b"TS3\n\rWelcome to the TeamSpeak ServerQuery interface\n\rerror id=0 msg=ok\n\r"
                    .to_vec(),
            )))
            .await
            .unwrap();

        let lines = read_banner(&mut channel, &mut parser, &cfg)
            .await
            .expect("LF-CR banner must parse to Ok");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "TS3");
        assert!(lines[1].contains("ServerQuery"));
    }

    #[tokio::test]
    async fn read_banner_surfaces_nonzero_error_as_upstream() {
        let (mut channel, read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = test_cfg();
        read_tx
            .send(Ok(Some(
                b"error id=2568 msg=insufficient\\sclient\\spermissions\r\n".to_vec(),
            )))
            .await
            .unwrap();
        let r = read_banner(&mut channel, &mut parser, &cfg).await;
        match r {
            Err(SshBridgeError::Upstream { code, message }) => {
                assert_eq!(code, 2568);
                assert!(message.contains("insufficient"));
            }
            other => panic!("expected Upstream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_banner_translates_auth_substring_to_auth_rejected() {
        let (mut channel, read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = test_cfg();
        // Channel reports a closed-with-error containing the §11.5 marker.
        read_tx
            .send(Err(TransportError::Closed("Authentication failed".into())))
            .await
            .unwrap();
        let r = read_banner(&mut channel, &mut parser, &cfg).await;
        assert!(matches!(r, Err(SshBridgeError::AuthRejected { .. })));
    }

    #[tokio::test]
    async fn read_banner_times_out() {
        let (mut channel, _read_tx, _writes) = StubChannel::new();
        let mut parser = LineBuffer::new();
        let cfg = TransportConfig {
            banner_timeout: Duration::from_millis(20),
            ..test_cfg()
        };
        // Hold the read_tx so recv() blocks. The deadline elapses.
        let r = read_banner(&mut channel, &mut parser, &cfg).await;
        match r {
            Err(SshBridgeError::Transport(s)) => assert!(s.contains("banner timeout")),
            other => panic!("expected banner-timeout Transport, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch_serialises_commands_in_submission_order() {
        let (channel, read_tx, mut writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        // Queue three commands.
        let (r1, h1) = oneshot::channel();
        let (r2, h2) = oneshot::channel();
        let (r3, h3) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "cmd-one".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: r1,
            })
            .await
            .unwrap();
        cmd_tx
            .send(CommandRequest {
                line: "cmd-two".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: r2,
            })
            .await
            .unwrap();
        cmd_tx
            .send(CommandRequest {
                line: "cmd-three".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: r3,
            })
            .await
            .unwrap();
        drop(cmd_tx);

        // Drive the dispatch loop in a background task so we can feed
        // scripted reads in order.
        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));

        // Each command terminates on `error id=0 msg=ok\r\n`.
        for _ in 0..3 {
            read_tx
                .send(Ok(Some(b"error id=0 msg=ok\r\n".to_vec())))
                .await
                .unwrap();
        }

        // Replies arrive in submission order.
        h1.await.unwrap().unwrap();
        h2.await.unwrap().unwrap();
        h3.await.unwrap().unwrap();

        let session = dispatch.await.unwrap();
        assert!(matches!(session, SessionResult::ShuttingDown));

        // Writes were issued in the same order — three lines, three CR-LFs.
        let written = collect_writes(&mut writes_rx);
        let one = written.find("cmd-one").unwrap();
        let two = written.find("cmd-two").unwrap();
        let three = written.find("cmd-three").unwrap();
        assert!(one < two && two < three, "writes out of order: {written:?}");
    }

    /// PURA-101 — the dispatch loop watches the channel between
    /// commands so an upstream Eof (channel closed by peer with no
    /// command in flight) returns `Reconnect` immediately rather than
    /// only when the next user command's write fails. Mirrors the
    /// real-world TS6 behaviour where `quit` is acked cleanly *then*
    /// the channel closes — without this branch the supervisor sits
    /// idle and the next user command surfaces a transport error.
    #[tokio::test]
    async fn dispatch_idle_channel_close_triggers_reconnect() {
        let (channel, read_tx, _writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));

        // No command in flight; simulate an upstream Eof. With the
        // pre-fix dispatch_loop this would have been ignored until the
        // next command tried to write.
        read_tx.send(Ok(None)).await.unwrap();

        let session = dispatch.await.unwrap();
        assert!(
            matches!(session, SessionResult::Reconnect),
            "expected Reconnect on idle channel close, got {session:?}"
        );
        // cmd_tx is preserved; the supervisor will resume with it on
        // the next reconnect cycle.
        drop(cmd_tx);
    }

    /// PURA-101 — stray notify frames that arrive between commands
    /// must still flow to subscribers even when no command is awaiting
    /// the channel. Without the idle channel watcher these would sit
    /// in russh's buffer until the next command consumed them.
    #[tokio::test]
    async fn dispatch_idle_channel_routes_notify_to_subscribers() {
        let (channel, read_tx, _writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, mut notify_rx) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));

        // Send a notify between commands — no command in flight.
        read_tx
            .send(Ok(Some(b"notifycliententer clid=99\r\n".to_vec())))
            .await
            .unwrap();
        // Subscriber receives it.
        let n = tokio::time::timeout(Duration::from_millis(200), notify_rx.recv())
            .await
            .expect("notify should be routed even with no command in flight")
            .unwrap();
        assert_eq!(n.event, "notifycliententer");

        // Then close the channel to terminate dispatch_loop.
        read_tx.send(Ok(None)).await.unwrap();
        let session = dispatch.await.unwrap();
        assert!(matches!(session, SessionResult::Reconnect));
        drop(cmd_tx);
    }

    /// PURA-101 — a forced session end (Reconnect) must NOT lose the
    /// outer command receiver. The previous `dispatch_loop_owning`
    /// implementation called `pump_handle.abort()` while pump was
    /// parked on `outer_rx.recv()`, and the recovery branch then
    /// synthesised a fresh, unconnected receiver — orphaning the
    /// public `cmd_tx` held by [`TransportHandle`] and breaking every
    /// post-reconnect submission with "ssh transport task is no
    /// longer running". This regression drives the supervisor through
    /// one full reconnect cycle and asserts a post-reconnect
    /// submission still reaches the new dispatch loop.
    #[tokio::test]
    async fn run_with_reconnect_preserves_outer_rx_across_reconnect() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cfg = TransportConfig {
            // Tight backoff so the test finishes quickly.
            backoff_initial: Duration::from_millis(5),
            backoff_max: Duration::from_millis(20),
            // Generous banner-detect so the second connection is
            // definitely up before we submit the post-reconnect cmd.
            banner_timeout: Duration::from_secs(2),
            banner_idle_window: Duration::from_millis(50),
            // Disable keepalive so it doesn't fire in this test window.
            keepalive_interval: Duration::from_secs(3600),
            ..test_cfg()
        };

        // Factory yields a fresh StubChannel each call. Each scripted
        // channel: first-cycle drives a normal banner+`error id=0` flow,
        // the script for that cycle is set up via the read_tx we get
        // from `StubChannel::new()`. We have to set up TWO cycles —
        // one for the initial connect, one for the reconnect.
        let cycle = Arc::new(AtomicUsize::new(0));
        let read_txs: Arc<Mutex<Vec<tmpsc::Sender<ScriptedRead>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let read_txs_factory = read_txs.clone();
        let cycle_factory = cycle.clone();
        let factory = move || {
            let read_txs = read_txs_factory.clone();
            let cycle = cycle_factory.clone();
            async move {
                let (channel, read_tx, _writes_rx) = StubChannel::new();
                read_txs.lock().await.push(read_tx);
                cycle.fetch_add(1, Ordering::SeqCst);
                Ok::<StubChannel, TransportError>(channel)
            }
        };

        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let (session_up_tx, _) = broadcast::channel::<()>(4);
        let auth_flag = Arc::new(Mutex::new(false));
        let host_key_flag = Arc::new(Mutex::new(false));

        let supervisor = tokio::spawn(run_with_reconnect(
            cfg.clone(),
            factory,
            cmd_rx,
            notify_tx,
            session_up_tx,
            auth_flag,
            host_key_flag,
            None,
        ));

        // Wait for the FIRST cycle's read_tx to land.
        loop {
            if let Some(tx) = read_txs.lock().await.first().cloned() {
                // Cycle 1 banner — canonical CR-LF form so the test
                // exercises the `error id=0` banner-detect path.
                tx.send(Ok(Some(b"TS3\r\nWelcome\r\nerror id=0 msg=ok\r\n".to_vec())))
                    .await
                    .unwrap();
                break;
            }
            tokio::task::yield_now().await;
        }

        // Force the dispatch loop to return Reconnect by closing the
        // first channel (Eof).
        let first_tx = {
            let guard = read_txs.lock().await;
            guard[0].clone()
        };
        first_tx.send(Ok(None)).await.unwrap();

        // Wait for the second cycle to come up. With backoff ~5ms,
        // this is essentially immediate.
        loop {
            if cycle.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Cycle 2 banner.
        let second_tx = {
            let guard = read_txs.lock().await;
            guard[1].clone()
        };
        second_tx
            .send(Ok(Some(b"TS3\r\nWelcome\r\nerror id=0 msg=ok\r\n".to_vec())))
            .await
            .unwrap();

        // The CRITICAL check: a submission via the original cmd_tx
        // (held by what would be `TransportHandle::cmd_tx`) must reach
        // the new dispatch loop. Pre-fix this would block forever
        // because the synthesised fresh outer_rx isn't connected to
        // cmd_tx.
        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "post-reconnect-cmd".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: reply_tx,
            })
            .await
            .expect("send to original cmd_tx must succeed after reconnect");

        // Cycle 2 dispatch_loop: respond with `error id=0 msg=ok`.
        second_tx
            .send(Ok(Some(b"error id=0 msg=ok\r\n".to_vec())))
            .await
            .unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("post-reconnect cmd should complete within 2s")
            .expect("reply channel must not be dropped")
            .expect("post-reconnect cmd must succeed");
        assert_eq!(outcome.error.id, 0);

        // Tear down: drop cmd_tx, supervisor exits via ShuttingDown.
        drop(cmd_tx);
        // Close the second channel so dispatch_loop returns ShuttingDown.
        second_tx.send(Ok(None)).await.ok();
        let _ = tokio::time::timeout(Duration::from_secs(2), supervisor).await;
    }

    #[tokio::test]
    async fn dispatch_routes_notify_lines_to_subscribers_during_command() {
        let (channel, read_tx, _writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, mut notify_rx) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "clientlist".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: Some(3),
                reply: reply_tx,
            })
            .await
            .unwrap();
        drop(cmd_tx);

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));

        // A notify line arrives mid-command; then a body line; then the
        // terminator.
        read_tx
            .send(Ok(Some(b"notifycliententer clid=5\r\n".to_vec())))
            .await
            .unwrap();
        read_tx
            .send(Ok(Some(b"clid=12 cid=1\r\n".to_vec())))
            .await
            .unwrap();
        read_tx
            .send(Ok(Some(b"error id=0 msg=ok\r\n".to_vec())))
            .await
            .unwrap();

        let outcome = reply_rx.await.unwrap().unwrap();
        assert_eq!(outcome.body_lines.len(), 1);
        assert!(outcome.body_lines[0].contains("clid=12"));
        assert_eq!(outcome.error.id, 0);

        let n = notify_rx.recv().await.unwrap();
        assert_eq!(n.event, "notifycliententer");

        dispatch.await.unwrap();
    }

    #[tokio::test]
    async fn dispatch_command_timeout_triggers_reconnect_signal() {
        let (channel, _read_tx, _writes_rx) = StubChannel::new();
        let cfg = TransportConfig {
            command_timeout: Duration::from_millis(20),
            ..test_cfg()
        };
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "version".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: reply_tx,
            })
            .await
            .unwrap();
        // Don't drop cmd_tx — we want the loop to return Reconnect on
        // the timeout, not ShuttingDown on a closed receiver.

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));
        // Caller sees a transport-class error.
        let err = reply_rx.await.unwrap().unwrap_err();
        match err {
            SshBridgeError::Transport(_) => {}
            other => panic!("expected Transport, got {other:?}"),
        }
        let session = dispatch.await.unwrap();
        assert!(matches!(session, SessionResult::Reconnect));
        drop(cmd_tx);
    }

    #[tokio::test]
    async fn dispatch_auth_rejected_short_circuits_no_reconnect() {
        let (channel, read_tx, _writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "version".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: reply_tx,
            })
            .await
            .unwrap();

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));

        // The channel reports an explicit AuthRejected.
        read_tx
            .send(Err(TransportError::AuthRejected))
            .await
            .unwrap();

        let r = reply_rx.await.unwrap();
        assert!(matches!(r, Err(SshBridgeError::AuthRejected { config_id: 7 })));
        let session = dispatch.await.unwrap();
        assert!(matches!(session, SessionResult::AuthRejected));
        drop(cmd_tx);
    }

    /// PURA-86 acceptance criterion #2 — a `connect_factory` returning
    /// `Err(TransportError::HostKeyMismatch)` makes the supervisor drain
    /// queued commands with [`SshBridgeError::HostKeyMismatch`] and
    /// return without entering the backoff/retry loop. Mirrors
    /// [`dispatch_auth_rejected_short_circuits_no_reconnect`] for the
    /// connect-side path.
    ///
    /// Without this short-circuit, an operator who legitimately re-keyed
    /// the upstream sees a backoff-loop noise of warn lines and any
    /// queued operator command resolves as a generic transport timeout —
    /// they have to grep `sshbridge::hostkey` logs to figure out the
    /// verifier rejected the new key.
    #[tokio::test]
    async fn run_with_reconnect_short_circuits_on_host_key_mismatch() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cfg = TransportConfig {
            // Tiny backoffs so a buggy implementation that DOES retry
            // would fail the "factory called exactly once" assertion
            // within the test's lifetime.
            backoff_initial: Duration::from_millis(1),
            backoff_max: Duration::from_millis(2),
            ..test_cfg()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<StubChannel, _>(TransportError::HostKeyMismatch)
                }
            }
        };

        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let (session_up_tx, _) = broadcast::channel::<()>(4);
        let auth_flag = Arc::new(Mutex::new(false));
        let host_key_flag = Arc::new(Mutex::new(false));

        // Pre-load two queued commands before the supervisor runs so
        // the drain has something to drain.
        let (reply_tx_a, reply_rx_a) = oneshot::channel();
        let (reply_tx_b, reply_rx_b) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "version".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: reply_tx_a,
            })
            .await
            .unwrap();
        cmd_tx
            .send(CommandRequest {
                line: "hostinfo".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: reply_tx_b,
            })
            .await
            .unwrap();

        let supervisor = tokio::spawn(run_with_reconnect(
            cfg.clone(),
            factory,
            cmd_rx,
            notify_tx,
            session_up_tx,
            auth_flag.clone(),
            host_key_flag.clone(),
            None,
        ));

        // Supervisor must terminate on its own — no abort, no timeout.
        // If this hangs, the short-circuit is broken and the supervisor
        // is stuck in the backoff/retry loop.
        supervisor.await.unwrap();

        // Both queued commands report the typed HostKeyMismatch error
        // with the config-id from the test config (7).
        let r_a = reply_rx_a.await.unwrap();
        let r_b = reply_rx_b.await.unwrap();
        assert!(
            matches!(r_a, Err(SshBridgeError::HostKeyMismatch { config_id: 7 })),
            "expected HostKeyMismatch on first queued cmd, got {r_a:?}"
        );
        assert!(
            matches!(r_b, Err(SshBridgeError::HostKeyMismatch { config_id: 7 })),
            "expected HostKeyMismatch on second queued cmd, got {r_b:?}"
        );

        // Factory invoked exactly once — fail-closed, no retry storm.
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "host-key mismatch must short-circuit reconnect; factory called \
             {} time(s) but expected exactly 1",
            calls.load(Ordering::SeqCst)
        );

        // Sticky flag is set so subsequent submissions through the
        // public TransportHandle short-circuit instead of blocking on a
        // dead mpsc.
        assert!(
            *host_key_flag.lock().await,
            "host_key_mismatch_flag must be set after a verifier rejection"
        );
        // Auth-rejected flag stays false — the two signals are
        // independent and operator-visible separately.
        assert!(
            !*auth_flag.lock().await,
            "auth_rejected_flag must NOT be set on a host-key mismatch"
        );

        drop(cmd_tx);
    }

    /// PURA-86 — once the supervisor flips `host_key_mismatch_flag`,
    /// fresh `TransportHandle::execute` submissions resolve to
    /// `HostKeyMismatch` immediately. Without the flag check the handle
    /// would either block on a dead mpsc or resolve to a generic
    /// `Transport(...)` error after the receiver-dropped detection,
    /// neither of which lets the REST layer surface the typed code.
    #[tokio::test]
    async fn handle_short_circuits_after_host_key_mismatch() {
        let factory = || async {
            Err::<StubChannel, _>(TransportError::HostKeyMismatch)
        };
        let cfg = test_cfg();
        let handle = spawn(cfg, factory);

        // First submission either races into the queue (then drained) or
        // arrives after the flag is set — both paths must yield the
        // typed error.
        let r1 = handle.execute("version", None, None).await;
        assert!(
            matches!(r1, Err(SshBridgeError::HostKeyMismatch { config_id: 7 })),
            "expected HostKeyMismatch on first submission, got {r1:?}"
        );

        // By now the supervisor has flipped the flag (the only way r1
        // resolved to HostKeyMismatch is via the supervisor reaching
        // either the drain or the sticky-flag short-circuit). Subsequent
        // submissions hit the flag check first.
        let r2 = handle.execute("hostinfo", None, None).await;
        assert!(
            matches!(r2, Err(SshBridgeError::HostKeyMismatch { config_id: 7 })),
            "expected HostKeyMismatch on second submission, got {r2:?}"
        );
    }

    #[tokio::test]
    async fn run_with_reconnect_bounds_hanging_connect_factory() {
        // F2 regression: a connect factory that never completes
        // (slow-loris peer, wedged MITM, or a misconfigured host:port
        // that accepts TCP without ever finishing the SSH handshake)
        // must NOT park the supervisor indefinitely. With
        // `connect_timeout` set, the supervisor unsticks after the
        // ceiling fires and proceeds into the backoff/retry loop.
        // Without the fix this test hangs (the outer
        // `tokio::time::timeout` makes that observable as a fail).
        use std::sync::atomic::{AtomicUsize, Ordering};

        let cfg = TransportConfig {
            connect_timeout: Duration::from_millis(20),
            backoff_initial: Duration::from_millis(1),
            backoff_max: Duration::from_millis(2),
            ..test_cfg()
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = {
            let calls = calls.clone();
            move || {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Hang forever — only the connect_timeout in
                    // run_with_reconnect can rescue the supervisor.
                    std::future::pending::<Result<StubChannel, TransportError>>().await
                }
            }
        };
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let (session_up_tx, _) = broadcast::channel::<()>(4);
        let auth_flag = Arc::new(Mutex::new(false));
        let host_key_flag = Arc::new(Mutex::new(false));

        let supervisor = tokio::spawn(run_with_reconnect(
            cfg.clone(),
            factory,
            cmd_rx,
            notify_tx,
            session_up_tx,
            auth_flag,
            host_key_flag,
            None,
        ));

        // Wait for several timeout-and-backoff cycles. Each cycle is
        // ~connect_timeout + backoff(<= backoff_max) ≈ 22 ms; 200 ms
        // gives plenty of headroom for >= 3 attempts on a busy CI box.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let observed = calls.load(Ordering::SeqCst);

        // The supervisor never escapes the connect loop on its own
        // (factory always hangs), so we abort it explicitly.
        drop(cmd_tx);
        supervisor.abort();
        let _ = supervisor.await;

        assert!(
            observed >= 3,
            "expected the supervisor to time out and retry the connect future at least 3 times within 200ms; observed {observed}"
        );
    }

    /// PURA-79 BLOCKER regression — drive a successful command through
    /// [`dispatch_loop`] with `db: Some(...)` wired into [`DispatchContext`]
    /// and assert a row lands in `ssh_audit_log`.
    ///
    /// Without the BLOCKER fix the dispatch loop only calls `.emit()` and
    /// the table stays empty in production. SecurityEngineer's review on
    /// commit `3e9c73d` flagged that the existing
    /// `audit_entry_persist_round_trips_through_table` test exercises only
    /// the standalone `AuditEntry::persist` seam — it does not catch a
    /// regression where someone refactors out the `dispatch_loop` ↔
    /// `persist` wiring. This end-to-end test is the regression belt.
    #[allow(non_snake_case)] // `commandLine` mirrors the on-disk column name.
    #[tokio::test]
    async fn dispatch_loop_persists_audit_row_when_db_wired() {
        use surrealdb::types::SurrealValue;

        let db = crate::db::connect_in_memory()
            .await
            .expect("in-memory connect");
        crate::db::migrations::run(&db).await.expect("migrations run");

        let (channel, read_tx, _writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: Some(db.clone()),
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "clientlist -uid".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: Some(1),
                reply: reply_tx,
            })
            .await
            .unwrap();
        drop(cmd_tx);

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));
        read_tx
            .send(Ok(Some(b"error id=0 msg=ok\r\n".to_vec())))
            .await
            .unwrap();

        // Reply lands first — caller is unblocked before persist runs.
        let outcome = reply_rx.await.unwrap().unwrap();
        assert_eq!(outcome.error.id, 0);
        let _session = dispatch.await.unwrap();

        // Persist is `tokio::spawn`-fired so the DB write may not have
        // completed by the time `cmd.reply.send(...)` resolved. Poll
        // under a generous deadline — anything over a few hundred ms in
        // practice means the wiring is broken.
        #[derive(serde::Deserialize, SurrealValue)]
        #[surreal(crate = "surrealdb::types")]
        struct Row {
            #[allow(dead_code)]
            id: i64,
            commandLine: String,
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut rows: Vec<Row> = Vec::new();
        while std::time::Instant::now() < deadline {
            let mut resp = db
                .query(
                    "SELECT record::id(id) AS id, commandLine FROM ssh_audit_log;",
                )
                .await
                .unwrap()
                .check()
                .unwrap();
            rows = resp.take(0).unwrap();
            if !rows.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !rows.is_empty(),
            "PURA-79 BLOCKER: dispatch_loop must persist an `ssh_audit_log` \
             row when `db: Some(...)` is wired into DispatchContext. Found 0 rows."
        );
        assert_eq!(rows[0].commandLine, "clientlist -uid");
    }

    /// Companion to [`dispatch_loop_persists_audit_row_when_db_wired`] —
    /// asserts that the `db: None` path stays viable for tests / future
    /// non-DB consumers (no panic, no DB write attempted).
    #[tokio::test]
    async fn dispatch_loop_emit_only_when_db_is_none() {
        let (channel, read_tx, _writes_rx) = StubChannel::new();
        let cfg = test_cfg();
        let (cmd_tx, cmd_rx) = mpsc::channel::<CommandRequest>(8);
        let (notify_tx, _) = broadcast::channel::<NotifyFrame>(16);
        let ctx = DispatchContext {
            cfg: cfg.clone(),
            notify_tx,
            db: None,
        };

        let (reply_tx, reply_rx) = oneshot::channel();
        cmd_tx
            .send(CommandRequest {
                line: "version".into(),
                timeout: None,
                user_id: None,
                virtual_server_id: None,
                reply: reply_tx,
            })
            .await
            .unwrap();
        drop(cmd_tx);

        let dispatch = tokio::spawn(dispatch_loop(channel, cmd_rx, ctx));
        read_tx
            .send(Ok(Some(b"error id=0 msg=ok\r\n".to_vec())))
            .await
            .unwrap();

        let outcome = reply_rx.await.unwrap().unwrap();
        assert_eq!(outcome.error.id, 0);
        let session = dispatch.await.unwrap();
        assert!(matches!(session, SessionResult::ShuttingDown));
    }
}
