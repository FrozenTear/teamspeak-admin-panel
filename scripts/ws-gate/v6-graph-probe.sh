#!/usr/bin/env bash
# scripts/ws-gate/v6-graph-probe.sh — WS-Gate V6g (graph flow) gate probe for v2.
#
# Adds the V6g row of the Chapter 1 verification matrix (flow-engine v2,
# PURA-259 / PURA-268). See docs/flows/v2/gate.md §2 and docs/flows/v2/
# http-api.md for the contract this probe drives.
#
# v6-probe.sh proves the *linear* flow path (create → enable → fire → ok).
# That exercises no topology. v6-graph-probe.sh builds one graph that uses
# every v2 feature — transform → branch → parallel(sub-flow) → join — fires
# it, and asserts the **per-node** outcome: the taken branch ran, the
# not-taken branches were skipped, the parallel fan-out ran, and the run
# settled `ok`. It also drives the negative path: POST /api/flows/validate
# with a cyclic graph must reject.
#
# Sequence (docs/flows/v2/gate.md §2.3):
#   1. GET    {HEALTH_PATH}                pre-check the backend is up
#   2. POST   /api/flows/validate          cyclic graph -> valid=false, graph_cycle
#   3. POST   /api/flows                   create the logLine sub-flow graph
#   4. POST   /api/flows                   create the main branch+parallel graph
#   5. PATCH  /api/flows/{id}              enabled=true
#   6. POST   /api/flows/{id}/fire         manual-fire it
#   7. GET    /api/flows/{id}/runs/{runId} poll until terminal; assert per-node
#   8. DELETE /api/flows/{id}?force=true   clean up both flows
#
# Exits 0 on success. All request/response bodies are captured into
#   qa-evidence/ws-gate/v6-graph/<ISO8601-UTC>/step-N.{req,resp}.json
# plus a verdict.json summary — same evidence shape as v6-probe.sh.
#
# Inputs:
#   BASE_URL           positional 1, required   e.g. https://manager.scuffedcrew.no
#   ADMIN_TOKEN        env, required            admin JWT for the manager
#   SERVER_CONFIG_ID   env, default 1
#   VIRTUAL_SERVER_ID  env, default 1
#   OBS_WINDOW_S       env, default 30          observation window, seconds
#   HEALTH_PATH        env, default /health     the manager mounts /health at
#                                               the root (not /api/health —
#                                               gate.md §2.3 is stale on this
#                                               exact point, the same way
#                                               v1.1-gate.md §2.2 was; see the
#                                               note in v6-probe.sh).
#   EVID_DIR           env, default qa-evidence/ws-gate/v6-graph/<timestamp>
#
# Exit codes (docs/flows/v2/gate.md §2.4):
#   0   all per-node assertions passed inside the observation window
#   64  usage error (missing BASE_URL or ADMIN_TOKEN)
#   65  health pre-check failed (HEALTH_PATH non-2xx)
#   66  POST /validate missing or did not reject the cyclic graph (validator
#       not wired — a v1.1 image fails fast here)
#   67  sub-flow or main-graph create failed (graph create rejected — a v1.1
#       image with no FlowSpec graph support fails fast here)
#   68  enable failed
#   69  fire failed
#   70  observation window expired without a terminal run status
#   71  run reached a terminal status but it was not `ok`
#   72  per-node assertion failed — a branch/parallel/skip outcome was wrong
#       (the v2-specific topology-regression code)
#   73  cleanup failed (probe verdict still GREEN for the matrix; warns)

set -euo pipefail

PROBE_NAME="v6-graph-probe"

EXIT_USAGE=64
EXIT_PRECHECK=65
EXIT_VALIDATE=66
EXIT_CREATE=67
EXIT_ENABLE=68
EXIT_FIRE=69
EXIT_OBSERVE=70
EXIT_ERRORED=71
EXIT_TOPOLOGY=72
EXIT_CLEANUP=73

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
EVID_DIR="${EVID_DIR:-qa-evidence/ws-gate/v6-graph/$TS}"
mkdir -p "$EVID_DIR"

