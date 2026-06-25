#!/usr/bin/env bash
# scripts/ws-gate/v3-probe.sh — WS-Gate V3 (live dashboard) gate probe.
#
# Matrix row V3 (THE-1014, parent THE-1009). Exercises the data endpoints the
# Dioxus/WASM dashboard consumes so the row is checkable WITHOUT a browser:
#
#   GET /api/servers/{cid}/vs/{sid}/dashboard   → DashboardData snapshot
#   GET /api/servers/{cid}/vs/{sid}/clients     → live client list
#   GET /api/servers/{cid}/vs/{sid}/channels    → channel tree
#
# Two assertion tiers:
#   * env-independent (runs green from the heartbeat / source build, no TS):
#       - each endpoint is auth-gated: unauthenticated GET → 401.
#       - authenticated GET returns a CLEAN result: a 2xx snapshot, or a
#         well-formed upstream error (502/503/504) when the configured TS is
#         unreachable — never a 500/panic. This proves the route + control
#         backend wiring without a reachable server.
#   * REQUIRE_LIVE_TS=1 (runner, with a live NON-PROD TS):
#       - dashboard 200 with the snapshot shape (onlineUsers/channelCount).
#       - clients + channels return 200 JSON arrays.
#
# Modes (see _probe-lib.sh): WS_GATE_DRY_RUN=1 | live (BASE_URL) | self-boot.
#
# Inputs (live): BASE_URL $1, ADMIN_TOKEN env.
#   SERVER_CONFIG_ID  env, default = first row of GET /api/servers
#   VIRTUAL_SERVER_ID env, default 1
#   REQUIRE_LIVE_TS   env, default 0
#
# Exit codes:
#   0  green   64 usage   65 boot   66 pre-check
#   67 auth-gate not enforced   68 dirty/500 response   69 live-shape assertion failed
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/ws-gate/_probe-lib.sh
source "$SCRIPT_DIR/_probe-lib.sh"

EXIT_AUTHGATE=67
EXIT_DIRTY=68
EXIT_SHAPE=69

VIRTUAL_SERVER_ID="${VIRTUAL_SERVER_ID:-1}"
REQUIRE_LIVE_TS="${REQUIRE_LIVE_TS:-0}"

wsg_init "ws-gate-v3-probe" "${1:-}"

if [[ "$DRY_RUN" -eq 1 ]]; then
    for ep in dashboard clients channels; do
        wsg_dry_check_payload "endpoint-$ep" "$(jq -n --arg e "$ep" '{endpoint:("/api/servers/{cid}/vs/{sid}/"+$e)}')"
    done
    VERDICT_NOTE="dry-run: 3 dashboard endpoints enumerated; live snapshot-shape needs REQUIRE_LIVE_TS + a runner"
    wsg_log "PASS (dry-run) — V3 dashboard harness is well-formed."
    exit 0
fi

wsg_resolve_target

# Resolve a server config id (default: first server in the pool).
CID="${SERVER_CONFIG_ID:-}"
if [[ -z "$CID" ]]; then
    do_req 0 GET /api/servers "" "$ADMIN_TOKEN"
    wsg_expect 200 "list servers (to resolve cid)" "$WSG_EXIT_PRECHECK"
    CID="$(jq -r '.[0].id // empty' "$RESP_BODY")"
    [[ -n "$CID" ]] || { wsg_fail "no server configured; set SERVER_CONFIG_ID"; exit "$WSG_EXIT_PRECHECK"; }
fi
wsg_log "target server cid=$CID vs=$VIRTUAL_SERVER_ID"

base="/api/servers/$CID/vs/$VIRTUAL_SERVER_ID"
step=1
for ep in dashboard clients channels; do
    # auth-gate: unauthenticated request must be rejected
    do_req "$step-unauth" GET "$base/$ep"
    if [[ "$RESP_CODE" != "401" ]]; then
        wsg_fail "$ep: unauthenticated GET expected 401, got $RESP_CODE"
        exit "$EXIT_AUTHGATE"
    fi
    # authenticated request must be clean (2xx or well-formed upstream error)
    do_req "$step-auth" GET "$base/$ep" "" "$ADMIN_TOKEN"
    case "$RESP_CODE" in
        2[0-9][0-9]) clean=1 ;;
        502|503|504) clean=1 ;;   # TS unreachable — well-formed upstream error
        *)           clean=0 ;;
    esac
    if [[ "$clean" -ne 1 ]]; then
        wsg_fail "$ep: authenticated GET returned dirty status $RESP_CODE (expected 2xx or 502/503/504)"
        exit "$EXIT_DIRTY"
    fi
    wsg_log "step $step OK — $ep: auth-gated (401) + clean authed status ($RESP_CODE)"
    step=$((step+1))
done

if [[ "$REQUIRE_LIVE_TS" == "1" ]]; then
    do_req live-dashboard GET "$base/dashboard" "" "$ADMIN_TOKEN"
    wsg_expect 200 "live dashboard" "$EXIT_SHAPE"
    jq -e 'has("onlineUsers") and has("channelCount")' "$RESP_BODY" >/dev/null \
        || { wsg_fail "live dashboard missing onlineUsers/channelCount"; exit "$EXIT_SHAPE"; }
    do_req live-clients GET "$base/clients" "" "$ADMIN_TOKEN"
    wsg_expect 200 "live clients" "$EXIT_SHAPE"
    jq -e 'type=="array"' "$RESP_BODY" >/dev/null || { wsg_fail "clients not a JSON array"; exit "$EXIT_SHAPE"; }
    do_req live-channels GET "$base/channels" "" "$ADMIN_TOKEN"
    wsg_expect 200 "live channels" "$EXIT_SHAPE"
    jq -e 'type=="array"' "$RESP_BODY" >/dev/null || { wsg_fail "channels not a JSON array"; exit "$EXIT_SHAPE"; }
    wsg_log "live snapshot shape OK — dashboard+clients+channels"
else
    VERDICT_NOTE="auth-gate + clean-response tier green; live snapshot shape gated behind REQUIRE_LIVE_TS"
    wsg_log "live snapshot SKIPPED — set REQUIRE_LIVE_TS=1 with a live NON-PROD TS"
fi

wsg_log "PASS — V3 dashboard probe green against ${BASE_URL}"
exit 0
