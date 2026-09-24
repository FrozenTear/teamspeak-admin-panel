#!/usr/bin/env bash
# Guard for scripts/update.sh image rewrite. Does not call podman.
# Fails when the committed kube manifest is playable as-is, when a
# requested tag is not applied to every ts6-manager-* image, or when
# a legacy :vX.Y.Z pin (live checkout) is left in place.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "${SCRIPT_DIR}/update.sh"

REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MANIFEST="${REPO_ROOT}/deploy/kube/ts6-manager.yaml"
REQUESTED="v9.9.9-guard"
LEGACY_TAG="v1.6.2"

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

[[ -n "${KUBE_IMAGE_UNPINNED}" ]] || fail "KUBE_IMAGE_UNPINNED is unset"
[[ "${KUBE_IMAGE_UNPINNED}" == @* ]] || fail "placeholder ${KUBE_IMAGE_UNPINNED} looks like a real tag"

# Committed file: fullstack, music, and sidecar must all be the
# placeholder. A real tag on only one of them is a failure.
for name in fullstack music sidecar; do
    if ! grep -q "image: ghcr.io/frozentear/ts6-manager-${name}${KUBE_IMAGE_UNPINNED}" "$MANIFEST"; then
        fail "committed ${name} image is not ${KUBE_IMAGE_UNPINNED}"
    fi
done
if grep -E 'image:[[:space:]]+[^[:space:]]*ts6-manager-[A-Za-z0-9._-]+:' "$MANIFEST"; then
    fail "committed manifest still has a ts6-manager-* image tag"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Copy plus an unknown fourth image. A rewrite that only names
# fullstack, music, and sidecar leaves this one on the placeholder.
cp "$MANIFEST" "${TMP}/in.yaml"
cat >> "${TMP}/in.yaml" <<EOF
    - name: future
      image: ghcr.io/frozentear/ts6-manager-future${KUBE_IMAGE_UNPINNED}
EOF

rewrite_kube_image_tags "${TMP}/in.yaml" "${TMP}/out.yaml" "$REQUESTED"

assert_rewritten() {
    local file="$1"
    local label="$2"
    local line ref
    for name in fullstack music sidecar future; do
        if grep -q "ts6-manager-${name}${KUBE_IMAGE_UNPINNED}" "$file"; then
            fail "${label}: ${name} still carries the placeholder"
        fi
        if ! grep -q "image: ghcr.io/frozentear/ts6-manager-${name}:${REQUESTED}" "$file"; then
            fail "${label}: ${name} is not on ${REQUESTED}"
        fi
    done
    while IFS= read -r line; do
        [[ "$line" == *"image:"* && "$line" == *"ts6-manager-"* ]] || continue
        ref="$(printf '%s\n' "$line" | sed -E 's/^[[:space:]]*image:[[:space:]]+//; s/[[:space:]]+$//')"
        case "$ref" in
            *"ts6-manager-"*":${REQUESTED}") ;;
            *) fail "${label}: rewrite did not touch ${ref}" ;;
        esac
    done < "$file"
    if grep -q 'value: "127.0.0.1/32"' "$MANIFEST" \
        && ! grep -q 'value: "127.0.0.1/32"' "$file"; then
        fail "${label}: rewrite changed TRUSTED_PROXY_CIDRS"
    fi
}

assert_rewritten "${TMP}/out.yaml" "placeholder"

# Live host copy: same manifest with a real release tag, not the placeholder.
sed "s#${KUBE_IMAGE_UNPINNED}#:${LEGACY_TAG}#g" "${TMP}/in.yaml" > "${TMP}/legacy.yaml"
if grep -q "${KUBE_IMAGE_UNPINNED}" "${TMP}/legacy.yaml"; then
    fail "legacy fixture still has the placeholder"
fi
rewrite_kube_image_tags "${TMP}/legacy.yaml" "${TMP}/legacy-out.yaml" "$REQUESTED"
assert_rewritten "${TMP}/legacy-out.yaml" "legacy ${LEGACY_TAG}"

echo "OK: kube image tag rewrite (placeholder and legacy tag)"
