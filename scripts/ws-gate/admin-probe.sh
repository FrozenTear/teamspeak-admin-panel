#!/usr/bin/env bash
# scripts/ws-gate/admin-probe.sh — WS-Gate admin-management probe for v1.1.
#
# Mirrors the scripts/ws-gate/*-probe.sh evidence/verdict pattern (see
# v6-probe.sh) for the admin-management workstream. Exercises the
# "bootstrap admin → second admin → sign-in → disable → audit replay →
# last-admin protection" gate shape ratified for PURA-227 / PURA-230.
#
# Verification steps (PURA-239 §Scope):
#   1. Fresh-deploy bootstrap → bootstrap admin signs in → access+refresh.
#   2. POST /api/users creates a second user `mod-1` (role `moderator`).
#   3. `mod-1` signs in; moderator CANNOT hit admin-only GET /api/users (403).
#   4. Bootstrap admin disables `mod-1` (PATCH enabled:false).
#   5. `mod-1`'s refresh-token family is revoked: POST /api/auth/refresh 401.
#   6. GET /api/audit on the bootstrap admin shows `userCreated` then
#      `userDisabled` for `mod-1`, with actorUsername / targetLabel / kind
#      matching docs/admin/audit-shape.md §2.
#   7. Last-enabled-admin protection: PATCH the sole bootstrap admin
#      role→moderator is refused (see CONTRACT NOTE below).
#
# CONTRACT NOTE (step 7) — issue text vs shipped contract.
#   PURA-239 §Scope step 7 asks for `409 Conflict` + a `last_admin_protected`
#   error code. The shipped routes (crates/.../routes/users.rs, the contract
#   in docs/admin/http-api.md §3.2) return `400 Bad Request` with the plain
#   envelope `{"error":"Cannot remove the last enabled admin"}` — there is no
#   `last_admin_protected` code and no 409. http-api.md §3.2 only uses 409 for
#   duplicate-username. This probe asserts the SHIPPED contract so it stays
#   green on a correct deploy; the 409/code wording is a doc-vs-impl
#   deviation tracked for docs/deviations/admin-routes-v1.1 (PURA-240, which
#   this gate blocks). Override via LAST_ADMIN_EXPECT_STATUS if the contract
#   later changes.
#
#   Step 7 uses the PATCH demote path, not DELETE: delete_user checks the
#   self-delete guard ("Cannot delete yourself") BEFORE the last-admin
#   branch, so `DELETE /api/users/{self}` can never reach last-admin
#   protection in a single-admin deploy. patch_user checks last-admin
#   FIRST, so a self-PATCH demote genuinely exercises it (matches the
#   handler's "example 4.3" comment).
#
# Usage:
#   scripts/ws-gate/admin-probe.sh [BASE_URL]
#     BASE_URL   optional positional. When given, the probe runs against an
#                already-running FRESH deploy (needsSetup must be true) and
#                does NOT boot or tear down a server. When omitted, the probe
#                builds + boots ts6-manager-server itself against an
#                in-memory SurrealDB (guaranteed fresh deploy) and tears it
#                down on exit.
#
# Optional env:
#   SERVER_BIN              prebuilt ts6-manager-server binary; skips the
#                           `cargo build` step (recommended for CI).
#   PROBE_PORT              listen port for the self-booted server
#                           (default: a free port chosen at runtime).
#   READY_TIMEOUT_S         seconds to wait for /api/setup/status (default 60).
#   BUILD_TIMEOUT_S         seconds for `cargo build` (default 600).
#   BOOTSTRAP_USER          bootstrap admin username (default `bootstrap`).
#   BOOTSTRAP_PASS          bootstrap admin password (default a compliant one).
#   MOD_USER                second-user username   (default `mod-1`).
#   MOD_PASS                second-user password   (default a compliant one).
#   LAST_ADMIN_EXPECT_STATUS  expected step-7 status (default 400; see note).
#   EVID_DIR                evidence dir (default qa-evidence/ws-gate/admin/<ts>).
#
# Exit codes:
#   0   all seven steps green
#   64  usage error
#   65  server build / boot / readiness failed
#   66  step 1 — bootstrap or bootstrap-admin login failed
#   67  step 2 — second-user create failed
#   68  step 3 — mod-1 login or moderator-403 check failed
#   69  step 4 — disable mod-1 failed
#   70  step 5 — refresh-revocation check failed
#   71  step 6 — audit replay failed
#   72  step 7 — last-admin protection failed

set -euo pipefail

PROBE_NAME="admin-probe"

