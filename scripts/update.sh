#!/usr/bin/env bash
# Kube upgrade on the host where this script runs: pull GHCR images,
# play a temp manifest, smoke /health, then re-apply the soft CPU pin.
# Packing B: fullstack stays 2-5 / -5. DECODE 2-5 is kube env.
# Bot SEND pin is in-process; this script only re-applies HostConfig
# + host nice (music HostConfig stays unset under packing B).
# Shrink (A) is not default.
#
# TS6_LOCAL_MUSIC decides whether this script starts the music
# container on this host. Default is skip: music is running on a
# different host, and starting the local container binds
# 127.0.0.1:3002 and clones the client. TS6_LOCAL_MUSIC=play starts
# the local container again, and only after the remote music process
# is stopped. The script prints which mode it is using.
#
# Usage (from any cwd, against a repo checkout):
#   ./scripts/update.sh vX.Y.Z
#   TS6_LOCAL_MUSIC=play ./scripts/update.sh vX.Y.Z
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
    echo "  Pull GHCR images for TAG, kube down (no --force), kube play," >&2
    echo "  curl /health, then re-apply the soft pin" >&2
    echo "  (packing B: fullstack 2-5; music HostConfig unset)." >&2
    echo "  TS6_LOCAL_MUSIC=skip (default) does not start the local music container." >&2
    echo "  The panel keeps http://127.0.0.1:3002. That port is the hop from the music host." >&2
    echo "  The hop must answer before this script restarts the API." >&2
    echo "  TS6_LOCAL_MUSIC=play starts the local music container again." >&2
    echo "  Stop the remote music process first. Two copies share one client identity." >&2
    echo "example: $0 v1.6.X" >&2
    echo "example: TS6_LOCAL_MUSIC=play $0 v1.6.X" >&2
    exit 2
}

# local_music_action
# Print "skip" or "play". Default is skip: music runs on another host.
# Aliases: skip/0/off/false/no and play/1/on/true/yes.
# The canonical value is what this script exports for the soft-pin child.
local_music_action() {
    local raw="${TS6_LOCAL_MUSIC:-skip}"
    local norm
    norm="$(printf '%s' "$raw" | tr '[:upper:]' '[:lower:]' | tr -d '[:space:]')"
    case "$norm" in
        skip|0|off|false|no) printf '%s\n' skip ;;
        play|1|on|true|yes) printf '%s\n' play ;;
        *)
            echo "error: TS6_LOCAL_MUSIC must be skip or play (got: ${raw})" >&2
            return 1
            ;;
    esac
}

# print_local_music_banner ACTION TAG
# Operator-visible mode line. Not a comment: this is what the run prints.
print_local_music_banner() {
    local action="$1"
    local tag="$2"
    if [[ "$action" == "skip" ]]; then
        cat <<EOF
==> Local music container: SKIP (TS6_LOCAL_MUSIC=skip).
    Music is running on a different host. This run will not start the local music container.
    Starting it binds 127.0.0.1:3002 and the TeamSpeak server sees a second copy of the same client identity.
    Turn it back on with: TS6_LOCAL_MUSIC=play $0 ${tag}
    Stop the remote music process first.
EOF
    else
        cat <<EOF
==> Local music container: PLAY (TS6_LOCAL_MUSIC=play).
    Stop the remote music process first.
    The local volume still has the same client identity. Two running copies and the TeamSpeak server sees the client twice.
    This container also takes 127.0.0.1:3002 from the hop.
EOF
    fi
}

# explain_local_music_running NAME
# Printed when skip mode finds the local music container already up.
# 127.0.0.1:3002 is then that container, not the remote hop, and this
# script must not restart the API.
explain_local_music_running() {
    local name="${1:-ts6-manager-music}"
    cat <<EOF
error: ${name} is running, so 127.0.0.1:3002 is not the remote hop.
  That container clones the client identity if it stays up beside the remote music process.
  Stop it only after the remote hop can bind 127.0.0.1:3002, then re-run.
  This run will not restart the API.
EOF
}

# explain_music_hop_down
# Printed when skip mode would restart the API without the remote hop.
explain_music_hop_down() {
    cat <<'EOF'
error: 127.0.0.1:3002 is not answering, so this update will not restart the API.
  The panel still uses http://127.0.0.1:3002 with no bearer. That socket is the reverse tunnel from the music host, not the local music container.
  Cut order: the hop has to be listening, then the API on this host starts. On a reboot of the music host, ts6-music-tunnel.service is what should bind the hop. The music process comes back from its restart policy. The client is pushed only when this API starts. If this API boots while 127.0.0.1:3002 is not answering, the saved row is never pushed and the music page is 404 (empty supervisor). A hop that dies after the client is already connected is a 5xx from the API. The TeamSpeak client stays in the channel, and the panel cannot drive it until the hop is back.
  Do not start the local music container to fill the port. Its volume still has the same client identity, and the TeamSpeak server would see a second copy.
  Do not publish :3002 or :7080, and do not change MUSIC_RUNTIME_URL.
EOF
}

