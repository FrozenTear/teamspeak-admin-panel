#!/usr/bin/env bash
# Contabo / kube upgrade: pull GHCR images, play a temp manifest
# with fullstack + music + sidecar on the same TAG, smoke /health
# (fullstack :3001 and music :3002), then re-apply Contabo soft CPU
# pin. Packing B (Robert): fullstack stays 2-5 / -5. DECODE 2-5 is
# kube env. Bot SEND pin is in-process; this script only re-applies
# HostConfig + host nice (music HostConfig stays unset under packing B).
# Shrink (A) is not default.
#
# Usage (from any cwd, against a repo checkout):
#   ./scripts/update.sh vX.Y.Z
#
# This is the only start/restart path. The committed manifest pins
# fullstack, music, and sidecar to KUBE_IMAGE_UNPINNED (@UNRELEASED),
# which fails reference parsing. A live checkout may still have :vX.Y.Z;
# rewrite_kube_image_tags substitutes either form.
#
# Never: podman kube down --force  (wipes ts6-data / ts6-db / ts6-music)

set -euo pipefail

# Committed image suffix in deploy/kube/ts6-manager.yaml. Not a tag and
# not a digest (digest is algorithm:hex). Podman fails reference
# parsing (`invalid reference format`) before any pull.
# Read by scripts/update.test.sh.
# shellcheck disable=SC2034
KUBE_IMAGE_UNPINNED='@UNRELEASED'

usage() {
    echo "usage: $0 vX.Y.Z" >&2
    echo "  Pull fullstack + music + sidecar GHCR images for TAG, kube down" >&2
    echo "  (no --force), kube play, curl fullstack /health and music /health," >&2
    echo "  then re-apply Contabo soft pin (packing B: fullstack 2-5; music HostConfig unset)." >&2
    echo "example: $0 v1.6.X" >&2
    exit 2
}

# rewrite_kube_image_tags SRC DST TAG
# Rewrite every ts6-manager-* image in SRC onto :TAG and write DST.
# Accepts the committed @UNRELEASED placeholder and a legacy :vX.Y.Z
# (or any other :tag / @digest) so a live host checkout still upgrades.
# Fullstack, music, and sidecar are explicit; the last expression
# catches any other ts6-manager-* image so a fourth container cannot
# keep the placeholder.
rewrite_kube_image_tags() {
    local src="$1"
    local dst="$2"
    local tag="$3"
    sed -E \
        -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-fullstack)([:@][^[:space:]]+)#\\1:${tag}#" \
        -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-music)([:@][^[:space:]]+)#\\1:${tag}#" \
        -e "s#(image:[[:space:]]+ghcr\\.io/frozentear/ts6-manager-sidecar)([:@][^[:space:]]+)#\\1:${tag}#" \
        -e "s#(image:[[:space:]]+[^[:space:]]*ts6-manager-[A-Za-z0-9._-]+)([:@][^[:space:]]+)#\\1:${tag}#" \
        "$src" > "$dst"
}

# kube_down_manifest COMMITTED REWRITTEN
# Print the file `podman kube down` should read.
# Podman 4.4.4, 5.6.0, and current main only unmarshal metadata.name
# (they do not parse image references), so down of the committed file
# succeeds today. The committed images are still `@UNRELEASED`, which
# is an invalid reference. Down uses the rewritten manifest — same pod
# name `ts6-manager`, valid tags — so a podman that checks image refs
# cannot abort teardown.
kube_down_manifest() {
    local committed="$1"
    local rewritten="$2"
    if [[ -z "$rewritten" || "$rewritten" == "$committed" ]]; then
        echo "error: kube down must read the rewritten manifest, not the committed file" >&2
        return 1
    fi
    printf '%s\n' "$rewritten"
}

die() {
    echo "error: $*" >&2
    echo "FAIL: upgrade to ${TAG} did not finish. Named volumes should still be intact — never kube down --force." >&2
    exit 1
}

main() {
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
# Pin every ts6-manager-* image to TAG. The committed file uses
# @UNRELEASED; a live checkout may still have :vX.Y.Z. Sidecar is
# rewritten explicitly, same as fullstack and music.
rewrite_kube_image_tags "$MANIFEST" "$PLAY_POD" "$TAG"

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
# Identity is the pod name in the YAML, not the image tag. Down reads
# the rewritten manifest (valid tags), not the committed @UNRELEASED file.
DOWN_MANIFEST="$(kube_down_manifest "$MANIFEST" "$PLAY_POD")"
if podman pod exists ts6-manager; then
    # Stale play-state IDs on Contabo: kube down can fail with
    # "no pod with ID … found" after the name is already gone.
    if ! KUBE_DOWN_OUT="$(podman kube down "$DOWN_MANIFEST" 2>&1)"; then
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
    local out
    out="${TMPDIR}/health-$(echo "$label" | tr ' /' '__').out"
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

echo "==> applying Contabo soft pin (packing B: fullstack 2-5; SEND in-process; music HostConfig unset)"
"${SCRIPT_DIR}/apply-fullstack-soft-pin.sh" \
    || die "soft pin requested but apply failed"

echo
echo "OK: ts6-manager is on ${TAG} (fullstack + music + sidecar)."
echo "    volumes ts6-data / ts6-db / ts6-music were left in place."
echo "    never run: podman kube down --force"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
