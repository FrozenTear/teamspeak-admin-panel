# TS6 Manager — Phase 1 UX Spec

Surfaces in scope: **Login**, **Setup Wizard**, **Dashboard**.
Implements spec Chapters 28, 29, 30 + dashboard data shape from §7.19.1.

Each surface specifies anatomy, states (happy / empty / loading / error), responsive behavior at desktop (1440×900) and mobile (390×844), and acceptance criteria. Screenshots in `preview/screenshots/`.

---

## 1. Login (`/login`)

### 1.1 Why this surface matters

First impression. Operator has just spun up a self-host (or been handed a URL). The screen must read as "trustworthy admin tool" in two seconds, even before any text is read.

### 1.2 Layout — desktop (1440×900)

Two-pane: left brand pane (40% width, max 560px), right form pane.

```
┌──────────────────────────────────┬─────────────────────────────┐
│                                  │                             │
│   ◉ TS6 Manager                  │     Sign in                 │
│                                  │                             │
│   Operate your TeamSpeak 6       │   Username                  │
│   community servers from one     │   ┌─────────────────────┐  │
│   panel.                         │   │                     │  │
│                                  │   └─────────────────────┘  │
│                                  │                             │
│   • Channels, clients, perms     │   Password                  │
│   • Bot-flow automation          │   ┌─────────────────────┐  │
│   • Public server widgets        │   │ ●●●●●●●●●        ⊘  │  │
│   • Music & video bots           │   └─────────────────────┘  │
│                                  │                             │
│   Open-source · Self-hosted      │   [✓] Stay signed in        │
│                                  │                             │
│                                  │           [   Sign in   ]   │
│                                  │                             │
│                                  │   First time here? ─────    │
│                                  │   Run the setup wizard.     │
│                                  │                             │
└──────────────────────────────────┴─────────────────────────────┘
   bg: --bg-canvas with subtle      bg: --bg-surface
   gradient + accent glow           form max-width 380px, centered
```

- Left pane background: `--bg-canvas` with a radial gradient sourced from top-left (accent glow at ~6% opacity). A faint silhouetted channel-tree SVG sits behind the bullets at ~3% opacity — visual texture without information.
- Right pane: `--bg-surface`, form vertically centered.
- Logo: word-mark "TS6 Manager" with an "◉" mark in `--accent-fg`. The mark is a stylised broadcast dot — reads as "live signal."
- Form spacing: each `Field` separated by `--space-6`; submit button `--space-7` from password.
- Setup-wizard link is a `link` button (always visible). When `GET /api/setup/status` returns `needsSetup: true`, it gets a `--accent-subtle-bg` highlight wrapper and the helper text changes to "**Setup needed** — create the first admin." (See §1.6.)

### 1.3 Layout — mobile (390×844)

Single column. Brand pane collapses to a 96px header strip showing only the logo + tagline. Form pane scrolls.

```
┌──────────────────────────────────────┐
│ ◉ TS6 Manager                        │  header strip (96px)
├──────────────────────────────────────┤
│                                      │
│  Sign in                             │
│                                      │
│  Username                            │
│  ┌──────────────────────────────┐    │
│  │                              │    │
│  └──────────────────────────────┘    │
│                                      │
│  Password                            │
│  ┌──────────────────────────────┐    │
│  │ ●●●●●●●●●                  ⊘ │    │
│  └──────────────────────────────┘    │
│                                      │
│  [✓] Stay signed in                  │
│                                      │
│  ┌──────────────────────────────┐    │
│  │           Sign in            │    │  full-width primary
│  └──────────────────────────────┘    │
│                                      │
│  First time here? Run the setup wizard.
│                                      │
└──────────────────────────────────────┘
```

Form padding: `--space-7` outer, `--space-6` between fields. Submit button is full-width (`width: 100%`) — matches mobile thumb expectation.

### 1.4 States

#### Happy
- Empty inputs, submit disabled until both fields have ≥1 character.
- "Stay signed in" defaults **off**. Reasoning: this is an admin tool; opt-in long-session is safer than opt-out.
- Submit kicks off `POST /api/auth/login`. Submit button shows loading state (label fades, spinner appears, width frozen).
- Success: redirect to `/dashboard` (or the path the user was attempting per Ch. 29.2).

