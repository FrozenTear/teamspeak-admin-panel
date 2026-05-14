# TS6 Manager OCI Images — naming, build, sign, release

Authoritative reference for the two published OCI images and the sidecar
binary release artefacts. Tracked under
[PURA-160](/PURA/issues/PURA-160) (WS-OPS-Images). Sibling tracks
(WS-OPS-Quadlet, WS-OPS-Kube, WS-Gate) reference image names and tags
defined here.

> **Note on external publication.** Pushing images to a public registry,
> attaching binaries to a GitHub Release, and signing artefacts under
> Teamspeak Heaven's identity are all externally-visible actions. They
> require explicit board approval per the overnight moratorium and the
> standing "no externally-visible posts without confirmation" rule.
> The `scripts/build-images.sh` helper builds locally; the push / sign /
> release steps below are a procedure the board signs off on, not
> something the build machinery executes autonomously.

## 1. Image names + tagging scheme

Decided in coordination with CTO. Sibling tracks must match these names.

### Registry path

```
ghcr.io/frozentear/ts6-manager-fullstack
ghcr.io/frozentear/ts6-manager-sidecar
```

Settled via the [PURA-160](/PURA/issues/PURA-160) `ask_user_questions`
interaction (operator picked `pivot_frozentear` 2026-05-14T12:51Z). Repo
provisioned by [PURA-167](/PURA/issues/PURA-167) at
`github.com/FrozenTear/teamspeak-admin-panel` — **private** during the
board's manual verification window, board flips to public after testing.
ghcr.io image visibility follows the repo (private repo → private image)
until the board makes the flip. See §4 for the full publish procedure.

### Tags

| Tag                | Purpose                                                              | Mutability |
| ------------------ | -------------------------------------------------------------------- | ---------- |
| `vX.Y.Z`           | Immutable release tag matching the git tag (e.g. `v1.0.0`).          | Immutable  |
| `vX.Y`             | Floats to the latest patch within a minor.                           | Mutable    |
| `vX`               | Floats to the latest minor within a major.                           | Mutable    |
| `latest`           | Floats to the latest stable release (set after operator gate passes).| Mutable    |
| `main-<short-sha>` | Per-merge dev builds for Phase 6 gate work.                          | Immutable  |

The first signed release will be `v1.0.0`, tagged by WS-Gate after all
seven Chapter 1 verifications pass against a fresh rootless Podman
deploy ([PURA-155](/PURA/issues/PURA-155) §Definition of done).

### Architectures

Both images publish a manifest list spanning **`linux/amd64`** and
**`linux/arm64`** — the Phase 6 non-goals (PURA-160 description) cap
the matrix at those two. ARM64 covers operators running on Raspberry Pi 5
and Ampere/Graviton hosts; AMD64 covers everything else.

Multi-arch builds use the build helper:

```sh
PLATFORMS=linux/amd64,linux/arm64 IMAGE_VERSION=v1.0.0 scripts/build-images.sh all
```

This invokes `podman build --platform <list> --manifest <ref>`, producing
a single manifest list ref that operators pull regardless of host arch.

### Sidecar binary release artefacts

The sidecar's pre-built binaries ship as GitHub Release assets paired
with the `vX.Y.Z` git tag. One archive per (os, arch):

| Asset                                                   | Contents                                                   |
| ------------------------------------------------------- | ---------------------------------------------------------- |
| `ts6-manager-sidecar-vX.Y.Z-linux-amd64.tar.gz`           | `ts6-media-sidecar` binary stripped (crate-internal bin name), plus `LICENSE`. |
| `ts6-manager-sidecar-vX.Y.Z-linux-arm64.tar.gz`           | Same, ARM64.                                               |
| `ts6-manager-sidecar-vX.Y.Z-linux-amd64.tar.gz.sha256`    | `sha256sum` of the archive.                                |
| `ts6-manager-sidecar-vX.Y.Z-linux-arm64.tar.gz.sha256`    | Same.                                                      |
| `ts6-manager-sidecar-vX.Y.Z-linux-amd64.tar.gz.sig`       | cosign blob signature (see §3).                            |
| `ts6-manager-sidecar-vX.Y.Z-linux-arm64.tar.gz.sig`       | Same.                                                      |

The matched fullstack server binary is *not* published standalone — it
ships inside the `ts6-manager-fullstack` image. Operators who want a
binary install go through the image.

## 2. Build (local, rootless)

```sh
# All images, host arch, dev tag.
scripts/build-images.sh all

# Just one.
scripts/build-images.sh fullstack
scripts/build-images.sh sidecar

# Release-grade multi-arch build (slow — kernel needs binfmt_misc/qemu
# user-mode emulation enabled for the foreign arch).
IMAGE_VERSION=v1.0.0 \
PLATFORMS=linux/amd64,linux/arm64 \
scripts/build-images.sh all
```

Both Containerfiles run a final non-root stage:

* `Containerfile.fullstack` — uid:gid `10001:10001` (`ts6:ts6`), home
  `/var/lib/ts6-manager` (db + music dir, owned by the runtime user).
* `Containerfile.sidecar` — uid:gid `10002:10002` (`sidecar:sidecar`),
  home `/var/lib/ts6-manager-sidecar`.

Rootless `podman build` works without modification; rootless `podman run`
needs named volumes (not host bind-mounts) for the writable paths,
because the in-container uid maps to a shifted host subuid that does
not own `./data/*` on the host (the existing dev `podman-compose.yml`
encodes this).

