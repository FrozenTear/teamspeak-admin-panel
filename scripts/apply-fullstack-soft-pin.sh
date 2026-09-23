#!/usr/bin/env bash
# Re-apply Contabo soft CPU pin + nice after podman kube play.
# Kube YAML does not persist HostConfig cpuset or process nice.
#
# Usage (from any cwd, against a repo checkout):
#   ./scripts/apply-fullstack-soft-pin.sh
#
# Config: deploy/contabo/soft-pin.env (or TS6_SOFT_PIN_ENV).
#
# APPLY-READY DEFAULT: packing B — fullstack 2-5 / nice -5 (no shrink).
# DECODE 2-5 is kube env (pre_exec sched_setaffinity before exec, with
# pin_decode_child as a leader-thread backup). This script does not inject it.
# Packing A (fullstack 4-5) is gated — requires TS6_SOFT_PIN_SHRINK_ACK=1.
# Packing B refuses any music container HostConfig cpuset, not only
# literal send-only 0-1. Music HostConfig stays unset. SEND 0-1 and
# DECODE 2-5 are in-process kube env, not HostConfig.
# Packing C (music HostConfig on send-only cores) is always refused,
# including when a shrink ACK is set.
# Music nice: renice the container leader AND every tid whose comm is
# voice-rt. Linux nice is per-thread; renice -p on the leader does not
# reach send threads. The music process starts that runtime before /health.
# Sidecar stays unpinned unless TS6_SIDECAR_* are set.
#
# podman update failure is fatal when a cpuset was requested.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SOFT_PIN_ENV="${TS6_SOFT_PIN_ENV:-${REPO_ROOT}/deploy/contabo/soft-pin.env}"
CONTAINER="${TS6_FULLSTACK_CONTAINER:-ts6-manager-fullstack}"
BOT_CONTAINER="${TS6_BOT_CONTAINER:-ts6-manager-music}"
SIDECAR_CONTAINER="${TS6_SIDECAR_CONTAINER:-ts6-manager-sidecar}"

normalize_cpuset() {
    echo "${1//[[:space:]]/}"
}

# Packing C: music HostConfig on send-only cores traps ffmpeg on 0-1.
is_send_only_music_cpuset() {
    local spec
    spec="$(normalize_cpuset "$1")"
    case "$spec" in
        0-1|0,1|1,0|0|1) return 0 ;;
        *) return 1 ;;
    esac
}

# Packing B is the apply-ready default. Packing A is a fullstack shrink
# together with TS6_SOFT_PIN_SHRINK_ACK=1. ACK without a shrink is still
# packing B, so a music cpuset cannot be applied beside live fullstack 2-5.
packing_b_in_effect() {
    if [[ "${TS6_SOFT_PIN_SHRINK_ACK:-}" == "1" ]] && [[ -n "${CPUSET:-}" ]] && is_fullstack_shrink_from_live "$CPUSET"; then
        return 1
    fi
    return 0
}

# Refuse music container HostConfig under packing B (any cpuset) and
# always refuse send-only cores. Empty means unset and is allowed.
# Prints a warning plus the reason on stderr. Returns 1 when refused.
music_hostconfig_allowed() {
    local spec
    spec="$(normalize_cpuset "$1")"
    if [[ -z "$spec" ]]; then
        return 0
    fi
    if is_send_only_music_cpuset "$spec"; then
        echo "warning: TS6_BOT_CONTAINER_CPUSET=${spec} is a music container HostConfig cpuset on send-only cores." >&2
        echo "error: packing C refused — container-wide send cores trap ffmpeg on the wire-send path (Angerfist v1.6.15: und/C/stall 163/590/117)." >&2
        echo "  Packing B keeps music HostConfig unset. SEND 0-1 and DECODE 2-5 are in-process, not HostConfig." >&2
        return 1
    fi
    if packing_b_in_effect; then
        echo "warning: TS6_BOT_CONTAINER_CPUSET=${spec} sets a music container HostConfig cpuset under packing B." >&2
        echo "error: packing B refuses any music HostConfig cpuset, not only literal 0-1. Music HostConfig stays unset." >&2
        echo "  Fullstack HostConfig stays 2-5. SEND 0-1 / DECODE 2-5 stay in-process (kube env)." >&2
        echo "  A wider music cpuset (for example 0-3) is packing A: fullstack shrink plus TS6_SOFT_PIN_SHRINK_ACK=1." >&2
        return 1
    fi
    return 0
}

# Profile A shrinks live 2-5. Refuse unless Robert ACK is set.
is_fullstack_shrink_from_live() {
    local spec
    spec="$(normalize_cpuset "$1")"
    case "$spec" in
        4-5|3-5|4,5|5,4|3,4,5|3-4,5|5|4) return 0 ;;
        *) return 1 ;;
    esac
}

apply_cpuset() {
    local name="$1"
    local cpuset="$2"
    if [[ -z "$cpuset" ]]; then
        return 0
    fi
    echo "==> podman update --cpuset-cpus=${cpuset} ${name}"
    if ! podman update --cpuset-cpus="$cpuset" "$name"; then
        echo "error: podman update --cpuset-cpus=${cpuset} ${name} failed" >&2
        return 1
    fi
}