#### Loading
- During the login request, all inputs are `disabled`. ESC does not abort (no abort UX wired for this trivial latency).

#### Error — invalid credentials (HTTP 401)
- Banner above the form (not a toast — this is a form-blocking error):
  - Background: `--c-danger-500/0.12`, border-left `4px --danger-bg`, fg `--danger-fg`.
  - Icon: alert-triangle.
  - Text: "**Invalid credentials.** Check your username and password and try again."
- Inputs do **not** show field-level red. We don't know which field is wrong; saying "username doesn't exist" leaks an enumeration vector.
- Password field is **cleared**, focus returns to password.
- Submit button returns to enabled.

#### Error — rate-limited (HTTP 429)
- Banner: "**Too many attempts.** Wait a moment and try again."
- All inputs disabled until the timeout passes (countdown shown in helper text under the form: "You can try again in 27s"). Counter ticks via `setTimeout`/Dioxus signal.
- Acceptance: spec §6.4 (login rate limit) — actual cooldown duration parsed from the `Retry-After` header if present, falls back to 30s.

#### Error — network down (no response)
- Banner: "**Can't reach the server.** Check your connection or that the back-end is running."
- "Try again" link in helper text retries the last submission with the cached form values (re-enables submit on the existing form).
- We do NOT redirect to a "you are offline" page — the operator is debugging their own deployment; keep them on the form.

#### Empty (gated to setup)
- If `GET /api/setup/status` returns `needsSetup: true` on page load, the login form **stays on screen** but the setup link is promoted (background highlight, "Setup needed — create the first admin"). The login form remains usable in case the operator already created an admin in another tab.

### 1.5 Accessibility

- `<form>` with `aria-labelledby="login-title"`.
- First field receives focus on mount (`autofocus`). Acceptable: this is a single-purpose page.
- Caps Lock detection on password — small inline indicator above the field ("Caps Lock is on") in `--warning-fg`.
- `autocomplete="username"` and `autocomplete="current-password"` for password manager support.

### 1.6 Acceptance criteria

- AC-L1. Form renders in <100ms after JS bundle parses (skeleton not needed — no async data on this surface).
- AC-L2. WCAG AA contrast on every text element verified.
- AC-L3. Tab order: username → password → password reveal → stay-signed-in → submit → setup link.
- AC-L4. Submit fires on Enter from any input.
- AC-L5. Banner errors do NOT shift form layout (reserved-height container).
- AC-L6. Reduced-motion: spinner replaced with "Signing in…" text; no slide animations.

---

## 2. Setup Wizard (`/setup`)

### 2.1 Why this surface matters

This is the **10-minute self-host promise**. A stranger on the internet has spun up the container, hit the URL, and is now staring at this. Every friction point here loses an operator.

### 2.2 Step model

The wizard is **3 steps** + a confirmation screen. Long-form first run.

| # | Step | Purpose | Required? |
|---|---|---|---|
| 1 | **Create admin** | Username, display name, password | Yes |
| 2 | **Add a TeamSpeak server** (optional) | Host, WebQuery port, API key, optional SSH credentials | Skip allowed |
| 3 | **You're ready** | Summary, "Go to dashboard" CTA | n/a |

Steps 1 is mandatory (gated by `POST /api/setup/init`).
Step 2 is **optional** because not every operator has a server ready in the same session — the dashboard will guide them through Add Server later. Surfacing the option here has the highest conversion to "I have a working setup."

### 2.3 Layout — desktop

Centered card, max-width `560px`. Sidebar/header chrome is **not** rendered for the setup route (per spec — `/setup` is unauthenticated and pre-chrome).

