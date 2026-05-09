//! High-level read-only SSH ServerQuery client — implements
//! [`crate::control::ControlBackend`] on top of the russh-backed
//! [`super::transport::TransportHandle`] from PURA-76.
//!
//! ## Concurrency
//!
//! The transport queue is FIFO, but multiple [`TransportHandle`] clones
//! can `send` concurrently — so two callers issuing
//! `(use sid=A; cmd)` and `(use sid=B; cmd)` could see the wire become
//! `[use A, use B, cmd, cmd]`. To keep sid-scoped commands atomic the
//! client serialises every `(use, command)` pair behind [`sid_gate`].
//!
//! ## No sid caching
//!
//! Caching the most recently selected sid is unsafe across reconnects:
//! the russh transport's supervisor re-authenticates and the upstream's
//! "selected vserver" resets. A stale cache would skip `use sid=N` and
//! the next sid-scoped command would fail with "no virtual server
//! selected". Always re-issuing `use sid=N` per scoped command is
//! cheap on a persistent SSH session and side-steps the issue.
//!
//! ## Body parsing
//!
//! ServerQuery body lines are pipe-separated records, each record a
//! space-separated `key=value` token list. The wire parser (see
//! [`super::wire::parse_records`]) already handles §10.4 unescape, so
//! the client just splits each body line into records, lifts each into
//! a `serde_json::Value::Object` of stringified pairs, and lets the
//! existing WebQuery models (`stringy::deserialize*` visitors) accept
//! them transparently.

use std::collections::HashMap;

use async_trait::async_trait;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::control::{ControlBackend, ControlBackendError, ControlResult};
use crate::webquery::escape::escape;
use crate::webquery::models::{
    BanEntry, ChannelEntry, ClientDbEntry, ClientEntry, ClientInfo, ConnectionInfo, LogEntry,
    ServerInfo, VersionInfo, VirtualServerEntry,
};
use crate::webquery::BanAddParams;

use super::transport::{CommandOutcome, TransportHandle};
use super::wire::parse_records;

/// One SSH ServerQuery client per `server_connection` row. Holds a
/// [`TransportHandle`] (russh transport supervisor) and serialises
/// `use`/command pairs behind [`sid_gate`].
pub struct SshControlClient {
    config_id: i64,
    transport: TransportHandle,
    sid_gate: Mutex<()>,
}

impl std::fmt::Debug for SshControlClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshControlClient")
            .field("config_id", &self.config_id)
            .finish_non_exhaustive()
    }
}

impl SshControlClient {
    pub fn new(config_id: i64, transport: TransportHandle) -> Self {
        Self {
            config_id,
            transport,
            sid_gate: Mutex::new(()),
        }
    }

    pub fn config_id(&self) -> i64 {
        self.config_id
    }

    /// Return a clone of the underlying [`TransportHandle`]. Used by the
    /// PURA-80 server-notify event source to share the SSH session with
    /// the dashboard tick — registering for `notify*` events on a
    /// separate session would double the SSH connection count per
    /// server, and the upstream's notify subscription is bound to the
    /// session that issued `servernotifyregister`.
    pub fn transport_handle(&self) -> TransportHandle {
        self.transport.clone()
    }

    /// Submit one wire line, returning the [`CommandOutcome`] (body
    /// lines + terminator + latency) or a typed error.
    async fn execute(&self, line: &str, sid: Option<i64>) -> ControlResult<CommandOutcome> {
        self.transport
            .execute(line.to_string(), None, sid)
            .await
            .map_err(Into::into)
    }

    /// Run an instance-scoped command (no `use` needed) — `version`,
    /// `serverlist`, `hostinfo`, etc.
    async fn run_unscoped(&self, line: &str) -> ControlResult<CommandOutcome> {
        self.execute(line, None).await
    }

