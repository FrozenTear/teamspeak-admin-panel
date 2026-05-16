# Phase 9.0-spike — TS6 complaint / ban command surface findings

> Workstream `9.0-spike` of [PURA-262](/PURA/issues/PURA-262) Phase 9 Moderation
> ([PURA-283](/PURA/issues/PURA-283)). Blocks `9.0-routes` ([PURA-286](/PURA/issues/PURA-286)).
> Verified 2026-05-16 against a real TS6 host: the local `ts6-fixture`
> (`teamspeaksystems/teamspeak6-server:latest`, `6.0.0-beta9`), `--network=host`,
> WebQuery on `127.0.0.1:10080`. See `docs/ts6-fixture.md`.

## TL;DR

- **Bans** — `banadd` (uid / ip / name), `bandel`, `bandelall`, `banlist` all behave
  as the existing wrappers assume. No wrapper changes needed.
- **`banadd?mytsid=` (myTeamSpeak ident ban) is UNVERIFIED** — the fixture has no
  myTeamSpeak-linked clients and TS6 validates the ident format (rejected a synthetic
  value with `1538 invalid parameter`). This is the middle tier of the plan §4
  identity priority "UID > myTeamSpeak ident > IP".
- **`complainlist`** works as assumed — empty list surfaces as `1281`, same coercion
  as `banlist`.
