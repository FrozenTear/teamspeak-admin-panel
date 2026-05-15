#!/usr/bin/env bash
# scripts/ws-gate/v6-probe.sh — WS-Gate V6 (flow trigger) gate probe for v1.1.
#
# Reinstates the V6 row of the Chapter 1 verification matrix (PURA-244).
# See docs/flows/v1.1-gate.md §2 and docs/flows/http-api.md for the contract.
#
# Sequence (the four-step probe the gate plan asks for):
#   1. GET    /health                pre-check the backend is up
#   2. POST   /api/flows             create a logLine-only flow (admin)
#   3. PATCH  /api/flows/{id}         set enabled=true
#   4. POST   /api/flows/{id}/fire    manual-fire it
#   5. GET    /api/flows/{id}/runs    poll within OBS_WINDOW_S until status=ok
#   6. DELETE /api/flows/{id}?force=true   clean up
#
# Exits 0 on success. All request/response bodies are captured into
#   qa-evidence/ws-gate/v6/<ISO8601-UTC>/step-N.{req,resp}.json
# plus a verdict.json summary, matching the V1-V5/V7 evidence shape.
#
# Inputs:
#   BASE_URL           positional 1, required   e.g. https://manager.scuffedcrew.no
#   ADMIN_TOKEN        env, required            admin JWT for the manager
#   SERVER_CONFIG_ID   env, default 1
#   VIRTUAL_SERVER_ID  env, default 1
#   OBS_WINDOW_S       env, default 30          observation window, seconds
#   HEALTH_PATH        env, default /health     the v1.1 server mounts /health
#                                               at the root (not /api/health —
#                                               see docs/flows/v1.1-gate.md §2.2,
#                                               which is stale on this point).
#   EVID_DIR           env, default qa-evidence/ws-gate/v6/<timestamp>
#
# Exit codes (docs/flows/v1.1-gate.md §2.3):
#   0   all steps succeeded inside the observation window
#   64  usage error (missing BASE_URL or ADMIN_TOKEN)
#   65  pre-check failed (/health non-2xx)
#   66  create failed
#   67  enable failed
#   68  fire failed
#   69  observation window expired without success
#   70  run errored (engine reported errored/interrupted; body in evidence)
#   71  cleanup failed (probe verdict still GREEN for the matrix; warns)

set -euo pipefail

PROBE_NAME="v6-probe"

EXIT_USAGE=64
EXIT_PRECHECK=65
EXIT_CREATE=66
EXIT_ENABLE=67
EXIT_FIRE=68
EXIT_OBSERVE=69
EXIT_ERRORED=70
EXIT_CLEANUP=71

BASE_URL="${1:-}"
ADMIN_TOKEN="${ADMIN_TOKEN:-}"
SERVER_CONFIG_ID="${SERVER_CONFIG_ID:-1}"
VIRTUAL_SERVER_ID="${VIRTUAL_SERVER_ID:-1}"
OBS_WINDOW_S="${OBS_WINDOW_S:-30}"
HEALTH_PATH="${HEALTH_PATH:-/health}"

log()  { printf '[%s] %s\n' "$PROBE_NAME" "$*"; }
fail() { printf '[%s] FAIL: %s\n' "$PROBE_NAME" "$*" >&2; }
warn() { printf '[%s] WARN: %s\n' "$PROBE_NAME" "$*" >&2; }

usage() {
    cat >&2 <<EOF
usage: $0 BASE_URL
  BASE_URL     required positional, e.g. https://manager.scuffedcrew.no
  ADMIN_TOKEN  required env, admin JWT
optional env: SERVER_CONFIG_ID VIRTUAL_SERVER_ID OBS_WINDOW_S HEALTH_PATH EVID_DIR
EOF
}

if [[ -z "$BASE_URL" ]]; then
    fail "BASE_URL is required"
    usage
    exit "$EXIT_USAGE"
fi
if [[ -z "$ADMIN_TOKEN" ]]; then
    fail "ADMIN_TOKEN env var is required"
    usage
    exit "$EXIT_USAGE"
fi

# Normalise: drop a trailing slash so "$BASE_URL/api/flows" never doubles up.
BASE_URL="${BASE_URL%/}"

TS="$(date -u +%Y%m%dT%H%M%SZ)"
EVID_DIR="${EVID_DIR:-qa-evidence/ws-gate/v6/$TS}"
mkdir -p "$EVID_DIR"

