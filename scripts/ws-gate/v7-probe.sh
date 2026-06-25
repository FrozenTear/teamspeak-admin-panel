#!/usr/bin/env bash
# scripts/ws-gate/v7-probe.sh — WS-Gate V7 (public widget URL) gate probe.
#
# Matrix row V7 (THE-1014, parent THE-1009). The public widget endpoints
# (spec §7.28) are the ONLY unauthenticated surface — the token in the URL is
# the sole credential. This probe asserts they are reachable, public, and
# leak no operator credential, across the SVG / PNG / JSON variants:
#
#   GET /api/widget/{token}/data        application/json
#   GET /api/widget/{token}/image.svg   image/svg+xml
#   GET /api/widget/{token}/image.png   image/png   (falls back to svg+xml)
#
# Assertion tiers:
#   * env-independent (heartbeat / source build, no TS):
#       - unknown token → 404 on every variant (route mounted; NOT 401/403;
#         404 body leaks no credential). This needs no TS backend.
#       - a real widget's variants are PUBLIC: an unauthenticated GET is not
#         rejected with 401/403, returns a clean status (2xx, or 502/503/504
#         when the configured TS is unreachable — never 500), and the body
#         never leaks apiKey / ssh secret / JWT / Bearer.
#   * REQUIRE_LIVE_TS=1 (runner, live NON-PROD TS):
#       - each variant returns 200, the documented content-type, non-empty.
#
# Modes (see _probe-lib.sh): WS_GATE_DRY_RUN=1 | live (BASE_URL) | self-boot.
#
# Inputs (live): BASE_URL $1, ADMIN_TOKEN env (to mint a widget if needed).
#   WIDGET_TOKEN   env, default = a widget created in self-boot/with admin
#   SERVER_CONFIG_ID  env, default = first GET /api/servers row
#   VIRTUAL_SERVER_ID env, default 1
#   REQUIRE_LIVE_TS   env, default 0
#
# Exit codes:
#   0 green   64 usage   65 boot   66 pre-check
#   67 widget create failed   68 unknown-token not 404   69 not public / dirty
#   72 credential leak in public body   73 live content-type/200 assertion failed
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/ws-gate/_probe-lib.sh
source "$SCRIPT_DIR/_probe-lib.sh"

EXIT_WCREATE=67
EXIT_NOT404=68
EXIT_NOTPUBLIC=69
EXIT_LEAK=72
EXIT_LIVESHAPE=73

VIRTUAL_SERVER_ID="${VIRTUAL_SERVER_ID:-1}"
REQUIRE_LIVE_TS="${REQUIRE_LIVE_TS:-0}"

wsg_init "ws-gate-v7-probe" "${1:-}"

if [[ "$DRY_RUN" -eq 1 ]]; then
    for v in data image.svg image.png; do
        wsg_dry_check_payload "variant-${v//./_}" "$(jq -n --arg v "$v" '{variant:("/api/widget/{token}/"+$v)}')"
    done
    VERDICT_NOTE="dry-run: 3 widget variants enumerated; live 200/content-type gated behind REQUIRE_LIVE_TS"
    wsg_log "PASS (dry-run) — V7 widget harness is well-formed."
    exit 0
fi

# fetch_public STEP PATH  — UNAUTHENTICATED GET; sets PUB_CODE + PUB_CTYPE,
# writes body to step-STEP.resp + a .head sidecar.
PUB_CODE=""; PUB_CTYPE=""
fetch_public() {
    local step="$1" path="$2"
    local body="$EVID_DIR/step-$step.resp"; local out
    out=$(curl -sS -o "$body" -D "$EVID_DIR/step-$step.head" \
        -w '%{http_code} %{content_type}' "$BASE_URL$path" 2>>"$EVID_DIR/curl.log" || echo "000 -")
    PUB_CODE="${out%% *}"; PUB_CTYPE="${out#* }"
    [[ -s "$body" ]] || : >"$body"
    PUB_BODY="$body"
}

wsg_resolve_target