```
┌──────────────────────────────────────────────────────────────┐
│                                                              │
│              ◉ TS6 Manager — first-time setup                │
│                                                              │
│       ●━━━━━━━○──────────○                                   │  step indicator
│       Admin    Server     Done                               │
│                                                              │
│   ┌─────────────────────────────────────────────────────┐   │
│   │ Step 1 — Create admin                               │   │
│   │                                                     │   │
│   │ This account can do anything in the panel.          │   │
│   │ Add additional users from the Settings page later.  │   │
│   │                                                     │   │
│   │ Username  *                                         │   │
│   │ ┌───────────────────────────────────────────────┐   │   │
│   │ │                                               │   │   │
│   │ └───────────────────────────────────────────────┘   │   │
│   │                                                     │   │
│   │ Display name  (optional)                            │   │
│   │ ┌───────────────────────────────────────────────┐   │   │
│   │ │                                               │   │   │
│   │ └───────────────────────────────────────────────┘   │   │
│   │                                                     │   │
│   │ Password  *                                         │   │
│   │ ┌───────────────────────────────────────────────┐   │   │
│   │ │ ●●●●●●●●●●●                              ⊘   │   │   │
│   │ └───────────────────────────────────────────────┘   │   │
│   │                                                     │   │
│   │ ✓ At least 8 characters                             │   │
│   │ ✓ Uppercase letter                                  │   │
│   │ ✓ Lowercase letter                                  │   │
│   │ ○ Digit                                             │   │
│   │ ○ Symbol  (! @ # $ % ^ & * …)                       │   │
│   │                                                     │   │
│   │                          [Cancel]  [Continue →]     │   │
│   └─────────────────────────────────────────────────────┘   │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

Step indicator: three nodes (●/○) connected by a 1px line. Active node is `--accent-bg`, completed nodes are `--success-bg` with a check icon, future nodes are `--c-neutral-300`. Nodes are clickable to revisit a previous step (forward navigation only via Continue).

**Password complexity hint** updates **live** as the user types (per spec §6.2.2 rules):

- Each rule is a row with `○` (unmet) or `✓` (met) icon.
- Met rules: `--success-fg`. Unmet: `--text-muted`.
- The Continue button enables only when **all** rules pass.
- The hint replaces the typical "your password is too weak" wall-of-error: the operator sees exactly what's needed, not a cryptic rejection.

**Lens:** Recognition over recall + Forgiveness — show the rules so the operator doesn't guess.

### 2.4 Step 2 — Add server (optional)

```
┌─────────────────────────────────────────────────────┐
│ Step 2 — Connect a TeamSpeak server  (optional)    │
│                                                     │
│ Skip and add servers later from the dashboard.     │
│                                                     │
│ Display name  *                                    │
│ ┌───────────────────────────────────────────────┐  │
│ │ My Community                                  │  │
│ └───────────────────────────────────────────────┘  │
│                                                     │
│ Host  *                          WebQuery port  *  │
│ ┌──────────────────────────┐  ┌─────────────────┐  │
│ │ ts.example.com           │  │ 10080           │  │
│ └──────────────────────────┘  └─────────────────┘  │
│                                                     │
│ WebQuery API key  *                                 │
│ ┌─────────────────────────────────────────────────┐│
│ │ ●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●●         ⊘    ││
│ └─────────────────────────────────────────────────┘│
│                                                     │
│ Find this in the TS6 server admin → API Keys.       │
│                                                     │
│ ▼ Advanced (SSH event subscription, optional)      │
│                                                     │
│ ┌──────────────────┐                                │
│ │ Test connection  │  ← runs before saving          │
│ └──────────────────┘                                │
│                                                     │
│       [← Back]  [Skip this step]  [Add server →]    │
└─────────────────────────────────────────────────────┘
```

- "Display name" defaults to the host (auto-filled on host blur unless user has typed something).
- Port pre-filled with `10080` (default per spec §3.1). Port input has helper text "TS6 default" below in `--text-muted`.
- API key is a `<PasswordInput>` with reveal toggle. The "find this in TS6 server admin" line is a `--text-muted` helper, not a link (we can't deep-link the operator's TS server).
- **Advanced section** is collapsed by default. Disclosure triangle expands SSH host/port/user/password fields. **Lens:** Progressive Disclosure — most operators don't need event subscription on day one.
- **Test connection** button calls a backend endpoint (TODO: spec a `POST /api/setup/test-server-connection` route — backend ticket needed). On success: green check + "Connected to *<server name>*". On failure: error banner with the raw TS error code + a one-line plain-language paraphrase. **Lens:** Postel's Law — accept input liberally, surface server-side feedback faithfully.
- "Skip this step" button is `ghost` variant — visible but not pulling focus. Skipping advances to Step 3.

### 2.5 Step 3 — Done

```
┌─────────────────────────────────────────────────────┐
│                                                     │
│              ✓ You're all set                       │
│                                                     │
│   Admin account "<username>" created.               │
│   1 server connected: My Community.                 │
│                                                     │
│   What's next:                                      │
│   • Open the dashboard for live server stats        │
│   • Connect a music bot in Music Bots               │
│   • Build your first automation in Bots             │
│   • Generate a public widget in Widgets             │
│                                                     │
│                       [Go to dashboard →]           │
└─────────────────────────────────────────────────────┘
```

- Confirmation copy summarises what was actually done. If Step 2 was skipped, the second line reads "No server connected yet — you can add one from the dashboard."
- "What's next" list uses `--text-secondary` body and `--accent-fg` for the bolded surface names. Click-targets all the way to `dashboard / music-bots / bots / widgets` routes.
- "Go to dashboard" auto-logs the new admin in (server-issued token from `/api/setup/init`'s response, per spec §29.1) and routes to `/dashboard`. **Lens:** Peak-End rule — last impression of setup is a solved-problem confirmation, not a "now log in again" friction.

### 2.6 Layout — mobile

The wizard collapses to single-column with the step indicator above the card. The card width is 100% with `--space-7` outer padding. All buttons full-width, stacked vertically with primary on top:

```
[ Continue → ]   primary, full-width
[ Cancel     ]   secondary, full-width
```

### 2.7 States

#### Happy
- Each step transitions on Continue with a subtle slide-up + fade. `--motion-slow` `--ease-emphasized`. (Reduced-motion: instant swap.)

#### Loading (Step 1 submit / Step 2 submit)
- Continue button shows loading state. All other inputs disabled.

#### Error (Step 1 submit fails)
- HTTP 400 with rule violation: each rule shown above maps from server-returned message (e.g., "Password must contain at least one digit." → flips `Digit` rule to red `✗`). Server is source of truth — never assume the client validated correctly.
- HTTP 403 (`needsSetup` was already false — race with another operator): banner "Setup is already complete. [Sign in →]" routes to `/login`. The wizard is closed.
- HTTP 5xx: banner "Couldn't create admin — server error. Try again." Continue button re-enables.

#### Error (Step 2 — Test connection fails)
- Inline result bar above the actions row, `--danger-fg`:
  - "Couldn't connect to ts.example.com:10080. Check the host, port, and API key, then try again."
  - If the back-end can distinguish error classes, append a one-line cause:
    - "WebQuery rejected the API key (HTTP 401)."
    - "Connection refused — is the server running?"
    - "Hostname not found."
- The Add Server button stays enabled but with a `--warning-fg` outline; clicking it opens a confirm modal: "Save without a successful test? You can test again from the dashboard." Default is to fix; saving past the warning is allowed (user might be testing offline).

#### Empty (Step 3 with no server)
- Confirmation copy reflects no server, with a more prominent "Connect a server" suggestion in "What's next."

### 2.8 Acceptance criteria

- AC-S1. Setup wizard reachable at `/setup` only when `GET /api/setup/status` returns `needsSetup: true`. After admin exists, navigating to `/setup` 302-redirects to `/login` (handled client-side per spec §29.1).
- AC-S2. Password complexity rules display **all five** rules from spec §6.2.2 verbatim.
- AC-S3. Time-to-first-dashboard from a fresh container, with operator providing valid TS6 credentials: **≤ 4 minutes** measured from first page load to first dashboard render. Tracked with timestamps.
- AC-S4. Step 2 is skippable; the wizard completes successfully with admin only.
- AC-S5. Tab order within a step: top-to-bottom field order, then Cancel → Continue.
- AC-S6. Browser back button between steps navigates correctly (router treats steps as sub-routes: `/setup`, `/setup/server`, `/setup/done`).
- AC-S7. Reduced-motion: step transitions are instant; no slide.

### 2.9 Self-host promise — the 10-minute path

| Time | Operator action |
|---|---|
| 0:00 | `podman pod up` |
| 1:00 | Container healthy, browses to `http://localhost:3000` |
| 1:10 | Sees setup wizard, fills in admin (username, password) |
| 2:30 | Submits Step 1, lands on Step 2 |
| 2:40 | Pastes TS6 host + API key from server admin tab |
| 3:30 | Tests connection — green check |
| 3:40 | Saves, lands on Step 3 |
| 4:00 | Clicks "Go to dashboard," sees live channel/client stats |

