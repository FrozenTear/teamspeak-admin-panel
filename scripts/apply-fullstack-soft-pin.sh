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
# DECODE 2-5 is kube env (pin_decode_child); this script does not inject it.
# Packing A (fullstack 4-5) is gated — requires TS6_SOFT_PIN_SHRINK_ACK=1.
# Packing C (music HostConfig send-only 0-1) is refused.
# SEND/DECODE in-process pins are kube env, not HostConfig.
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

if [[ -f "$SOFT_PIN_ENV" ]]; then
    # shellcheck disable=SC1090
    source "$SOFT_PIN_ENV"
    echo "==> sourced ${SOFT_PIN_ENV}"
else
    echo "==> no soft-pin env at ${SOFT_PIN_ENV}; skipping"
fi

CPUSET="${TS6_FULLSTACK_CPUSET:-}"
NICE="${TS6_FULLSTACK_NICE:-}"
BOT_CONTAINER_CPUSET="${TS6_BOT_CONTAINER_CPUSET:-}"
BOT_NICE="${TS6_BOT_NICE:-}"
BOT_CHRT_SCHED="${TS6_BOT_CHRT_SCHED:-}"
BOT_CHRT_PRIO="${TS6_BOT_CHRT_PRIO:-}"
SIDECAR_CPUSET="${TS6_SIDECAR_CPUSET:-}"
SIDECAR_NICE="${TS6_SIDECAR_NICE:-}"

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

# Profile A shrinks live 2-5. Refuse unless Robert ACK is set.
is_fullstack_shrink_from_live() {
    local spec
    spec="$(normalize_cpuset "$1")"
    case "$spec" in
        4-5|3-5|4,5|5,4|3,4,5|3-4,5|5|4) return 0 ;;
        *) return 1 ;;
    esac
}

if [[ -n "$BOT_CONTAINER_CPUSET" ]] && is_send_only_music_cpuset "$BOT_CONTAINER_CPUSET"; then
    echo "error: TS6_BOT_CONTAINER_CPUSET=${BOT_CONTAINER_CPUSET} is packing C (music HostConfig on send-only 0-1)." >&2
    echo "  v1.6.15 Angerfist dig 163/590/117: container-wide 0-1 traps ffmpeg on send cores." >&2
    echo "  Use unset/0-5 (profile B) or 0-3 after fullstack shrink (profile A). Never 0-1." >&2
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
    echo "==> renice -n ${nice} -p ${pid} (${name})"
    if ! renice -n "$nice" -p "$pid"; then
        echo "error: renice -n ${nice} -p ${pid} (${name}) failed" >&2
        return 1
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

apply_cpuset "$CONTAINER" "$CPUSET"
apply_nice "$CONTAINER" "$NICE"

# Never apply TS6_BOT_CPUSET / TS6_BOT_SEND_CPUSET as HostConfig —
# those keys are send-thread affinity inside the music process.
if [[ -n "${TS6_BOT_CPUSET:-}" || -n "${TS6_BOT_SEND_CPUSET:-}" ]]; then
    echo "==> TS6_BOT_CPUSET/TS6_BOT_SEND_CPUSET=${TS6_BOT_SEND_CPUSET:-${TS6_BOT_CPUSET}} is in-process send-thread pin (not container cpuset)"
fi
if [[ -n "${TS6_BOT_DECODE_CPUSET:-}" ]]; then
    echo "==> TS6_BOT_DECODE_CPUSET=${TS6_BOT_DECODE_CPUSET} is in-process pin_decode_child (kube env); this script does not inject it"
fi
apply_cpuset "$BOT_CONTAINER" "$BOT_CONTAINER_CPUSET"
apply_nice "$BOT_CONTAINER" "$BOT_NICE"
apply_chrt "$BOT_CONTAINER" "$BOT_CHRT_SCHED" "$BOT_CHRT_PRIO"

apply_cpuset "$SIDECAR_CONTAINER" "$SIDECAR_CPUSET"
apply_nice "$SIDECAR_CONTAINER" "$SIDECAR_NICE"

echo "OK: soft pin applied (fullstack cpuset=${CPUSET:-unset} nice=${NICE:-unset}" \
    "bot-container cpuset=${BOT_CONTAINER_CPUSET:-unset} bot nice=${BOT_NICE:-unset}" \
    "bot chrt=${BOT_CHRT_SCHED:-off} sidecar cpuset=${SIDECAR_CPUSET:-unset} nice=${SIDECAR_NICE:-unset})"
