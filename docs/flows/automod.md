# Automod — operator guide (Phase 9.1)

Automod lets the server moderate itself: a [Flow v2 graph](v2/architecture.md)
watches a TS6 event, decides whether it crosses a line, and applies a
moderation effect — all audited through the Phase 9.0 case model.

This guide is for operators running a TS6 6 Manager panel. It covers what an
automod rule is, the four starter rules shipped under
[`automod-seeds/`](automod-seeds/), how to promote a rule from `shadow` to
`enforce`, the kill switch, and the one caveat that bites people: `mute`.

> **The single most important fact:** every new automod rule starts in
> **shadow mode**. A shadow rule records what it *would* have done but
> applies no effect to TS6. Nothing actions a real user until you
> explicitly promote a rule. Read [§3](#3-shadowenforce-promotion) before
> you promote anything.

## 1. How an automod rule works

An automod rule is an ordinary Flow v2 graph with one extra node kind — the
`Moderate` action. The shape is always:

```
trigger  ─▶  [ branch ]  ─▶  Moderate(effect)
```

- **Trigger** — the TS6 event that starts the rule. Automod uses four:
  - `ts6ClientJoined` — a client joins the server.
  - `ts6ChatMessage` — a client sends a chat message.
  - `ts6Flood{source: clientJoined}` — repeated reconnects in a window.
  - `ts6Flood{source: clientMoved}` — repeated channel switches in a window.
- **Branch** *(optional)* — a boolean test on the trigger document. Used by
  the name and chat-filter rules to check the nickname / message against a
  block-list. A flood rule needs no branch: the windowed counter *is* the
  condition. A branch routes a non-matching event to its unwired `default`
  port, which drops the path — no effect, no case.
- **`Moderate(effect)`** — applies one of four effects to the subject
  resolved from the trigger:
  - `warn` — a private message to the client.
  - `mute` — revokes the client's talker flag (see [§5](#5-the-mute-caveat)).
  - `kick` — `clientkick`.
  - `ban` — a **temporary** `banadd`. Automod may not issue permanent bans
    (a permanent ban is an operator-only action); a `ban` rule must set
    `durationSecs`.

Every `Moderate` node carries a stable **`ruleKey`**. The rule key drives
case dedup (one open automod case per subject per rule), per-rule metrics,
and the shadow/enforce mode. Keep it stable — renaming it resets a rule's
history and breaker state.

When a `Moderate` node fires it opens (or reuses) a `moderation_case` for
the subject and appends a timeline action and an audit-log row — whether it
ran in shadow or enforce. In shadow mode the case is the *only* output: no
TS6 command is sent.

## 2. The four starter rules

Importable graph blobs live in [`automod-seeds/`](automod-seeds/). All four
are authored in **shadow mode** (by virtue of being unpromoted — see §3).

| File | Rule key | Trigger | Effect | What it catches |
|---|---|---|---|---|
| [`bad-name-kick.json`](automod-seeds/bad-name-kick.json) | `automod-bad-name-kick` | `ts6ClientJoined` | `kick` | Display names advertising other servers / impersonating staff |
| [`chat-filter-warn.json`](automod-seeds/chat-filter-warn.json) | `automod-chat-filter-warn` | `ts6ChatMessage` | `warn` | Messages containing blocked phrases (spam / scam) |
| [`connect-flood-ban.json`](automod-seeds/connect-flood-ban.json) | `automod-connect-flood-ban` | `ts6Flood{clientJoined}` | `ban` (1h) | A client reconnecting 5× in 30s |
| [`channel-hop-mute.json`](automod-seeds/channel-hop-mute.json) | `automod-channel-hop-mute` | `ts6Flood{clientMoved}` | `mute` | A client switching channel 6× in 15s |

The block-lists in the name and chat-filter rules are deliberately small
placeholders. **Edit the branch `when` expressions** to match your
community before you rely on them — the expression dialect (`contains`,
`lower`, `matches_glob`, `and`/`or`) is documented in
[v2/architecture.md §7](v2/architecture.md).

### Importing a starter rule

Create the flow with the panel API — paste the seed file as the request
body's `graph` field. Substitute your `serverConfigId` / `virtualServerId`:

```bash
curl -X POST https://your-panel/api/flows \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
        "name": "Automod — bad-name kick",
        "serverConfigId": 1,
        "virtualServerId": 1,
        "enabled": true,
        "graph": '"$(jq .graph docs/flows/automod-seeds/bad-name-kick.json)"'
      }'
```

`enabled: true` registers the trigger so the rule observes events. Because
the rule is unpromoted it still runs in shadow mode — enabling a flow and
enforcing a rule are two separate switches.

## 3. Shadow/enforce promotion

A rule is either `shadow` or `enforce`. **Unknown / unpromoted rules are
`shadow`** — that is the default and cannot be changed by accident.

1. **Run it in shadow first.** Leave the rule shadowed for long enough to
   build a sample. Each trigger still opens a case, so you can review the
   would-have-done stream.
2. **Read the metrics.** `GET /api/moderation/automod/metrics` returns
   per-`ruleKey` aggregates — shadow hits, enforced actions, false-positive
   count. Use it to decide whether a rule is accurate enough to enforce.
   The automod review surface (Phase 9.1.4) lists the shadow cases and lets
   you flag false positives.
3. **Promote.** A rule is promoted to `enforce` via the
   `AUTOMOD_ENFORCE_RULES` boot allowlist — a comma-separated list of rule
   keys set in the panel server's environment:

   ```
   AUTOMOD_ENFORCE_RULES=automod-chat-filter-warn,automod-connect-flood-ban
   ```

   Every rule key *not* in the list stays `shadow`. Restart the panel
   server to apply.

Promote one rule at a time, and prefer to promote the least severe rules
first (`warn` before `kick` before `ban`).

### The safeguards (why a misfiring enforce rule cannot run away)

Even an enforced rule is bounded by the Phase 9.1.3 false-positive
safeguards:

- **Per-rule circuit breaker.** If an enforced rule actions more than
  `AUTOMOD_BREAKER_MAX_ACTIONS` subjects within `AUTOMOD_BREAKER_WINDOW`
  (default 10 / 60s), the rule **auto-demotes itself to shadow** and raises
  an audit alert. A raid-detection bug cannot mass-ban.
- **Subject cooldown.** After actioning a subject, further automod effects
  on that subject are suppressed for `AUTOMOD_SUBJECT_COOLDOWN` (default
  300s) — a rejoin folds into the open case instead of re-kicking.
- **Exemptions.** `AUTOMOD_EXEMPT_UIDS` and `AUTOMOD_EXEMPT_SERVER_GROUPS`
  are allowlists checked before any effect — put your staff group here.
- **Severity ceiling.** Automod `ban` is temporary-only; a permanent ban
  from a flow is rejected as a configuration error.

A safeguard that suppresses an effect still records the case, tagged with a
`shadowReason` (`circuitBreakerTripped`, `subjectCooldown`, `subjectExempt`,
…) so the suppression is visible in review.

## 4. The kill switch

`AUTOMOD_KILL_SWITCH` is the global off switch. Set it to `1` / `true` and
**every** automod rule is forced to `shadow` instantly, regardless of
`AUTOMOD_ENFORCE_RULES` — cases are still recorded, but no TS6 effect is
applied.

```
AUTOMOD_KILL_SWITCH=1
```

Use it the moment automod misbehaves: flip the switch, restart, and
investigate with enforcement off. It is faster and broader than editing the
enforce allowlist rule by rule.

## 5. The mute caveat

`mute` does **not** force the TS6 client-side mute flags. Server-side mute
is implemented by revoking the client's **talker flag**
(`client_is_talker = 0`).

The talker flag is **only meaningful in a moderated channel.** In a normal,
unmoderated channel every client may speak regardless of the flag, so an
automod `mute` there has no audible effect — TS6 even answers error `1538`
("not in a moderated channel"). The case and audit row are still recorded,
but the subject keeps talking.

If you intend to rely on the `channel-hop-mute` rule, make the channels it
guards **moderated channels**. Otherwise prefer `kick` or a short `ban`.

## See also

- [v2/architecture.md](v2/architecture.md) — the Flow v2 graph engine, node
  kinds, and the expression dialect.
- [v2/http-api.md](v2/http-api.md) — the `POST /api/flows` / `validate`
  surface used to import a rule.
- [../admin/moderation-data.md](../admin/moderation-data.md) — the
  `moderation_case` model an automod rule writes into.
