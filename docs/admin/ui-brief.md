# Admin management — v1.1 UI brief

- **Status:** draft, pending board ratification ([PURA-228](/PURA/issues/PURA-228)).
- **Companion docs:** [`architecture.md`](./architecture.md), [`http-api.md`](./http-api.md), [`audit-shape.md`](./audit-shape.md).
- **Owners (implementation):** [DioxusLead](/PURA/agents/dioxuslead) for component scaffolding, routing, state; [UXDesigner](/PURA/agents/uxdesigner) for the create-user form layout, role badge iconography, audit-log filter UX, and the destructive-action confirmation copy.
- **Style anchor:** `crates/ts6-manager-server/src/ui/pages/servers_index.rs` + `server_edit.rs` — the existing CRUD table-plus-form pattern. Audit viewer borrows the dense-table style from `crates/ts6-manager-server/src/ui/pages/logs.rs`.

## 1. Goal

A community operator on a fresh deployment can, after running setup:

1. Promote a teammate to admin in under two minutes from "I want to add a co-admin" to "they can sign in".
2. Disable a compromised account immediately and see in the audit log who-did-it-and-when within the same heartbeat.
3. Read the last 24 h of admin activity from a single page without leaving the panel.

That is the v1.1 wedge for the multi-admin gap.

## 2. Route map

| Route                          | Page                   | Notes |
| ------------------------------ | ---------------------- | ----- |
| `/admin`                       | Admin landing          | Tabbed shell: **Users**, **Audit**. Default tab = Users. |
| `/admin/users`                 | User list              | Equivalent to `/admin` Users tab — explicit route for deep-linking. |
| `/admin/users/new`             | Create-user form       | Modal-style page; "Cancel" returns to `/admin/users`. |
| `/admin/users/{id}`            | User detail            | Tabs: **Profile**, **Sessions**. Default tab = Profile. |
| `/admin/users/{id}/edit`       | Edit-user form         | Same fields as `/admin/users/new` plus role/enable controls. |
| `/admin/audit`                 | Audit log viewer       | Filter sidebar + dense table + side-panel for row detail. |

Routing wires into the existing Dioxus router in `crates/ts6-manager-server/src/ui/pages/mod.rs`. Add an "Admin" entry to the nav bar **between Settings and the user-menu dropdown** — admin pages are the operator-management end of the existing settings cluster.

## 3. Page-by-page

### 3.1 `/admin/users` — list

**Empty state.** This only fires after a manual `DELETE` of the bootstrap admin (which the route layer already refuses — so this is a recovery-mode scaffold, not a routine state). Show a destructive card:

> No admin users exist. Run setup to create the first admin, or restore a row directly via the database.

**Populated state.** A dense table:

| Column           | Source                                | Notes                                                |
| ---------------- | ------------------------------------- | ---------------------------------------------------- |
| Username         | `UserSummary.username`                | Plain text, monospace.                               |
| Display name     | `UserSummary.displayName`             |                                                      |
| Role             | `UserSummary.role`                    | Badge with icon — see §4.                            |
| Status           | derived from `enabled` + last-login   | "Active" (green), "Disabled" (grey), "Never signed in" (amber). |
| Last login       | `UserSummary.lastLoginAt`             | Relative time ("3 h ago"); absolute on hover.        |
| Active sessions  | `UserSummary.activeSessionCount`      | Number; click → user detail Sessions tab.            |
| Actions          | —                                     | Inline icons: **edit** (pencil), **disable/enable** (power), **delete** (trash). |

**Toolbar:**

- "**+ New user**" button (top right) — primary action, routes to `/admin/users/new`.
- Filter chips: `Role: All / Admin / Moderator / Viewer`, `Status: All / Active / Disabled`.
- Search input (debounced, 300 ms): `username` or `displayName` substring; passes through to `?q=` query param.

**Row behaviour:**

- Click row body → `/admin/users/{id}`.
- Click action icons → inline confirmation popover, then PATCH/DELETE.
- Disabled rows have reduced opacity but are still clickable (read still works).

### 3.2 `/admin/users/new` — create-user form

**Fields (in display order):**

1. **Username** — text input, `[a-z0-9._-]+`, max 64. Inline validation on blur:
   - too short / empty → "Username is required."
   - invalid chars → "Only lowercase letters, numbers, `.`, `_`, `-`."
   - too long → "Maximum 64 characters."
   - On submit, server may return 409; show "A user with this username already exists." inline.
2. **Display name** — text input, max 128, freeform. Required.
3. **Password** — password input + a "Suggest a strong password" button that generates a 16-char CSPRNG string (browser-side, `crypto.getRandomValues`) into the field and copies to clipboard. Show a small warning beneath the field:

   > Share this password through a secure channel. The operator will not be able to retrieve it later — only reset it.

   Inline validation runs spec §6.2.2 client-side so the operator gets fast feedback; the server re-validates as the authoritative check.
4. **Role** — radio group: `viewer` (default selection, listed first), `moderator`, `admin`. Each option shows a one-line capability summary borrowed from architecture §3.2.

**Form actions:**