FINAL_FLOW_ID=""
FINAL_SUB_FLOW_ID=""
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
            --arg subflow "$FINAL_SUB_FLOW_ID" \
            --arg run "$FINAL_RUN_ID" \
            --argjson rc "$rc" \
            '{
                probe: $probe,
                timestamp: $ts,
                baseUrl: $base,
                flowId: $flow,
                subFlowId: $subflow,
                runId: $run,
                exitCode: $rc,
                verdict: (if $rc == 0 then "PASS"
                          elif $rc == 73 then "PASS_CLEANUP_WARNING"
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

# ---- Graph builders ----------------------------------------------------
# Every node carries a `position` (required by the v2 wire type — the engine
# ignores it). `kind` is flattened onto the node object; trigger/action
# `config` reuses the v1.1 Trigger/Action enums verbatim (http-api.md §2).

# A cyclic graph for the negative validate step: trigger -> c_a -> c_b -> c_a.
cyclic_graph() {
    jq -n '{
        nodes: [
          { id: "c_trigger", position: { x: 0, y: 0 },
            kind: "trigger", config: { kind: "manualFire" } },
          { id: "c_a", position: { x: 0, y: 100 },
            kind: "action", config: { kind: "logLine", message: "cycle a" } },
          { id: "c_b", position: { x: 0, y: 200 },
            kind: "action", config: { kind: "logLine", message: "cycle b" } }
        ],
        edges: [
          { id: "ce1", from: { node: "c_trigger", port: "out" },
                       to:   { node: "c_a",       port: "in"  } },
          { id: "ce2", from: { node: "c_a", port: "out" },
                       to:   { node: "c_b", port: "in"  } },
          { id: "ce3", from: { node: "c_b", port: "out" },
                       to:   { node: "c_a", port: "in"  } }
        ]
    }'
}

# The sub-flow the parallel node fans out over: a one-action logLine graph.
sub_flow_graph() {
    local msg="$1"
    jq -n --arg msg "$msg" '{
        nodes: [
          { id: "sub_trigger", position: { x: 0, y: 0 },
            kind: "trigger", config: { kind: "manualFire" } },
          { id: "sub_log", position: { x: 0, y: 100 },
            kind: "action", config: { kind: "logLine", message: $msg } }
        ],
        edges: [
          { id: "se1", from: { node: "sub_trigger", port: "out" },
                       to:   { node: "sub_log",     port: "in"  } }
        ]
    }'
}

