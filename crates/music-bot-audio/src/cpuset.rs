//! Linux cpuset helpers for Contabo Music+Voice isolation.
//!
//! **Packaging option (1) — intra-container send affinity.**
//!
//! Contabo is `nproc=6` (CPUs 0–5). **Apply-ready packing B**
//! (Robert): fullstack soft pin stays `2-5` (no shrink).
//!
//! - `TS6_BOT_SEND_CPUSET` (preferred) or `TS6_BOT_CPUSET` pins **only**
//!   the Voice send runtime threads (`voice-rt`) to `0-1`. Pipeline,
//!   fetch, bridge, and resolve run on `decode-rt` via
//!   [`pin_current_thread_decode`] (`TS6_BOT_DECODE_CPUSET=2-5`).
//! - [`install_decode_pre_exec`] parks ffmpeg / yt-dlp / the Python warm
//!   resolver on `TS6_BOT_DECODE_CPUSET=2-5` (share Axum, never send
//!   `0-1`) by calling `sched_setaffinity(0, …)` in a `pre_exec` hook,
//!   so the new image inherits the mask before any worker thread exists.
//!   [`pin_decode_child`] is only a post-spawn backup for the child
//!   leader; it cannot rewind threads that already ran on send cores.
//!   The same decode set pins `decode-rt` worker threads. Packing A
//!   (`DECODE=2-3` after fullstack→`4-5`) is a gated comment only.
//!   Packing C (HostConfig `0-1` + DECODE on send cores) is rejected
//!   by the apply script. Music HostConfig stays unset (or wide `0-5`).
//! - `TS6_BOT_NICE` is per-thread. [`nice_voice_rt_from_env`] walks
//!   `/proc/<pid>/task/*/comm` for `voice-rt`. A negative nice from
//!   uid 10001 is `EPERM` (no `CAP_SYS_NICE`). The host
//!   `scripts/apply-fullstack-soft-pin.sh` walk is one-shot after
//!   music `/health` and only sees tids that exist then. Tokio's
//!   blocking pool reuses this comm and is created later
//!   (`spawn_blocking` / `block_in_place`); Linux `clone` copies the
//!   spawning thread's nice, so a parent the walk already reniced
//!   passes that nice on, and a parent still at 0 does not. A music
//!   container restart drops the nice until the script runs again.
//!   `renice -p` on the container pid alone only changes the
//!   thread-group leader. Do not add `CAP_SYS_NICE` to paper over it.
//! - A whole-container `podman update --cpuset-cpus=0-1` is **not**
//!   implemented. v1.6.15 Angerfist dig 163/590/117: that trap made
//!   `C_loop_deferral` worse.
//!
//! Both env vars accept Linux list syntax (`0-1`, `2-5`, `0,2,4`).
//! Empty / unset → no-op. Overlap between send and decode sets is
//! rejected at music-runtime boot when both are set.

use std::env;
use std::io;
use std::path::Path;

/// Legacy / agreed send-thread pin key. Same meaning as
/// [`SEND_CPUSET_ENV_PREFERRED`] — **not** a container-wide HostConfig pin.
pub const SEND_CPUSET_ENV: &str = "TS6_BOT_CPUSET";
/// Preferred alias that makes the send-thread (not container) scope obvious.
pub const SEND_CPUSET_ENV_PREFERRED: &str = "TS6_BOT_SEND_CPUSET";
pub const DECODE_CPUSET_ENV: &str = "TS6_BOT_DECODE_CPUSET";
/// Shared bearer for the music control API. The runtime reads it at
/// startup and stores only a SHA-256 digest. Child processes are built
/// with [`music_command`] or [`std_music_command`], which remove this
/// variable so ffmpeg, yt-dlp (and any Deno it starts), and the warm
/// Python resolver do not inherit the value. Do not `remove_var` it in
/// the parent.
pub const MUSIC_RUNTIME_TOKEN_ENV: &str = "MUSIC_RUNTIME_TOKEN";
/// Host and in-process nice for Voice send threads. Negative values
/// need `CAP_SYS_NICE` inside the container; the host script is the
/// path that works for uid 10001.
pub const NICE_ENV: &str = "TS6_BOT_NICE";
/// `comm` of the dedicated voice runtime workers (`thread_name`).
pub const VOICE_RT_THREAD_COMM: &str = "voice-rt";

