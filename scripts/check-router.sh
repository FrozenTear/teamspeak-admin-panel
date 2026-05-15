#!/usr/bin/env bash
# scripts/check-router.sh — build-time guard that a named router is actually
# mounted in the manager server's `main.rs`.
#
# Background: the v0.1.0-rc1 image silently shipped without the `/api/flows`
# surface because the `flows_router` was never merged into the axum router —
# the code compiled fine, the route just did not exist at runtime. This guard
# turns that class of regression into a hard build failure.
#
# Per docs/flows/v1.1-gate.md §5 this runs as a `RUN` step in
# Containerfile.fullstack: `RUN scripts/check-router.sh flows_router`.
#
# Usage:
#   scripts/check-router.sh <router-ident> [main.rs path]
# Example:
#   scripts/check-router.sh flows_router
#
# Exit codes:
#   0  router is defined and mounted (.merge/.nest)
#   2  usage error
#   3  main.rs not found
#   4  router identifier never defined
#   5  router identifier defined but never mounted via .merge()/.nest()

set -euo pipefail

router="${1:-}"
main_rs="${2:-crates/ts6-manager-server/src/main.rs}"

if [ -z "$router" ]; then
  echo "check-router: usage: check-router.sh <router-ident> [main.rs path]" >&2
  exit 2
fi

if [ ! -f "$main_rs" ]; then
  echo "check-router: '$main_rs' not found (run from repo root)" >&2
  exit 3
fi

# 1. The router must be defined somewhere (e.g. `let flows_router = ...`).
if ! grep -Eq "\\b${router}\\b" "$main_rs"; then
  echo "check-router: FAIL — '${router}' is never referenced in ${main_rs}." >&2
  echo "check-router: the v1.1 flow REST surface is not present in this build." >&2
  exit 4
fi

# 2. The router must be mounted onto the app router via .merge() or .nest().
#    A definition that is never merged is exactly the rc1 silent-gap failure.
if ! grep -Eq "\\.(merge|nest)\\([^)]*\\b${router}\\b" "$main_rs"; then
  echo "check-router: FAIL — '${router}' is defined but never mounted." >&2
  echo "check-router: expected a '.merge(${router})' or '.nest(\"...\", ${router})' call." >&2
  exit 5
fi

echo "check-router: OK — '${router}' is defined and mounted in ${main_rs}."
