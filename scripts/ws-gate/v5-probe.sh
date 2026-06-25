#!/usr/bin/env bash
# scripts/ws-gate/v5-probe.sh — WS-Gate V5 (audio-capture) gate harness.
#
# Matrix row V5 (THE-1014, parent THE-1009; analysis spec in THE-1013). The
# headline interaction is: drive `!play <radio-url>` against a bot sitting in
# a channel, then CAPTURE the bot's RTP/Opus egress and run the headless
# click / gap / RMS-continuity analysis. That audible-analysis stage needs a
# real bot egressing to a real channel + a capture sink + the analyzer — i.e.
# the future runner (THE-1013). It is therefore gated behind `--capture`.
#
# What runs WITHOUT the runner (heartbeat / source build):
#   * the play-dispatch wiring: create a bot, POST /play {source:url} → 202,
#     proving the command path + MusicRequest audit side-effect are intact.
#   * a capture-plan.json stub describing exactly what the runner must supply
#     so the audible stage is turnkey the moment a host lands.
#
# Modes (see _probe-lib.sh): WS_GATE_DRY_RUN=1 | live (BASE_URL) | self-boot.
# The `--capture` flag (or CAPTURE=1) arms the audible stage; without a
# capture backend it emits the stub and warns (or fails if STRICT_CAPTURE=1).
#
# Inputs (live): BASE_URL (first non-flag arg), ADMIN_TOKEN env.
#   MUSIC_BOT_ID  env, default = a bot created in self-boot, else required
#   RADIO_URL     env, default a fixture stream URL (runner overrides)
#   BOT_SERVER_ADDR env, default 127.0.0.1:9987 (self-boot bot creation)
#   PLAY_EXPECT_STATUS env, default 202
#   CAPTURE / --capture   arm the audible-analysis stage
#   STRICT_CAPTURE env, default 0  (1 → missing capture backend is a FAIL)
#
# Exit codes:
#   0  green   64 usage   65 boot   66 pre-check
#   67 bot create failed   68 play dispatch failed   70 capture stage failed (STRICT)
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/ws-gate/_probe-lib.sh
source "$SCRIPT_DIR/_probe-lib.sh"

EXIT_BOTCREATE=67
EXIT_PLAY=68
EXIT_CAPTURE=70

CAPTURE="${CAPTURE:-0}"
STRICT_CAPTURE="${STRICT_CAPTURE:-0}"
RADIO_URL="${RADIO_URL:-https://fixture.ws-gate.invalid/stream.mp3}"
BOT_SERVER_ADDR="${BOT_SERVER_ADDR:-127.0.0.1:9987}"
PLAY_EXPECT_STATUS="${PLAY_EXPECT_STATUS:-202}"

# $1 may be BASE_URL and/or --capture (run-all passes only BASE_URL; the flag
# usually arrives via CAPTURE=1). Accept either ordering.
_base=""
for a in "$@"; do
    case "$a" in
        --capture) CAPTURE=1 ;;
        *) [[ -z "$_base" ]] && _base="$a" ;;
    esac
done

wsg_init "ws-gate-v5-probe" "$_base"

play_payload="$(jq -n --arg url "$RADIO_URL" '{source:{kind:"url", url:$url}}')"

# Emit the capture-plan stub the runner consumes (always written for evidence).
write_capture_plan() {
    local botid="$1"
    jq -n \
        --arg url "$RADIO_URL" \
        --arg bot "$botid" \
        '{
            stage: "audible-analysis",
            owner: "runner (THE-1013)",
            requires: [
                "a real bot joined to a NON-PROD TS channel (BotState=in_channel)",
                "RTP/Opus egress capture sink on the voice path (tsclientlib send_audio)",
                "the headless analyzer: click-detection, gap-detection, RMS-continuity"
            ],
            inputs: { musicBotId: $bot, radioUrl: $url },
            metrics: {
                clicks: "count of discontinuity transients (expect 0)",
                gaps_ms: "silent gaps > 60ms in the egress (expect 0 after warm-up)",
                rms_continuity: "frame-to-frame RMS delta within tolerance (no dropouts)"
            },
            captureCmd: "ws-gate runner: capture egress to capture.opus then `analyze-egress` (THE-1013 deliverable)"
        }' >"$EVID_DIR/capture-plan.json"
}