/// Env key actually consulted for the send-thread pin (`SEND` wins).
pub fn effective_send_env_key() -> &'static str {
    match env::var(SEND_CPUSET_ENV_PREFERRED) {
        Ok(v) if !v.trim().is_empty() => SEND_CPUSET_ENV_PREFERRED,
        _ => SEND_CPUSET_ENV,
    }
}

/// Parse a Linux cpuset list into CPU indexes.
///
/// Accepts `n`, `a-b`, and comma-separated combinations. Rejects empty
/// tokens, inverted ranges, and values that do not fit `usize`.
pub fn parse_cpuset(spec: &str) -> Result<Vec<usize>, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() {
            return Err("empty cpuset token".into());
        }
        if let Some((a, b)) = token.split_once('-') {
            let start: usize = a
                .trim()
                .parse()
                .map_err(|_| format!("invalid cpuset range start: {token}"))?;
            let end: usize = b
                .trim()
                .parse()
                .map_err(|_| format!("invalid cpuset range end: {token}"))?;
            if end < start {
                return Err(format!("inverted cpuset range: {token}"));
            }
            out.extend(start..=end);
        } else {
            let cpu: usize = token
                .parse()
                .map_err(|_| format!("invalid cpuset cpu: {token}"))?;
            out.push(cpu);
        }
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// True when both sets are non-empty and share at least one CPU.
pub fn sets_overlap(a: &[usize], b: &[usize]) -> bool {
    a.iter().any(|cpu| b.contains(cpu))
}

/// Fail when send and decode cpusets are both set and share a core.
pub fn validate_send_vs_decode() -> Result<(), String> {
    let send = cpuset_from_env(effective_send_env_key())?;
    let decode = cpuset_from_env(DECODE_CPUSET_ENV)?;
    if let (Some(send), Some(decode)) = (send, decode)
        && sets_overlap(&send, &decode)
    {
        return Err(format!(
            "{SEND_CPUSET_ENV}={send:?} overlaps {DECODE_CPUSET_ENV}={decode:?} — \
             ffmpeg/yt-dlp must not share Voice send-critical cores"
        ));
    }
    Ok(())
}

/// Read and parse `env_key`. `Ok(None)` when unset or empty.
pub fn cpuset_from_env(env_key: &str) -> Result<Option<Vec<usize>>, String> {
    match env::var(env_key) {
        Ok(raw) if !raw.trim().is_empty() => parse_cpuset(&raw).map(Some),
        _ => Ok(None),
    }
}

/// Pin the calling thread to the send cpuset (`TS6_BOT_SEND_CPUSET`
/// or `TS6_BOT_CPUSET`) when set. Does not pin ffmpeg/yt-dlp.
pub fn pin_current_thread_send() {
    pin_current_thread_from_env(effective_send_env_key());
}

/// Pin the calling thread to `TS6_BOT_DECODE_CPUSET` when set.
///
/// Used by `decode-rt` (pipeline / fetch / bridge / resolve). Never
/// the send cpuset. Empty / unset is a no-op.
pub fn pin_current_thread_decode() {
    pin_current_thread_from_env(DECODE_CPUSET_ENV);
}

/// Pin `pid` (ffmpeg / yt-dlp / python resolver) to `TS6_BOT_DECODE_CPUSET`.
pub fn pin_decode_pid(pid: u32) {
    pin_pid_from_env(DECODE_CPUSET_ENV, pid);
}

/// Convenience after `tokio::process::Command::spawn`.
///
/// This pins the child **leader** only. ffmpeg / yt-dlp / the warm
/// resolver create worker threads at startup; those threads keep
/// whatever mask was in force at `clone`. Call [`install_decode_pre_exec`]
/// before `spawn` so the mask is inherited from the first instruction.
pub fn pin_decode_child(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        pin_decode_pid(pid);
    }
}

