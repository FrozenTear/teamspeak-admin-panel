# shellcheck shell=bash
# scripts/ws-gate/_probe-lib.sh — shared plumbing for the WS-Gate probes.
#
# This file is NOT a probe: the run-all.sh umbrella globs `*-probe.sh`, and
# this `-lib.sh` suffix keeps it out of that fan-out. It is `source`d by the
# v2/v3/v5/v7 probes (THE-1014) to avoid copy-pasting the boot / evidence /
# request / verdict machinery first written inline in v6-probe.sh and
# admin-probe.sh. The v6/admin probes stay self-contained on purpose (they
# predate this lib); new probes share it.
#
# Three run modes, selected per probe from BASE_URL + WS_GATE_DRY_RUN:
#
#   1. DRY RUN  (WS_GATE_DRY_RUN=1)
#        No server boot, no network. Validates tooling + that every request
#        payload the probe will send actually builds, writes evidence +
#        verdict, exits 0. This is the path run-all.sh uses to prove the
#        suite is turnkey from the headless heartbeat env (no reachable TS
#        server, no release-image runner — see THE-1013/THE-1009).
#   2. LIVE     (BASE_URL given as $1)
#        Runs against an already-running manager. The future runner provides
#        a reachable manager + a NON-PROD TeamSpeak target via the TS_* env.
#   3. SELF-BOOT (no BASE_URL, not dry-run)
#        Boots ts6-manager-server itself against an in-memory SurrealDB
#        (guaranteed fresh deploy), exactly like admin-probe.sh. Lets the
#        env-independent assertions (row persistence, auth contract, 404
#        paths) run green against the *source build* without any TS server.
#
# Convention: evidence lands in
#   qa-evidence/ws-gate/<probe>/<ISO8601-UTC>/step-N.{req,resp}.json + verdict.json
# (see reference QA evidence convention).

# --- shared exit codes (probes may add their own >=72) ---------------------
WSG_EXIT_USAGE=64       # bad args / missing tool
WSG_EXIT_BOOT=65        # self-boot build/boot/readiness failed
WSG_EXIT_PRECHECK=66    # health / setup pre-check failed
# probe-specific step failures use 67..71 to mirror v6/admin.

# Populated by wsg_init: PROBE_NAME, TS, EVID_DIR, BASE_URL, DRY_RUN.
PROBE_NAME="${PROBE_NAME:-ws-gate-probe}"
DRY_RUN=0
SELF_BOOTED=0
SERVER_PID=""
VERDICT_NOTE=""

wsg_log()  { printf '[%s] %s\n' "$PROBE_NAME" "$*"; }
wsg_fail() { printf '[%s] FAIL: %s\n' "$PROBE_NAME" "$*" >&2; }
wsg_warn() { printf '[%s] WARN: %s\n' "$PROBE_NAME" "$*" >&2; }

# wsg_require_tools curl jq [...]
wsg_require_tools() {
    local t
    for t in "$@"; do
        command -v "$t" >/dev/null 2>&1 || {
            wsg_fail "missing required tool: $t"
            exit "$WSG_EXIT_USAGE"
        }
    done
}

# wsg_init PROBE_NAME BASE_URL
# Sets PROBE_NAME, DRY_RUN, TS, EVID_DIR, BASE_URL (normalised). Installs the
# verdict EXIT trap. Honours WS_GATE_DRY_RUN and the EVID_DIR override.
wsg_init() {
    PROBE_NAME="$1"
    BASE_URL="${2:-}"
    BASE_URL="${BASE_URL%/}"

    case "${WS_GATE_DRY_RUN:-0}" in
        1|true|yes) DRY_RUN=1 ;;
        *)          DRY_RUN=0 ;;
    esac

    wsg_require_tools curl jq

    TS="$(date -u +%Y%m%dT%H%M%SZ)"
    EVID_DIR="${EVID_DIR:-qa-evidence/ws-gate/${PROBE_NAME#ws-gate-}/$TS}"
    # Strip a redundant "-probe" so the dir reads .../ws-gate/v2/<ts>.
    EVID_DIR="${EVID_DIR/-probe\//\/}"
    mkdir -p "$EVID_DIR"
    trap wsg_on_exit EXIT
    wsg_log "mode=$( [[ $DRY_RUN -eq 1 ]] && echo dry-run || { [[ -n $BASE_URL ]] && echo live || echo self-boot; } ) evidence=$EVID_DIR"
}

# shellcheck disable=SC2317  # runs via the EXIT trap, not inline.
wsg_on_exit() {
    local rc=$?
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ -d "${EVID_DIR:-/nonexistent}" ]]; then
        jq -n \
            --arg probe "$PROBE_NAME" \
            --arg ts "$TS" \
            --arg base "${BASE_URL:-self-booted}" \
            --arg note "$VERDICT_NOTE" \
            --argjson dry "$DRY_RUN" \
            --argjson rc "$rc" \
            '{
                probe: $probe,
                timestamp: $ts,
                baseUrl: $base,
                dryRun: ($dry == 1),
                note: $note,
                exitCode: $rc,
                verdict: (if $rc == 0 then "PASS"
                          elif $rc == 71 then "PASS_CLEANUP_WARNING"
                          else "FAIL" end)
            }' >"$EVID_DIR/verdict.json" 2>/dev/null || true
    fi
}