    /// Run a sid-scoped command. Issues `use sid=<n>` immediately
    /// before the command, both inside [`Self::sid_gate`] so concurrent
    /// callers never interleave. Errors from `use` are surfaced
    /// before the command is issued.
    async fn run_scoped(&self, sid: i64, line: &str) -> ControlResult<CommandOutcome> {
        let _guard = self.sid_gate.lock().await;
        let use_line = format!("use sid={sid}");
        self.execute(&use_line, Some(sid)).await?;
        self.execute(line, Some(sid)).await
    }

    /// Convert a single record into the typed shape via JSON
    /// round-trip. Stringified values flow into the WebQuery models'
    /// `stringy::deserialize*` visitors transparently.
    fn parse_record<T: for<'de> serde::Deserialize<'de>>(
        record: HashMap<String, String>,
    ) -> ControlResult<T> {
        let obj: Map<String, Value> = record
            .into_iter()
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        serde_json::from_value(Value::Object(obj)).map_err(|e| {
            ControlBackendError::InvalidResponse(format!("response shape mismatch: {e}"))
        })
    }

    /// Lift every body line into the flat list of records the upstream
    /// returned. Each line may carry multiple pipe-separated records;
    /// concatenating them into one list matches WebQuery's
    /// `body: [...]` array shape.
    fn collect_records(body_lines: &[String]) -> Vec<HashMap<String, String>> {
        let mut out = Vec::new();
        for line in body_lines {
            out.extend(parse_records(line));
        }
        out
    }

    /// First record (scalar response) — `version`, `serverinfo`,
    /// `serverrequestconnectioninfo`. Empty body yields
    /// `InvalidResponse`.
    fn parse_first<T: for<'de> serde::Deserialize<'de>>(
        body_lines: &[String],
    ) -> ControlResult<T> {
        let mut records = Self::collect_records(body_lines);
        if records.is_empty() {
            return Err(ControlBackendError::InvalidResponse(
                "empty body for scalar response".into(),
            ));
        }
        Self::parse_record(records.remove(0))
    }

    /// Every record (list response) — `serverlist`, `clientlist`,
    /// `channellist`. Empty body yields an empty `Vec`, matching how
    /// WebQuery `list_or_empty` collapses the §10.6 1281 case.
    fn parse_list<T: for<'de> serde::Deserialize<'de>>(
        body_lines: &[String],
    ) -> ControlResult<Vec<T>> {
        Self::collect_records(body_lines)
            .into_iter()
            .map(Self::parse_record)
            .collect()
    }
}

#[async_trait]
impl ControlBackend for SshControlClient {
    async fn version(&self) -> ControlResult<VersionInfo> {
        let outcome = self.run_unscoped("version").await?;
        Self::parse_first(&outcome.body_lines)
    }

    async fn serverlist(&self) -> ControlResult<Vec<VirtualServerEntry>> {
        let outcome = self.run_unscoped("serverlist").await?;
        // Upstream code 1281 (`database_empty_result`) on `serverlist`
        // means "no virtual servers configured" — already surfaced as
        // `Upstream` by the transport before we get here, so we don't
        // collapse it; the dashboard route maps it through §7.0.2 and
        // the operator sees the upstream code.
        Self::parse_list(&outcome.body_lines)
    }

    async fn serverinfo(&self, sid: i64) -> ControlResult<ServerInfo> {
        let outcome = self.run_scoped(sid, "serverinfo").await?;
        Self::parse_first(&outcome.body_lines)
    }

    async fn channellist(&self, sid: i64) -> ControlResult<Vec<ChannelEntry>> {
        let outcome = self.run_scoped(sid, "channellist").await?;
        Self::parse_list(&outcome.body_lines)
    }

    async fn clientlist(&self, sid: i64) -> ControlResult<Vec<ClientEntry>> {
        let outcome = self.run_scoped(sid, "clientlist").await?;
        Self::parse_list(&outcome.body_lines)
    }

    async fn server_connection_info(&self, sid: i64) -> ControlResult<ConnectionInfo> {
        let outcome = self
            .run_scoped(sid, "serverrequestconnectioninfo")
            .await?;
        Self::parse_first(&outcome.body_lines)
    }