EXIT_USAGE=64
EXIT_BOOT=65
EXIT_BOOTSTRAP=66
EXIT_CREATE=67
EXIT_MODLOGIN=68
EXIT_DISABLE=69
EXIT_REFRESH=70
EXIT_AUDIT=71
EXIT_LASTADMIN=72

log()  { printf '[%s] %s\n' "$PROBE_NAME" "$*"; }
fail() { printf '[%s] FAIL: %s\n' "$PROBE_NAME" "$*" >&2; }

for tool in curl jq; do
    command -v "$tool" >/dev/null 2>&1 || { fail "missing required tool: $tool"; exit "$EXIT_USAGE"; }
done

BASE_URL="${1:-}"
BOOTSTRAP_USER="${BOOTSTRAP_USER:-bootstrap}"
BOOTSTRAP_PASS="${BOOTSTRAP_PASS:-Bootstrap1!pw}"
MOD_USER="${MOD_USER:-mod-1}"
MOD_PASS="${MOD_PASS:-Moderator1!pw}"
READY_TIMEOUT_S="${READY_TIMEOUT_S:-60}"
BUILD_TIMEOUT_S="${BUILD_TIMEOUT_S:-600}"
LAST_ADMIN_EXPECT_STATUS="${LAST_ADMIN_EXPECT_STATUS:-400}"

TS="$(date -u +%Y%m%dT%H%M%SZ)"
EVID_DIR="${EVID_DIR:-qa-evidence/ws-gate/admin/$TS}"
mkdir -p "$EVID_DIR"

SERVER_PID=""
SERVER_LOG="$EVID_DIR/server.log"
VERDICT_STEP="boot"

# shellcheck disable=SC2317  # runs via the EXIT trap, not inline.
on_exit() {
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
            --arg step "$VERDICT_STEP" \
            --argjson rc "$rc" \
            '{
                probe: $probe,
                timestamp: $ts,
                baseUrl: $base,
                lastStep: $step,
                exitCode: $rc,
                verdict: (if $rc == 0 then "PASS" else "FAIL" end)
            }' >"$EVID_DIR/verdict.json" 2>/dev/null || true
    fi
}
trap on_exit EXIT

# --- Server boot ----------------------------------------------------------
if [[ -z "$BASE_URL" ]]; then
    PROBE_PORT="${PROBE_PORT:-$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1]);s.close()')}"
    BASE_URL="http://127.0.0.1:${PROBE_PORT}"

    if [[ -n "${SERVER_BIN:-}" ]]; then
        [[ -x "$SERVER_BIN" ]] || { fail "SERVER_BIN not executable: $SERVER_BIN"; exit "$EXIT_BOOT"; }
        BIN="$SERVER_BIN"
    else
        log "building ts6-manager-server (set SERVER_BIN to skip)…"
        if ! timeout "$BUILD_TIMEOUT_S" cargo build -p ts6-manager-server >"$EVID_DIR/build.log" 2>&1; then
            fail "cargo build failed — see $EVID_DIR/build.log"
            tail -20 "$EVID_DIR/build.log" >&2 || true
            exit "$EXIT_BOOT"
        fi
        BIN="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
                | jq -r '.target_directory')/debug/ts6-manager-server"
        [[ -x "$BIN" ]] || BIN="target/debug/ts6-manager-server"
    fi
    [[ -x "$BIN" ]] || { fail "server binary not found: $BIN"; exit "$EXIT_BOOT"; }

    # dioxus-server (0.7.7) serves static assets from `<exe-dir>/public` and
    # PANICS at boot if that directory is absent (server.rs:409 read_dir
    # unwrap). The CLI's `dx build` normally creates it; a bare `cargo build`
    # does not. This is an API-only gate — no SPA bundle is needed — so an
    # empty `public/` dir is sufficient: dioxus-server falls back to its
    # default SSR-only index.html and the /api/* routes are unaffected.
    PUBLIC_DIR="$(dirname "$BIN")/public"
    if [[ ! -d "$PUBLIC_DIR" ]]; then
        mkdir -p "$PUBLIC_DIR"
        log "created empty $PUBLIC_DIR (API-only gate; no SPA bundle required)."
    fi

    log "booting server on $BASE_URL (in-memory SurrealDB, fresh deploy)…"
    # NODE_ENV unset → development; in-memory DB → guaranteed fresh deploy
    # with migration 0009 applied at boot. JWT_SECRET is a throwaway >=32B.
    env -u NODE_ENV \
        HOST=127.0.0.1 \
        PORT="$PROBE_PORT" \
        DATABASE_URL="memory" \
        JWT_SECRET="ws-gate-admin-probe-throwaway-secret-0001" \
        "$BIN" >"$SERVER_LOG" 2>&1 &
    SERVER_PID=$!