- **`complaindelall` requires a `tcldbid`** — it is *per-target* ("dismiss all
  complaints about subject X"), **not** a server-wide bulk delete. The plan §8 listed
  it next to `complaindel` in a way that reads as a bulk dismiss; `9.0-routes` must
  scope it correctly.
- **`complaindel` returns `512` for both invalid ids and a non-existent complaint** —
  the two cases are indistinguishable; map `512` → `404`.
- **`complainadd` cannot be exercised from WebQuery at all** (`512 invalid clientID`).
  Recommendation: drop `complainadd` from the 9.0 route surface — a deviation from
  spec §7.15 that needs a board nod.

## Method

WebQuery is path-prefixed `/{sid}/<command>` with `x-api-key`. Each command below
was issued against the live fixture; raw `status` envelopes are quoted. Test bans
were created and then removed with `bandelall`; the fixture is left clean.

## Bans

| Command | Form tested | Result | Verdict |
|---|---|---|---|
| `banadd` | `?uid=…&banreason=…&time=60` | `{banid:1}`, code 0 | ✅ works as assumed |
| `banadd` | `?ip=203.0.113.7&banreason=…&time=60` | `{banid:2}`, code 0 | ✅ works as assumed |
| `banadd` | `?name=spikename&banreason=…&time=60` | `{banid:3}`, code 0 | ✅ works as assumed |
| `banadd` | `?mytsid=<synthetic>&banreason=…` | `1538 invalid parameter` | ⚠️ **unverified** — synthetic ident rejected by format validation; needs a real myTS-linked client |
| `banadd` | `?banreason=…&time=60` (no selector) | `1542 missing required parameter` | ✅ at-least-one-selector rule confirmed; route already pre-checks this |
| `banlist` | `/1/banlist` | rows; empty → `1281` | ✅ `1281` → `[]` coercion correct (already in `banlist` wrapper) |
| `bandel` | `?banid=3` | code 0 | ✅ works |
| `bandel` | `?banid=999` (nonexistent) | `3328 invalid ban id` | ✅ distinct error code — map → `404` |
| `bandelall` | `/1/bandelall` | code 0 (idempotent — ok on empty list) | ✅ works |
| `banclient` | `?clid=1` (no online client) | `512 invalid clientID` | not testable headless — see below |

`banlist` row shape matches the existing `BanEntry` model verbatim (all fields are
JSON strings; `banid`/`created`/`duration`/`enforcements`/`invokercldbid` are
stringy-numeric; `uid` bans populate `lastnickname`).

**`banclient`** bans a *currently-connected* client by `clid`. It is in the spec
§16.4 flow whitelist but is **not** in the existing WebQuery client and is **not
needed by 9.0**: per plan §4, moderation cases key on the durable client **UID**, not
the volatile `clid`. `9.0-routes` should ban via `banadd?uid=` (resolve `clid → uid`
with `clientinfo` first when the operator acts on an online client) so every ban is
UID-keyed. No `banclient` wrapper required.

## Complaints

| Command | Form tested | Result | Verdict |
|---|---|---|---|
| `complainlist` | `/1/complainlist` | empty → `1281` | ✅ works — needs `1281` → `[]` coercion |
| `complainlist` | `?tcldbid=N` | `1281` (no data to filter) | filter is per spec §7.15; **filter-against-populated-list unverified** (no complaints could be created — see below) |
| `complainadd` | `?tcldbid=3&message=…` | `512 invalid clientID` | ❌ **structurally unavailable via WebQuery** |
| `complaindel` | `?tcldbid=3&fcldbid=4` (valid ids, no complaint) | `512 invalid clientID` | maps "no such complaint" → `512` |
| `complaindel` | `?tcldbid=99999&fcldbid=88888` (invalid ids) | `512 invalid clientID` | same code — **invalid-id and no-complaint are indistinguishable** |
| `complaindelall` | `?tcldbid=3` | code 0 (idempotent — ok with no complaints) | ✅ works; per-target |
| `complaindelall` | `?tcldbid=99999` (invalid target) | code 0 | idempotent even for a nonexistent target |
| `complaindelall` | (no `tcldbid`) | `1539 parameter not found` | ✅ **`tcldbid` is required** |

### Why `complainadd` fails — and why it cascades

A complaint is a `(target cldbid, from cldbid)` pair. `complainadd` attributes the
complaint to the **invoking** client. The WebQuery admin identity (`whoami`:
`client_database_id=1`, `virtualserver_id=0`) has **no client-database row on the
virtual server** — `clientdblist` on vserver 1 returns cldbids 2–6, never 1. TS6
therefore has no "from" client to attach the complaint to and returns
`512 invalid clientID`.

Consequence: **no complaint could be created during this spike**, so `complaindel`
against a *real* complaint and `complainlist?tcldbid=` filtering against a populated
list could not be exercised. Both `complaindel` probes returned `512` because the
complaint does not exist; that is the same code TS6 returns for a genuinely invalid
client id.

## Per-virtual-server scoping (board decision 4)

Scoping is **structural and free**. Every ban and complaint command is path-prefixed
`/{sid}/…`; there is no instance-level (`sid=0`) complaint or ban surface. A command
aimed at a nonexistent/offline virtual server (`/2/banlist`, `/2/complainlist`)
returns `7 canceled`. Decision 4 ("per-virtual-server only, no org-wide propagation")
is the TS6 default — `9.0-routes` keys every case/complaint/ban on
`(serverConfigId, virtualServerId)` and there is nothing to actively opt out of.

## Impact on `9.0-routes` design (PURA-286)

1. **New WebQuery wrappers + model.** Add `complainlist` (with `1281` → `[]`
   coercion, like `banlist`), `complaindel`, `complaindelall`, and a `ComplaintEntry`
   model. Existing `banadd`/`bandel`/`bandelall`/`banlist` wrappers need **no change**.
2. **`complaindelall` is per-target.** Surface it as "dismiss all complaints about
   subject X" (`tcldbid` required), not a vserver-wide purge. It is idempotent — a
   dismiss-all action never errors on an already-clean target.
3. **`complaindel` error mapping.** `512` from `complaindel` must map to **`404`**
   (complaint not found). It cannot be distinguished from an invalid client id;
   `404` is the correct browser-facing outcome either way.
4. **Drop `complainadd` from the 9.0 route surface.** Spec §7.15 lists
   `POST /complaints` → `complainadd` (Y+admin passthrough), but it is structurally
   unavailable via WebQuery (`512`). Complaints are user-generated in the TS client;
   the moderation panel **reads and dismisses** them. An operator who wants to record
   a grievance should open a `moderation_case` / `moderation_note` (our own model,
   plan §5), not a TS6 complaint. **This is a deviation from spec §7.15 and needs a
   board decision** — raised on PURA-283 for the board / CTO.
5. **`mytsid` ban form is unverified.** `9.0-routes` may still pass `mytsid` through
   to `banadd` as a best-effort selector, but the plan §4 "UID > myTeamSpeak ident >
   IP" middle tier is **not proven**. `9.0-qa` (PURA-288) must verify it with a real
   myTeamSpeak-linked test client, or the board accepts `mytsid` as best-effort.
6. **Ban by UID, not `clid`.** `9.0-routes` action endpoints resolve an online
   client's `clid` → `uid` (via `clientinfo`) and call `banadd?uid=`. No `banclient`
   wrapper — keeps every ban UID-keyed per plan §4.
7. **Error-translation table additions.** Codes observed that the
   `{code, message}` → HTTP table should cover for the moderation routes:

   | TS code | Message | Suggested HTTP |
   |---|---|---|
   | `7` | canceled (vserver offline / nonexistent) | `502` / `409` |
   | `512` | invalid clientID (also: complaint/`banclient` target not found) | `404` |
   | `1281` | database empty result set | list reads → `[]`; else `404` |
   | `1538` | invalid parameter (e.g. malformed `mytsid`) | `400` |
   | `1539` | parameter not found (missing required arg) | `400` |
   | `1542` | missing required parameter | `400` |
   | `3328` | invalid ban id | `404` |

## Open items handed to other workstreams

- **`9.0-qa` (PURA-288)** — verify `mytsid` ban with a real myTeamSpeak-linked
  client; verify `complaindel` against a genuine complaint and `complainlist?tcldbid=`
  filtering against a populated list (connect two real clients, one complains).
- **Board / CTO** — decision on item 4 (drop `complainadd` from the 9.0 surface,
  deviating from spec §7.15).
