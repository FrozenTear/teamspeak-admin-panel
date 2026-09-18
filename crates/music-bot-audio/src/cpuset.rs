//! Linux cpuset helpers for Contabo Music+Voice isolation.
//!
//! **Packaging option (1) — intra-container send affinity.**
//!
//! Contabo is `nproc=6` (CPUs 0–5). **Apply-ready packing B**
//! (Robert): fullstack soft pin stays `2-5` (no shrink).
//!
//! - `TS6_BOT_SEND_CPUSET` (preferred) or `TS6_BOT_CPUSET` pins **only**
//!   the Voice send runtime threads (`voice-rt`) to `0-1`.
//! - [`pin_decode_child`] parks ffmpeg / yt-dlp / the Python warm
//!   resolver on `TS6_BOT_DECODE_CPUSET=2-5` (share Axum, never send
//!   `0-1`). Packing A (`DECODE=2-3` after fullstack→`4-5`) is a
//!   gated comment only. Packing C (HostConfig `0-1` + DECODE on send
//!   cores) is rejected by the apply script.
//! - A whole-container `podman update --cpuset-cpus=0-1` is **not**
//!   implemented. v1.6.15 Angerfist dig 163/590/117: that trap made
//!   `C_loop_deferral` worse.
//!
//! Both env vars accept Linux list syntax (`0-1`, `2-5`, `0,2,4`).
//! Empty / unset → no-op. Overlap between send and decode sets is
//! rejected at music-runtime boot when both are set.

use std::env;

/// Legacy / agreed send-thread pin key. Same meaning as
/// [`SEND_CPUSET_ENV_PREFERRED`] — **not** a container-wide HostConfig pin.
pub const SEND_CPUSET_ENV: &str = "TS6_BOT_CPUSET";
/// Preferred alias that makes the send-thread (not container) scope obvious.
pub const SEND_CPUSET_ENV_PREFERRED: &str = "TS6_BOT_SEND_CPUSET";
pub const DECODE_CPUSET_ENV: &str = "TS6_BOT_DECODE_CPUSET";

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

/// Pin `pid` (ffmpeg / yt-dlp / python resolver) to `TS6_BOT_DECODE_CPUSET`.
pub fn pin_decode_pid(pid: u32) {
    pin_pid_from_env(DECODE_CPUSET_ENV, pid);
}

/// Convenience after `tokio::process::Command::spawn`.
pub fn pin_decode_child(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        pin_decode_pid(pid);
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
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        tracing::debug!(?cpus, "cpuset pin skipped on non-linux");
        Ok(())
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut set = std::mem::zeroed::<libc::cpu_set_t>();
        libc::CPU_ZERO(&mut set);
        for &cpu in cpus {
            if cpu >= 1024 {
                return Err(format!("cpu index {cpu} exceeds CPU_SETSIZE"));
            }
            libc::CPU_SET(cpu, &mut set);
        }
        let rc = libc::sched_setaffinity(
            pid as libc::pid_t,
            std::mem::size_of::<libc::cpu_set_t>(),
            &set,
        );
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            return Err(format!("sched_setaffinity({pid}): {err}"));
        }
        Ok(())
    }
}

/// Apply `nice` to the calling thread (Linux per-thread nice).
pub fn nice_current_thread_from_env(env_key: &str) {
    let raw = match env::var(env_key) {
        Ok(v) if !v.trim().is_empty() => v,
        _ => return,
    };
    let nice: i32 = match raw.trim().parse() {
        Ok(v) => v,
        Err(_) => {
            tracing::warn!(env_key, raw, "invalid nice value");
            return;
        }
    };
    #[cfg(target_os = "linux")]
    unsafe {
        let rc = libc::setpriority(libc::PRIO_PROCESS, 0, nice);
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            tracing::warn!(env_key, nice, error = %err, "setpriority failed");
        } else {
            tracing::info!(env_key, nice, "niced current thread");
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = nice;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