/// Remove [`MUSIC_RUNTIME_TOKEN_ENV`] from a child environment.
///
/// [`music_command`] and [`std_music_command`] call this. The parent
/// keeps the variable. `std::env::remove_var` is unsafe once other
/// tokio threads are running, so only the `Command` is cleared.
/// [`install_decode_pre_exec`] calls this again so a token placed on
/// the command after construction is still removed before a decode
/// child starts.
pub fn strip_music_runtime_token(cmd: &mut std::process::Command) {
    cmd.env_remove(MUSIC_RUNTIME_TOKEN_ENV);
}

/// Tokio command that does not inherit [`MUSIC_RUNTIME_TOKEN_ENV`].
///
/// This is the spawn entry point for `tokio::process::Command` in the
/// music crates. Clippy `disallowed_methods` forbids `Command::new`
/// there, with an allow only on the line below. Affinity and nice stay
/// at the call site; decode children still call
/// [`install_decode_pre_exec`] before `spawn`.
pub fn music_command(program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    #[allow(clippy::disallowed_methods)]
    let mut cmd = tokio::process::Command::new(program);
    strip_music_runtime_token(cmd.as_std_mut());
    cmd
}

/// `std::process::Command` that does not inherit [`MUSIC_RUNTIME_TOKEN_ENV`].
///
/// Same rule as [`music_command`] for short-lived spawns that are not
/// decode children (cookie self-check, version probes, `pgrep`).
pub fn std_music_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(clippy::disallowed_methods)]
    let mut cmd = std::process::Command::new(program);
    strip_music_runtime_token(&mut cmd);
    cmd
}

/// Install a `pre_exec` hook that applies `TS6_BOT_DECODE_CPUSET` with
/// `sched_setaffinity(0, …)` before `exec`.
///
/// Always strips [`MUSIC_RUNTIME_TOKEN_ENV`], including when the cpuset
/// is unset, including a token set on the command after [`music_command`].
/// Unset or empty cpuset → no affinity hook. Invalid spec → warn and no
/// affinity hook. When the hook is installed, a failed
/// `sched_setaffinity` fails the spawn so a decode child is not left
/// free to run on send cores.
pub fn install_decode_pre_exec(cmd: &mut tokio::process::Command) {
    strip_music_runtime_token(cmd.as_std_mut());
    match cpuset_from_env(DECODE_CPUSET_ENV) {
        Ok(Some(cpus)) if !cpus.is_empty() => {
            if let Err(err) = install_affinity_pre_exec(cmd, &cpus) {
                tracing::warn!(
                    env_key = DECODE_CPUSET_ENV,
                    error = %err,
                    "failed to install decode cpuset pre_exec hook"
                );
            } else {
                tracing::debug!(
                    env_key = DECODE_CPUSET_ENV,
                    ?cpus,
                    "installed decode cpuset pre_exec hook"
                );
            }
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(env_key = DECODE_CPUSET_ENV, error = %err, "invalid cpuset env")
        }
    }
}

/// `pre_exec` hook: `sched_setaffinity(0, cpus)` in the child before `exec`.
///
/// Empty `cpus` installs nothing. Non-Linux is a no-op (same as [`pin_pid`]).
/// The closure only issues the affinity syscall — no allocation, no
/// tracing — so it stays async-signal-safe across `fork`.
pub fn install_affinity_pre_exec(
    cmd: &mut tokio::process::Command,
    cpus: &[usize],
) -> Result<(), String> {
    if cpus.is_empty() {
        return Ok(());
    }
    validate_cpu_indexes(cpus)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cmd;
        tracing::debug!(?cpus, "decode pre_exec pin skipped on non-linux");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let cpus = cpus.to_vec();
        // Safety: the hook is async-signal-safe (sched_setaffinity + errno only).
        unsafe {
            cmd.pre_exec(move || apply_affinity_current(&cpus));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn apply_affinity_current(cpus: &[usize]) -> io::Result<()> {
    // Safety: `set` is a local cpu_set_t; CPU_SET indexes were checked
    // in the parent against CPU_SETSIZE before the hook was installed.
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut set);
        for &cpu in cpus {
            libc::CPU_SET(cpu, &mut set);
        }
        let rc = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if rc != 0 {
            let err = *libc::__errno_location();
            return Err(io::Error::from_raw_os_error(err));
        }
        Ok(())
    }
}