# The main graph (gate.md §2.2): trigger -> transform -> branch -> {logA,
# logB, logDefault} -> logA -> parallel(fan_out) -> join. The transform sets
# route="a", so branch case "a" fires and case "b"/default are pruned.
#
# `fan_out.collection` is the expression `trigger.context.items` — the v2
# expression dialect has no array-literal syntax (`[ … ]` is the index
# operator only), so the 2-element collection gate.md §2.2 calls for is
# supplied via the fire context (step 6) and read back off the blackboard,
# the pattern the engine's own parallel tests use.
main_graph() {
    local sub_flow_id="$1" msg="$2"
    jq -n --argjson sub "$sub_flow_id" --arg msg "$msg" '{
        nodes: [
          { id: "trigger", position: { x: 0, y: 0 },
            kind: "trigger", config: { kind: "manualFire" } },
          { id: "transform", position: { x: 0, y: 100 },
            kind: "transform",
            output: { route: "\"a\"", n: "2" } },
          { id: "branch", position: { x: 0, y: 200 },
            kind: "branch",
            cases: [
              { label: "a", when: "input.route == \"a\"" },
              { label: "b", when: "input.route == \"b\"" }
            ] },
          { id: "logA", position: { x: -100, y: 300 },
            kind: "action",
            config: { kind: "logLine", message: ($msg + " branch=a") } },
          { id: "logB", position: { x: 0, y: 300 },
            kind: "action",
            config: { kind: "logLine", message: ($msg + " branch=b") } },
          { id: "logDefault", position: { x: 100, y: 300 },
            kind: "action",
            config: { kind: "logLine", message: ($msg + " branch=default") } },
          { id: "fan_out", position: { x: -100, y: 400 },
            kind: "parallel",
            collection: "trigger.context.items",
            subFlowId: $sub,
            maxConcurrency: 2 },
          { id: "join", position: { x: -100, y: 500 },
            kind: "action",
            config: { kind: "logLine", message: ($msg + " join") } }
        ],
        edges: [
          { id: "e1", from: { node: "trigger",   port: "out"     },
                      to:   { node: "transform", port: "in"      } },
          { id: "e2", from: { node: "transform", port: "out"     },
                      to:   { node: "branch",    port: "in"      } },
          { id: "e3", from: { node: "branch",    port: "a"       },
                      to:   { node: "logA",      port: "in"      } },
          { id: "e4", from: { node: "branch",    port: "b"       },
                      to:   { node: "logB",      port: "in"      } },
          { id: "e5", from: { node: "branch",    port: "default" },
                      to:   { node: "logDefault", port: "in"     } },
          { id: "e6", from: { node: "logA",      port: "out"     },
                      to:   { node: "fan_out",   port: "in"      } },
          { id: "e7", from: { node: "fan_out",   port: "out"     },
                      to:   { node: "join",      port: "in"      } }
        ]
    }'
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

# --- Step 2: negative validate — a cyclic graph must reject ------------------
# Proves the validator is wired (a v1.1 image has no /validate route — the
# 404 here is the documented v1.1 fail-fast, exit 66).
validate_payload="$(jq -n --argjson g "$(cyclic_graph)" '{ graph: $g }')"
do_req 2 POST "$BASE_URL/api/flows/validate" "$validate_payload"
if [[ "$RESP_CODE" != "200" ]]; then
    fail "validate: POST /api/flows/validate expected 200, got $RESP_CODE"
    fail "(a v1.1 image has no /validate route — this is the expected v1.1 fail-fast)"
    exit "$EXIT_VALIDATE"
fi
v_valid="$(jq -r '.valid' "$RESP_BODY" 2>/dev/null || echo "null")"
v_has_cycle="$(jq -r '[.errors[]?.code] | any(. == "graph_cycle")' \
    "$RESP_BODY" 2>/dev/null || echo "false")"
if [[ "$v_valid" != "false" ]]; then
    fail "validate: cyclic graph reported valid=$v_valid, expected false"
    exit "$EXIT_VALIDATE"
fi
if [[ "$v_has_cycle" != "true" ]]; then
    fail "validate: cyclic graph rejected but no errors[].code == graph_cycle"
    exit "$EXIT_VALIDATE"
fi
log "step 2 OK — cyclic graph rejected (valid=false, graph_cycle)"

# --- Step 3: create the sub-flow ---------------------------------------------
rand_hex="$(od -An -N4 -tx1 /dev/urandom | tr -d ' \n')"
probe_id="$TS-$$"
sub_name="ws-gate-v6g-sub-$rand_hex"
sub_payload="$(jq -n \
    --arg name "$sub_name" \
    --arg desc "WS-Gate V6g sub-flow" \
    --argjson scid "$SERVER_CONFIG_ID" \
    --argjson vsid "$VIRTUAL_SERVER_ID" \
    --argjson graph "$(sub_flow_graph "ws-gate v6-graph-probe sub-flow $probe_id")" \
    '{
        name: $name,
        description: $desc,
        serverConfigId: $scid,
        virtualServerId: $vsid,
        enabled: false,
        graph: $graph
    }')"

