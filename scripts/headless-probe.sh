#!/usr/bin/env bash
# PURA-132 — headless screenshot probe for the dx 0.7.7 SPA.
#
# Use chrome-headless-shell (one-shot CLI mode) with `--virtual-time-budget`
# so the page settles deterministically without an attached CDP session.
# Playwright's `chromium-1217 --headless=new --screenshot=` mode does not
# return on its own and must be driven through Playwright's API instead;
# this script is the bare-minimum probe and a paired Playwright run still
# works for richer assertions.
#
# Usage:
#   scripts/headless-probe.sh <base-url> <route1> [route2 ...]
# Example:
#   scripts/headless-probe.sh http://127.0.0.1:9080 /login /dashboard /music-bots
#
# PURA-243 — the flow-engine landing page is a smoke target; add `/flows`
# to the route list so the flows nav surface is covered:
#   scripts/headless-probe.sh http://127.0.0.1:9080 /flows /flows/new
#
# PURA-267 — v2 canvas routes to smoke after a `dx serve --release` build:
#   scripts/headless-probe.sh http://127.0.0.1:9080 \
#     /flows /flows/new /dev/flow-canvas
# The /dev/flow-canvas route (debug builds only) exercises the canvas without
# auth; /flows/new and /flows/{id}/edit are the production canvas routes.
# Run against a `dx serve --release` bundle — debug WASM is too large for a
# heartbeat-feasible smoke (see project_dx_serve_debug_wasm_blocks_qa).

set -euo pipefail

BASE_URL=${1:-}
shift || true
ROUTES=("$@")

if [[ -z "$BASE_URL" || ${#ROUTES[@]} -eq 0 ]]; then
    echo "usage: $0 <base-url> <route1> [route2 ...]" >&2
    exit 64
fi

SHELL_BIN=${HEADLESS_SHELL:-$HOME/.cache/ms-playwright/chromium_headless_shell-1217/chrome-headless-shell-linux64/chrome-headless-shell}
if [[ ! -x "$SHELL_BIN" ]]; then
    echo "chrome-headless-shell not found at $SHELL_BIN — set HEADLESS_SHELL." >&2
    exit 65
fi

OUT_DIR=${OUT_DIR:-qa-evidence/headless}
mkdir -p "$OUT_DIR"

for route in "${ROUTES[@]}"; do
    name=$(echo "$route" | tr '/' '-' | sed -E 's/^-+//;s/-+$//;s/-+/-/g')
    name=${name:-index}
    ud="$OUT_DIR/.ud-$name"
    rm -rf "$ud"
    out="$OUT_DIR/$name.png"
    rm -f "$out"
    if timeout 25 "$SHELL_BIN" \
        --no-sandbox --disable-gpu \
        --user-data-dir="$ud" \
        --window-size=1440,900 \
        --virtual-time-budget=10000 \
        --screenshot="$out" \
        "$BASE_URL$route" >/dev/null 2>&1; then
        if [[ -s "$out" ]]; then
            printf '  route=%-24s size=%s OK\n' "$route" "$(stat -c '%s' "$out")"
            continue
        fi
    fi
    printf '  route=%-24s NO_SCREENSHOT\n' "$route"
done