This budget assumes the operator already has a TS6 server with WebQuery enabled in another tab. It does **not** include time spent finding the API key in the TS6 admin (variable, operator-side).

---

## 3. Dashboard (`/dashboard`)

### 3.1 Why this surface matters

This is the operator's **default landing**. Every login terminates here. It must answer "is the server healthy right now?" in one glance.

### 3.2 Layout — desktop

Authenticated chrome (sidebar + header) wraps the page. Content area uses a 12-column grid at `--bp-xl`+; collapses to 6-col at `--bp-lg`, 1-col at `--bp-sm`.

```
┌── Sidebar (240px) ───┬── Header (56px) ─────────────────────────────────┐
│ ◉ TS6 Manager        │ My Community ▾   ●ws    🌙   robert ▾   [Logout] │
├──────────────────────┼──────────────────────────────────────────────────┤
│                      │                                                   │
│ ─ Server             │  My Community    /dashboard                       │
│   Dashboard          │  ──────────────────────────────────────           │
│   Channels           │                                                   │
│   Clients            │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────┐ │
│   Files              │  │ Online   │ │ Channels │ │ Uptime   │ │Status│ │
│                      │  │  ●  47/512   │  124    │ │ 12d 3h   │ │● Live│ │
│ ─ Moderation         │  └──────────┘ └──────────┘ └──────────┘ └──────┘ │
│   Server groups      │                                                   │
│   Channel groups     │  ┌─────────────────────────┐ ┌──────────────────┐│
│   Permissions        │  │ Bandwidth (last min)    │ │ Connection       ││
│   Bans               │  │   ▁▂▃▅▆▇█▇▅▃▂▁ in       │ │ Ping       42 ms ││
│   Tokens             │  │   ▁▁▂▂▃▃▄▄▅▅▄▃ out      │ │ Packet loss  0%  ││
│   Complaints         │  └─────────────────────────┘ │ Platform  Linux  ││
│   Messages           │                              │ Version  6.1.4   ││
│                      │  ┌─────────────────────────┐ └──────────────────┘│
│ ─ Automation         │  │ Recent activity         │                     │
│   Bots               │  │ • +Alice joined #Lobby  │                     │
│   Music bots         │  │ • -Bob left #Project    │                     │
│   Widgets            │  │ • !welcome triggered    │                     │
│                      │  │ • Music bot started     │                     │
│ ─ Admin              │  └─────────────────────────┘                     │
│   Logs               │                                                   │
│   Instance           │                                                   │
│   Settings           │                                                   │
│                      │                                                   │
│ ─◀ collapse          │                                                   │
└──────────────────────┴───────────────────────────────────────────────────┘
```