fi

BASE_URL="${BASE_URL%/}"

# --- Readiness probe ------------------------------------------------------
log "waiting for $BASE_URL/api/setup/status …"
ready=0
deadline=$(( $(date +%s) + READY_TIMEOUT_S ))
while [[ $(date +%s) -lt $deadline ]]; do
    if [[ -n "$SERVER_PID" ]] && ! kill -0 "$SERVER_PID" 2>/dev/null; then
        fail "server process exited during boot — see $SERVER_LOG"
        tail -20 "$SERVER_LOG" >&2 || true
        exit "$EXIT_BOOT"
    fi
    code=$(curl -fsS -o /dev/null -w '%{http_code}' "$BASE_URL/api/setup/status" 2>/dev/null || echo 000)
    if [[ "$code" == "200" ]]; then ready=1; break; fi
    sleep 1
done
[[ "$ready" == "1" ]] || { fail "server not ready within ${READY_TIMEOUT_S}s"; exit "$EXIT_BOOT"; }
log "server ready."

# --- Request helper -------------------------------------------------------
# do_req STEP METHOD PATH [JSON_BODY] [BEARER_TOKEN]
# Writes step-STEP.{req,resp}.json into $EVID_DIR.
# Sets RESP_CODE (int) and RESP_BODY (file path to the response body).
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
        # Redact secrets in the evidence copy of the request body.
        echo "$data" | jq '
            if has("password") then .password = "***REDACTED***" else . end
            | if has("refreshToken") then .refreshToken = "***REDACTED***" else . end
            | if .server? and (.server|type=="object") and (.server|has("apiKey"))
              then .server.apiKey = "***REDACTED***" else . end
        ' >"$req_file" 2>/dev/null || echo '{"redacted":true}' >"$req_file"
    else
        echo 'null' >"$req_file"
    fi

    RESP_CODE=$(curl "${args[@]}" 2>/dev/null || echo 000)
    [[ -s "$resp_file" ]] || echo '{}' >"$resp_file"
}

# Fail unless RESP_CODE == expected.
expect_code() {
    local want="$1" ctx="$2" exit_code="$3"
    if [[ "$RESP_CODE" != "$want" ]]; then
        fail "$ctx — expected HTTP $want, got $RESP_CODE"
        echo "  response body:" >&2
        sed 's/^/    /' "$RESP_BODY" >&2 || true
        exit "$exit_code"
    fi
}

# ==========================================================================
# Step 1 — fresh-deploy bootstrap + bootstrap-admin login
# ==========================================================================
VERDICT_STEP="1-bootstrap"
log "step 1: bootstrap admin + login"

do_req 1a GET /api/setup/status
expect_code 200 "step 1: GET /api/setup/status" "$EXIT_BOOTSTRAP"
if [[ "$(jq -r '.needsSetup' "$RESP_BODY")" != "true" ]]; then
    fail "step 1: needsSetup is not true — BASE_URL is not a fresh deploy"
    exit "$EXIT_BOOTSTRAP"
fi

SETUP_BODY=$(jq -n --arg u "$BOOTSTRAP_USER" --arg p "$BOOTSTRAP_PASS" '{
    username: $u, password: $p, displayName: "WS-Gate Bootstrap Admin",
    server: { name: "ws-gate-probe", host: "ts.invalid.example", apiKey: "ws-gate-probe-key" }
}')
do_req 1b POST /api/setup/init "$SETUP_BODY"
expect_code 201 "step 1: POST /api/setup/init" "$EXIT_BOOTSTRAP"

LOGIN_BODY=$(jq -n --arg u "$BOOTSTRAP_USER" --arg p "$BOOTSTRAP_PASS" '{username:$u,password:$p}')
do_req 1c POST /api/auth/login "$LOGIN_BODY"
expect_code 200 "step 1: bootstrap admin login" "$EXIT_BOOTSTRAP"
ADMIN_TOKEN=$(jq -r '.accessToken // empty' "$RESP_BODY")
ADMIN_REFRESH=$(jq -r '.refreshToken // empty' "$RESP_BODY")
[[ -n "$ADMIN_TOKEN" && -n "$ADMIN_REFRESH" ]] || {
    fail "step 1: login response missing accessToken/refreshToken"; exit "$EXIT_BOOTSTRAP"; }
log "  bootstrap admin '$BOOTSTRAP_USER' signed in (access + refresh captured)."