# wsg_dry_check_payload LABEL JSON
# In dry-run, asserts JSON is well-formed (it is built via jq already, so this
# is a belt-and-braces check) and records it under step-dry-LABEL.req.json.
wsg_dry_check_payload() {
    local label="$1" json="$2"
    if ! printf '%s' "$json" | jq -e . >/dev/null 2>&1; then
        wsg_fail "dry-run: payload '$label' is not valid JSON"
        exit "$WSG_EXIT_USAGE"
    fi
    printf '%s' "$json" | jq '
        if has("apiKey") then .apiKey = "***REDACTED***" else . end
        | if has("password") then .password = "***REDACTED***" else . end
        | if has("sshPassword") then .sshPassword = "***REDACTED***" else . end
    ' >"$EVID_DIR/step-dry-$label.req.json" 2>/dev/null || true
    wsg_log "dry-run: payload '$label' builds OK"
}

# do_req STEP METHOD PATH [JSON_BODY] [BEARER_TOKEN]
# Writes step-STEP.{req,resp}.json into $EVID_DIR. Redacts secrets in the
# stored request body. Sets RESP_CODE (int string) and RESP_BODY (file path).
RESP_CODE=""
RESP_BODY=""
do_req() {
    local step="$1" method="$2" path="$3" data="${4:-}" token="${5:-}"
    local url="$BASE_URL$path"
    local req_file="$EVID_DIR/step-$step.req.json"
    local resp_file="$EVID_DIR/step-$step.resp.json"
    RESP_BODY="$resp_file"

    local -a args=(-sS -o "$resp_file" -w '%{http_code}' -X "$method" "$url")
    [[ -n "$token" ]] && args+=(-H "Authorization: Bearer $token")
    if [[ -n "$data" ]]; then
        args+=(-H 'Content-Type: application/json' --data "$data")
        printf '%s' "$data" | jq '
            if has("apiKey") then .apiKey = "***REDACTED***" else . end
            | if has("password") then .password = "***REDACTED***" else . end
            | if has("sshPassword") then .sshPassword = "***REDACTED***" else . end
            | if has("sshPrivateKey") then .sshPrivateKey = "***REDACTED***" else . end
            | if has("refreshToken") then .refreshToken = "***REDACTED***" else . end
        ' >"$req_file" 2>/dev/null || echo '{"redacted":true}' >"$req_file"
    else
        echo 'null' >"$req_file"
    fi

    RESP_CODE=$(curl "${args[@]}" 2>>"$EVID_DIR/curl.log" || echo 000)
    [[ -s "$resp_file" ]] || echo '{}' >"$resp_file"
}

# wsg_expect CODE CONTEXT EXIT_CODE
wsg_expect() {
    local want="$1" ctx="$2" rc="$3"
    if [[ "$RESP_CODE" != "$want" ]]; then
        wsg_fail "$ctx — expected HTTP $want, got $RESP_CODE"
        echo "  response body:" >&2
        sed 's/^/    /' "$RESP_BODY" >&2 || true
        exit "$rc"
    fi
}

# wsg_assert_no_secret_leak FILE CONTEXT
# Fails if a response body contains anything that looks like a credential the
# wire contract must never expose (sealed apiKey/ssh secrets, JWT, Bearer).
wsg_assert_no_secret_leak() {
    local file="$1" ctx="$2"
    if grep -Eiq '"(apikey|sshpassword|sshprivatekey|password|accesstoken|refreshtoken)"[[:space:]]*:[[:space:]]*"[^"]+"|bearer [a-z0-9._-]{12,}|eyJ[a-z0-9._-]{20,}' "$file"; then
        wsg_fail "$ctx — response body appears to leak a credential/token"
        sed 's/^/    /' "$file" >&2 || true
        exit "$WSG_EXIT_PRECHECK"
    fi
}