pub fn pin_current_thread_from_env(env_key: &str) {
    match cpuset_from_env(env_key) {
        Ok(Some(cpus)) if !cpus.is_empty() => {
            if let Err(err) = pin_current_thread(&cpus) {
                tracing::warn!(
                    env_key,
                    error = %err,
                    "failed to pin current thread to cpuset"
                );
            } else {
                tracing::info!(env_key, ?cpus, "pinned current thread to cpuset");
            }
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(env_key, error = %err, "invalid cpuset env"),
    }
}

pub fn pin_pid_from_env(env_key: &str, pid: u32) {
    match cpuset_from_env(env_key) {
        Ok(Some(cpus)) if !cpus.is_empty() => {
            if let Err(err) = pin_pid(pid, &cpus) {
                tracing::warn!(
                    env_key,
                    pid,
                    error = %err,
                    "failed to pin child to decode cpuset"
                );
            } else {
                tracing::debug!(env_key, pid, ?cpus, "pinned child to decode cpuset");
            }
        }
        Ok(_) => {}
        Err(err) => tracing::warn!(env_key, error = %err, "invalid cpuset env"),
    }
}

pub fn pin_current_thread(cpus: &[usize]) -> Result<(), String> {
    pin_pid(0, cpus)
}

pub fn pin_pid(pid: u32, cpus: &[usize]) -> Result<(), String> {
    if cpus.is_empty() {
        return Ok(());
    }
    validate_cpu_indexes(cpus)?;
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        tracing::debug!(?cpus, "cpuset pin skipped on non-linux");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        // Safety: indexes were checked against CPU_SETSIZE.
        let rc = unsafe {
            let mut set = std::mem::zeroed::<libc::cpu_set_t>();
            libc::CPU_ZERO(&mut set);
            for &cpu in cpus {
                libc::CPU_SET(cpu, &mut set);
            }
            libc::sched_setaffinity(
                pid as libc::pid_t,
                std::mem::size_of::<libc::cpu_set_t>(),
                &set,
            )
        };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("sched_setaffinity({pid}): {err}"));
        }
        Ok(())
    }
}

fn validate_cpu_indexes(cpus: &[usize]) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let limit = libc::CPU_SETSIZE as usize;
    #[cfg(not(target_os = "linux"))]
    let limit = 1024usize;
    for &cpu in cpus {
        if cpu >= limit {
            return Err(format!("cpu index {cpu} exceeds CPU_SETSIZE"));
        }
    }
    Ok(())
}

/// `comm` file bytes match `expected`.
///
/// Linux writes `TASK_COMM_LEN - 1` characters plus a trailing newline.
/// A truncated longer name must not match a shorter expected comm.
pub fn comm_matches(contents: &str, expected: &str) -> bool {
    let name = contents.trim_matches(|c: char| c == '\n' || c == '\r' || c == '\0' || c == ' ');
    !expected.is_empty() && name == expected
}

/// Tids under `{proc_root}/{pid}/task/*/comm` whose comm equals `expected`.
///
/// Missing task directory → empty list (the process or thread group is
/// already gone). A comm file that disappears mid-walk is skipped.
pub fn tids_with_comm_under(proc_root: &Path, pid: u32, expected: &str) -> io::Result<Vec<u32>> {
    let task_dir = proc_root.join(pid.to_string()).join("task");
    let entries = match std::fs::read_dir(&task_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    let mut tids = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(tid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        match std::fs::read_to_string(entry.path().join("comm")) {
            Ok(contents) if comm_matches(&contents, expected) => tids.push(tid),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    tids.sort_unstable();
    Ok(tids)
}

/// `voice-rt` tids of `pid` as seen in `/proc`.
pub fn voice_rt_tids(pid: u32) -> io::Result<Vec<u32>> {
    tids_with_comm_under(Path::new("/proc"), pid, VOICE_RT_THREAD_COMM)
}

/// Apply `nice` to one tid.
///
/// `tid == 0` is the calling thread (`PRIO_PROCESS` / who 0). Any other
/// tid is that thread only — Linux nice is not a process-wide attribute,
/// so the thread-group leader's nice does not cover `voice-rt`.
pub fn nice_tid(tid: u32, nice: i32) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (tid, nice);
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            Err(format!("setpriority({tid}, {nice}): {err}"))
        } else {
            Ok(())
        }
    }
}