## 3. Signing

We sign images with **cosign keyless OIDC** when an external CI identity
is available (GitHub Actions OIDC token), and **cosign with a long-lived
keypair** otherwise. Both produce signatures that `cosign verify` /
`cosign verify-blob` can check; the difference is provenance attestation.

Operators verifying a release:

```sh
# Image: verify the manifest list.
cosign verify ghcr.io/frozentear/ts6-manager-fullstack:v1.0.0 \
    --certificate-identity-regexp 'https://github.com/FrozenTear/teamspeak-admin-panel/.+' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com

# Sidecar binary: blob verification.
cosign verify-blob \
    --signature ts6-manager-sidecar-v1.0.0-linux-amd64.tar.gz.sig \
    --certificate-identity-regexp 'https://github.com/FrozenTear/teamspeak-admin-panel/.+' \
    --certificate-oidc-issuer https://token.actions.githubusercontent.com \
    ts6-manager-sidecar-v1.0.0-linux-amd64.tar.gz
```

WS-Gate's rootless-deploy validation MUST run `cosign verify` before
pulling the image and refuse if verification fails — that is the
"verify image provenance" line in PURA-160's deliverables.

Coordinate the exact OIDC identity / repo path / Fulcio root with
SecurityEngineer before the first signed publish.

## 4. Release procedure (board-gated)

This is the procedure the board signs off on for the first publish (and
for every subsequent `vX.Y.Z`). None of these steps fire from the local
build helper.

1. **Local build verification.** Run
   `IMAGE_VERSION=vX.Y.Z PLATFORMS=linux/amd64,linux/arm64 scripts/build-images.sh all`
   and capture `podman image inspect` digests.
2. **Operator: refresh GitHub token scope.** The host gh token currently
   has `gist, read:org, repo, workflow` — `write:packages` is needed for
   ghcr.io push. Operator runs `gh auth refresh -h github.com -s write:packages`
   in a browser flow and confirms via `gh auth status`.
3. **Operator: flip repo to public.** Per [PURA-167](/PURA/issues/PURA-167)
   the repo is private during board verification. ghcr.io image visibility
   inherits from the linked repo, so the "Pull works from a clean host
   with no auth (public)" line of PURA-160's DoD requires the repo to be
   public at publish time. Operator flips
   `github.com/FrozenTear/teamspeak-admin-panel` → public when verification
   completes.
4. **OIDC + Fulcio identity locked.** SecurityEngineer pairs with CTO to
   confirm the certificate-identity regex that WS-Gate will pin against
   (default proposal: `https://github.com/FrozenTear/teamspeak-admin-panel/.+`).
5. **Push manifest lists.**
   ```sh
   podman manifest push ghcr.io/frozentear/ts6-manager-fullstack:vX.Y.Z \
       docker://ghcr.io/frozentear/ts6-manager-fullstack:vX.Y.Z
   podman manifest push ghcr.io/frozentear/ts6-manager-sidecar:vX.Y.Z \
       docker://ghcr.io/frozentear/ts6-manager-sidecar:vX.Y.Z
   ```
6. **Sign images.**
   ```sh
   cosign sign --yes ghcr.io/frozentear/ts6-manager-fullstack:vX.Y.Z
   cosign sign --yes ghcr.io/frozentear/ts6-manager-sidecar:vX.Y.Z
   ```
7. **Bundle + sign sidecar binaries.** Per-arch tar.gz of the
   release-stripped `ts6-media-sidecar` binary (the crate's bin name),
   archive named `ts6-manager-sidecar-vX.Y.Z-linux-<arch>.tar.gz` for
   product-family consistency, with sha256 + cosign blob signature next
   to each.
8. **Create GitHub Release `vX.Y.Z`** with the binary archives,
   sha256 files, and `.sig` files attached. Body cites the manifest
   digests pushed in step 5.
9. **Hand off to WS-Gate.** Comment on the WS-Gate tracking issue with
   the published refs and signatures so the rootless-deploy validation
   can pin against them.

## 5. Reproducibility notes

* Both Containerfiles pin `docker.io/rust:1.95-slim-bookworm` as the
  builder base, matching `rust-toolchain.toml` (channel `1.95.0`).
* The fullstack image pins `dioxus-cli` to `0.7.7` exactly (matches the
  `dioxus` crate version — see
  [project_dx_cli_version_pin.md](https://github.com/FrozenTear/teamspeak-admin-panel)
  for the rationale).
* Sidecar `Cargo.lock` ships in-tree (`crates/ts6-media-sidecar/Cargo.lock`
  — crate path is named after the bin) and the build uses
  `cargo build --release --locked`.
* `Containerfile.fullstack` tolerates missing root `Cargo.lock` via the
  `Cargo.loc[k]` glob pattern, so a fresh checkout still builds.
* The base image digest is intentionally NOT pinned today — operators
  who require a fully reproducible release should pin to `@sha256:...`
  manually before tagging; we revisit when the v1.0 gate has settled.

## 6. Coordination

* **WS-OPS-Quadlet / WS-OPS-Kube** must reference image names exactly as
  above (no version suffix in the unit file — that comes from the
  deploy-time variable).
* **WS-Gate** uses the manifest list refs + cosign verification.
* **SecurityEngineer** owns the OIDC identity + Fulcio root choice and
  the `cosign verify` invocation that WS-Gate runs.
* **CTO** owns the GitHub org / registry path decision and the
  board-approval gate for the first publish.