# wsg_self_boot — boot ts6-manager-server on a free port against in-memory
# SurrealDB. Sets BASE_URL + SERVER_PID + SELF_BOOTED. Mirrors admin-probe.sh.
# Honours SERVER_BIN (skip build), PROBE_PORT, BUILD_TIMEOUT_S, READY_TIMEOUT_S.
wsg_self_boot() {
    wsg_require_tools python3 cargo
    PROBE_PORT="${PROBE_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')}"
    BASE_URL="http://127.0.0.1:${PROBE_PORT}"
    local build_timeout="${BUILD_TIMEOUT_S:-600}"
    local ready_timeout="${READY_TIMEOUT_S:-60}"
    local bin

    if [[ -n "${SERVER_BIN:-}" ]]; then
        [[ -x "$SERVER_BIN" ]] || { wsg_fail "SERVER_BIN not executable: $SERVER_BIN"; exit "$WSG_EXIT_BOOT"; }
        bin="$SERVER_BIN"
    else
        wsg_log "building ts6-manager-server (set SERVER_BIN to skip)…"
        if ! timeout "$build_timeout" cargo build -p ts6-manager-server >"$EVID_DIR/build.log" 2>&1; then
            wsg_fail "cargo build failed — see $EVID_DIR/build.log"; tail -20 "$EVID_DIR/build.log" >&2 || true
            exit "$WSG_EXIT_BOOT"
        fi
        bin="$(cargo metadata --format-version 1 --no-deps 2>/dev/null | jq -r '.target_directory')/debug/ts6-manager-server"
        [[ -x "$bin" ]] || bin="target/debug/ts6-manager-server"
    fi
    [[ -x "$bin" ]] || { wsg_fail "server binary not found: $bin"; exit "$WSG_EXIT_BOOT"; }

    # dioxus-server panics at boot if <exe-dir>/public is absent; an empty dir
    # is fine for an API-only gate (see admin-probe.sh for the rationale).
    local public_dir; public_dir="$(dirname "$bin")/public"
    [[ -d "$public_dir" ]] || { mkdir -p "$public_dir"; wsg_log "created empty $public_dir (API-only)."; }

    wsg_log "booting server on $BASE_URL (in-memory SurrealDB, fresh deploy)…"
    env -u NODE_ENV \
        HOST=127.0.0.1 PORT="$PROBE_PORT" \
        DATABASE_URL="memory" \
        JWT_SECRET="ws-gate-probe-throwaway-secret-0001" \
        "$bin" >"$EVID_DIR/server.log" 2>&1 &
    SERVER_PID=$!
    SELF_BOOTED=1

    local deadline=$(( $(date +%s) + ready_timeout ))
    while [[ $(date +%s) -lt $deadline ]]; do
        if ! kill -0 "$SERVER_PID" 2>/dev/null; then
            wsg_fail "server exited during boot — see $EVID_DIR/server.log"; tail -20 "$EVID_DIR/server.log" >&2 || true
            exit "$WSG_EXIT_BOOT"
        fi
        local code; code=$(curl -fsS -o /dev/null -w '%{http_code}' "$BASE_URL/api/setup/status" 2>/dev/null || echo 000)
        [[ "$code" == "200" ]] && { wsg_log "server ready."; return 0; }
        sleep 1
    done
    wsg_fail "server not ready within ${ready_timeout}s"
    exit "$WSG_EXIT_BOOT"
}

# wsg_bootstrap_login — on a fresh self-booted deploy, run setup/init + login.
# Exports ADMIN_TOKEN. Uses BOOTSTRAP_USER/PASS (defaults compliant).
wsg_bootstrap_login() {
    local user="${BOOTSTRAP_USER:-bootstrap}" pass="${BOOTSTRAP_PASS:-Bootstrap1!pw}"
    do_req boot-status GET /api/setup/status
    wsg_expect 200 "setup/status" "$WSG_EXIT_PRECHECK"
    if [[ "$(jq -r '.needsSetup' "$RESP_BODY")" == "true" ]]; then
        local init_body
        init_body=$(jq -n --arg u "$user" --arg p "$pass" '{
            username:$u, password:$p, displayName:"WS-Gate Admin",
            server:{ name:"ws-gate-probe", host:"ts.invalid.example", apiKey:"ws-gate-probe-key" }
        }')
        do_req boot-init POST /api/setup/init "$init_body"
        wsg_expect 201 "setup/init" "$WSG_EXIT_PRECHECK"
    fi
    local login_body
    login_body=$(jq -n --arg u "$user" --arg p "$pass" '{username:$u,password:$p}')
    do_req boot-login POST /api/auth/login "$login_body"
    wsg_expect 200 "bootstrap login" "$WSG_EXIT_PRECHECK"
    ADMIN_TOKEN=$(jq -r '.accessToken // empty' "$RESP_BODY")
    [[ -n "$ADMIN_TOKEN" ]] || { wsg_fail "login: no accessToken"; exit "$WSG_EXIT_PRECHECK"; }
    wsg_log "bootstrap admin '$user' signed in."
}

# wsg_resolve_target — establish BASE_URL + ADMIN_TOKEN for live/self-boot.
# Live mode expects ADMIN_TOKEN in env. Self-boot mints one via bootstrap.
wsg_resolve_target() {
    if [[ -n "$BASE_URL" ]]; then
        ADMIN_TOKEN="${ADMIN_TOKEN:-}"
        [[ -n "$ADMIN_TOKEN" ]] || { wsg_fail "live mode: ADMIN_TOKEN env is required"; exit "$WSG_EXIT_USAGE"; }
    else
        wsg_self_boot
        wsg_bootstrap_login
    fi
}