### 3.3 KPI strip

Four cards at the top. Each is a `Card` (see `components.md`) with:

- Label: `--text-xs --weight-semibold --text-secondary`, uppercase
- Value: `--text-2xl --weight-bold --text-primary`, `font-variant-numeric: tabular-nums`
- Trailing visual: status dot or sparkline depending on metric

| Card | Source field | Trailing |
|---|---|---|
| Online | `onlineUsers / maxClients` | Status dot (green if onlineUsers > 0; neutral if 0) |
| Channels | `channelCount` | none |
| Uptime | `uptime` (formatted "12d 3h") | none |
| Status | derived: `Live`/`Degraded` from latest WS heartbeat + ping | Status pill |

The Status pill aggregates: `Live` (green) when SSH bridge connected + WebQuery responding; `Degraded` (warning) when one of those is failing; `Offline` (danger) when both fail.

### 3.4 Bandwidth chart

- Sparkline of in/out from `bandwidth.incoming` and `bandwidth.outgoing`. The dashboard endpoint returns a single point; the SPA caches the last 60 samples client-side (one per WS heartbeat or one per minute polled — TBD with backend) and renders both as overlaid lines.
- In: `--accent-fg` line. Out: `--success-fg` line. 1px stroke.
- Y-axis hidden; current value labelled at the line endpoints in `--text-xs`. **Lens:** information density without chart-junk.
- Hover (desktop only) reveals a vertical guide + tooltip with both values at that timestamp.