# Identify the bootstrap admin id for step 7.
do_req 1d GET /api/users "" "$ADMIN_TOKEN"
expect_code 200 "step 1: GET /api/users (admin)" "$EXIT_BOOTSTRAP"
ADMIN_ID=$(jq -r --arg u "$BOOTSTRAP_USER" '.[] | select(.username==$u) | .id' "$RESP_BODY")
[[ -n "$ADMIN_ID" ]] || { fail "step 1: could not resolve bootstrap admin id"; exit "$EXIT_BOOTSTRAP"; }
log "  bootstrap admin id = $ADMIN_ID"

# ==========================================================================
# Step 2 — create second user `mod-1` (role moderator)
# ==========================================================================
VERDICT_STEP="2-create"
log "step 2: create second user '$MOD_USER' (role moderator)"

CREATE_BODY=$(jq -n --arg u "$MOD_USER" --arg p "$MOD_PASS" '{
    username:$u, password:$p, displayName:"WS-Gate Moderator", role:"moderator"
}')
do_req 2 POST /api/users "$CREATE_BODY" "$ADMIN_TOKEN"
expect_code 201 "step 2: POST /api/users" "$EXIT_CREATE"
MOD_ID=$(jq -r '.id // empty' "$RESP_BODY")
MOD_ROLE=$(jq -r '.role // empty' "$RESP_BODY")
[[ -n "$MOD_ID" ]] || { fail "step 2: create response missing id"; exit "$EXIT_CREATE"; }
[[ "$MOD_ROLE" == "moderator" ]] || { fail "step 2: created role is '$MOD_ROLE', expected moderator"; exit "$EXIT_CREATE"; }
log "  created '$MOD_USER' id=$MOD_ID role=$MOD_ROLE"

# ==========================================================================
# Step 3 — mod-1 signs in; moderator is denied admin-only GET /api/users
# ==========================================================================
VERDICT_STEP="3-modlogin"
log "step 3: '$MOD_USER' login + moderator-403 check"

MOD_LOGIN_BODY=$(jq -n --arg u "$MOD_USER" --arg p "$MOD_PASS" '{username:$u,password:$p}')
do_req 3a POST /api/auth/login "$MOD_LOGIN_BODY"
expect_code 200 "step 3: '$MOD_USER' login" "$EXIT_MODLOGIN"
MOD_TOKEN=$(jq -r '.accessToken // empty' "$RESP_BODY")
MOD_REFRESH=$(jq -r '.refreshToken // empty' "$RESP_BODY")
[[ -n "$MOD_TOKEN" && -n "$MOD_REFRESH" ]] || {
    fail "step 3: '$MOD_USER' login missing tokens"; exit "$EXIT_MODLOGIN"; }
log "  '$MOD_USER' signed in (access + refresh captured)."

do_req 3b GET /api/users "" "$MOD_TOKEN"
expect_code 403 "step 3: moderator must NOT reach admin-only GET /api/users" "$EXIT_MODLOGIN"
log "  moderator correctly denied GET /api/users (403)."

# ==========================================================================
# Step 4 — bootstrap admin disables mod-1
# ==========================================================================
VERDICT_STEP="4-disable"
log "step 4: disable '$MOD_USER'"

do_req 4 PATCH "/api/users/$MOD_ID" '{"enabled":false}' "$ADMIN_TOKEN"
expect_code 200 "step 4: PATCH /api/users/$MOD_ID (enabled:false)" "$EXIT_DISABLE"
if [[ "$(jq -r '.enabled' "$RESP_BODY")" != "false" ]]; then
    fail "step 4: user not reported disabled after PATCH"
    exit "$EXIT_DISABLE"
fi
log "  '$MOD_USER' disabled."

# ==========================================================================
# Step 5 — mod-1 refresh-token family revoked → refresh returns 401
# ==========================================================================
VERDICT_STEP="5-refresh"
log "step 5: refresh-token revocation check"

REFRESH_BODY=$(jq -n --arg t "$MOD_REFRESH" '{refreshToken:$t}')
do_req 5 POST /api/auth/refresh "$REFRESH_BODY"
expect_code 401 "step 5: refresh after disable must be rejected" "$EXIT_REFRESH"
log "  '$MOD_USER' refresh correctly rejected (401) — token family revoked."

# ==========================================================================
# Step 6 — audit replay: userCreated then userDisabled for mod-1
# ==========================================================================
VERDICT_STEP="6-audit"
log "step 6: audit replay"

do_req 6 GET "/api/audit?limit=200" "" "$ADMIN_TOKEN"
expect_code 200 "step 6: GET /api/audit" "$EXIT_AUDIT"

