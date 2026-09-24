#!/usr/bin/env bash
# Selects voice-rt tids for host renice. Does not call podman or renice(1).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/apply-fullstack-soft-pin.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

PROC="${TMP}/proc"
PID=4242
TASK="${PROC}/${PID}/task"
mkdir -p "${TASK}/1" "${TASK}/11" "${TASK}/12" "${TASK}/13"
printf 'ts6-manager-mu\n' > "${TASK}/1/comm"
printf 'voice-rt\n' > "${TASK}/11/comm"
printf 'tokio-runtime-w\n' > "${TASK}/12/comm"
printf 'voice-rt\n' > "${TASK}/13/comm"

export TS6_SOFT_PIN_PROC_ROOT="$PROC"
LOG="${TMP}/renice.log"
: > "$LOG"

renice() {
    printf '%s\n' "$*" >> "$LOG"
    return 0
}

found=0
renice_matching_comm "$PID" -5 voice-rt found
[[ "$found" -eq 2 ]]
grep -qx -- '-n -5 -p 11' "$LOG"
grep -qx -- '-n -5 -p 13' "$LOG"
if grep -qx -- '-n -5 -p 12' "$LOG" || grep -qx -- '-n -5 -p 1' "$LOG"; then
    echo "reniced a tid that is not voice-rt" >&2
    exit 1
fi

: > "$LOG"
export TS6_BOT_NICE_RETRIES=2
export TS6_BOT_NICE_RETRY_SLEEP=0
renice_voice_rt_tasks 99999 -5
[[ ! -s "$LOG" ]]

: > "$LOG"
renice_voice_rt_tasks "$PID" -5
[[ "$(grep -c . "$LOG")" -eq 2 ]]
grep -qx -- '-n -5 -p 11' "$LOG"
grep -qx -- '-n -5 -p 13' "$LOG"

# apply_nice on the music container renices the leader and voice-rt tids.
# Fullstack nice stays leader-only (no voice-rt walk).
container_pid() {
    echo "$PID"
}
: > "$LOG"
apply_nice ts6-manager-music -5
grep -qx -- '-n -5 -p 4242' "$LOG"
grep -qx -- '-n -5 -p 11' "$LOG"
grep -qx -- '-n -5 -p 13' "$LOG"
[[ "$(grep -c . "$LOG")" -eq 3 ]]

: > "$LOG"
apply_nice ts6-manager-fullstack -5
[[ "$(grep -c . "$LOG")" -eq 1 ]]
grep -qx -- '-n -5 -p 4242' "$LOG"

echo "OK: voice-rt renice targeting"

# L19 — packing B refuses every music HostConfig cpuset, not only 0-1.
assert_music_cpuset_refused() {
    local spec="$1"
    local needle="$2"
    local err="${TMP}/refuse.err"
    if music_hostconfig_allowed "$spec" 2>"$err"; then
        echo "accepted music HostConfig ${spec}" >&2
        exit 1
    fi
    if ! grep -q "^warning:" "$err"; then
        echo "missing warning for music HostConfig ${spec}" >&2
        cat "$err" >&2
        exit 1
    fi
    if ! grep -q "$needle" "$err"; then
        echo "missing '${needle}' for music HostConfig ${spec}" >&2
        cat "$err" >&2
        exit 1
    fi
}

unset TS6_SOFT_PIN_SHRINK_ACK
CPUSET="2-5"
for spec in 0-5 2-5 4 0-3 "2,3,4,5" "  2-5  "; do
    assert_music_cpuset_refused "$spec" "packing B refuses any music HostConfig"
done
for spec in 0-1 "0,1" "1,0" 0 1 "0 - 1"; do
    assert_music_cpuset_refused "$spec" "packing C refused"
done

if ! music_hostconfig_allowed "" 2>"${TMP}/empty.err"; then
    echo "empty music HostConfig was refused" >&2
    exit 1
fi
if ! music_hostconfig_allowed "   " 2>"${TMP}/ws.err"; then
    echo "whitespace music HostConfig was refused" >&2
    exit 1
fi

# Shrink ACK without a fullstack shrink is still packing B.
export TS6_SOFT_PIN_SHRINK_ACK=1
CPUSET="2-5"
assert_music_cpuset_refused "0-3" "packing B refuses any music HostConfig"
assert_music_cpuset_refused "0-5" "packing B refuses any music HostConfig"

# Packing A (shrink + ACK) may set a wider music cpuset. Send-only stays refused.
CPUSET="4-5"
if ! music_hostconfig_allowed "0-3" 2>"${TMP}/packing-a.err"; then
    echo "packing A refused music HostConfig 0-3" >&2
    cat "${TMP}/packing-a.err" >&2
    exit 1
fi
assert_music_cpuset_refused "0-1" "packing C refused"
unset TS6_SOFT_PIN_SHRINK_ACK
# Read by music_hostconfig_allowed in the sourced script.
# shellcheck disable=SC2034
CPUSET=""

# main() must refuse before podman. The real packing B env leaves music
# HostConfig unset; an override must not be applied.
if (
    export TS6_BOT_CONTAINER_CPUSET=2-5
    unset TS6_SOFT_PIN_SHRINK_ACK
    main
) >"${TMP}/main-b.out" 2>"${TMP}/main-b.err"; then
    echo "main accepted music HostConfig under packing B" >&2
    cat "${TMP}/main-b.err" >&2
    exit 1
fi
grep -q "packing B refuses any music HostConfig" "${TMP}/main-b.err"
grep -q "^warning:" "${TMP}/main-b.err"
unset TS6_BOT_CONTAINER_CPUSET

echo "OK: packing B refuses any music HostConfig cpuset"