### 3.5 Connection card

- Static-ish data: `ping`, `packetloss`, `platform`, `version`.
- Two-column grid inside, label left + value right.
- Values use `--font-mono` for ping and packet-loss; `--font-sans` for text values.
- Color rules:
  - `ping > 100ms` → value `--warning-fg`
  - `packetloss > 1%` → value `--warning-fg`
  - `packetloss > 5%` → value `--danger-fg`

### 3.6 Recent activity

A 6-row max feed of WS-pushed events filtered down to high-signal kinds:

- Client join / leave / nickname change
- Bot flow execution (start/finish + name)
- Music bot lifecycle changes
- Connection state changes (degraded/recovered)

Each row: timestamp (relative — "2m ago"), icon, one-line description. Click expands a drawer with full event payload (for debugging).

If no events in the last hour: empty-state inside the card ("Quiet here. Events will show up as clients join, bots run, and channels change.").

### 3.7 Layout — mobile

Stack everything single-column. Sidebar collapses to a hamburger (overlay drawer); header server selector becomes a full-width bar below the header.

```
┌──────────────────────────────────────┐
│ ☰  My Community ▾    ●ws    robert ▾ │
├──────────────────────────────────────┤
│ My Community / Dashboard             │
│                                      │
│ ┌──────────────────────────────────┐ │
│ │ Online            ● 47 / 512     │ │
│ └──────────────────────────────────┘ │
│ ┌──────────────────────────────────┐ │
│ │ Channels                  124    │ │
│ └──────────────────────────────────┘ │
│ ┌──────────────────────────────────┐ │
│ │ Uptime                12d 3h     │ │
│ └──────────────────────────────────┘ │
│ ┌──────────────────────────────────┐ │
│ │ Status            ● Live          │ │
│ └──────────────────────────────────┘ │
│                                      │
│ ┌──────────────────────────────────┐ │
│ │ Bandwidth (last min)             │ │
│ │ ▁▂▃▅▆▇█▇▅▃▂▁ in                  │ │
│ │ ▁▁▂▂▃▃▄▄▅▅▄▃ out                 │ │
│ └──────────────────────────────────┘ │
│ ┌──────────────────────────────────┐ │
│ │ Connection                       │ │
│ │ Ping              42 ms          │ │
│ │ Packet loss        0 %           │ │
│ │ Platform         Linux           │ │
│ │ Version          6.1.4           │ │
│ └──────────────────────────────────┘ │
│ ┌──────────────────────────────────┐ │
│ │ Recent activity                  │ │
│ │ ...                              │ │
│ └──────────────────────────────────┘ │
└──────────────────────────────────────┘
```

### 3.8 States

#### Happy
- All cards rendered, sparkline updating once per second from WS messages.

#### Loading (initial)
- Each KPI value is a 28×72 skeleton bar. Bandwidth sparkline area shows a single low-amplitude shimmer line. Connection-card values: 14-px skeleton bars.
- Page title + breadcrumb render immediately (server name comes from cached selected-server state, not the dashboard fetch).
- Loading state visible for ~200–600ms typical; under 200ms, no skeleton (would flash).

#### Empty (no server selected — viewer with zero access)
- The whole page collapses to an `EmptyState`:
  - Icon: padlock.
  - Title: "No server access yet."
  - Description: "An admin needs to grant you access to a server before you can use the dashboard. Contact your administrator with your username (`<username>`)."
  - No primary action; secondary `link`: "Go to settings".
