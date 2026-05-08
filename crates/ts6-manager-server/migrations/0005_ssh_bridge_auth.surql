-- =====================================================================
-- TS6 Manager — D-SSH-AUTH schema bump on server_connection (PURA-77).
--
-- Adds the SSHBridge auth-model fields the parent issue PURA-69 mandated:
-- a per-server controlPath selector ('webquery' default | 'ssh') and a
-- per-server sshAuthMethod selector ('password' default | 'agent' | 'key')
-- plus the option fields each non-default method consumes.
--
-- See study-documents/ts6-manager-impl-deviations.md → D-SSH-AUTH for the
-- full spec/cleanroom audit. Spec §4.2.4 only ships sshUsername+sshPassword;
-- the five fields below are the deviation. None of them are exposed on
-- /api/servers (spec §7.5) until SecurityEngineer signs off on the public
-- surface — `hasSshCredentials: !!sshUsername` stays as the public boolean.
--
-- sshPrivateKey is stored as `enc:<iv-hex>:<tag-hex>:<ct-hex>` ciphertext
-- per spec §6.3.2 — the same envelope used by apiKey and sshPassword. The
-- repo writes ciphertext only; the russh consumer (PURA-69 follow-up A)
-- unseals it inside the SSH transport bring-up path.
--
-- sshKeyAgentSocket is a filesystem path, not a credential — stored
-- plaintext. The agent socket itself is the trust anchor; storing the path
-- in plaintext does not weaken the auth posture.
--
-- sshHostKeyFingerprint feeds the strict-fingerprint host-key verifier
-- (PURA-69 follow-up A); NULL means "accept on first use is allowed".
-- =====================================================================

DEFINE FIELD controlPath           ON server_connection TYPE string DEFAULT 'webquery';
DEFINE FIELD sshAuthMethod         ON server_connection TYPE string DEFAULT 'password';
DEFINE FIELD sshPrivateKey         ON server_connection TYPE option<string>;
DEFINE FIELD sshKeyAgentSocket     ON server_connection TYPE option<string>;
DEFINE FIELD sshHostKeyFingerprint ON server_connection TYPE option<string>;
