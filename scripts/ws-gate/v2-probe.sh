#!/usr/bin/env bash
# scripts/ws-gate/v2-probe.sh — WS-Gate V2 (add-TS-server) gate probe.
#
# Matrix row V2 of the Chapter 1 verification suite (THE-1014, parent
# THE-1009). Proves the admin "add a TeamSpeak server connection" flow:
# the POST persists a server row and the row is readable back, with the
# sealed apiKey never returned on the wire (spec §7.5 data-protection lens).
#
# Sequence:
#   1. POST   /api/servers            create a connection (admin) → 201
#   2. assert .id present, apiKey/sshPassword ABSENT from the response
#   3. GET    /api/servers            the new row is listed + fields match
#   4. (REQUIRE_HEALTHY=1 only) GET dashboard → 200 proves the row is
#      actually reachable. Gated because it needs a live NON-PROD TS server.
#   5. DELETE /api/servers/{id}        clean up (warn-only on failure)
#
# Modes (see _probe-lib.sh): WS_GATE_DRY_RUN=1 | BASE_URL given (live) |
# no BASE_URL (self-boot in-memory server). In self-boot / no-TS mode the
# row still persists (the host need not be reachable to be stored), so the
# persistence assertions are green against the source build alone; only the
# REQUIRE_HEALTHY probe-back needs a reachable TS.
#
# Inputs (live mode):
#   BASE_URL       positional 1     manager base URL
#   ADMIN_TOKEN    env, required    admin JWT
# Target TS (point at a NON-PROD server only):
#   TS_HOST        env, default ts.invalid.example
#   TS_WEBQUERY_PORT env, default 10080
#   TS_API_KEY     env, default ws-gate-probe-key   (redacted in evidence)
#   TS_USE_HTTPS   env, default false
#   VIRTUAL_SERVER_ID env, default 1   (for the REQUIRE_HEALTHY dashboard hit)
#   REQUIRE_HEALTHY  env, default 0    set 1 on the runner w/ a live TS
#
# Exit codes:
#   0   persistence assertions green (cleanup OK)
#   64  usage error      65 boot failed       66 pre-check failed
#   67  create failed    68 readback failed    69 health probe-back failed
#   71  cleanup failed (verdict still GREEN; warns)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/ws-gate/_probe-lib.sh
source "$SCRIPT_DIR/_probe-lib.sh"

EXIT_CREATE=67
EXIT_READBACK=68
EXIT_HEALTH=69
EXIT_CLEANUP=71

TS_HOST="${TS_HOST:-ts.invalid.example}"
TS_WEBQUERY_PORT="${TS_WEBQUERY_PORT:-10080}"
TS_API_KEY="${TS_API_KEY:-ws-gate-probe-key}"
TS_USE_HTTPS="${TS_USE_HTTPS:-false}"
VIRTUAL_SERVER_ID="${VIRTUAL_SERVER_ID:-1}"
REQUIRE_HEALTHY="${REQUIRE_HEALTHY:-0}"

wsg_init "ws-gate-v2-probe" "${1:-}"

rand_hex="$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
srv_name="ws-gate-v2-$rand_hex"
create_payload="$(jq -n \
    --arg name "$srv_name" --arg host "$TS_HOST" \
    --argjson port "$TS_WEBQUERY_PORT" --arg key "$TS_API_KEY" \
    --argjson https "$TS_USE_HTTPS" \
    '{name:$name, host:$host, webqueryPort:$port, apiKey:$key, useHttps:$https}')"

# --- dry run: prove the payload builds, then stop ---------------------------
if [[ "$DRY_RUN" -eq 1 ]]; then
    wsg_dry_check_payload "create-server" "$create_payload"
    VERDICT_NOTE="dry-run: POST /api/servers payload validated; live run needs a reachable manager + NON-PROD TS"
    wsg_log "PASS (dry-run) — V2 add-TS-server harness is well-formed."
    exit 0
fi

wsg_resolve_target

# --- Step 1/2: create + response hygiene ------------------------------------
do_req 1 POST /api/servers "$create_payload" "$ADMIN_TOKEN"
wsg_expect 201 "create server" "$EXIT_CREATE"
SRV_ID="$(jq -r '.id // empty' "$RESP_BODY")"
[[ -n "$SRV_ID" ]] || { wsg_fail "create: 201 but no .id"; exit "$EXIT_CREATE"; }
if jq -e 'has("apiKey") or has("sshPassword") or has("sshPrivateKey")' "$RESP_BODY" >/dev/null; then
    wsg_fail "create: response leaks a sealed credential field"
    exit "$EXIT_CREATE"
fi
wsg_assert_no_secret_leak "$RESP_BODY" "create response"
[[ "$(jq -r '.enabled' "$RESP_BODY")" == "true" ]] || wsg_warn "created server not enabled=true"
wsg_log "step 1/2 OK — created server id=$SRV_ID name=$srv_name (apiKey not returned)"

# --- Step 3: read back ------------------------------------------------------
do_req 3 GET /api/servers "" "$ADMIN_TOKEN"
wsg_expect 200 "list servers" "$EXIT_READBACK"
row="$(jq -c --argjson id "$SRV_ID" 'map(select(.id==$id)) | .[0] // empty' "$RESP_BODY")"
[[ -n "$row" ]] || { wsg_fail "readback: server id=$SRV_ID not present in GET /api/servers"; exit "$EXIT_READBACK"; }
got_host="$(printf '%s' "$row" | jq -r '.host')"
[[ "$got_host" == "$TS_HOST" ]] || { wsg_fail "readback: host '$got_host' != '$TS_HOST'"; exit "$EXIT_READBACK"; }
wsg_assert_no_secret_leak <(printf '%s' "$row") "list row"
wsg_log "step 3 OK — row persists: id=$SRV_ID host=$got_host port=$(printf '%s' "$row" | jq -r '.webqueryPort')"

# --- Step 4: optional live health probe-back --------------------------------
if [[ "$REQUIRE_HEALTHY" == "1" ]]; then
    do_req 4 GET "/api/servers/$SRV_ID/vs/$VIRTUAL_SERVER_ID/dashboard" "" "$ADMIN_TOKEN"
    if [[ ! "$RESP_CODE" =~ ^2[0-9][0-9]$ ]]; then
        wsg_fail "health: dashboard for server $SRV_ID returned $RESP_CODE (TS unreachable?)"
        try_del() { curl -sS -o /dev/null -X DELETE -H "Authorization: Bearer $ADMIN_TOKEN" "$BASE_URL/api/servers/$SRV_ID" 2>>"$EVID_DIR/curl.log" || true; }
        try_del
        exit "$EXIT_HEALTH"
    fi
    wsg_log "step 4 OK — server $SRV_ID reachable (dashboard 200)"
else
    wsg_log "step 4 SKIPPED — set REQUIRE_HEALTHY=1 with a live NON-PROD TS to assert reachability"
    VERDICT_NOTE="health probe-back skipped (REQUIRE_HEALTHY!=1); persistence assertions green"
fi

# --- Step 5: cleanup --------------------------------------------------------
do_req 5 DELETE "/api/servers/$SRV_ID" "" "$ADMIN_TOKEN"
if [[ ! "$RESP_CODE" =~ ^2(00|04)$ ]]; then
    wsg_warn "cleanup: DELETE server $SRV_ID expected 200/204, got $RESP_CODE — stray row needs manual removal"
    exit "$EXIT_CLEANUP"
fi
wsg_log "step 5 OK — deleted server $SRV_ID"
wsg_log "PASS — V2 add-TS-server probe green against ${BASE_URL}"
exit 0
