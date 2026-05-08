# ADR-0002 — SSHBridge host-key verification policy

- **Status:** Accepted (PURA-76 follow-up A under [PURA-69](/PURA/issues/PURA-69)).
- **Date:** 2026-05-08.
- **Author:** RustPlatform.
- **Reviewers:** SecurityEngineer (gate before SSHBridge merge — required by ADR-0001 §Negative).

## Context

ADR-0001 picked `russh` as the SSH client crate but explicitly deferred the
host-key verification model: "`russh` exposes the host-key callback but does
not provide a `known_hosts` reader of its own. SSHBridge will need a
`known_hosts`-style verifier (or strict pre-registered host-key fingerprints
in DB)." PURA-76 requires that this slice ship a verifier whose **default
MUST NOT be unconditional accept**.

The reference TS3/TS6 deployment model treats each TeamSpeak server as a
discrete operator-configured row in `server_connection`. Operators bring
their own host (often self-hosted, often behind NAT, often regenerated as
part of a fresh container build), so a single shared `known_hosts` across
the manager process is not a natural fit — the canonical store of
"trusted state for THIS server" is the row that already holds host, port,
and credentials.

`russh` 0.60.x absorbed the previously-separate `russh-keys` crate into
`russh::keys` and now exposes `ssh_key::Fingerprint` + `HashAlg` directly
(SHA-256-base64 the OpenSSH default). It also ships an OpenSSH
`known_hosts` parser for operators who want the file-based model anyway.

## Decision

SSHBridge ships a `HostKeyVerifier` with three configurable policies; the
operator-facing default is **strict per-server fingerprint pinning** stored
on the `server_connection.sshHostKeyFingerprint` column (added by
migration `0005_ssh_bridge_auth.surql`, ratified under D-SSH-AUTH).

| Policy | Behaviour | When |
|---|---|---|
| **`StrictFingerprint`** (default) | Compute `Fingerprint::Sha256(ssh-key)` of the offered server key; accept iff it matches one of the operator-supplied fingerprints. Empty list rejects every key. | Per-server row in DB. **Default for all production deployments.** |
| **`KnownHostsFile`** | Defer to `russh::keys::known_hosts::check_known_hosts_path(host, port, key, path)`. Returns `false` if the file does not contain a matching entry. | Operator escape hatch — set `TS_SSH_KNOWN_HOSTS=/etc/ts6-manager/ssh_known_hosts` to share a known-hosts file across all servers. |
| **`Reject`** | Reject every key. The default constructor used when neither a per-server fingerprint nor `TS_SSH_KNOWN_HOSTS` is configured. | Initialisation safety net — the bridge refuses to talk to a server until verification is opted into. |

The verifier is invoked from the russh `client::Handler::check_server_key`
callback. A reject decision is logged at `tracing::warn!` under target
`sshbridge::hostkey` with `config_id`, `host`, `port`, and the observed
fingerprint. An accept decision is logged at `tracing::info!` with the
same fields so the audit stream shows every host-key validation event,
not just the failures.

## Alternatives considered

| Option | Verdict | Reason |
|---|---|---|
| **Strict fingerprint in DB** | ✅ **Chosen as default** | Aligns with the rest of the per-server config model; no separate file to provision; canonical place for the operator to pin trust; round-trips cleanly through PURA-77's `sshHostKeyFingerprint` column. |
| **`known_hosts` file only** | ⚠️ Shipped as a secondary policy, not the default | Familiar to operators but the file becomes a separate provisioning concern, and a single shared file across many TS6 servers concentrates blast radius if an attacker can write to it. Useful as an escape hatch for sites with existing OpenSSH tooling. |
| **TOFU (trust on first use)** | ❌ Rejected | Trades a deterministic "configure first" failure mode for a silent "first-connect attacker wins" failure mode. The TS6 admin panel is the canonical control plane for the operator's TeamSpeak instance; a MITM during initial setup would leak the operator's `serveradmin` credentials. The PURA-69 thread explicitly warns against this. |
| **Unconditional accept** | ❌ Rejected | PURA-76 forbids it. Equivalent to no verification — defeats the SSH security model. |
| **Public-key-with-CA pinning** | ❌ Out of scope | TS6 servers do not ship CA-issued host keys in practice. May revisit if community deployment patterns shift. |

## Consequences

**Positive.**

- Host-key trust lives in the same place as the rest of the server's
  trust state (its `server_connection` row). Operators rotate trust by
  editing the row, not by editing a file on disk.
- The verifier is a pure function of `(policy, offered key)` — easy to
  unit-test under `cfg(test)` without touching russh I/O.
- The `Reject` default protects fresh deployments: an operator who
  forgets to set the fingerprint sees an SSH connection failure
  immediately, not a successful connection to a possibly-attacker-
  controlled host.

**Negative.**

- Operators must extract the SHA-256 fingerprint of their TS6 server's
  SSH host key once during initial setup. Documented friction; the
  `ssh-keyscan -t ed25519 host -p 10022 | ssh-keygen -lf -` recipe
  belongs in the ops runbook (separate doc).
- Re-keying the TS6 server requires the operator to update the row.
  This is the *intended* failure mode — the bridge stops trusting the
  server until the operator confirms the new key. The REST surface
  (PURA-69 follow-up C) will return a typed `HOST_KEY_MISMATCH` error
  that the UI can render as "the host key changed; verify and update."

**Schema integration.**

The `sshHostKeyFingerprint` column is `option<string>`; the verifier
reads it as `OpenSSH SHA256:base64` format (the standard `ssh-keygen
-lf -` shape) and parses it via `russh::keys::ssh_key::Fingerprint`'s
`FromStr`. A NULL value triggers the `Reject` policy at construct time
unless the global `TS_SSH_KNOWN_HOSTS` escape hatch is set, in which
case `KnownHostsFile` takes over.

## Cleanroom + upstream-PR posture

Inherited from PURA-66 / PURA-69 / ADR-0001: if the chosen policy hits a
russh API gap (e.g. the `known_hosts` helper's port-handling has a bug),
follow the round-trip — document internally, draft locally, post the
exact draft on the relevant Paperclip thread, wait for board ack, then
file under the board's identity. **No upstream PR/FR/bug without that
round-trip.**