- Sidebar still rendered (with disabled Server section), header server selector shows "No servers."

#### Empty (admin, zero servers configured)
- Page collapses to:
  - Icon: server-plus.
  - Title: "No TeamSpeak servers connected."
  - Description: "Add your first server connection to start managing channels, clients, and automation."
  - Primary action: `Add server` → opens AddServer modal.
  - Secondary: `link` to setup-wizard re-entry (re-runs Step 2 only).
- Notice: this is the *same empty state* that appears after deleting the last server connection — don't lose them in a dead-end.

#### Error — dashboard fetch failed (HTTP 5xx)
- All cards show `Skeleton` content with a centered banner *across* the cards row:
  - Icon: alert-triangle (`--danger-fg`).
  - Title: "Couldn't load dashboard."
  - Description: error code + one-line cause if known.
  - Primary action: `Retry`.
- Other surfaces (sidebar, header) remain functional.

#### Error — server unreachable (WebQuery 503 / 504)
- KPI cards show "—" placeholders. Status pill flips to `Offline` (red). Bandwidth chart shows "Server unreachable" overlay. Recent activity continues to show locally-cached recent events with a "(stale)" caption.
- Yellow banner across the top: "Couldn't reach My Community. Re-checking every 10 s." with manual `Retry` button.

#### Degraded (SSH bridge down, WebQuery up)
- Yellow status pill in header (`Degraded`).
- Recent activity card shows: "Live events paused. Reconnecting…" with spinner.
- KPIs continue to update from WebQuery polling.

### 3.9 Acceptance criteria

- AC-D1. Initial dashboard load (TTFB → first KPI value rendered) ≤ 800ms on a hot server, ≤ 2.5s on a cold server.
- AC-D2. WS reconnect within 30s of network restoration; status indicator reflects state within 2s.
- AC-D3. KPI cards do not shift layout when values arrive (skeleton width = expected value width range).
- AC-D4. All states (happy / loading / empty-zero-access / empty-zero-servers / error / degraded) renderable in storybook-equivalent for Phase 1 review.
- AC-D5. WCAG AA contrast verified on all text, status indicators non-color-dependent (icons + labels).
- AC-D6. Mobile breakpoint <`--bp-sm`: all cards stack, sidebar drawer overlays content with backdrop dismiss.

---

## 4. Cross-cutting Phase 1 acceptance

- AC-X1. Theme toggle (header) flips `data-theme` attribute and persists to UI-prefs storage; reload preserves the choice (per spec §28.3).
- AC-X2. Logout button (header) clears tokens, closes WS, returns to `/login`.
- AC-X3. Sidebar collapse/expand persists to UI-prefs storage.
- AC-X4. All buttons in scope have a `data-testid` attribute keyed by their semantic role (e.g., `data-testid="login-submit"`) so the future QA agent can write resilient browser tests.
- AC-X5. No surface uses `dangerouslySetInnerHTML` or any equivalent (HTML inserted from non-app source). This is a security-and-CSP precondition for the Ch. 28.5 token-storage strategy.

---

## 5. Open questions / handoff

1. **Backend ticket required:** Spec a `POST /api/setup/test-server-connection` route that accepts the same body as `POST /api/servers` but returns a structured success/failure response without persisting the record. Currently the spec implies the operator must save first to get feedback. Owner: CTO/RustPlatform.
2. **Stay-signed-in semantics:** The spec leaves refresh-token persistence flexible (Ch. 28.3). My recommendation is: when checked, refresh-token cookie sets `Max-Age=7d` (matches `JWT_REFRESH_EXPIRY`); when unchecked, refresh-token is session-scoped only. SecurityEngineer to validate.
3. **Bandwidth history:** The dashboard endpoint returns one snapshot. For sparkline rendering the SPA needs ~60 samples. Decide: (a) WS pushes a `dashboard:tick` every second with bandwidth fields, or (b) SPA polls the dashboard endpoint every second. Backend pref required; (a) is cheaper.