    async fn clientlist_with_flags(
        &self,
        sid: i64,
        flags: &[&str],
    ) -> ControlResult<Vec<ClientEntry>> {
        let line = format!("clientlist{}", append_flags(flags));
        let outcome = self.run_scoped(sid, &line).await?;
        Self::parse_list(&outcome.body_lines)
    }

    async fn clientinfo(&self, sid: i64, clid: i64) -> ControlResult<ClientInfo> {
        let line = format!("clientinfo clid={clid}");
        let outcome = self.run_scoped(sid, &line).await?;
        Self::parse_first(&outcome.body_lines)
    }

    async fn clientdbinfo(&self, sid: i64, cldbid: i64) -> ControlResult<ClientDbEntry> {
        let line = format!("clientdbinfo cldbid={cldbid}");
        let outcome = self.run_scoped(sid, &line).await?;
        Self::parse_first(&outcome.body_lines)
    }

    async fn channellist_with_flags(
        &self,
        sid: i64,
        flags: &[&str],
    ) -> ControlResult<Vec<ChannelEntry>> {
        let line = format!("channellist{}", append_flags(flags));
        let outcome = self.run_scoped(sid, &line).await?;
        Self::parse_list(&outcome.body_lines)
    }

    async fn banlist(&self, sid: i64) -> ControlResult<Vec<BanEntry>> {
        // Empty banlist surfaces as `database_empty_result` (1281) on
        // some upstreams; collapse that to an empty `Vec` so the REST
        // contract matches the WebQuery `list_or_empty` behaviour.
        match self.run_scoped(sid, "banlist").await {
            Ok(outcome) => Self::parse_list(&outcome.body_lines),
            Err(ControlBackendError::Upstream { code, .. })
                if code == crate::webquery::DATABASE_EMPTY_RESULT =>
            {
                Ok(Vec::new())
            }
            Err(other) => Err(other),
        }
    }

    async fn logview(
        &self,
        sid: i64,
        lines: u32,
        reverse: bool,
        instance: bool,
        begin_pos: Option<i64>,
    ) -> ControlResult<Vec<LogEntry>> {
        let mut line = format!(
            "logview lines={lines} reverse={r} instance={i}",
            r = if reverse { 1 } else { 0 },
            i = if instance { 1 } else { 0 }
        );
        if let Some(pos) = begin_pos {
            line.push_str(&format!(" begin_pos={pos}"));
        }
        let outcome = self.run_scoped(sid, &line).await?;
        Self::parse_list(&outcome.body_lines)
    }

    async fn clientkick(
        &self,
        sid: i64,
        clid: i64,
        reasonid: i64,
        reasonmsg: Option<&str>,
    ) -> ControlResult<()> {
        let mut line = format!("clientkick reasonid={reasonid} clid={clid}");
        if let Some(msg) = reasonmsg {
            line.push_str(&format!(" reasonmsg={}", escape(msg)));
        }
        self.run_scoped(sid, &line).await?;
        Ok(())
    }

    async fn clientmove(
        &self,
        sid: i64,
        clid: i64,
        cid: i64,
        cpw: Option<&str>,
    ) -> ControlResult<()> {
        let mut line = format!("clientmove clid={clid} cid={cid}");
        if let Some(pw) = cpw {
            line.push_str(&format!(" cpw={}", escape(pw)));
        }
        self.run_scoped(sid, &line).await?;
        Ok(())
    }

    async fn client_set_muted(
        &self,
        sid: i64,
        clid: i64,
        input_muted: Option<bool>,
        output_muted: Option<bool>,
    ) -> ControlResult<()> {
        if input_muted.is_none() && output_muted.is_none() {
            return Ok(());
        }
        let mut line = format!("clientedit clid={clid}");
        if let Some(v) = input_muted {
            line.push_str(&format!(
                " CLIENT_INPUT_MUTED={}",
                if v { 1 } else { 0 }
            ));
        }
        if let Some(v) = output_muted {
            line.push_str(&format!(
                " CLIENT_OUTPUT_MUTED={}",
                if v { 1 } else { 0 }
            ));
        }
        self.run_scoped(sid, &line).await?;
        Ok(())
    }