do_req 3 POST "$BASE_URL/api/flows" "$sub_payload"
if [[ "$RESP_CODE" != "201" ]]; then
    fail "create sub-flow: expected 201, got $RESP_CODE (see step-3.resp.json)"
    fail "(a v1.1 image rejects a FlowSpec graph body — expected v1.1 fail-fast)"
    exit "$EXIT_CREATE"
fi
sub_flow_id="$(jq -r '.id // empty' "$RESP_BODY")"
if [[ -z "$sub_flow_id" ]]; then
    fail "create sub-flow: 201 but no .id in response body"
    exit "$EXIT_CREATE"
fi
FINAL_SUB_FLOW_ID="$sub_flow_id"
log "step 3 OK — created sub-flow id=$sub_flow_id"

# --- Step 4: create the main graph -------------------------------------------
main_name="ws-gate-v6g-$rand_hex"
main_payload="$(jq -n \
    --arg name "$main_name" \
    --arg desc "WS-Gate V6g graph probe" \
    --argjson scid "$SERVER_CONFIG_ID" \
    --argjson vsid "$VIRTUAL_SERVER_ID" \
    --argjson graph "$(main_graph "$sub_flow_id" "ws-gate v6-graph-probe $probe_id")" \
    '{
        name: $name,
        description: $desc,
        serverConfigId: $scid,
        virtualServerId: $vsid,
        enabled: false,
        graph: $graph
    }')"

do_req 4 POST "$BASE_URL/api/flows" "$main_payload"
if [[ "$RESP_CODE" != "201" ]]; then
    fail "create main graph: expected 201, got $RESP_CODE (see step-4.resp.json)"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_CREATE"
fi
flow_id="$(jq -r '.id // empty' "$RESP_BODY")"
flow_version="$(jq -r '.flowVersion // empty' "$RESP_BODY")"
if [[ -z "$flow_id" ]]; then
    fail "create main graph: 201 but no .id in response body"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_CREATE"
fi
FINAL_FLOW_ID="$flow_id"
if [[ "$flow_version" != "2" ]]; then
    fail "create main graph: expected flowVersion=2, got '$flow_version'"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_CREATE"
fi
log "step 4 OK — created graph flow id=$flow_id flowVersion=2"

# --- Step 5: enable ----------------------------------------------------------
do_req 5 PATCH "$BASE_URL/api/flows/$flow_id" '{"enabled":true}'
if [[ "$RESP_CODE" != "200" ]]; then
    fail "enable flow $flow_id: expected 200, got $RESP_CODE"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_ENABLE"
fi
enabled="$(jq -r '.enabled // empty' "$RESP_BODY")"
if [[ "$enabled" != "true" ]]; then
    fail "enable flow $flow_id: 200 but enabled=$enabled"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_ENABLE"
fi
log "step 5 OK — flow $flow_id enabled"

# --- Step 6: manual fire -----------------------------------------------------
# The fire context carries `items` — a 2-element array — which the `fan_out`
# parallel node reads via `trigger.context.items` (see main_graph()).
do_req 6 POST "$BASE_URL/api/flows/$flow_id/fire" '{"context":{"items":[1,2]}}'
if [[ "$RESP_CODE" != "202" ]]; then
    fail "fire flow $flow_id: expected 202, got $RESP_CODE"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_FIRE"
fi
run_id="$(jq -r '.runId // empty' "$RESP_BODY")"
if [[ -z "$run_id" ]]; then
    fail "fire flow $flow_id: 202 but no .runId in response body"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_FIRE"
fi
FINAL_RUN_ID="$run_id"
log "step 6 OK — fired flow $flow_id, runId=$run_id"

