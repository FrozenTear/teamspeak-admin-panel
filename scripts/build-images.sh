#!/usr/bin/env bash
# Build TS6 Manager OCI images locally (rootless podman).
#
# This script intentionally does NOT push, sign, or tag against any registry —
# external publication is a board-approved step documented in
# docs/ops/images.md. Use this script for:
#   * dev-host smoke builds
#   * pre-release reproducibility checks
#   * CI multi-arch builds (driven by --platform)
#
# Usage:
#   scripts/build-images.sh [fullstack|sidecar|all] [--platform linux/amd64,linux/arm64]
#
# Defaults: builds both images for the host platform with version=dev.
#
# Environment overrides:
#   IMAGE_VERSION    — semver tag (default: dev)
#   IMAGE_REVISION   — git rev (default: `git rev-parse --short HEAD`)
#   IMAGE_REGISTRY   — registry prefix to tag against (default: localhost)
#   IMAGE_NAMESPACE  — registry namespace (default: empty when IMAGE_REGISTRY=localhost)
#   PLATFORMS        — comma-separated --platform spec; overrides default
#
# Example: release-grade local build, both arches, version pinned:
#   IMAGE_VERSION=v1.0.0 PLATFORMS=linux/amd64,linux/arm64 scripts/build-images.sh all

set -euo pipefail

target="${1:-all}"
platforms="${PLATFORMS:-}"
if [[ "${2:-}" == "--platform" && -n "${3:-}" ]]; then
    platforms="$3"
fi

version="${IMAGE_VERSION:-dev}"
revision="${IMAGE_REVISION:-$(git rev-parse --short HEAD 2>/dev/null || echo unknown)}"
registry="${IMAGE_REGISTRY:-localhost}"
namespace="${IMAGE_NAMESPACE:-}"

if [[ "$registry" == "localhost" ]]; then
    fullstack_ref="localhost/ts6-manager-fullstack:${version}"
    sidecar_ref="localhost/ts6-manager-sidecar:${version}"
else
    prefix="${registry}"
    [[ -n "$namespace" ]] && prefix="${prefix}/${namespace}"
    fullstack_ref="${prefix}/ts6-manager-fullstack:${version}"
    sidecar_ref="${prefix}/ts6-manager-sidecar:${version}"
fi

build_args=(
    --build-arg "IMAGE_VERSION=${version}"
    --build-arg "IMAGE_REVISION=${revision}"
)

if [[ -n "$platforms" ]]; then
    # Manifest-list builds. Requires `podman manifest` support (>= 3.0).
    build_cmd=(podman build --platform "$platforms" --manifest)
else
    build_cmd=(podman build --tag)
fi

build_image() {
    local containerfile="$1"
    local ref="$2"

    echo ">>> Building $ref from $containerfile"
    "${build_cmd[@]}" "$ref" "${build_args[@]}" -f "$containerfile" .
}

case "$target" in
    fullstack)
        build_image Containerfile.fullstack "$fullstack_ref"
        ;;
    sidecar)
        build_image Containerfile.sidecar "$sidecar_ref"
        ;;
    all)
        build_image Containerfile.fullstack "$fullstack_ref"
        build_image Containerfile.sidecar "$sidecar_ref"
        ;;
    *)
        echo "Usage: $0 [fullstack|sidecar|all] [--platform <comma-separated>]" >&2
        exit 2
        ;;
esac

echo ""
echo "Built images:"
podman image ls --format "  {{.Repository}}:{{.Tag}}  {{.Size}}" \
    | grep -E "ts6-(manager-fullstack|media-sidecar)" || true
