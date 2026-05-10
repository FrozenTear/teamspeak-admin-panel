# `tsdeclarations` warm-up patches — VoiceEngineer / WS-7

PURA-120 deliverable. Documents the three known schema gaps in our
`tsclientlib` master pin (`04aa24917abbf6a0c8442a79742d6d2d40ecf71e`,
which embeds `tsdeclarations` submodule `83cb8a9`), prepares the exact
upstream-PR diff text, and files an upstream-candidate verdict per
field under the round-trip rule.

The patches are **prepared, not landed**. Per the
no-upstream-PR-without-board-ack constraint on
[PURA-117](/PURA/issues/PURA-117), the actual file-upstream step
requires explicit CEO ack on the PURA-117 thread before any PR is
filed against `ReSpeak/tsdeclarations`.

## Why no live overlay landed

Acceptance for PURA-120 allows a clean negative outcome
("close as `done` with a one-line 'no schema gaps surfaced' comment").
We are landing a softer negative: gaps **are** surfaced (three of
them, observable on the wire today against the
`teamspeaksystems/teamspeak6-server` fixture), but none of them blocks
the music-bot work plan WS-1 → WS-6. The current symptoms are
`tracing::warn!(command, argument, "Unknown argument")` log lines emitted
once per connect by `ts-bookkeeping`'s parser (built from
`build/MessageDeclarations.tt`) — cosmetic, not functional. See
`crates/ts6-manager-server/tests/README.md` § "Known nuisances" for
the live observation note added back in
[PURA-106](/PURA/issues/PURA-106).

A vendored-overlay overlay would either require:

1. forking `ReSpeak/tsclientlib` so we can re-pin the embedded
   `tsdeclarations` submodule, **or**
2. vendoring the entire `utils/tsproto-structs/declarations/` tree
   into our repo and pointing tsclientlib at the vendored copy via
   `[patch]` mechanics that don't naturally reach a transitive git
   submodule.

Both are heavier than warranted while none of WS-1 → WS-6 has a hard
build/runtime block on the missing fields. We park the patches as
prepared diffs here and revisit at the moment WS-1 (lifecycle / bot
identity flags) actually wants `client_is_streaming` set on its own
clientupdate frame, which is the first plausible trigger.

## Field 1 — `virtualserver_address`

### What it is

A string published by the TS6 server inside the `initserver` notify on
connect. Holds the public address (host:port) the server believes it
is reachable at — distinct from the per-listener `virtualserver_ip`
array (which is already declared as `Ips` on the `Server` book
struct). On a self-hosted `teamspeaksystems/teamspeak6-server`
container with `--network=host`, this comes back as the host's
externally-resolvable address rather than `0.0.0.0`.

### Where it appears on the wire

- `initserver` (server → client, post-handshake), field
  `virtualserver_address=<str>`.
- Also echoed inside the `notifyserveredited`/`notifyserverupdated`
  envelopes when an admin changes the listener.

### Why a music-bot might want it

Useful for stream-discovery + ICY metadata roundtrips: if the bot
publishes a `now-playing` payload that includes the canonical server
address rather than whatever the operator typed into config, the
admin-panel can deep-link without an additional reverse-resolve. Not
required for WS-1 → WS-6.

### Patch text (Messages.toml)

```diff
@@ Messages.toml — `[[msg]]` block "InitServer" attributes ... @@
-{ name="InitServer",                notify="initserver",                      attributes=["virtualserver_name", "virtualserver_welcomemessage", "virtualserver_platform", "virtualserver_version", "virtualserver_maxclients", "virtualserver_created", "virtualserver_codec_encryption_mode", "virtualserver_hostmessage", "virtualserver_hostmessage_mode", "virtualserver_default_server_group", "virtualserver_default_channel_group", "virtualserver_id", "virtualserver_ip?", "virtualserver_ask_for_privilegekey", "lt?", ... ] },
+{ name="InitServer",                notify="initserver",                      attributes=["virtualserver_name", "virtualserver_welcomemessage", "virtualserver_platform", "virtualserver_version", "virtualserver_maxclients", "virtualserver_created", "virtualserver_codec_encryption_mode", "virtualserver_hostmessage", "virtualserver_hostmessage_mode", "virtualserver_default_server_group", "virtualserver_default_channel_group", "virtualserver_id", "virtualserver_ip?", "virtualserver_address?", "virtualserver_ask_for_privilegekey", "lt?", ... ] },
```