# --- resolve / mint a widget token ------------------------------------------
TOKEN="${WIDGET_TOKEN:-}"
if [[ -z "$TOKEN" ]]; then
    CID="${SERVER_CONFIG_ID:-}"
    if [[ -z "$CID" ]]; then
        do_req 0 GET /api/servers "" "$ADMIN_TOKEN"
        wsg_expect 200 "list servers (to resolve cid)" "$WSG_EXIT_PRECHECK"
        CID="$(jq -r '.[0].id // empty' "$RESP_BODY")"
        [[ -n "$CID" ]] || { wsg_fail "no server configured; set SERVER_CONFIG_ID or WIDGET_TOKEN"; exit "$WSG_EXIT_PRECHECK"; }
    fi
    wcreate="$(jq -n --argjson cid "$CID" --argjson vs "$VIRTUAL_SERVER_ID" \
        '{name:"ws-gate-v7-widget", serverConfigId:$cid, virtualServerId:$vs}')"
    do_req 1 POST /api/widgets "$wcreate" "$ADMIN_TOKEN"
    wsg_expect 201 "create widget" "$EXIT_WCREATE"
    TOKEN="$(jq -r '.token // empty' "$RESP_BODY")"
    [[ -n "$TOKEN" ]] || { wsg_fail "create widget: 201 but no .token"; exit "$EXIT_WCREATE"; }
    wsg_log "step 1 OK — minted widget token ${TOKEN:0:4}… (cid=$CID)"
fi

# --- unknown-token 404 (TS-independent) -------------------------------------
fetch_public 2 "/api/widget/ws-gate-nonexistent-000/data"
if [[ "$PUB_CODE" != "404" ]]; then
    wsg_fail "unknown token expected 404, got $PUB_CODE (must not be 401/403 — endpoint is public)"
    exit "$EXIT_NOT404"
fi
wsg_assert_no_secret_leak "$PUB_BODY" "404 body"
wsg_log "step 2 OK — unknown token → 404, public (no 401), no leak"

# --- the three real variants are public + clean + leak-free -----------------
step=3
declare -A want_ctype=( [data]="application/json" [image.svg]="image/svg+xml" [image.png]="image/png" )
for v in data image.svg image.png; do
    fetch_public "$step" "/api/widget/$TOKEN/$v"
    if [[ "$PUB_CODE" == "401" || "$PUB_CODE" == "403" ]]; then
        wsg_fail "$v: public widget rejected unauthenticated request ($PUB_CODE) — auth leak into a public route"
        exit "$EXIT_NOTPUBLIC"
    fi
    case "$PUB_CODE" in
        2[0-9][0-9]|502|503|504) : ;;   # clean: served, or TS-unreachable upstream error
        *) wsg_fail "$v: dirty status $PUB_CODE (expected 2xx or 502/503/504)"; exit "$EXIT_NOTPUBLIC" ;;
    esac
    if grep -Eiq '"(apikey|sshpassword|sshprivatekey|accesstoken|refreshtoken)"[[:space:]]*:|bearer [a-z0-9._-]{12,}|eyJ[a-z0-9._-]{20,}' "$PUB_BODY"; then
        wsg_fail "$v: public body leaks a credential/token"
        sed 's/^/    /' "$PUB_BODY" >&2 || true
        exit "$EXIT_LEAK"
    fi
    wsg_log "step $step OK — $v public+clean ($PUB_CODE, ct=$PUB_CTYPE), no leak"

    if [[ "$REQUIRE_LIVE_TS" == "1" ]]; then
        [[ "$PUB_CODE" == "200" ]] || { wsg_fail "$v: REQUIRE_LIVE_TS expected 200, got $PUB_CODE"; exit "$EXIT_LIVESHAPE"; }
        [[ -s "$PUB_BODY" ]] || { wsg_fail "$v: 200 but empty body"; exit "$EXIT_LIVESHAPE"; }
        # png gracefully falls back to svg+xml when the rasteriser is disabled.
        if [[ "$v" == "image.png" ]]; then
            [[ "$PUB_CTYPE" == image/png* || "$PUB_CTYPE" == image/svg+xml* ]] \
                || { wsg_fail "$v: content-type '$PUB_CTYPE' not image/png|svg+xml"; exit "$EXIT_LIVESHAPE"; }
        else
            [[ "$PUB_CTYPE" == "${want_ctype[$v]}"* ]] \
                || { wsg_fail "$v: content-type '$PUB_CTYPE' != ${want_ctype[$v]}"; exit "$EXIT_LIVESHAPE"; }
        fi
        wsg_log "       live shape OK — $v 200 + content-type + non-empty"
    fi
    step=$((step+1))
done

[[ "$REQUIRE_LIVE_TS" == "1" ]] || \
    VERDICT_NOTE="public+leak-free tier green; 200/content-type gated behind REQUIRE_LIVE_TS + a live TS"

wsg_log "PASS — V7 widget probe green against ${BASE_URL}"
exit 0