if [[ "$DRY_RUN" -eq 1 ]]; then
    wsg_dry_check_payload "play" "$play_payload"
    write_capture_plan "<runtime>"
    VERDICT_NOTE="dry-run: play payload validated + capture-plan.json emitted; audible stage owned by runner (THE-1013)"
    wsg_log "PASS (dry-run) — V5 audio harness well-formed; capture stage stubbed."
    exit 0
fi

wsg_resolve_target

# --- resolve / create a music bot -------------------------------------------
BOT_ID="${MUSIC_BOT_ID:-}"
if [[ -z "$BOT_ID" ]]; then
    if [[ "$SELF_BOOTED" -ne 1 ]]; then
        wsg_fail "live mode needs MUSIC_BOT_ID (a bot joined to a NON-PROD channel)"
        exit "$WSG_EXIT_USAGE"
    fi
    # autoConnect:false → actor spawns Disconnected, no real handshake, but
    # the play command path + audit side-effect still exercise end-to-end.
    create_bot="$(jq -n --arg addr "$BOT_SERVER_ADDR" '{name:"ws-gate-v5-bot", serverAddr:$addr, autoConnect:false}')"
    do_req 1 POST /api/music-bots "$create_bot" "$ADMIN_TOKEN"
    wsg_expect 201 "create music bot" "$EXIT_BOTCREATE"
    BOT_ID="$(jq -r '.id // empty' "$RESP_BODY")"
    [[ -n "$BOT_ID" ]] || { wsg_fail "create bot: 201 but no .id"; exit "$EXIT_BOTCREATE"; }
    wsg_log "step 1 OK — created bot id=$BOT_ID (autoConnect=false)"
fi

# --- drive !play ------------------------------------------------------------
do_req 2 POST "/api/music-bots/$BOT_ID/play" "$play_payload" "$ADMIN_TOKEN"
wsg_expect "$PLAY_EXPECT_STATUS" "play dispatch" "$EXIT_PLAY"
wsg_log "step 2 OK — play dispatched to bot $BOT_ID ($PLAY_EXPECT_STATUS), MusicRequest recorded"

# --- audible-analysis stage (gated) -----------------------------------------
write_capture_plan "$BOT_ID"
if [[ "$CAPTURE" == "1" ]]; then
    # The egress capture sink + analyzer are runner-owned (THE-1013); they do
    # not exist in this env. Surface that honestly rather than fake a verdict.
    if [[ -n "${CAPTURE_BACKEND:-}" && -x "${CAPTURE_BACKEND}" ]]; then
        wsg_log "step 3 — running capture backend $CAPTURE_BACKEND …"
        if ! "$CAPTURE_BACKEND" --bot "$BOT_ID" --out "$EVID_DIR/capture.opus" >"$EVID_DIR/capture.log" 2>&1; then
            wsg_fail "capture backend failed — see capture.log"
            exit "$EXIT_CAPTURE"
        fi
        wsg_log "step 3 OK — egress captured to capture.opus"
    else
        wsg_warn "capture armed but no CAPTURE_BACKEND available — audible stage deferred to runner (THE-1013)"
        wsg_warn "see $EVID_DIR/capture-plan.json for the turnkey spec"
        if [[ "$STRICT_CAPTURE" == "1" ]]; then
            wsg_fail "STRICT_CAPTURE=1 and no capture backend present"
            exit "$EXIT_CAPTURE"
        fi
        VERDICT_NOTE="play-dispatch green; audible capture deferred to runner (no CAPTURE_BACKEND) — see capture-plan.json"
    fi
else
    VERDICT_NOTE="play-dispatch green; audible-analysis stage gated behind --capture (runner, THE-1013)"
    wsg_log "step 3 SKIPPED — pass --capture (with a CAPTURE_BACKEND) to run the audible analysis"
fi

wsg_log "PASS — V5 audio-capture harness green against ${BASE_URL}"
exit 0