FINAL_FLOW_ID=""
FINAL_RUN_ID=""

# Always leave a machine-readable verdict next to the step evidence.
# shellcheck disable=SC2317  # body runs via the EXIT trap, not inline.
on_exit() {
    local rc=$?
    if [[ -d "${EVID_DIR:-/nonexistent}" ]]; then
        jq -n \
            --arg probe "$PROBE_NAME" \
            --arg ts "$TS" \
            --arg base "$BASE_URL" \
            --arg flow "$FINAL_FLOW_ID" \
            --arg run "$FINAL_RUN_ID" \
            --argjson rc "$rc" \
            '{
                probe: $probe,
                timestamp: $ts,
                baseUrl: $base,
                flowId: $flow,
                runId: $run,
                exitCode: $rc,
                verdict: (if $rc == 0 then "PASS"
                          elif $rc == 71 then "PASS_CLEANUP_WARNING"
                          else "FAIL" end)
            }' >"$EVID_DIR/verdict.json" 2>/dev/null || true
    fi
}
trap on_exit EXIT

# do_req STEP METHOD URL [JSON_DATA]
# Writes step-STEP.req.json and step-STEP.resp.json into $EVID_DIR, and sets
# the globals RESP_CODE (HTTP status string) and RESP_BODY (response file
# path). Call it as a plain statement — NOT inside $(...) — so the globals
# propagate to the caller's shell.
RESP_CODE=""
RESP_BODY=""
do_req() {
    local step="$1" method="$2" url="$3" data="${4:-}"
    local req_file="$EVID_DIR/step-$step.req.json"
    local resp_file="$EVID_DIR/step-$step.resp.json"

    if [[ -n "$data" ]]; then
        jq -n --arg m "$method" --arg u "$url" --argjson b "$data" \
            '{method: $m, url: $u, body: $b}' >"$req_file"
    else
        jq -n --arg m "$method" --arg u "$url" \
            '{method: $m, url: $u, body: null}' >"$req_file"
    fi

    local curl_args=(
        -sS -o "$resp_file" -w '%{http_code}'
        -X "$method"
        -H "Authorization: Bearer $ADMIN_TOKEN"
    )
    if [[ -n "$data" ]]; then
        curl_args+=(-H 'Content-Type: application/json' --data "$data")
    fi

    local code
    code="$(curl "${curl_args[@]}" "$url" 2>>"$EVID_DIR/curl.log" || true)"
    [[ -s "$resp_file" ]] || printf '{}' >"$resp_file"
    RESP_CODE="${code:-000}"
    RESP_BODY="$resp_file"
}

# Best-effort cleanup used on the failure paths so a probe abort does not
# leak a stray flow into the gate environment.
try_cleanup() {
    local flow_id="$1"
    [[ -z "$flow_id" || "$flow_id" == "null" ]] && return 0
    curl -sS -o /dev/null \
        -X DELETE \
        -H "Authorization: Bearer $ADMIN_TOKEN" \
        "$BASE_URL/api/flows/$flow_id?force=true" \
        2>>"$EVID_DIR/curl.log" || true
}

log "evidence dir: $EVID_DIR"
log "target: $BASE_URL"

# --- Step 1: health pre-check ------------------------------------------------
do_req 1 GET "$BASE_URL$HEALTH_PATH"
if [[ ! "$RESP_CODE" =~ ^2[0-9][0-9]$ ]]; then
    fail "health pre-check: GET $HEALTH_PATH returned $RESP_CODE"
    exit "$EXIT_PRECHECK"
fi
log "step 1 OK — health $RESP_CODE"

# --- Step 2: create the probe flow (admin) -----------------------------------
rand_hex="$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
flow_name="ws-gate-v6-$rand_hex"
probe_id="$TS-$$"
create_payload="$(jq -n \
    --arg name "$flow_name" \
    --arg desc "WS-Gate V6 probe" \
    --argjson scid "$SERVER_CONFIG_ID" \
    --argjson vsid "$VIRTUAL_SERVER_ID" \
    --arg msg "ws-gate v6-probe fired at $probe_id" \
    '{
        name: $name,
        description: $desc,
        serverConfigId: $scid,
        virtualServerId: $vsid,
        enabled: false,
        definition: {
            trigger: { kind: "manualFire" },
            actions: [ { kind: "logLine", message: $msg } ]
        }
    }')"

