#!/usr/bin/env bash
# Contabo / kube upgrade: pull GHCR images, play a temp manifest
# with fullstack + music + sidecar on the same TAG, smoke /health
# (fullstack :3001 and music :3002), then re-apply Contabo soft CPU
# pin. Live apply-ready fullstack pin stays 2-5 until Robert picks
# packing A (shrink ACK) or B. Bot SEND pin is in-process; this
# script only re-applies HostConfig + host nice (never music 0-1).
#
# Usage (from any cwd, against a repo checkout):
#   ./scripts/update.sh vX.Y.Z
#
# Never: podman kube down --force  (wipes ts6-data / ts6-db / ts6-music)

set -euo pipefail

usage() {
    echo "usage: $0 vX.Y.Z" >&2
    echo "  Pull fullstack + music + sidecar GHCR images for TAG, kube down" >&2
    echo "  (no --force), kube play, curl fullstack /health and music /health," >&2
    echo "  then re-apply Contabo soft pin (live fullstack 2-5; shrink needs ACK)." >&2
    echo "example: $0 v1.6.2" >&2
    exit 2
}

if [[ $# -ne 1 ]]; then
    usage
fi

TAG="$1"
if [[ ! "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-].*)?$ ]]; then
    echo "error: TAG must look like vX.Y.Z (got: ${TAG})" >&2
    usage
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MANIFEST="${REPO_ROOT}/deploy/kube/ts6-manager.yaml"
SECRETS="${REPO_ROOT}/deploy/kube/secrets.yaml"
FULLSTACK="ghcr.io/frozentear/ts6-manager-fullstack:${TAG}"
MUSIC="ghcr.io/frozentear/ts6-manager-music:${TAG}"
SIDECAR="ghcr.io/frozentear/ts6-manager-sidecar:${TAG}"

die() {
    echo "error: $*" >&2
    echo "FAIL: upgrade to ${TAG} did not finish. Named volumes should still be intact — never kube down --force." >&2
    exit 1
}

if [[ ! -f "$MANIFEST" ]]; then
    die "missing kube manifest: ${MANIFEST}"
fi
if ! command -v podman >/dev/null; then
    die "podman not found on PATH"
fi
if ! command -v curl >/dev/null; then
    die "curl not found on PATH (needed for /health)"
fi

HAVE_SECRET=0
if podman secret exists ts6-manager-secrets; then
    HAVE_SECRET=1
fi
if [[ "$HAVE_SECRET" -ne 1 && ! -f "$SECRETS" ]]; then
    echo "error: podman secret ts6-manager-secrets is missing and ${SECRETS} is not present." >&2
    echo "  copy deploy/kube/secrets.example.yaml → deploy/kube/secrets.yaml and fill JWT_SECRET," >&2
    echo "  or create the secret on the host first." >&2
    echo "FAIL: upgrade to ${TAG} did not finish. Named volumes should still be intact — never kube down --force." >&2
    exit 1
fi

TMPDIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TMPDIR"
}
trap cleanup EXIT
trap 'echo "FAIL: upgrade to ${TAG} did not finish. Named volumes should still be intact — never kube down --force." >&2' ERR

PLAY_POD="${TMPDIR}/ts6-manager.kube.yaml"
# Pin all three images to TAG. Never leave music or sidecar on the committed pin.
sed -E \
    -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-fullstack:)[^[:space:]]+#\\1${TAG}#" \
    -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-music:)[^[:space:]]+#\\1${TAG}#" \
    -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-sidecar:)[^[:space:]]+#\\1${TAG}#" \
    "$MANIFEST" > "$PLAY_POD"

if ! grep -q "image: ${FULLSTACK}" "$PLAY_POD" \
    || ! grep -q "image: ${MUSIC}" "$PLAY_POD" \
    || ! grep -q "image: ${SIDECAR}" "$PLAY_POD"; then
    die "failed to rewrite fullstack + music + sidecar image tags to ${TAG} in the temp manifest"
fi

echo "==> pulling ${FULLSTACK}"
podman pull "$FULLSTACK"
echo "==> pulling ${MUSIC}"
podman pull "$MUSIC"
echo "==> pulling ${SIDECAR}"
podman pull "$SIDECAR"

echo "==> podman kube down (no --force; volumes stay)"
# Identity is the pod name in the YAML, not the image tag.
if podman pod exists ts6-manager; then
    # Stale play-state IDs on Contabo: kube down can fail with
    # "no pod with ID … found" after the name is already gone.
    if ! KUBE_DOWN_OUT="$(podman kube down "$MANIFEST" 2>&1)"; then
        if printf '%s\n' "$KUBE_DOWN_OUT" | grep -qiE 'no pod with ID'; then
            echo "    stale pod id from kube down; treating as already down"
            printf '%s\n' "$KUBE_DOWN_OUT" | sed 's/^/    /'
        else
            printf '%s\n' "$KUBE_DOWN_OUT" >&2
            die "podman kube down failed"
        fi
    else
        printf '%s\n' "$KUBE_DOWN_OUT"
    fi
else
    echo "    no ts6-manager pod; skipping down"
fi

PLAY_FILE="$PLAY_POD"
if [[ "$HAVE_SECRET" -eq 1 ]]; then
    echo "==> podman secret ts6-manager-secrets exists; playing pod+PVCs only"
else
    echo "==> concatenating ${SECRETS} + temp manifest"
    PLAY_FILE="${TMPDIR}/ts6-manager.with-secrets.yaml"
    cat "$SECRETS" "$PLAY_POD" > "$PLAY_FILE"
fi

echo "==> podman kube play ${PLAY_FILE}"
podman kube play "$PLAY_FILE"

wait_health() {
    local url="$1"
    local label="$2"
    local out="${TMPDIR}/health-$(echo "$label" | tr ' /' '__').out"
    local ok=0
    for _ in $(seq 1 45); do
        if curl -fsS "$url" >"$out" 2>/dev/null; then
            ok=1
            break
        fi
        sleep 2
    done
    if [[ "$ok" -ne 1 ]]; then
        die "${label} did not succeed after kube play"
    fi
    echo "    ${label}: $(cat "$out")"
}

echo "==> waiting for http://127.0.0.1:3001/health (fullstack)"
wait_health "http://127.0.0.1:3001/health" "fullstack /health"
echo "==> waiting for http://127.0.0.1:3002/health (music)"
wait_health "http://127.0.0.1:3002/health" "music /health"

echo "==> applying Contabo soft pin (live fullstack 2-5; SEND in-process; never music HostConfig 0-1)"
"${SCRIPT_DIR}/apply-fullstack-soft-pin.sh" \
    || die "soft pin requested but apply failed"

echo
echo "OK: ts6-manager is on ${TAG} (fullstack + music + sidecar)."
echo "    volumes ts6-data / ts6-db / ts6-music were left in place."
echo "    never run: podman kube down --force"
