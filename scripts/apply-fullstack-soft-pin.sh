#!/usr/bin/env bash
# Re-apply Contabo soft CPU pin + nice after podman kube play.
# Kube YAML does not persist HostConfig cpuset or process nice.
#
# Usage (from any cwd, against a repo checkout):
#   ./scripts/apply-fullstack-soft-pin.sh
#
# Config: deploy/contabo/soft-pin.env (or TS6_SOFT_PIN_ENV).
#
# Fullstack: TS6_FULLSTACK_CPUSET / TS6_FULLSTACK_NICE — live Contabo
# pin stays 2-5 / -5 until cutover. This script does not shrink it.
#
# Bot/music (option 1): do NOT apply TS6_BOT_CPUSET as a container-wide
# cpuset (that would pin ffmpeg/yt-dlp onto send cores 0-1). Send-thread
# affinity is in-process (sched_setaffinity on voice-rt). This script
# applies TS6_BOT_NICE to ts6-manager-music and, only if explicitly set,
# TS6_BOT_CONTAINER_CPUSET (default empty). Sidecar stays unpinned
# unless TS6_SIDECAR_* are set.
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
SIDECAR_CPUSET="${TS6_SIDECAR_CPUSET:-}"
SIDECAR_NICE="${TS6_SIDECAR_NICE:-}"

if [[ -z "$CPUSET" && -z "$NICE" && -z "$BOT_CONTAINER_CPUSET" && -z "$BOT_NICE" && -z "$SIDECAR_CPUSET" && -z "$SIDECAR_NICE" ]]; then
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

apply_nice() {
    local name="$1"
    local nice="$2"
    if [[ -z "$nice" ]]; then
        return 0
    fi
    if ! podman container exists "$name"; then
        echo "error: container ${name} missing (nice ${nice} requested)" >&2
        return 1
    fi
    local pid
    pid="$(podman inspect -f '{{.State.Pid}}' "$name")"
    if [[ -z "$pid" || "$pid" == "0" ]]; then
        echo "    skip renice ${name}: pid is ${pid:-empty}"
        return 0
    fi
    echo "==> renice -n ${nice} -p ${pid} (${name})"
    if ! renice -n "$nice" -p "$pid"; then
        echo "error: renice -n ${nice} -p ${pid} (${name}) failed" >&2
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
apply_cpuset "$BOT_CONTAINER" "$BOT_CONTAINER_CPUSET"
apply_nice "$BOT_CONTAINER" "$BOT_NICE"

apply_cpuset "$SIDECAR_CONTAINER" "$SIDECAR_CPUSET"
apply_nice "$SIDECAR_CONTAINER" "$SIDECAR_NICE"

echo "OK: soft pin applied (fullstack cpuset=${CPUSET:-unset} nice=${NICE:-unset}" \
    "bot-container cpuset=${BOT_CONTAINER_CPUSET:-unset} bot nice=${BOT_NICE:-unset}" \
    "sidecar cpuset=${SIDECAR_CPUSET:-unset} nice=${SIDECAR_NICE:-unset})"
