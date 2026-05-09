# Operator note — SSH host-key trust-on-first-use (TS_SSH_TOFU)

**TL;DR — this is opt-in and weakens your security posture.** Default
operator behaviour is unchanged; the SSH bridge stays fail-closed
(`HostKeyPolicy::Reject`) for every server row whose
`sshHostKeyFingerprint` column is NULL. Read this doc only if you've
been hit by the "I can't extract the fingerprint out-of-band" problem
that PURA-100 was filed for.

## When to use TOFU

- You're bringing up a fresh TS6 server *and* you cannot extract the
  upstream's SSH host-key fingerprint via console / IPMI / cloud-init
  before pointing the manager at it.
- You've already considered (and ruled out) running `ssh-keyscan -t
  ed25519 host -p 10022 | ssh-keygen -lf -` from a host inside the
  same trust boundary as the TS6 server, then pasting the result into
  the wizard's `sshHostKeyFingerprint` field.

## When NOT to use TOFU

- **Production deployments where the operator can extract the
  fingerprint out-of-band.** Use the strict-fingerprint path — pin
  `sshHostKeyFingerprint` on the row and never enable
  `TS_SSH_TOFU`. That's the path ADR-0002 was designed around.
- **Multi-tenant environments where multiple operators share one
  manager.** TOFU is a process-wide opt-in; one operator turning it
  on changes the trust posture for every server row whose
  fingerprint is NULL.
- **Anywhere the network between the manager and the TS6 server is
  not under your control during initial setup.** The first-connect
  MitM window is the security exposure surface.

## How to enable

Set the environment variable on the manager process and restart:

```bash
export TS_SSH_TOFU=1
```

The manager logs a `WARN` line on every boot when TOFU is enabled —
look for `TS_SSH_TOFU=1` in the journal to confirm. If you don't see
that line, the env var was not parsed (acceptable values are `1`,
`true`, `True`, `TRUE`).

## What happens on the first connect

1. The SSH bridge opens a session to the configured host:port.
2. `check_server_key` observes the upstream's offered host key.
3. Because the row's `sshHostKeyFingerprint` is NULL and `TS_SSH_TOFU=1`:
   - The fingerprint is captured into a per-process in-memory pin slot.
   - A background worker writes the fingerprint into the row via a
     CAS-style update — only succeeds if the column is still NULL.
   - The connect proceeds.
4. A `WARN`-level audit line lands under target `sshbridge::hostkey`
   with `config_id`, `host`, `port`, the captured fingerprint, and
   the operator `user_id` (when reachable).

## What happens on every later connect

The row now has a non-NULL fingerprint. The verifier selection logic
picks `HostKeyPolicy::StrictFingerprint`, exactly as if the operator
had pinned it through the wizard. TOFU never re-fires for that row.

## Failure modes you should expect

| Symptom | Cause | Action |
|---|---|---|
| `TOFU fingerprint persistence failed` warn line | DB hiccup, schema drift, or migration not run. | The in-memory pin still protects this process; if you restart the manager before the DB recovers, TOFU re-fires against whatever the upstream presents at that moment. Fix the DB, then either restart cleanly or pin the fingerprint manually. |
| `TOFU fingerprint already pinned on row (CAS no-op)` warn line | Another writer (operator pin, prior TOFU capture) won the race. | This is benign — the row is pinned to whatever the first writer captured. Verify it matches the actual upstream key (`ssh-keyscan ...`); if not, edit the row to correct it. |
| `TOFU capture channel full; persistence event dropped` warn line | Worker is back-pressured (genuinely unusual — the channel has 32 slots and TOFU-eligible connects are rare). | The in-memory pin still protects this process, but the row stays NULL on disk so a manager restart re-TOFUs. Investigate why the worker is slow; pin the fingerprint manually as a fix. |
| `host-key REJECTED (TOFU pin mismatch — server key changed mid-process)` | The upstream presented a *different* host key on a reconnect within the same manager process. | Either you legitimately re-keyed the upstream (in which case: edit the row to match), or someone is MitMing your reconnect. Treat this as suspicious and verify out-of-band. |

## Rotating the pinned key

TOFU only writes once per row. Once the fingerprint is set:

- Rotate by editing the `sshHostKeyFingerprint` column directly. The
  verifier selection picks the new value on the next backend rebuild
  (which happens when the manager restarts or the pool's
  `remove(config_id)` is called).
- Setting the column back to NULL re-arms TOFU for that row — but the
  manager warns this is dangerous and the operator note above
  applies.

## Disabling TOFU after the fact

Unset `TS_SSH_TOFU` (or set it to `0` / `false`) and restart the
manager. Rows that were captured under TOFU keep their pinned
fingerprints — the disable only prevents future TOFU captures. To
also clear a captured pin, edit the row to set `sshHostKeyFingerprint`
back to NULL; the verifier will fall through to `Reject` (no TOFU
sink available) and the bridge will refuse to connect until the
operator pins manually.

## See also

- [ADR-0002 — SSHBridge host-key verification policy](adr/0002-host-key-verifier.md),
  including the PURA-100 amendment.
- `crates/ts6-manager-server/src/sshbridge/tofu.rs` — capture worker
  implementation.
- `crates/ts6-manager-server/src/sshbridge/hostkey.rs` —
  `HostKeyPolicy::TrustOnFirstUse` selection + verify.