# strip_music_container SRC DST
# Drop the music container item from a rewritten kube manifest.
# Container items in deploy/kube/ts6-manager.yaml are indented four spaces.
# A four-space comment block sitting directly above that item is dropped
# with it. Nested "name: music" (the port) is inside the dropped item.
strip_music_container() {
    local src="$1"
    local dst="$2"
    awk '
        function flush_comments() {
            for (i = 1; i <= ncomments; i++) print comments[i]
            ncomments = 0
        }
        /^    #/ && !skip {
            comments[++ncomments] = $0
            next
        }
        ncomments > 0 && /^[[:space:]]*$/ && !skip {
            comments[++ncomments] = $0
            next
        }
        /^    - name: music[[:space:]]*$/ {
            ncomments = 0
            skip = 1
            next
        }
        skip && /^    - name: [^[:space:]]+/ {
            skip = 0
            flush_comments()
            print
            next
        }
        skip { next }
        {
            flush_comments()
            print
        }
    ' "$src" > "$dst"
}

# prepare_local_play_manifest SRC DST ACTION
# skip: DST is SRC without the music container, still pointing the API
# at http://127.0.0.1:3002, still loopback for sidecar :7080.
# play: DST is a copy of SRC (the music container is played).
prepare_local_play_manifest() {
    local src="$1"
    local dst="$2"
    local action="$3"
    case "$action" in
        play)
            cp "$src" "$dst"
            ;;
        skip)
            strip_music_container "$src" "$dst" || return 1
            if grep -q 'ghcr.io/frozentear/ts6-manager-music' "$dst"; then
                echo "error: play manifest still contains the music image" >&2
                return 1
            fi
            if grep -Eq '^    - name: music[[:space:]]*$' "$dst"; then
                echo "error: play manifest still contains the music container" >&2
                return 1
            fi
            if grep -q 'containerPort: 3002' "$dst"; then
                echo "error: play manifest still has containerPort 3002" >&2
                return 1
            fi
            ;;
        *)
            echo "error: music action must be skip or play (got: ${action})" >&2
            return 1
            ;;
    esac
    if ! grep -q 'value: "http://127.0.0.1:3002"' "$dst"; then
        echo "error: play manifest lost MUSIC_RUNTIME_URL http://127.0.0.1:3002" >&2
        return 1
    fi
    if ! grep -q '127.0.0.1:7080' "$dst"; then
        echo "error: play manifest lost sidecar loopback :7080" >&2
        return 1
    fi
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

# wait_music_hop URL
# Probe the remote music hop on this host's loopback. Short budget:
# this runs before kube down. Returns 1 when the hop never answers.
wait_music_hop() {
    local url="$1"
    local out="${TMPDIR}/music-hop.out"
    local ok=0
    local try
    for try in $(seq 1 5); do
        if curl -fsS --max-time 5 "$url" >"$out" 2>/dev/null; then
            ok=1
            break
        fi
        if [[ "$try" -lt 5 ]]; then
            sleep 2
        fi
    done
    if [[ "$ok" -ne 1 ]]; then
        return 1
    fi
    echo "    music hop: $(cat "$out")"
}

# refuse_running_local_music
# Skip mode must not restart the API while the local music container is up.
# A running container is holding 127.0.0.1:3002, so a health check would
# not be the remote hop. Leave it for the operator to stop once the hop
# can bind. Do not remove volumes.
refuse_running_local_music() {
    local name="${TS6_BOT_CONTAINER:-ts6-manager-music}"
    local state
    if ! podman container exists "$name"; then
        echo "==> ${name} is not present (TS6_LOCAL_MUSIC=skip)"
        return 0
    fi
    state="$(podman inspect -f '{{.State.Status}}' "$name")"
    if [[ "$state" == "running" ]]; then
        explain_local_music_running "$name" >&2
        return 1
    fi
    echo "==> ${name} status is ${state} (left stopped)"
}