do_req 2 POST "$BASE_URL/api/flows" "$create_payload"
if [[ "$RESP_CODE" != "201" ]]; then
    fail "create flow: expected 201, got $RESP_CODE (see step-2.resp.json)"
    exit "$EXIT_CREATE"
fi
flow_id="$(jq -r '.id // empty' "$RESP_BODY")"
if [[ -z "$flow_id" ]]; then
    fail "create flow: 201 but no .id in response body"
    exit "$EXIT_CREATE"
fi
FINAL_FLOW_ID="$flow_id"
log "step 2 OK — created flow id=$flow_id name=$flow_name"

# --- Step 3: enable ----------------------------------------------------------
do_req 3 PATCH "$BASE_URL/api/flows/$flow_id" '{"enabled":true}'
if [[ "$RESP_CODE" != "200" ]]; then
    fail "enable flow $flow_id: expected 200, got $RESP_CODE"
    try_cleanup "$flow_id"
    exit "$EXIT_ENABLE"
fi
enabled="$(jq -r '.enabled // empty' "$RESP_BODY")"
if [[ "$enabled" != "true" ]]; then
    fail "enable flow $flow_id: 200 but enabled=$enabled"
    try_cleanup "$flow_id"
    exit "$EXIT_ENABLE"
fi
log "step 3 OK — flow $flow_id enabled"

# --- Step 4: manual fire -----------------------------------------------------
do_req 4 POST "$BASE_URL/api/flows/$flow_id/fire"
if [[ "$RESP_CODE" != "202" ]]; then
    fail "fire flow $flow_id: expected 202, got $RESP_CODE"
    try_cleanup "$flow_id"
    exit "$EXIT_FIRE"
fi
run_id="$(jq -r '.runId // empty' "$RESP_BODY")"
if [[ -z "$run_id" ]]; then
    fail "fire flow $flow_id: 202 but no .runId in response body"
    try_cleanup "$flow_id"
    exit "$EXIT_FIRE"
fi
FINAL_RUN_ID="$run_id"
log "step 4 OK — fired flow $flow_id, runId=$run_id"

# --- Step 5: observe the run -------------------------------------------------
# FlowRunStatus is snake_case on the wire: in_flight | ok | errored |
# interrupted | skipped_disabled (docs/flows/http-api.md §2).
deadline=$(( $(date +%s) + OBS_WINDOW_S ))
final_status="timeout"
polls=0
while :; do
    polls=$(( polls + 1 ))
    do_req 5 GET "$BASE_URL/api/flows/$flow_id/runs?limit=1"
    if [[ "$RESP_CODE" == "200" ]]; then
        last_id="$(jq -r '.runs[0].id // empty' "$RESP_BODY")"
        last_status="$(jq -r '.runs[0].status // empty' "$RESP_BODY")"
        if [[ -n "$last_id" && "$last_id" == "$run_id" ]]; then
            case "$last_status" in
                ok)                    final_status="ok"; break ;;
                errored|interrupted)   final_status="$last_status"; break ;;
            esac
        fi
    fi
    if [[ "$(date +%s)" -ge "$deadline" ]]; then
        final_status="timeout"
        break
    fi
    sleep 0.5
done

case "$final_status" in
    ok)
        log "step 5 OK — run $run_id reached status=ok after $polls poll(s)"
        ;;
    errored|interrupted)
        run_err="$(jq -r '.runs[0].error // "no error string"' "$RESP_BODY")"
        fail "run $run_id ended status=$final_status: $run_err"
        try_cleanup "$flow_id"
        exit "$EXIT_ERRORED"
        ;;
    timeout)
        fail "run $run_id did not complete within ${OBS_WINDOW_S}s ($polls polls)"
        try_cleanup "$flow_id"
        exit "$EXIT_OBSERVE"
        ;;
esac

# --- Step 6: cleanup ---------------------------------------------------------
do_req 6 DELETE "$BASE_URL/api/flows/$flow_id?force=true"
if [[ "$RESP_CODE" != "204" ]]; then
    warn "cleanup: DELETE flow $flow_id expected 204, got $RESP_CODE"
    warn "probe verdict is GREEN; stray flow id=$flow_id needs manual removal"
    exit "$EXIT_CLEANUP"
fi
log "step 6 OK — deleted flow $flow_id"

log "PASS — V6 flow trigger probe green against $BASE_URL"
exit 0