container_pid() {
    local name="$1"
    if ! podman container exists "$name"; then
        echo "error: container ${name} missing" >&2
        return 1
    fi
    local pid
    pid="$(podman inspect -f '{{.State.Pid}}' "$name")"
    if [[ -z "$pid" || "$pid" == "0" ]]; then
        echo ""
        return 0
    fi
    echo "$pid"
}

# Tests point this at a fake tree. Production reads the host /proc.
proc_task_dir() {
    local pid="$1"
    echo "${TS6_SOFT_PIN_PROC_ROOT:-/proc}/${pid}/task"
}

# Renice every tid of `pid` whose comm equals `want`.
# The count is stored in the nameref given as the fourth argument.
# Logs stay on stdout so `update.sh` shows which tids moved.
renice_matching_comm() {
    local pid="$1"
    local nice="$2"
    local want="$3"
    local -n _found="$4"
    local task_dir comm tid name nmatched=0
    task_dir="$(proc_task_dir "$pid")"
    _found=0
    if [[ ! -d "$task_dir" ]]; then
        return 0
    fi
    for comm in "$task_dir"/*/comm; do
        [[ -f "$comm" ]] || continue
        name="$(tr -d '\r\n\0' < "$comm" || true)"
        if [[ "$name" != "$want" ]]; then
            continue
        fi
        tid="$(basename "$(dirname "$comm")")"
        echo "==> renice -n ${nice} -p ${tid} (${want} tid of pid ${pid})"
        if ! renice -n "$nice" -p "$tid"; then
            echo "error: renice -n ${nice} -p ${tid} (${want}) failed" >&2
            return 1
        fi
        nmatched=$((nmatched + 1))
    done
    _found=$nmatched
}

# voice-rt is created at music-process boot (before /health) but a
# short retry covers a process that is still starting its runtime.
# Missing tids warn and do not fail the fullstack pin: an older music
# image has no such threads yet. A renice that finds a tid and fails
# is fatal.
renice_voice_rt_tasks() {
    local pid="$1"
    local nice="$2"
    local attempts="${TS6_BOT_NICE_RETRIES:-10}"
    local pause="${TS6_BOT_NICE_RETRY_SLEEP:-1}"
    local try found=0
    for ((try = 1; try <= attempts; try++)); do
        renice_matching_comm "$pid" "$nice" "voice-rt" found || return 1
        if [[ "$found" -gt 0 ]]; then
            echo "==> reniced ${found} voice-rt tid(s) of pid ${pid} to ${nice}"
            return 0
        fi
        if [[ "$try" -lt "$attempts" ]]; then
            sleep "$pause"
        fi
    done
    echo "warning: no voice-rt tids for pid ${pid} after ${attempts} tries; TS6_BOT_NICE=${nice} did not reach send threads. Re-run after the music voice runtime is up." >&2
    return 0
}

apply_nice() {
    local name="$1"
    local nice="$2"
    if [[ -z "$nice" ]]; then
        return 0
    fi
    local pid
    pid="$(container_pid "$name")" || return 1
    if [[ -z "$pid" ]]; then
        echo "    skip renice ${name}: pid is empty/0"
        return 0
    fi
    echo "==> renice -n ${nice} -p ${pid} (${name} leader)"
    if ! renice -n "$nice" -p "$pid"; then
        echo "error: renice -n ${nice} -p ${pid} (${name} leader) failed" >&2
        return 1
    fi
    # Music send threads are not the leader. Fullstack / sidecar have
    # no voice-rt workers; only the bot container gets the comm walk.
    if [[ "$name" == "$BOT_CONTAINER" ]]; then
        renice_voice_rt_tasks "$pid" "$nice" || return 1
    fi
}

# Host-side chrt only. Default off. fifo/rr are opt-in; do not set a
# kube privileged / CAP_SYS_NICE default. uid 10001 cannot setpriority
# in-process (EPERM) — this is the supported nice/rt path.
apply_chrt() {
    local name="$1"
    local sched="$2"
    local prio="$3"
    if [[ -z "$sched" ]]; then
        return 0
    fi
    if ! command -v chrt >/dev/null; then
        echo "error: chrt not found on PATH (TS6_BOT_CHRT_SCHED=${sched} requested)" >&2
        return 1
    fi
    local pid
    pid="$(container_pid "$name")" || return 1
    if [[ -z "$pid" ]]; then
        echo "    skip chrt ${name}: pid is empty/0"
        return 0
    fi
    local -a args
    case "${sched,,}" in
        other) args=(--pid -o 0 "$pid") ;;
        idle) args=(--pid -i 0 "$pid") ;;
        batch) args=(--pid -b 0 "$pid") ;;
        fifo|rr)
            if [[ -z "$prio" ]]; then
                echo "error: TS6_BOT_CHRT_PRIO required for sched ${sched} (CAP_SYS_NICE / rootful host chrt)" >&2
                return 1
            fi
            if [[ "${sched,,}" == "fifo" ]]; then
                args=(--pid -f "$prio" "$pid")
            else
                args=(--pid -r "$prio" "$pid")
            fi
            echo "==> RT opt-in: chrt ${sched} prio ${prio} on ${name} (host chrt; may need CAP_SYS_NICE if not rootful)"
            ;;
        *)
            echo "error: unsupported TS6_BOT_CHRT_SCHED=${sched} (other|idle|batch|fifo|rr)" >&2
            return 1
            ;;
    esac
    echo "==> chrt ${args[*]} (${name})"
    if ! chrt "${args[@]}"; then
        echo "error: chrt ${args[*]} (${name}) failed" >&2
        return 1
    fi
}

apply_soft_pin() {
apply_cpuset "$CONTAINER" "$CPUSET"
apply_nice "$CONTAINER" "$NICE"

# Never apply TS6_BOT_CPUSET / TS6_BOT_SEND_CPUSET as HostConfig —
# those keys are send-thread affinity inside the music process.
if [[ -n "${TS6_BOT_CPUSET:-}" || -n "${TS6_BOT_SEND_CPUSET:-}" ]]; then
    echo "==> TS6_BOT_CPUSET/TS6_BOT_SEND_CPUSET=${TS6_BOT_SEND_CPUSET:-${TS6_BOT_CPUSET}} is in-process send-thread pin (not container cpuset)"
fi
if [[ -n "${TS6_BOT_DECODE_CPUSET:-}" ]]; then
    echo "==> TS6_BOT_DECODE_CPUSET=${TS6_BOT_DECODE_CPUSET} is in-process decode pre_exec (kube env); this script does not inject it"
fi
apply_cpuset "$BOT_CONTAINER" "$BOT_CONTAINER_CPUSET"
apply_nice "$BOT_CONTAINER" "$BOT_NICE"
apply_chrt "$BOT_CONTAINER" "$BOT_CHRT_SCHED" "$BOT_CHRT_PRIO"

apply_cpuset "$SIDECAR_CONTAINER" "$SIDECAR_CPUSET"
apply_nice "$SIDECAR_CONTAINER" "$SIDECAR_NICE"

echo "OK: soft pin applied (fullstack cpuset=${CPUSET:-unset} nice=${NICE:-unset}" \
    "bot-container cpuset=${BOT_CONTAINER_CPUSET:-unset} bot nice=${BOT_NICE:-unset}" \
    "bot chrt=${BOT_CHRT_SCHED:-off} sidecar cpuset=${SIDECAR_CPUSET:-unset} nice=${SIDECAR_NICE:-unset})"
}

main() {
    if [[ -f "$SOFT_PIN_ENV" ]]; then
        # shellcheck disable=SC1090
        source "$SOFT_PIN_ENV"
        echo "==> sourced ${SOFT_PIN_ENV}"
    else
        echo "==> no soft-pin env at ${SOFT_PIN_ENV}; skipping"
    fi

    CPUSET="${TS6_FULLSTACK_CPUSET:-}"
    NICE="${TS6_FULLSTACK_NICE:-}"
    BOT_CONTAINER_CPUSET="$(normalize_cpuset "${TS6_BOT_CONTAINER_CPUSET:-}")"
    BOT_NICE="${TS6_BOT_NICE:-}"
    BOT_CHRT_SCHED="${TS6_BOT_CHRT_SCHED:-}"
    BOT_CHRT_PRIO="${TS6_BOT_CHRT_PRIO:-}"
    SIDECAR_CPUSET="${TS6_SIDECAR_CPUSET:-}"
    SIDECAR_NICE="${TS6_SIDECAR_NICE:-}"

    if ! music_hostconfig_allowed "$BOT_CONTAINER_CPUSET"; then
        exit 1
    fi

    if [[ -n "$CPUSET" ]] && is_fullstack_shrink_from_live "$CPUSET"; then
        if [[ "${TS6_SOFT_PIN_SHRINK_ACK:-}" != "1" ]]; then
            echo "error: fullstack cpuset ${CPUSET} shrinks live 2-5 (profile A)." >&2
            echo "  Packing B (Robert) keeps fullstack 2-5. Shrink is packing A and needs TS6_SOFT_PIN_SHRINK_ACK=1." >&2
            echo "  Do not apply shrink via update.sh / tag without that ACK." >&2
            exit 1
        fi
        echo "==> TS6_SOFT_PIN_SHRINK_ACK=1 — applying fullstack shrink ${CPUSET} (profile A)"
    fi

    if [[ -z "$CPUSET" && -z "$NICE" && -z "$BOT_CONTAINER_CPUSET" && -z "$BOT_NICE" && -z "$BOT_CHRT_SCHED" && -z "$SIDECAR_CPUSET" && -z "$SIDECAR_NICE" ]]; then
        echo "==> soft pin unset; no-op"
        exit 0
    fi

    if ! command -v podman >/dev/null; then
        echo "error: podman not found on PATH (soft pin requested)" >&2
        exit 1
    fi

    apply_soft_pin
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
