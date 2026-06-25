#!/usr/bin/env bash
# scripts/ws-gate/run-all.sh — WS-Gate umbrella runner (docs/flows/v1.1-gate.md §3/§5).
#
# Fans out to every per-verification probe (scripts/ws-gate/*-probe.sh),
# runs each against BASE_URL, and aggregates the result into a single
# pass/fail matrix dump. v1.1 introduces this runner with v6-probe.sh as the
# first row; future Chapter 1 widenings drop their probe into this directory
# and are picked up automatically.
#
# Usage:
#   scripts/ws-gate/run-all.sh BASE_URL
#   WS_GATE_DRY_RUN=1 scripts/ws-gate/run-all.sh [BASE_URL]
#
# ADMIN_TOKEN (and any per-probe env) must be exported by the caller; it is
# passed through to each probe unchanged.
#
# Dry-run mode (WS_GATE_DRY_RUN=1): BASE_URL becomes optional (a placeholder
# is substituted). Each dry-run-aware probe validates its own request
# payloads / tooling and exits green WITHOUT a server boot or any network —
# the path used to prove the suite is turnkey from the headless heartbeat env
# (no reachable TS server, no release-image runner — THE-1009/THE-1013).
#
# Exit codes:
#   0   every probe passed (a cleanup-only warning, exit 71, still counts pass)
#   1   one or more probes failed
#   64  usage error
#   65  no probes found

set -uo pipefail

case "${WS_GATE_DRY_RUN:-0}" in 1|true|yes) DRY=1 ;; *) DRY=0 ;; esac
export WS_GATE_DRY_RUN

BASE_URL="${1:-}"
if [[ -z "$BASE_URL" ]]; then
    if [[ "$DRY" -eq 1 ]]; then
        BASE_URL="http://dry-run.invalid"
    else
        echo "usage: $0 BASE_URL   (or WS_GATE_DRY_RUN=1 $0 for a dry run)" >&2
        exit 64
    fi
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

shopt -s nullglob
probes=("$SCRIPT_DIR"/*-probe.sh)
shopt -u nullglob

if [[ ${#probes[@]} -eq 0 ]]; then
    echo "run-all: no *-probe.sh found in $SCRIPT_DIR" >&2
    exit 65
fi

names=()
verdicts=()
overall=0

for probe in "${probes[@]}"; do
    name="$(basename "$probe" .sh)"
    echo "=== run-all: $name ==="
    "$probe" "$BASE_URL"
    rc=$?
    case "$rc" in
        0)  verdict="PASS" ;;
        71) verdict="PASS (cleanup warning)" ;;
        *)  verdict="FAIL (exit $rc)"; overall=1 ;;
    esac
    names+=("$name")
    verdicts+=("$verdict")
    echo
done

echo "=== WS-Gate matrix ==="
for i in "${!names[@]}"; do
    printf '  %-20s %s\n' "${names[$i]}" "${verdicts[$i]}"
done

if [[ "$overall" -eq 0 ]]; then
    echo "WS-Gate: PASS"
else
    echo "WS-Gate: FAIL"
fi
exit "$overall"