/// Parse `env_key` as a nice value. `Ok(None)` when unset or empty.
pub fn nice_from_env(env_key: &str) -> Result<Option<i32>, String> {
    match env::var(env_key) {
        Ok(raw) if !raw.trim().is_empty() => raw
            .trim()
            .parse::<i32>()
            .map(Some)
            .map_err(|_| format!("invalid nice value: {raw}")),
        _ => Ok(None),
    }
}

/// `setpriority` every tid of `pid` whose comm equals `comm`.
pub fn nice_tids_with_comm(pid: u32, comm: &str, nice: i32) -> Result<Vec<u32>, String> {
    let tids =
        tids_with_comm_under(Path::new("/proc"), pid, comm).map_err(|err| err.to_string())?;
    let mut failed = Vec::new();
    for tid in &tids {
        if let Err(err) = nice_tid(*tid, nice) {
            failed.push(format!("{tid}: {err}"));
        }
    }
    if failed.is_empty() {
        Ok(tids)
    } else {
        Err(format!(
            "setpriority failed for {} of {} {comm} tids: {}",
            failed.len(),
            tids.len(),
            failed.join("; ")
        ))
    }
}

/// Apply `nice` to the calling thread (Linux per-thread nice).
pub fn nice_current_thread_from_env(env_key: &str) {
    let nice = match nice_from_env(env_key) {
        Ok(Some(nice)) => nice,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(env_key, error = %err, "invalid nice value");
            return;
        }
    };
    match nice_tid(0, nice) {
        Ok(()) => tracing::info!(env_key, nice, "niced current thread"),
        Err(err) => tracing::warn!(env_key, nice, error = %err, "setpriority failed"),
    }
}