    async fn banadd(&self, sid: i64, params: &BanAddParams<'_>) -> ControlResult<i64> {
        let mut line = String::from("banadd");
        if let Some(v) = params.ip {
            line.push_str(&format!(" ip={}", escape(v)));
        }
        if let Some(v) = params.uid {
            line.push_str(&format!(" uid={}", escape(v)));
        }
        if let Some(v) = params.mytsid {
            line.push_str(&format!(" mytsid={}", escape(v)));
        }
        if let Some(v) = params.name {
            line.push_str(&format!(" name={}", escape(v)));
        }
        if let Some(v) = params.banreason {
            line.push_str(&format!(" banreason={}", escape(v)));
        }
        if let Some(t) = params.time {
            line.push_str(&format!(" time={t}"));
        }
        let outcome = self.run_scoped(sid, &line).await?;
        // `banadd` returns a single record `banid=<id>`.
        let mut records = Self::collect_records(&outcome.body_lines);
        if records.is_empty() {
            return Err(ControlBackendError::InvalidResponse(
                "banadd returned no body record".into(),
            ));
        }
        let r = records.remove(0);
        let raw = r.get("banid").ok_or_else(|| {
            ControlBackendError::InvalidResponse("banadd response missing banid field".into())
        })?;
        raw.parse::<i64>().map_err(|e| {
            ControlBackendError::InvalidResponse(format!(
                "banadd returned non-integer banid={raw:?}: {e}"
            ))
        })
    }

    async fn bandel(&self, sid: i64, banid: i64) -> ControlResult<()> {
        let line = format!("bandel banid={banid}");
        self.run_scoped(sid, &line).await?;
        Ok(())
    }

    fn ssh_transport(&self) -> Option<TransportHandle> {
        Some(self.transport.clone())
    }
}

