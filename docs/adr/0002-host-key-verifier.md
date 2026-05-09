# ADR-0002 — SSHBridge host-key verification policy

- **Status:** Accepted; **Amended 2026-05-09 ([PURA-100](/PURA/issues/PURA-100))** — TOFU added as opt-in fourth policy.
- **Date:** 2026-05-08; amended 2026-05-09.
- **Author:** RustPlatform; amendment by SecurityEngineer.
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
| **`TrustOnFirstUse`** (PURA-100) | On first connect: capture the offered key's SHA-256 fingerprint into the verifier's per-instance pin slot, fire a non-blocking persistence request onto the [`sshbridge::tofu`] worker, and accept. On every subsequent verify call: enforce strict equality against the pinned value and reject mismatches — same behaviour as `StrictFingerprint` from that point. **Opt-in only**, gated by `TS_SSH_TOFU=1` AND the row's `sshHostKeyFingerprint` being NULL AND `TS_SSH_KNOWN_HOSTS` being unset. |
| **`Reject`** | Reject every key. The default constructor used when no policy above applies — i.e. neither a per-server fingerprint, nor `TS_SSH_KNOWN_HOSTS`, nor an opt-in TOFU sink is configured. | Initialisation safety net — the bridge refuses to talk to a server until verification is opted into. |

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
| **TOFU (trust on first use)** | ⚠️ Initially rejected; **shipped as opt-in fourth policy in PURA-100** | The original objection — silent first-connect attacker wins — still stands; the amendment narrows the surface to opt-in via `TS_SSH_TOFU=1` so operators bringing up a fresh TS6 server without out-of-band fingerprint access have an audited path. The default-off posture means every operator who has not actively chosen the tradeoff still gets the deterministic `Reject` failure mode the original ADR demanded. See the **TOFU amendment** section below. |
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

## TOFU amendment (PURA-100)

QA flagged that operators bringing up a fresh TS6 server have no
audited first-trust path: the wizard accepts a pinned
`sshHostKeyFingerprint` but the operator has to `ssh-keyscan` the box
themselves, paste the result into the wizard, and then connect. For
self-hosted deployments behind NAT, that is the same chicken-and-egg
the operator is trying to solve. The amendment adds an opt-in TOFU
policy for that case.

### Selection rules

`HostKeyPolicy::TrustOnFirstUse` is selected only when ALL of the
following hold:

1. `TS_SSH_TOFU=1` is set in the manager's environment at boot.
2. The per-server `server_connection.sshHostKeyFingerprint` column is
   NULL (the operator has not pinned a fingerprint).
3. `TS_SSH_KNOWN_HOSTS` is unset (no shared known-hosts file).

If any of (1)–(3) is missing, the original selection logic applies —
strict-fingerprint, known-hosts, or `Reject`. Strict-fingerprint and
known-hosts policies always take precedence over TOFU even when TOFU
is enabled, so an operator who pinned a row gets strict verification,
period. TOFU only fills the gap for rows that have never been pinned.

### Capture pipeline

`russh::client::Handler::check_server_key` is sync — doing a SurrealDB
write from inside it would block the SSH key exchange on DB latency
and put the dispatch supervisor's wedge surface inside a `!Send` russh
boundary. Instead:

1. The verifier's per-instance `OnceLock<Fingerprint>` pin slot is set
   to the observed fingerprint. This is the load-bearing in-memory
   trust anchor; every later `verify` call on this verifier compares
   against the pinned value and rejects mismatches even if persistence
   is delayed or fails.
2. A non-blocking `try_send` posts a `TofuCaptureRequest` onto the
   bounded mpsc consumed by `sshbridge::tofu::spawn_capture_worker`.
3. The worker drains the channel and runs a CAS-style update:
   `UPDATE server_connection MERGE { sshHostKeyFingerprint: $fp }
   WHERE sshHostKeyFingerprint = NONE`. Already-pinned rows are
   left untouched (logged as a warn) so a later TOFU race or worker
   re-run cannot clobber an operator's manual pin.

### Audit

Every capture emits `tracing::warn!` under target `sshbridge::hostkey`
with `config_id`, `host`, `port`, observed fingerprint, and operator
`user_id` (when reachable — system-driven connects log `None`). The
persistence outcome (success / CAS no-op / DB error) emits a
follow-up `info`/`warn` line under the same target.

### Residual risk and mitigations

| Risk | Mitigation |
|---|---|
| First-connect MitM pins the wrong key permanently. | Documented operator tradeoff. The boot path warns on every start when `TS_SSH_TOFU=1` is on. Operators who can extract the fingerprint out-of-band are directed to pin manually instead. |
| MitM swaps the key on a reconnect within the same process. | The verifier's per-instance pin slot enforces strict equality from the second `verify` call onward — `tofu_second_call_with_different_key_is_rejected` is the regression test. |
| Two concurrent first-connects against different MitMs both succeed and clobber each other on disk. | The worker's CAS write (`WHERE sshHostKeyFingerprint = NONE`) ensures only the first writer wins on disk. The verifier's `OnceLock::set`'s `Err(prior)` branch handles the in-memory race the same way. |
| Worker channel saturated → persistence dropped, row stays NULL on disk. | The verifier still enforces the in-memory pin for this process's lifetime — accepted as documented degraded mode. Operators who see `TOFU capture channel full` warns must redeploy with sufficient channel capacity or pin manually. |
| TOFU enabled in production by accident. | `parse_bool_flag` requires an explicit `1`/`true`. The boot-time `tracing::warn!` line in `Config::log_hardening_summary` makes the state operator-visible in logs and in any monitoring journal grep. |

## Cleanroom + upstream-PR posture

Inherited from PURA-66 / PURA-69 / ADR-0001: if the chosen policy hits a
russh API gap (e.g. the `known_hosts` helper's port-handling has a bug),
follow the round-trip — document internally, draft locally, post the
exact draft on the relevant Paperclip thread, wait for board ack, then
file under the board's identity. **No upstream PR/FR/bug without that
round-trip.**
