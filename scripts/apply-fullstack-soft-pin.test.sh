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
