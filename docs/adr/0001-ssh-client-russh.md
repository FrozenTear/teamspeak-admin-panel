# ADR-0001 — SSH client crate: `russh`

- **Status:** Accepted (foundation slice of [PURA-69](/PURA/issues/PURA-69)).
- **Date:** 2026-05-08.
- **Author:** RustPlatform.
- **Reviewers:** SecurityEngineer (gate before SSHBridge merge).

## Context

Phase 2 ships SSHBridge — a parallel control path that issues TS6 ServerQuery
commands over the TeamSpeak SSH ServerQuery interface (default port `10022`,
spec Chapter 11). Operators select per-server between the WebQuery HTTP
backend (Phase 1, `crates/ts6-manager-server/src/webquery`) and SSHBridge.

Implementation-plan §2 already names `russh` as the working assumption for
"TS SSH bridge"; this ADR confirms the choice and documents why the
alternatives were rejected.

## Decision

Use **`russh`** (with `russh-keys` for key parsing) as the SSH client crate
for SSHBridge.

## Alternatives considered

| Crate | Verdict | Reason |
|---|---|---|
| **`russh`** | ✅ **Chosen** | Pure-Rust async (tokio). Maintained on the Pijul `nest.pijul.com` git tree with regular crates.io releases. Supports password + public-key + ssh-agent auth. Exposes both session and channel layers, which matches the spec §11 model (one base session per `(configId, sid)` plus optional command-listener sessions). No C dependencies — keeps the OCI build single-stage friendly. |
| `async-ssh2-tokio` | ❌ Rejected | libssh2 wrapper. Requires linking against system libssh2 + OpenSSL → brings native-tls into the build, conflicts with the rustls posture (`reqwest` uses rustls-tls per workspace `Cargo.toml`). Adds a stage to `Containerfile.fullstack`. |
| `openssh` (the crate) | ❌ Rejected | Wraps the host `ssh` binary via `ControlMaster`. Inherits the host's `~/.ssh` config and `known_hosts`, which fights the in-DB key-storage model and the rootless Containerfile final stage. Process-per-call overhead, hard to multiplex within one process, hard to audit at the wire level. |
| `thrussh` | ❌ Rejected | Predecessor to `russh`. Effectively unmaintained — successor is `russh`. |
| `libssh-rs` | ❌ Rejected | libssh wrapper. Same C-dep issues as `async-ssh2-tokio`. |
| Hand-rolled SSH | ❌ Not seriously considered | SSH protocol surface is too large to hand-roll for a parallel control path. |

## Consequences

**Positive.**

- Pure-Rust build keeps the existing `Containerfile.fullstack` stages
  (rustls + scrypt + aes-gcm — no OpenSSL, no libssh).
- Async-tokio model composes with the existing axum/`tokio::main` runtime.
- Channel API gives SSHBridge a native fit for spec §11.2's "one base
  session, optional command-listener sessions per channel" model — each
  TS ServerQuery line goes onto a single SSH channel; multiple channels
  on the same SSH transport remain available if the listener concept
  arrives later.
- Public-key + ssh-agent support already there. PURA-69's "Default to
  ssh-agent or an encrypted-at-rest private key" requirement is reachable
  without a second SSH crate.

**Negative.**

- `russh`'s API surface evolves between minor versions. Pin to a single
  exact minor (`russh = "<X.Y>"`, `russh-keys = "<X.Y>"`) and bump
  deliberately, not via `cargo update`.
- Host-key verification is the implementer's responsibility — `russh`
  exposes the host-key callback but does not provide a `known_hosts`
  reader of its own. SSHBridge will need a `known_hosts`-style verifier
  (or strict pre-registered host-key fingerprints in DB). Treat this as a
  P0 review item with SecurityEngineer; the foundation slice does NOT
  ship a verifier yet.

## Scope handed forward

This ADR covers the crate-selection decision only. The follow-up slices
under PURA-69 will:

1. Wire `russh` into `sshbridge::transport`, including channel + I/O
   plumbing.
2. Implement password / encrypted-private-key / ssh-agent auth selectors,
   keyed off the operator's per-server configuration.
3. Define the host-key verification model (`known_hosts` vs strict-fp vs
   TOFU) — explicit SecurityEngineer review item.
4. Add the env-gated integration test against a containerised TS6 SSH
   ServerQuery target.

Each slice lands as its own child issue under PURA-69.

## Hard constraint reminder

If `russh` proves to need an upstream PR, FR, or bug report during this
work, the round-trip captured on [PURA-69](/PURA/issues/PURA-69) and
[PURA-66](/PURA/issues/PURA-66) applies: document internally, draft locally,
post the exact draft on the relevant Paperclip thread, wait for explicit
board ack, only then file under the board's identity.