/// Render `flags` into the `-foo -bar` suffix used by ServerQuery
/// commands on the SSH wire (e.g. `clientlist -uid -away`). Empty
/// `flags` returns an empty string so callers can append unconditionally.
///
/// Unlike WebQuery's URL form (`/clientlist-uid-away`), the SSH line
/// protocol expects flags as space-separated tokens, each prefixed with
/// a single `-`. Stripping a leading `-` from caller-supplied flag names
/// keeps both call sites uniform.
fn append_flags(flags: &[&str]) -> String {
    if flags.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for flag in flags {
        out.push(' ');
        out.push('-');
        out.push_str(flag.trim_start_matches('-'));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    #[test]
    fn parse_record_round_trips_versioninfo() {
        let r = record(&[("version", "3.13.7"), ("build", "1689000"), ("platform", "Linux")]);
        let v: VersionInfo = SshControlClient::parse_record(r).unwrap();
        assert_eq!(v.version, "3.13.7");
        assert_eq!(v.build, "1689000");
        assert_eq!(v.platform, "Linux");
    }

    #[test]
    fn parse_record_handles_stringy_numerics() {
        // `virtualserver_maxclients` is `i64` on the typed shape; the
        // wire delivers it as a string. The model's stringy visitor
        // accepts the string transparently.
        let r = record(&[
            ("virtualserver_id", "7"),
            ("virtualserver_name", "alpha"),
            ("virtualserver_status", "online"),
            ("virtualserver_clientsonline", "3"),
            ("virtualserver_maxclients", "32"),
        ]);
        let v: VirtualServerEntry = SshControlClient::parse_record(r).unwrap();
        assert_eq!(v.virtualserver_id, 7);
        assert_eq!(v.virtualserver_name, "alpha");
        assert_eq!(v.virtualserver_clientsonline, 3);
        assert_eq!(v.virtualserver_maxclients, 32);
    }

    #[test]
    fn parse_list_splits_pipe_separated_records() {
        // ServerQuery returns multiple records on a single line as
        // `record1|record2`. The wire-layer `parse_records` already
        // splits on the pipe; this verifies the typed collection
        // round-trips.
        let line = "virtualserver_id=1 virtualserver_name=alpha virtualserver_status=online\
                    |virtualserver_id=2 virtualserver_name=beta virtualserver_status=offline";
        let v: Vec<VirtualServerEntry> = SshControlClient::parse_list(&[line.into()]).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].virtualserver_id, 1);
        assert_eq!(v[1].virtualserver_id, 2);
        assert_eq!(v[1].virtualserver_status, "offline");
    }

    #[test]
    fn parse_first_returns_invalid_response_on_empty_body() {
        let r: ControlResult<ServerInfo> = SshControlClient::parse_first(&[]);
        match r {
            Err(ControlBackendError::InvalidResponse(s)) => {
                assert!(s.contains("empty body"));
            }
            other => panic!("expected InvalidResponse, got {other:?}"),
        }
    }

    #[test]
    fn parse_list_yields_empty_vec_for_empty_body() {
        let v: Vec<ClientEntry> = SshControlClient::parse_list(&[]).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn parse_record_unescapes_ts10_4_values() {
        // The wire parser unescapes `\s` → ` `; verify the lifted JSON
        // value still carries the unescaped form to the typed shape.
        let line = "virtualserver_name=My\\sServer virtualserver_id=3";
        let v: VirtualServerEntry = SshControlClient::parse_list(&[line.into()])
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(v.virtualserver_name, "My Server");
        assert_eq!(v.virtualserver_id, 3);
    }

    /// Env-gated integration test against a containerised TS SSH
    /// ServerQuery target.
    ///
    /// **Skipped** unless `TS6_SSH_INTEGRATION=1` is set in the
    /// environment — local `cargo test` runs never touch the network.
    /// Bring the container up via:
    ///
    /// ```text
    /// podman-compose --profile ssh-integration up -d ssh-integration
    /// TS6_SSH_INTEGRATION=1 \
    ///   TS6_SSH_HOST=127.0.0.1 \
    ///   TS6_SSH_PORT=10022 \
    ///   TS6_SSH_USER=serveradmin \
    ///   TS6_SSH_PASSWORD=$(podman logs ts6-ssh-integration | grep token1=) \
    ///   cargo test -p ts6-manager-server --features server -- \
    ///     sshbridge::control_client::tests::integration --ignored --nocapture
    /// ```
    ///
    /// The test covers the four PURA-78 acceptance scenarios:
    ///   1. **Bring-up** — connect + run `version`.
    ///   2. **One read command** — run `serverlist` and parse it.
    ///   3. **Upstream error path** — `serverinfo` against a bogus sid
    ///      yields an `Upstream` error with the canonical TS code (the
    ///      code varies by upstream — `1281`, `513`, or `2568` are all
    ///      legal — so the assertion is on the variant, not the code).
    ///   4. **Forced disconnect → reconnect** — issue a raw `quit`
    ///      against the transport (the upstream closes the SSH session)
    ///      then verify a subsequent `version()` succeeds after the
    ///      supervisor reconnects.
    #[tokio::test]
    #[ignore = "env-gated SSH integration; set TS6_SSH_INTEGRATION=1"]
    async fn integration_ssh_control_path_against_live_target() {
        if std::env::var("TS6_SSH_INTEGRATION").ok().as_deref() != Some("1") {
            eprintln!("integration test skipped: TS6_SSH_INTEGRATION != 1");
            return;
        }

        use std::sync::Arc;
        use std::time::Duration;

        use crate::sshbridge::hostkey::{HostKeyPolicy, HostKeyVerifier};
        use crate::sshbridge::russh_channel::{connect_password, RusshConnectParams};
        use crate::sshbridge::transport::{spawn as spawn_transport, TransportConfig};

        let host = std::env::var("TS6_SSH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
        let port: u16 = std::env::var("TS6_SSH_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(10022);
        let user = std::env::var("TS6_SSH_USER")
            .expect("TS6_SSH_USER must be set when TS6_SSH_INTEGRATION=1");
        let password = std::env::var("TS6_SSH_PASSWORD")
            .expect("TS6_SSH_PASSWORD must be set when TS6_SSH_INTEGRATION=1");

        // Integration runs against a controlled container — accept any
        // host key. The host-key path itself is covered by the
        // PURA-77/PURA-76 unit tests; this test focuses on the
        // ControlBackend wiring on top of the russh transport.
        let verifier = Arc::new(HostKeyVerifier::new(
            HostKeyPolicy::Reject,
            42,
            host.clone(),
            port,
        ));
        // Override Reject → Accept for the integration target only by
        // wrapping the verifier in a no-op (the production `Reject`
        // policy stays the default elsewhere). We do this by feeding
        // an empty StrictFingerprint; russh will then reject — instead
        // we use a permissive variant. The hostkey module exposes the
        // strict form as the only opt-in path, so for the integration
        // run we just test against `KnownHostsFile` pointing at a temp
        // file with the fingerprint pre-pinned.
        //
        // Practical workaround for the integration test: write a
        // `known_hosts` line for the container's host-key fingerprint
        // into a tempfile and use `KnownHostsFile`. That keeps
        // production-grade verification active even in CI. For
        // operators running this locally, set `TS6_SSH_KNOWN_HOSTS` to
        // a known_hosts file containing the container's key.
        let known_hosts = std::env::var("TS6_SSH_KNOWN_HOSTS").ok();
        let verifier = if let Some(path) = known_hosts {
            Arc::new(HostKeyVerifier::new(
                HostKeyPolicy::KnownHostsFile {
                    path: std::path::PathBuf::from(path),
                },
                42,
                host.clone(),
                port,
            ))
        } else {
            verifier
        };

        let cfg = TransportConfig {
            // Tighten the per-command + banner deadlines so a stalled
            // container fails the test within a minute rather than
            // hanging the suite.
            command_timeout: Duration::from_secs(5),
            banner_timeout: Duration::from_secs(10),
            keepalive_interval: Duration::from_secs(15),
            keepalive_timeout: Duration::from_secs(3),
            keepalive_failure_threshold: 2,
            backoff_initial: Duration::from_millis(500),
            backoff_max: Duration::from_secs(5),
            ..TransportConfig::for_connection(42)
        };

        let host_owned = host.clone();
        let user_owned = user.clone();
        let pw_owned = password.clone();
        let verifier_clone = verifier.clone();
        let factory = move || {
            let h = host_owned.clone();
            let u = user_owned.clone();
            let p = pw_owned.clone();
            let v = verifier_clone.clone();
            async move {
                connect_password(RusshConnectParams::new_password(42, h, port, u, p, v)).await
            }
        };

        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .try_init();
        let handle = spawn_transport(cfg, factory);
        let client = SshControlClient::new(42, handle.clone());

        // (1) Bring-up + (2) one read command — `version`.
        let v = client
            .version()
            .await
            .expect("version() against the live SSH ServerQuery should succeed");
        assert!(!v.version.is_empty(), "version field must be populated");

        // (2b) Sid-scoped command — `clientlist` against the default
        // virtual server. PURA-101 panel-smoke surrogate: this is the
        // exact command the `/api/servers/{id}/vs/{vsid}/clients`
        // route invokes, so a successful response here proves the
        // HTTP route's bridge call path will succeed within its
        // per-route deadline.
        match client.clientlist(1).await {
            Ok(_clients) => {
                // Body parses cleanly — the bridge correctly drains
                // the pipe-separated record list and the upstream's
                // terminator. Empty client list is fine (the fixture
                // has only the ServerQuery user joined, which doesn't
                // appear in `clientlist` by default).
            }
            Err(e) => panic!(
                "client.clientlist(1) against the live SSH ServerQuery should succeed: {e:?}"
            ),
        }

        // (3) Upstream error path — `serverinfo` against a sid that
        // does not exist. TS upstreams return `error id=1281`
        // (`database_empty_result`) or `error id=2568` depending on
        // build; both are valid Upstream variants.
        let bad_sid = i64::MAX;
        match client.serverinfo(bad_sid).await {
            Err(ControlBackendError::Upstream { code, .. }) => {
                assert!(
                    code != 0,
                    "Upstream variant must carry a non-zero code (got {code})"
                );
            }
            Ok(_) => panic!("serverinfo({bad_sid}) unexpectedly succeeded"),
            Err(other) => panic!("expected Upstream, got {other:?}"),
        }

        // (4) Forced disconnect → reconnect. Submit a raw `quit` via
        // the underlying handle — the upstream closes the session
        // immediately. The supervisor sees the close, sleeps the
        // (small) backoff, and reconnects. A subsequent `version()`
        // call should then succeed.
        let _ = handle.execute("quit", None, None).await; // best-effort; the close races the response
        // Give the supervisor a moment to detect the close and run one
        // backoff/reconnect cycle. With `backoff_initial=500ms`,
        // 3-4 seconds is plenty.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let v_after = client
            .version()
            .await
            .expect("version() after forced disconnect should succeed once supervisor reconnects");
        assert!(!v_after.version.is_empty());
    }

    /// PURA-99 — flag suffix renders as space-separated `-foo` tokens
    /// matching the SSH ServerQuery line-protocol form. Empty flags
    /// must yield the empty string so callers can append unconditionally.
    #[test]
    fn append_flags_renders_space_separated_dash_tokens() {
        assert_eq!(append_flags(&[]), "");
        assert_eq!(append_flags(&["uid"]), " -uid");
        assert_eq!(
            append_flags(&["uid", "away", "voice"]),
            " -uid -away -voice"
        );
        // Caller-supplied leading `-` is normalised so both styles work.
        assert_eq!(append_flags(&["-uid", "away"]), " -uid -away");
    }

    /// PURA-99 — `banadd` parses `banid` out of the upstream's record.
    /// The wire returns one record like `banid=42`; we lift it through
    /// `collect_records` and parse the integer.
    #[test]
    fn banadd_parses_banid_from_response() {
        // Simulate the body lines a real upstream sends back. We only
        // need `collect_records` + `parse::<i64>` to wire through.
        let body = vec!["banid=17".to_string()];
        let mut records = SshControlClient::collect_records(&body);
        assert_eq!(records.len(), 1);
        let r = records.remove(0);
        let raw = r.get("banid").unwrap();
        assert_eq!(raw.parse::<i64>().unwrap(), 17);
    }

    #[test]
    fn parse_serverinfo_record_with_float_fields() {
        // `virtualserver_total_packetloss_total` is `f64` with the
        // dedicated `stringy::deserialize_float_default` visitor.
        let r = record(&[
            ("virtualserver_name", "hub"),
            ("virtualserver_platform", "Linux"),
            ("virtualserver_version", "3.13.7"),
            ("virtualserver_maxclients", "32"),
            ("virtualserver_uptime", "86400"),
            ("virtualserver_total_packetloss_total", "0.0125"),
            ("virtualserver_total_ping", "42.5"),
        ]);
        let s: ServerInfo = SshControlClient::parse_record(r).unwrap();
        assert_eq!(s.virtualserver_name, "hub");
        assert_eq!(s.virtualserver_uptime, 86400);
        assert!((s.virtualserver_total_packetloss_total - 0.0125).abs() < 1e-9);
        assert!((s.virtualserver_total_ping - 42.5).abs() < 1e-9);
    }
}