# Diagnostics — print actor + target + kind for EVERY row read (PURA-239
# §Acceptance: "captures and prints actor + target + kind for every audit
# row read in step 6 to ease triage").
echo "  --- audit rows (newest first) ---"
jq -r '.items[] | "  id=\(.id) kind=\(.kind) actor=\(.actorUsername) target=\(.targetLabel // "-") targetId=\(.targetId // "-")"' \
    "$RESP_BODY" >&2 || true
echo "  ---------------------------------"

# Pull the mod-1 create + disable rows by kind + targetId.
CREATE_ID=$(jq -r --argjson mid "$MOD_ID" \
    'first(.items[] | select(.kind=="userCreated" and .targetId==$mid) | .id) // empty' "$RESP_BODY")
DISABLE_ID=$(jq -r --argjson mid "$MOD_ID" \
    'first(.items[] | select(.kind=="userDisabled" and .targetId==$mid) | .id) // empty' "$RESP_BODY")

if [[ -z "$CREATE_ID" ]]; then
    fail "step 6: no 'userCreated' audit row for targetId=$MOD_ID"
    exit "$EXIT_AUDIT"
fi
if [[ -z "$DISABLE_ID" ]]; then
    fail "step 6: no 'userDisabled' audit row for targetId=$MOD_ID"
    exit "$EXIT_AUDIT"
fi
# Ordering: userCreated must precede userDisabled chronologically. The row id
# is monotonic with insertion; the API sorts occurredAt DESC so the response
# order alone is not a reliable proxy.
if [[ "$CREATE_ID" -ge "$DISABLE_ID" ]]; then
    fail "step 6: userCreated (id=$CREATE_ID) does not precede userDisabled (id=$DISABLE_ID)"
    exit "$EXIT_AUDIT"
fi

# Field shape per docs/admin/audit-shape.md §2 — actorUsername is the
# bootstrap admin, targetLabel is the mod-1 username snapshot.
CREATE_ACTOR=$(jq -r --argjson id "$CREATE_ID" 'first(.items[]|select(.id==$id)|.actorUsername)' "$RESP_BODY")
CREATE_TARGET=$(jq -r --argjson id "$CREATE_ID" 'first(.items[]|select(.id==$id)|.targetLabel)' "$RESP_BODY")
DISABLE_ACTOR=$(jq -r --argjson id "$DISABLE_ID" 'first(.items[]|select(.id==$id)|.actorUsername)' "$RESP_BODY")
DISABLE_TARGET=$(jq -r --argjson id "$DISABLE_ID" 'first(.items[]|select(.id==$id)|.targetLabel)' "$RESP_BODY")

for pair in "userCreated:$CREATE_ACTOR:$CREATE_TARGET" "userDisabled:$DISABLE_ACTOR:$DISABLE_TARGET"; do
    k="${pair%%:*}"; rest="${pair#*:}"; a="${rest%%:*}"; t="${rest#*:}"
    if [[ "$a" != "$BOOTSTRAP_USER" ]]; then
        fail "step 6: $k actorUsername='$a', expected '$BOOTSTRAP_USER'"
        exit "$EXIT_AUDIT"
    fi
    if [[ "$t" != "$MOD_USER" ]]; then
        fail "step 6: $k targetLabel='$t', expected '$MOD_USER'"
        exit "$EXIT_AUDIT"
    fi
done
log "  audit replay OK — userCreated (id=$CREATE_ID) → userDisabled (id=$DISABLE_ID),"
log "  both actor='$BOOTSTRAP_USER' target='$MOD_USER'."

# ==========================================================================
# Step 7 — last-enabled-admin protection
# ==========================================================================
VERDICT_STEP="7-lastadmin"
log "step 7: last-enabled-admin protection (PATCH demote of sole admin)"

do_req 7 PATCH "/api/users/$ADMIN_ID" '{"role":"moderator"}' "$ADMIN_TOKEN"
if [[ "$RESP_CODE" != "$LAST_ADMIN_EXPECT_STATUS" ]]; then
    fail "step 7: demoting the sole admin returned HTTP $RESP_CODE, expected $LAST_ADMIN_EXPECT_STATUS"
    echo "  response body:" >&2
    sed 's/^/    /' "$RESP_BODY" >&2 || true
    exit "$EXIT_LASTADMIN"
fi
ERR_MSG=$(jq -r '.error // empty' "$RESP_BODY")
if [[ "$ERR_MSG" != *"last enabled admin"* ]]; then
    fail "step 7: error body '$ERR_MSG' does not mention last-admin protection"
    exit "$EXIT_LASTADMIN"
fi
log "  last admin protected — HTTP $RESP_CODE: \"$ERR_MSG\""

# --- Verdict --------------------------------------------------------------
VERDICT_STEP="done"
log "PASS — all 7 verification steps green. Evidence: $EVID_DIR"
exit 0