/// Once `voice-rt` workers exist, renice each of them to [`NICE_ENV`].
///
/// In-process `setpriority` of a negative nice is `EPERM` for uid
/// 10001. Failure is logged. `scripts/apply-fullstack-soft-pin.sh`
/// performs the same comm walk as root on the host, once, after music
/// `/health`. Tids created later share this comm (tokio's blocking
/// pool) and inherit the spawning thread's nice; the walk does not
/// run again. A music process restart drops the nice until that
/// script runs. Opus #66 L16.
pub fn nice_voice_rt_from_env() {
    let nice = match nice_from_env(NICE_ENV) {
        Ok(Some(nice)) => nice,
        Ok(None) => return,
        Err(err) => {
            tracing::warn!(env_key = NICE_ENV, error = %err, "invalid nice value");
            return;
        }
    };
    let pid = std::process::id();
    match nice_tids_with_comm(pid, VOICE_RT_THREAD_COMM, nice) {
        Ok(tids) if tids.is_empty() => {
            tracing::warn!(pid, nice, "no voice-rt tids to apply TS6_BOT_NICE");
        }
        Ok(tids) => {
            tracing::info!(pid, ?tids, nice, "applied TS6_BOT_NICE to voice-rt tids");
        }
        Err(err) => {
            tracing::warn!(
                pid,
                nice,
                error = %err,
                "in-process voice-rt renice failed (expected EPERM for a negative \
                 TS6_BOT_NICE without CAP_SYS_NICE). Host \
                 apply-fullstack-soft-pin.sh is a one-shot voice-rt comm walk \
                 after /health; a music restart drops that nice until the \
                 script runs again. Later voice-rt tids (tokio blocking pool) \
                 inherit the spawning thread's nice"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawned_child_does_not_inherit_music_runtime_token() {
        let token = "child-must-not-see-this-token";
        let mut cmd = music_command("sh");
        cmd.arg("-c")
            .arg(format!(
                "printf '%s' \"${{{MUSIC_RUNTIME_TOKEN_ENV}-UNSET}}\""
            ))
            // The constructor already removed the variable. Putting it
            // back here checks that `install_decode_pre_exec` still
            // clears a token placed on the command before spawn.
            .env(MUSIC_RUNTIME_TOKEN_ENV, token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        install_decode_pre_exec(&mut cmd);
        let output = cmd.output().await.expect("spawn sh");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert_eq!(
            stdout, "UNSET",
            "decode child inherited the token: {stdout:?}"
        );
        assert!(!stdout.contains(token));
    }

    #[test]
    fn std_command_child_does_not_inherit_music_runtime_token() {
        let token = "std-child-must-not-see-this-token";
        let mut cmd = std_music_command("sh");
        cmd.arg("-c")
            .arg(format!(
                "printf '%s' \"${{{MUSIC_RUNTIME_TOKEN_ENV}-UNSET}}\""
            ))
            // The constructor already removed the variable. Putting it
            // back, then applying the same removal the constructor uses,
            // proves a present value does not reach the child.
            .env(MUSIC_RUNTIME_TOKEN_ENV, token)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        strip_music_runtime_token(&mut cmd);
        let output = cmd.output().expect("spawn sh");
        let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
        assert_eq!(stdout, "UNSET", "std child inherited the token: {stdout:?}");
        assert!(!stdout.contains(token));
    }

    /// `std_music_command` is not a decode child: no `pre_exec` pin.
    /// The child must inherit the caller's CPU affinity. A decode cpuset
    /// hooked onto this constructor would fail the equality. The
    /// constructor's own `env_remove` is asserted here with no second
    /// `strip_music_runtime_token` call.
    #[cfg(target_os = "linux")]
    #[test]
    fn std_music_command_child_inherits_parent_cpuset() {
        let parent = current_affinity(0);
        assert!(!parent.is_empty(), "parent affinity must be non-empty");
        let mut cmd = std_music_command("sleep");
        assert!(
            removes_runtime_token(&cmd),
            "std_music_command must env_remove {MUSIC_RUNTIME_TOKEN_ENV} in the constructor"
        );
        cmd.arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = cmd.spawn().expect("spawn sleep");
        let pid = child.id();
        let child_aff = current_affinity(pid);
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(
            child_aff, parent,
            "std_music_command must inherit the parent cpuset, not a decode pin"
        );
    }

    #[test]
    fn music_command_strips_runtime_token() {
        let std_cmd = std_music_command("sh");
        assert!(
            removes_runtime_token(&std_cmd),
            "std_music_command must env_remove {MUSIC_RUNTIME_TOKEN_ENV}"
        );
        let tokio_cmd = music_command("sh");
        assert!(
            removes_runtime_token(tokio_cmd.as_std()),
            "music_command must env_remove {MUSIC_RUNTIME_TOKEN_ENV}"
        );
    }

    fn removes_runtime_token(cmd: &std::process::Command) -> bool {
        cmd.get_envs()
            .any(|(key, value)| key == MUSIC_RUNTIME_TOKEN_ENV && value.is_none())
    }

    #[test]
    fn parse_single_and_range() {
        assert_eq!(parse_cpuset("6-7").unwrap(), vec![6, 7]);
        assert_eq!(parse_cpuset("0-1,6-7").unwrap(), vec![0, 1, 6, 7]);
        assert_eq!(parse_cpuset("0,2,4").unwrap(), vec![0, 2, 4]);
        assert_eq!(parse_cpuset(" 3 ").unwrap(), vec![3]);
        assert!(parse_cpuset("").unwrap().is_empty());
    }

    #[test]
    fn parse_rejects_inverted_and_garbage() {
        assert!(parse_cpuset("7-6").is_err());
        assert!(parse_cpuset("a-b").is_err());
        assert!(parse_cpuset("1,,2").is_err());
    }

    #[test]
    fn overlap_detects_shared_send_and_decode() {
        assert!(sets_overlap(&[6, 7], &[7, 8]));
        assert!(!sets_overlap(&[6, 7], &[0, 1]));
        assert!(!sets_overlap(&[], &[0]));
    }

    #[test]
    fn contabo_nproc6_send_0_1_does_not_overlap_empty_decode() {
        let send = parse_cpuset("0-1").unwrap();
        assert_eq!(send, vec![0, 1]);
        assert!(!sets_overlap(&send, &[]));
    }

    #[test]
    fn packing_a_send_0_1_does_not_overlap_decode_2_3() {
        let send = parse_cpuset("0-1").unwrap();
        let decode = parse_cpuset("2-3").unwrap();
        assert!(!sets_overlap(&send, &decode));
    }

    #[test]
    fn packing_b_send_0_1_does_not_overlap_decode_2_5() {
        let send = parse_cpuset("0-1").unwrap();
        let decode = parse_cpuset("2-5").unwrap();
        assert!(!sets_overlap(&send, &decode));
    }

    #[test]
    fn packing_c_send_overlaps_decode_inside_0_1() {
        let send = parse_cpuset("0-1").unwrap();
        let decode = parse_cpuset("0-1").unwrap();
        assert!(sets_overlap(&send, &decode));
    }

    #[test]
    fn comm_matches_trims_proc_newline_and_rejects_prefixes() {
        assert!(comm_matches("voice-rt\n", "voice-rt"));
        assert!(comm_matches("voice-rt\0\n", "voice-rt"));
        assert!(comm_matches("voice-rt\r\n", "voice-rt"));
        assert!(!comm_matches("voice-runtime\n", "voice-rt"));
        assert!(!comm_matches("tokio-runtime-w\n", "voice-rt"));
        assert!(!comm_matches("\n", "voice-rt"));
        assert!(!comm_matches("voice-rt\n", ""));
    }

    #[test]
    fn tids_with_comm_under_selects_only_named_tasks() {
        let root = std::env::temp_dir().join(format!(
            "cpuset-proc-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let pid = 42u32;
        write_comm(&root, pid, 1, "ts6-manager-mu\n");
        write_comm(&root, pid, 11, "voice-rt\n");
        write_comm(&root, pid, 12, "tokio-runtime-w\n");
        write_comm(&root, pid, 13, "voice-rt\n");
        let tids = tids_with_comm_under(&root, pid, VOICE_RT_THREAD_COMM).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(tids, vec![11, 13]);
        assert!(
            tids_with_comm_under(&root, pid, VOICE_RT_THREAD_COMM)
                .unwrap()
                .is_empty()
        );
    }

    fn write_comm(root: &std::path::Path, pid: u32, tid: u32, comm: &str) {
        let dir = root
            .join(pid.to_string())
            .join("task")
            .join(tid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("comm"), comm).unwrap();
    }

    #[cfg(target_os = "linux")]
    fn current_affinity(pid: u32) -> Vec<usize> {
        unsafe {
            let mut set = std::mem::zeroed::<libc::cpu_set_t>();
            let rc = libc::sched_getaffinity(
                pid as libc::pid_t,
                std::mem::size_of::<libc::cpu_set_t>(),
                &mut set,
            );
            assert_eq!(
                rc,
                0,
                "sched_getaffinity: {}",
                std::io::Error::last_os_error()
            );
            let mut out = Vec::new();
            for cpu in 0..libc::CPU_SETSIZE as usize {
                if libc::CPU_ISSET(cpu, &set) {
                    out.push(cpu);
                }
            }
            out
        }
    }

    #[cfg(target_os = "linux")]
    fn thread_nice(tid: u32) -> i32 {
        unsafe {
            *libc::__errno_location() = 0;
            let rc = libc::getpriority(libc::PRIO_PROCESS, tid);
            assert!(
                rc != -1 || *libc::__errno_location() == 0,
                "getpriority({tid}): {}",
                std::io::Error::last_os_error()
            );
            rc
        }
    }

    /// H1: the child must already be on the decode mask when `spawn`
    /// returns. The only pin in this test is the pre_exec hook — there
    /// is no post-spawn `pin_pid`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pre_exec_sets_child_affinity_before_spawn_returns() {
        let parent = current_affinity(0);
        assert!(!parent.is_empty(), "parent affinity must be non-empty");
        let cpu = parent[0];
        let mut cmd = music_command("sleep");
        cmd.arg("30")
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        install_affinity_pre_exec(&mut cmd, &[cpu]).unwrap();
        let mut child = cmd.spawn().expect("spawn sleep with decode pre_exec");
        let pid = child.id().expect("child pid");
        let child_aff = current_affinity(pid);
        let _ = child.start_kill();
        let _ = child.wait().await;
        assert_eq!(child_aff, vec![cpu]);
        if parent.len() >= 2 {
            assert_ne!(
                child_aff, parent,
                "pre_exec must narrow the child below the parent mask"
            );
        }
    }

    /// H3: nice is per-tid. Raising one `voice-rt` thread must not change
    /// the thread-group leader — which is what `renice -p <container pid>`
    /// does, and why send threads stayed at the default.
    ///
    /// `comm` is published inside the new thread, and a one-shot
    /// `/proc/<pid>/task` readdir can miss that tid while other tests
    /// are spawning threads. The worker waits until its own comm matches
    /// before publishing `started`. The parent retries the walk.
    #[cfg(target_os = "linux")]
    #[test]
    fn nice_targets_voice_rt_tid_and_not_the_leader() {
        let leader = std::process::id();
        let leader_before = thread_nice(leader);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let tid_slot = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let started = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_t = std::sync::Arc::clone(&stop);
        let tid_t = std::sync::Arc::clone(&tid_slot);
        let started_t = std::sync::Arc::clone(&started);
        let handle = std::thread::Builder::new()
            .name(VOICE_RT_THREAD_COMM.into())
            .spawn(move || {
                let tid = std::fs::read_link("/proc/thread-self")
                    .ok()
                    .and_then(|p| {
                        p.file_name()
                            .and_then(|s| s.to_str())
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(0);
                tid_t.store(tid, std::sync::atomic::Ordering::SeqCst);
                let comm_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                loop {
                    let comm =
                        std::fs::read_to_string("/proc/thread-self/comm").unwrap_or_default();
                    if comm_matches(&comm, VOICE_RT_THREAD_COMM)
                        || std::time::Instant::now() >= comm_deadline
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                started_t.store(true, std::sync::atomic::Ordering::SeqCst);
                while !stop_t.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
            .unwrap();
        while !started.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let tid = tid_slot.load(std::sync::atomic::Ordering::SeqCst);
        assert_ne!(tid, 0, "voice-rt thread published a tid");
        assert_ne!(tid, leader, "worker tid must differ from the leader");
        let walk_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut found = voice_rt_tids(leader).unwrap();
        while !found.contains(&tid) && std::time::Instant::now() < walk_deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
            found = voice_rt_tids(leader).unwrap();
        }
        assert!(
            found.contains(&tid),
            "comm walk must find the voice-rt tid {tid}, got {found:?}"
        );

        let before = thread_nice(tid);
        if before < 19 {
            nice_tid(tid, before + 1).unwrap();
            assert_eq!(thread_nice(tid), before + 1);
            assert_eq!(
                thread_nice(leader),
                leader_before,
                "renicing a voice-rt tid must not be a leader-only nice"
            );
        } else {
            nice_tid(tid, before).unwrap();
        }

        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        handle.join().unwrap();
    }

    /// Linux `clone` copies the caller's nice. A `voice-rt` tid created
    /// after the host walk therefore keeps the spawning thread's nice;
    /// it does not reset to 0 when that parent was already reniced.
    /// Opus #66 L16. Raising nice needs no capability; the raised value
    /// dies with this thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn spawned_thread_inherits_caller_nice() {
        let parent_nice = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(i32::MIN));
        let child_nice = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(i32::MIN));
        let parent_slot = std::sync::Arc::clone(&parent_nice);
        let child_slot = std::sync::Arc::clone(&child_nice);
        let handle = std::thread::spawn(move || {
            let before = thread_nice(0);
            let target = before.saturating_add(3);
            if target > 19 || nice_tid(0, target).is_err() {
                return;
            }
            let now = thread_nice(0);
            parent_slot.store(now, std::sync::atomic::Ordering::SeqCst);
            let child_slot = std::sync::Arc::clone(&child_slot);
            let child = std::thread::spawn(move || {
                child_slot.store(thread_nice(0), std::sync::atomic::Ordering::SeqCst);
            });
            child.join().unwrap();
        });
        handle.join().unwrap();
        let parent = parent_nice.load(std::sync::atomic::Ordering::SeqCst);
        let child = child_nice.load(std::sync::atomic::Ordering::SeqCst);
        assert_ne!(
            parent,
            i32::MIN,
            "this thread must be able to raise its own nice"
        );
        assert_ne!(child, i32::MIN, "child must publish its nice");
        assert_eq!(
            child, parent,
            "a thread spawned after renice inherits that nice (Opus #66 L16)"
        );
    }
}