Plus a `[[field]]` row:

```toml
{ map="virtualserver_address", ts="virtualserver_address", pretty="Address", type="str" },
```

### Patch text (Book.toml — `Server` struct)

```diff
@@ Book.toml — [[struct]] name = "Server" properties ... @@
 { name="Ips", type="IpAddr", mod="array", doc="A list of listen ips, can be empty" },
+{ name="Address", type="str", opt=true, doc="The server's externally-reachable host:port as advertised by the server itself; distinct from `Ips` which is the per-listener bind list." },
 { name="AskForPrivilegekey", type="bool" },
```

### Patch text (MessagesToBook.toml)

No change required. The existing `from = "InitServer"` rule already
auto-maps fields with matching pretty-names from the message into the
`Server` book struct; adding the `Address` property to `Server` plus
the `Address` pretty-name on the message field is sufficient.

### Verdict

**File when board acks** under the round-trip rule. Low-risk, additive,
mirrors `Ips`. Cite `83cb8a9` as the base. Draft PR title:
"Add `virtualserver_address` to `initserver`/`Server` (TS6
public-address advertisement)".

## Field 2 — `virtualserver_version_sign`

### What it is

A signature string that a TS6 server emits in the `initserver`
envelope, parallel to the existing `client_version_sign` field. It is
the server's signed version blob — clients use it (in addition to the
client-side `client_version_sign`) to verify the server is running an
authentic build.

### Where it appears on the wire

- `initserver`, field `virtualserver_version_sign=<base64-str>`.
- Re-emitted on `notifyserveredited` when an admin upgrades the build.

### Why a music-bot might want it