- "**Create user**" (primary) → `POST /api/users`. On 201 → toast "User `<username>` created", redirect to `/admin/users/{id}`.
- "Cancel" → `/admin/users`.

**Error handling:**

- 400 (validation) → field-level error message under the offending field (server message verbatim).
- 401 / 403 → redirect to login (`RequireAuth` gate failure).
- 409 → see Username inline behaviour above.
- 500 → toast "Could not create user. Try again." plus the error banner pattern from `server_edit.rs`.

### 3.3 `/admin/users/{id}` — user detail

**Profile tab.** Read-only summary cards:

- Basic info: username, displayName, role badge, status badge.
- Activity: createdAt, updatedAt, lastLoginAt, active session count (click → Sessions tab).
- Inline actions matching the list-row actions: **Edit profile**, **Disable / Enable**, **Reset password**, **Delete**.

All destructive actions go through a modal confirmation. The "Disable", "Reset password", and "Delete" modals show a one-line consequence preview:

| Action          | Modal copy                                                                                                |
| --------------- | --------------------------------------------------------------------------------------------------------- |
| Disable         | "Disabling **`<displayName>`** will sign them out of all sessions immediately."                            |
| Reset password  | "Resetting **`<displayName>`**'s password will sign them out of all sessions and require admin to share the new password." |
| Delete          | "Deleting **`<displayName>`** is permanent. Their sessions and per-server grants will be removed. Audit log entries remain." |

If any of the protections fire (self-action, last-enabled-admin), the modal disables the confirm button and replaces the body with the server's 400 error verbatim — surfacing the rule rather than letting the user attempt a doomed POST.

**Sessions tab.** A table of refresh-token rows:

| Column     | Source                                                | Notes                                                       |
| ---------- | ----------------------------------------------------- | ----------------------------------------------------------- |
| Created    | `SessionSummary.createdAt`                            | Relative + absolute hover.                                  |
| Expires    | `SessionSummary.expiresAt`                            | If past → "Expired" badge.                                  |
| Family     | `SessionSummary.family`                               | First 8 chars + ellipsis; full on hover.                    |
| State      | derived from `replacedBy` + `expiresAt`              | "Active" (green), "Rotated" (grey), "Expired" (amber).      |
| Actions    | —                                                     | Active rows only: **Revoke** button.                        |

**Revoke flow:** click → confirmation popover "Revoke this session and every session in its family?" → `DELETE /api/users/{id}/sessions/{sid}` → toast on success → reload list.

Empty state: "This user has no recorded sessions."

### 3.4 `/admin/users/{id}/edit` — edit-user form

Same shape as `/admin/users/new` except:

- **Username** is read-only (rename is out-of-scope for v1.1; restoring uniqueness across audit-log history is gnarly).
- **Password** field becomes "**Reset password**" — empty by default; only sent on submit if non-empty.
- **Enabled** is a separate toggle pair sibling of the role radio group.
- "**Save changes**" primary action submits a `PATCH /api/users/{id}` with only the changed fields (form-dirty tracking).

If the operator attempts a forbidden self-action (e.g., demoting their own role), the form disables the primary button on field-change and shows an inline banner: "You cannot remove your own admin role. Ask another admin to do this."

### 3.5 `/admin/audit` — audit log viewer

**Layout.** Three columns:

- **Left rail (filters):** `Kind` multi-select, `Actor` dropdown (loaded from `/api/users` list), `Target kind` dropdown (`user`, `session`, `serverConfig`, `*`), `Outcome` (success/failure/any), date range picker (`from` / `to`).
- **Centre (dense table):** time-ordered (newest first), columns Time | Actor | Kind | Target | Outcome.
- **Right panel (detail):** clicked-row expanded view; shows full `payload`, `errorMsg`, `requestIp`, `requestUserAgent`. Closeable.

**Row visual style:**

- `outcome: failure` → red left-border accent (1 px), but row stays in the same scroll order. Failure events are forensic, not toasts.
- `actorUserId: null` (actor was deleted post-event) → actor cell renders as `<deleted user "<snapshot username>">` in italic.
- `targetId: null` (target was deleted post-event) → similar treatment in the target cell.

**Pagination.** Cursor-style scroll-to-load (50 rows per fetch, server-side `offset` increments). "Loading more..." sentinel at the bottom of the table.

**Toolbar:**

- Auto-refresh toggle (off by default) — when on, polls `GET /api/audit?from=<latest-occurredAt-seen>` every 10 s, prepends new rows.
- "**Export...**" button is **out of scope for v1.1** (architecture §2.2). Show it greyed with a tooltip "Coming in v1.2." OR remove for v1.1 and let v1.2 introduce — UXDesigner picks.

**Empty state:** "No admin activity recorded yet. Mutating an admin user from the Users tab will appear here."

## 4. Visual system

### 4.1 Role badges

Per architecture §3.2:

| Role        | Colour token (Tailwind)        | Icon (lucide-dioxus)     | Tooltip copy                                       |
| ----------- | ------------------------------ | ------------------------ | -------------------------------------------------- |
| `admin`     | `bg-red-100 text-red-800`      | `shield-check`           | "Full read/write across the panel and audit log." |
| `moderator` | `bg-amber-100 text-amber-800`  | `shield-half`            | "Read/write on granted servers; cannot manage users." |
| `viewer`    | `bg-slate-100 text-slate-800`  | `eye`                    | "Read-only on granted servers."                    |

The badges follow the existing `server-status` pill conventions (see `crates/ts6-manager-server/src/ui/components/badge.rs` if it exists; otherwise file a new component under `ui/components/role_badge.rs`).

### 4.2 Status badges

| State              | Colour                        | Tooltip                                              |
| ------------------ | ----------------------------- | ---------------------------------------------------- |
| Active             | `bg-emerald-100 text-emerald-800` | "Sign-in enabled."                              |
| Disabled           | `bg-slate-200 text-slate-600`     | "Sign-in disabled; all sessions revoked."        |
| Never signed in    | `bg-amber-100 text-amber-800`     | "User has not signed in since creation."         |

### 4.3 Destructive-action copy register

All destructive flows (disable, reset password, delete, revoke session) use **second-person address** ("Disable Alice", not "Disable user"), name the **specific affected scope** ("sign them out of all sessions", not "this action is irreversible"), and use the consequence as the verb in the confirm button ("Disable and revoke sessions", not "Confirm").

Operators on the [Discord-comparison wedge](../../README.md) are commonly under time pressure — vague modal copy creates compromise hesitation.

## 5. Header gating

The nav bar entry "Admin" is visible only to users with `RequireAuth.role == 'admin'` (read from the existing auth context store). Same logic gates the `/admin/...` routes — non-admins land on a 403 "Insufficient permissions" page (existing pattern from the per-server-access redirect).

The role read on the front end is the JWT's claim, **purely for visual gating**. The DB-current role enforced server-side (spec §6.4.1) remains the authoritative check; a viewer who somehow forges the nav entry hits a 403 at the API.

## 6. Accessibility notes

- All inline action icons MUST have `aria-label`s with the action verb ("Edit user Alice", not just "Edit").
- The audit-log table MUST be keyboard navigable: `↑/↓` row focus, `Enter` to open the detail panel, `Esc` to close it.
- The password-suggest button announces "Strong password copied to clipboard" via `aria-live="polite"`.
- Modal confirmations MUST trap focus, restore focus on close, and respect `prefers-reduced-motion`.
- Colour-only differentiation is forbidden: role and status badges pair colour with an icon and text label (already in §4.1–§4.2).

## 7. Coordination checklist for DioxusLead + UXDesigner

When the ratify gate fires, DioxusLead + UXDesigner pick up the implementation child. To unblock that handoff, this brief commits to:

- [x] Route map locked (§2).
- [x] Wire types stable (`UserSummary`, `UserPatch`, `SessionSummary`, `AuditEvent`, `Page<T>` — see [`http-api.md`](./http-api.md) §2).
- [x] Empty-state copy specified (§3.1, §3.3, §3.5).
- [x] Destructive-action consequence copy specified (§3.3).
- [x] Role / status badge tokens specified (§4).
- [x] Accessibility floor specified (§6).
- [ ] **DioxusLead to decide:** state-store pattern — fresh `users_store` module mirroring `servers_store`, or extend `servers_store` to host both. Recommendation: **fresh module** to keep the lock surface small.
- [ ] **UXDesigner to decide:** the password-strength UI — bar meter + rule checklist, or rule checklist only? FLOW brief has a similar trigger-config decision; same pattern recommended.
- [ ] **UXDesigner to decide:** audit-log filter sidebar — sticky-left-on-desktop, drawer-on-mobile. Mobile audit-log read is a v1.2 nicety; v1.1 can punt to "desktop-first, mobile-acceptable".

The two open decisions are scoped so DioxusLead and UXDesigner can resolve them inside the implementation child without coming back here for a re-ratification.

## 8. Out of scope for v1.1 UI

- Admin self-service profile editing (out — admins use the same edit form as for any user, gated to themselves).
- MFA / 2FA UI (out per architecture §2.2).
- Email-driven password reset UI (out per architecture §2.2).
- Audit log CSV export UI (out per architecture §2.2; greyed in §3.5 toolbar).
- Bulk user actions (out — checkbox row select + "Disable selected" deferred to v1.2).
- Per-server-group permission editor (out per architecture §2.2).
- "Global sessions" cross-user view (out per architecture §2.2).

## 9. References

- [`architecture.md`](./architecture.md) — the surface that drives this UI.
- [`http-api.md`](./http-api.md) — the routes the UI calls.
- [`audit-shape.md`](./audit-shape.md) — the AuditEvent payload shapes the detail panel renders.
- `crates/ts6-manager-server/src/ui/pages/servers_index.rs`, `server_edit.rs` — CRUD table + form anchor.
- `crates/ts6-manager-server/src/ui/pages/logs.rs` — dense-table anchor for the audit viewer.
- [`docs/flows/ui-brief.md`](../flows/ui-brief.md) — sibling v1.1 UI brief, same pattern.