# assert_local_music_not_running
# After a skip play, the local music container must not be running. A
# running copy binds 127.0.0.1:3002 and the TeamSpeak server sees a
# second copy of the client. Stop a leftover. Do not remove volumes.
assert_local_music_not_running() {
    local name="${TS6_BOT_CONTAINER:-ts6-manager-music}"
    local state
    if ! podman container exists "$name"; then
        echo "==> ${name} is not present (TS6_LOCAL_MUSIC=skip)"
        return 0
    fi
    state="$(podman inspect -f '{{.State.Status}}' "$name")"
    if [[ "$state" == "running" ]]; then
        echo "==> stopping ${name}: it was running after play and would bind 127.0.0.1:3002 beside the remote music process"
        podman stop "$name"
        return 0
    fi
    echo "==> ${name} status is ${state} (not started)"
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

if ! MUSIC_ACTION="$(local_music_action)"; then
    usage
fi
# Canonical skip|play for this process and for apply-fullstack-soft-pin.sh.
export TS6_LOCAL_MUSIC="$MUSIC_ACTION"
print_local_music_banner "$MUSIC_ACTION" "$TAG"

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

PLAY_SPEC="${TMPDIR}/ts6-manager.play.yaml"
if ! prepare_local_play_manifest "$PLAY_POD" "$PLAY_SPEC" "$MUSIC_ACTION"; then
    die "failed to build the play manifest (TS6_LOCAL_MUSIC=${MUSIC_ACTION})"
fi

echo "==> pulling ${FULLSTACK}"
podman pull "$FULLSTACK"
if [[ "$MUSIC_ACTION" == "play" ]]; then
    echo "==> pulling ${MUSIC}"
    podman pull "$MUSIC"
else
    echo "==> not pulling ${MUSIC} (TS6_LOCAL_MUSIC=skip; local music container will not start)"
fi
echo "==> pulling ${SIDECAR}"
podman pull "$SIDECAR"

# The API pushes saved bots when it comes back. With the local music
# container skipped, 127.0.0.1:3002 is the remote hop and has to be up
# before kube down. If this API boots while that port is silent, the
# saved row is never pushed and the music page is 404 (empty supervisor).
# A running local music container would make that health check a lie.
if [[ "$MUSIC_ACTION" == "skip" ]]; then
    if ! refuse_running_local_music; then
        die "local music container is running"
    fi
    echo "==> checking music hop http://127.0.0.1:3002/health before the API restarts"
    if ! wait_music_hop "http://127.0.0.1:3002/health"; then
        explain_music_hop_down >&2
        die "music hop on 127.0.0.1:3002 is not answering"
    fi
fi

echo "==> podman kube down (no --force; volumes stay)"
# Identity is the pod name in the YAML, not the image tag. Down reads
# the rewritten manifest (valid tags), not the committed @UNRELEASED file.
DOWN_MANIFEST="$(kube_down_manifest "$MANIFEST" "$PLAY_POD")"
if podman pod exists ts6-manager; then
    # Stale play-state IDs on the panel host: kube down can fail with
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

PLAY_FILE="$PLAY_SPEC"
if [[ "$HAVE_SECRET" -eq 1 ]]; then
    echo "==> podman secret ts6-manager-secrets exists; playing pod+PVCs only"
else
    echo "==> concatenating ${SECRETS} + temp manifest"
    PLAY_FILE="${TMPDIR}/ts6-manager.with-secrets.yaml"
    cat "$SECRETS" "$PLAY_SPEC" > "$PLAY_FILE"
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
if [[ "$MUSIC_ACTION" == "play" ]]; then
    echo "==> waiting for http://127.0.0.1:3002/health (local music container)"
    wait_health "http://127.0.0.1:3002/health" "music /health"
else
    echo "==> re-checking music hop http://127.0.0.1:3002/health (remote music host)"
    if ! wait_music_hop "http://127.0.0.1:3002/health"; then
        explain_music_hop_down >&2
        die "music hop on 127.0.0.1:3002 stopped answering during the API restart"
    fi
    assert_local_music_not_running
fi

echo "==> applying soft pin (packing B: fullstack 2-5; SEND in-process; music HostConfig unset)"
"${SCRIPT_DIR}/apply-fullstack-soft-pin.sh" \
    || die "soft pin requested but apply failed"

echo
if [[ "$MUSIC_ACTION" == "skip" ]]; then
    echo "OK: ts6-manager is on ${TAG} (fullstack + sidecar)."
    echo "    Local music container was not started (TS6_LOCAL_MUSIC=skip)."
    echo "    Turn it back on with: TS6_LOCAL_MUSIC=play $0 ${TAG}"
    echo "    Stop the remote music process first."
else
    echo "OK: ts6-manager is on ${TAG} (fullstack + music + sidecar)."
fi
echo "    volumes ts6-data / ts6-db / ts6-music were left in place."
echo "    never run: podman kube down --force"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