Not strictly. Useful for diagnostics in the admin panel (showing the
server's signed version) but no WS-1 → WS-6 deliverable depends on it.

### Patch text (Messages.toml)

```diff
@@ Messages.toml — `[[msg]]` block "InitServer" attributes ... @@
+{ name="InitServer",                notify="initserver",                      attributes=[..., "virtualserver_version", ..., "virtualserver_version_sign?", ...] },
```

Plus a `[[field]]` row:

```toml
{ map="virtualserver_version_sign", ts="virtualserver_version_sign", pretty="VirtualServerVersionSign", type="str" },
```

(Pretty-name distinct from existing `VersionSign` to avoid book-side
collision with `OptionalClientData.VersionSign`.)

### Patch text (Book.toml — `Server` struct)

```diff
@@ Book.toml — [[struct]] name = "Server" properties ... @@
 { name="Version", type="str" },
+{ name="VersionSign", type="str", opt=true, doc="Server-side signed version blob (TS6). Distinct from the per-client OptionalClientData.VersionSign." },
```

### Patch text (MessagesToBook.toml)

```diff
@@ MessagesToBook.toml — `[[rule]]` from = "InitServer" properties ... @@
 properties = [
   { from="VirtualServerId", to="Id" },
+  { from="VirtualServerVersionSign", to="VersionSign" },
   # `Uid` is generated from public key
   # ClientName, ClientId, TalkPower, NeededServerqueryViewPower
   { function="SetClientDataFun", tolist=[] },
 ]
```

### Verdict

**File when board acks** under the round-trip rule. Slightly trickier
than field 1 because of the pretty-name collision risk with the
client-side `VersionSign`. Draft PR title: "Add
`virtualserver_version_sign` to `initserver`/`Server` (TS6 server
build signature)".

## Field 3 — `client_is_streaming`

### What it is

A bool flag emitted by TS6 servers parallel to the existing
`client_is_recording`, marking that a client is in
streaming/broadcast mode (typical of music-bot / radio-bridge
clients). Some TS6 client UIs render a distinct icon when the flag is
set; a few admin-panel surfaces filter by it.

### Where it appears on the wire

- `notifycliententerview` (server → client, when a streaming client
  enters a visible channel).
- `notifyclientupdated` (when an existing client toggles streaming
  state, paralleling how `client_is_recording` updates today).
- Sent by the client itself on `clientupdate` to declare/clear its
  own streaming state.

### Why a music-bot might want it

This is the only one of the three with a plausible WS-1 (bot
lifecycle) trigger. When the music bot starts a stream we'd like to
flip `client_is_streaming=1` so the admin panel — and stock TS6
clients in the channel — render the stream icon instead of the
recording icon. Today we'd have to either set `client_is_recording=1`
(wrong semantic, but already declared) or push the field as a raw
ad-hoc command (clunky).

If WS-1 wants the stream icon to render correctly, this is the
patch that earns a live overlay first.

### Patch text (Messages.toml — field row + attribute lists)

```toml
{ map="client_is_streaming", ts="client_is_streaming", pretty="IsStreaming", type="bool" },
```

```diff
@@ ClientEnterView attributes ... @@
-attributes=[..., "client_is_recording", ...]
+attributes=[..., "client_is_recording", "client_is_streaming?", ...]

@@ ClientUpdated attributes ... @@
-attributes=[..., "client_is_recording?", ...]
+attributes=[..., "client_is_recording?", "client_is_streaming?", ...]

@@ ClientUpdate (outbound) attributes ... @@
-attributes=[..., "client_is_recording?", ...]
+attributes=[..., "client_is_recording?", "client_is_streaming?", ...]
```

### Patch text (Book.toml — `Client` struct)

```diff
@@ Book.toml — [[struct]] name = "Client" properties ... @@
 { name="IsRecording", type="bool", doc="Whether the client is recording" },
+{ name="IsStreaming", type="bool", doc="Whether the client is in streaming/broadcast mode (TS6). Parallels IsRecording but with distinct UI semantics." },
```

### Patch text (MessagesToBook.toml)

No new rule needed. The existing `ClientEnterView → Client (add)` and
`ClientUpdated → Client (update)` rules auto-map identically-named
attributes.

### Verdict

**File when board acks** under the round-trip rule. **Highest WS-1
relevance**: if/when WS-1 lifecycle wants the stream icon set
correctly we should bring the live overlay forward as the first patch
to land downstream. Until then, set `client_is_recording=1` as the
WS-1-compatible fallback — the existing schema declares it and TS6
clients tolerate the mismatched icon.

Draft PR title: "Add `client_is_streaming` to client view / update
messages and `Client.IsStreaming` (TS6 streaming-client flag)".

## Round-trip rule reminder

Per the role contract on [PURA-117](/PURA/issues/PURA-117):

> No upstream PR / FR / bug filings without explicit board ack on
> the PURA-117 thread.

Sequence for any of the three patches above:

1. (this document) Internal documentation + diff text — done.
2. Draft external PR text on the PURA-117 thread — pending,
   on-demand when a WS surfaces a hard need or when the board signals
   readiness to seed the upstream-maintainer relationship.
3. Wait for CEO ack on the exact post text.
4. File the PR against `ReSpeak/tsdeclarations` (and the matching
   submodule bump on `ReSpeak/tsclientlib` once tsdeclarations
   merges).

## References

- Submodule: `ReSpeak/tsdeclarations` @ `83cb8a9` (current pin),
  upstream HEAD `fb4e50d` adds no schema fields relevant to these
  three (only new client version rows + a few badge entries).
- Embedding crate: `tsclientlib/utils/tsproto-structs/declarations/`
  via `.gitmodules`.
- Generator: `tsclientlib/utils/ts-bookkeeping/build/MessageDeclarations.tt`
  is the `.tt` template that emits the `Unknown argument` warn line
  when a wire attribute does not match any declared attribute.
- Live observation note: `crates/ts6-manager-server/tests/README.md`
  § 5 "Known nuisances".
- Master-pin policy: see auto-memory note
  `project_tsclientlib_master_pin_for_ts6.md` and the
  `tsclientlib/Cargo.toml` rev pin in `crates/ts6-voice-prototype/`
  and `crates/ts6-voice-fixture/`.