# --- Step 7: observe the run, then assert per-node ---------------------------
# FlowRunStatus is snake_case on the wire: in_flight | ok | errored |
# interrupted | skipped_disabled (http-api.md §2.1). `skipped` is a *node*
# status only, never a run status.
deadline=$(( $(date +%s) + OBS_WINDOW_S ))
final_status="timeout"
polls=0
while :; do
    polls=$(( polls + 1 ))
    do_req 7 GET "$BASE_URL/api/flows/$flow_id/runs/$run_id"
    if [[ "$RESP_CODE" == "200" ]]; then
        last_status="$(jq -r '.status // empty' "$RESP_BODY")"
        case "$last_status" in
            ok)                                final_status="ok"; break ;;
            errored|interrupted|skipped_disabled)
                final_status="$last_status"; break ;;
        esac
    fi
    if [[ "$(date +%s)" -ge "$deadline" ]]; then
        final_status="timeout"
        break
    fi
    sleep 0.5
done

if [[ "$final_status" == "timeout" ]]; then
    fail "run $run_id did not reach a terminal status within ${OBS_WINDOW_S}s ($polls polls)"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_OBSERVE"
fi
if [[ "$final_status" != "ok" ]]; then
    run_err="$(jq -r '.error // "no error string"' "$RESP_BODY")"
    fail "run $run_id ended status=$final_status: $run_err"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_ERRORED"
fi
log "step 7a OK — run $run_id reached status=ok after $polls poll(s)"

# Per-node assertions (gate.md §2.3 step 7). nodeResults is the array on the
# run-detail body; index it by nodeId. A wrong outcome here is exit 72 — the
# v2-specific topology regression (branch took the wrong path, a not-taken
# node ran, a join settled early).
node_status() {
    jq -r --arg id "$1" \
        'first(.nodeResults[]? | select(.nodeId == $id) | .status) // "<absent>"' \
        "$RESP_BODY" 2>/dev/null || echo "<absent>"
}

topo_fail=0
assert_node() {
    local node="$1" want="$2" got
    got="$(node_status "$node")"
    if [[ "$got" == "$want" ]]; then
        log "  node $node: $got (expected $want) — OK"
    else
        fail "  node $node: got '$got', expected '$want'"
        topo_fail=$(( topo_fail + 1 ))
    fi
}

# transform produced data; branch routed on it; the "a" path ran; both
# not-taken branches were pruned to skipped; the parallel fan-out ran; the
# join settled.
assert_node transform  ok
assert_node branch     ok
assert_node logA       ok
assert_node logB       skipped
assert_node logDefault skipped
assert_node fan_out    ok
assert_node join       ok

if [[ "$topo_fail" -ne 0 ]]; then
    fail "per-node assertions failed ($topo_fail node(s)) — v2 topology regression"
    try_cleanup "$flow_id"
    try_cleanup "$sub_flow_id"
    exit "$EXIT_TOPOLOGY"
fi
log "step 7b OK — all per-node outcomes correct (branch + parallel + skips)"

# --- Step 8: cleanup ---------------------------------------------------------
cleanup_warn=0
do_req 8 DELETE "$BASE_URL/api/flows/$flow_id?force=true"
if [[ "$RESP_CODE" != "204" ]]; then
    warn "cleanup: DELETE flow $flow_id expected 204, got $RESP_CODE"
    warn "stray flow id=$flow_id needs manual removal"
    cleanup_warn=1
else
    log "step 8a OK — deleted main flow $flow_id"
fi

do_req 8b DELETE "$BASE_URL/api/flows/$sub_flow_id?force=true"
if [[ "$RESP_CODE" != "204" ]]; then
    warn "cleanup: DELETE sub-flow $sub_flow_id expected 204, got $RESP_CODE"
    warn "stray flow id=$sub_flow_id needs manual removal"
    cleanup_warn=1
else
    log "step 8b OK — deleted sub-flow $sub_flow_id"
fi

if [[ "$cleanup_warn" -ne 0 ]]; then
    warn "probe verdict is GREEN; cleanup left stray flow(s) — see warnings above"
    exit "$EXIT_CLEANUP"
fi

log "PASS — V6g graph flow probe green against $BASE_URL"
exit 0
