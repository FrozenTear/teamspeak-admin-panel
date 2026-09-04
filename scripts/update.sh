#!/usr/bin/env bash
# Contabo / kube upgrade: pull both GHCR images, play a temp manifest
# with fullstack + sidecar on the same TAG, then smoke /health.
#
# Usage (from any cwd, against a repo checkout):
#   ./scripts/update.sh vX.Y.Z
#
# Never: podman kube down --force  (wipes ts6-data / ts6-db / ts6-music)

set -euo pipefail

usage() {
    echo "usage: $0 vX.Y.Z" >&2
    echo "  Pull both GHCR images for TAG, kube down (no --force), kube play," >&2
    echo "  and curl http://127.0.0.1:3001/health." >&2
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
# Pin both images to TAG. Never leave sidecar on the committed pin.
sed -E \
    -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-fullstack:)[^[:space:]]+#\\1${TAG}#" \
    -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-sidecar:)[^[:space:]]+#\\1${TAG}#" \
    "$MANIFEST" > "$PLAY_POD"

if ! grep -q "image: ${FULLSTACK}" "$PLAY_POD" \
    || ! grep -q "image: ${SIDECAR}" "$PLAY_POD"; then
    die "failed to rewrite both image tags to ${TAG} in the temp manifest"
fi

echo "==> pulling ${FULLSTACK}"
podman pull "$FULLSTACK"
echo "==> pulling ${SIDECAR}"
podman pull "$SIDECAR"

echo "==> podman kube down (no --force; volumes stay)"
# Identity is the pod name in the YAML, not the image tag.
if podman pod exists ts6-manager; then
    podman kube down "$MANIFEST"
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

echo "==> waiting for http://127.0.0.1:3001/health"
HEALTH_OUT="${TMPDIR}/health.out"
HEALTH_OK=0
for _ in $(seq 1 45); do
    if curl -fsS http://127.0.0.1:3001/health >"$HEALTH_OUT" 2>/dev/null; then
        HEALTH_OK=1
        break
    fi
    sleep 2
done
if [[ "$HEALTH_OK" -ne 1 ]]; then
    die "/health did not succeed after kube play"
fi
echo "    $(cat "$HEALTH_OUT")"

echo
echo "OK: ts6-manager is on ${TAG} (fullstack + sidecar)."
echo "    volumes ts6-data / ts6-db / ts6-music were left in place."
echo "    never run: podman kube down --force"
