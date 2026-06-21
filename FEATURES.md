# ArcadeLauncher — Platform Features

> This document is **identical in both repos** (`ArcadeLauncher-Client` and
> `ArcadeLauncher-Server`) and describes the whole platform — the native Windows
> launcher, the Rust backend, the social subsystem, and the companion services —
> as a single product. Keep the two copies in sync when editing.

ArcadeLauncher is a private, self-hosted game platform: a native Windows launcher
that unifies local emulators and PC storefronts with a personal, server-hosted
game library you stream and install on demand — plus an account system, a
Steam/Discord-style social layer (friends, DMs, presence, voice), cloud saves,
and a community game-request board.

---

## 1. The Launcher (client)

Native **C++17 / Win32 / Direct2D** application for Windows. No managed runtime,
no Electron — the entire UI is GPU-drawn with Direct2D/DirectWrite, images decode
through WIC, networking is WinHTTP, and archive extraction uses a bundled LZMA
SDK. Everything is statically linked or shipped in the MSI, so there are no
external runtime dependencies.

> **Unified Client (current):** a cross-platform **Tauri v2 (Rust) + React**
> client — `ArcadeLauncher-Unified-Client` — now replaces the separate native
> Windows (C++/Direct2D) and Linux (C++/nanovg) launchers with one codebase
> shipping on both OSes, at full parity with the features below (plus a card
> right-click context menu, Steam-style Validate & Repair, a controller remap
> editor with firmware/BIOS auto-deploy, and a pending-friend-requests tab). Its
> own `FEATURES.md` lives in that repo. The capabilities in §1 describe the
> launcher product regardless of which client renders them.

### 1.1 Unified game library
- A single cover-art grid spanning **three sources** at once:
  - **Local emulator ROMs** (see Platform Support below).
  - **PC storefront installs** auto-discovered from **Steam**, **Epic Games**,
    and **GOG Galaxy**.
  - **Server-backed games** from your private ArcadeLauncher Server catalog,
    downloaded and installed on demand.
- Per-game install states: *missing*, *downloading*, *installed*, *update
  available*. Launch always runs the local installed copy through the correct
  emulator or directly as an exe — nothing streams off a network share at play
  time.
- Cover art and metadata are **IGDB-enriched** with art pulled from SteamGridDB.

### 1.2 Library navigation & presentation
- **Per-platform sidebar tabs** — GameCube and Wii are surfaced as their own
  tabs (both launch through Dolphin), plus N64, SNES, NES, PS1/PS2, Switch
  (Ryujinx), PS3 (RPCS3), Xbox/Xbox 360, and PC repacks.
- **Rich search & filtering** — search by title, genre, platform, release year,
  and developer.
- **Sort options & collections** — multiple sort modes and user-defined
  collections for organizing the grid.
- **Favorites & hidden games** — pin favorites; hide entries you don't want to
  see.
- **Detail panel** — slides in from the right with cover/hero art, screenshots,
  and developer / publisher / franchise metadata.
- **Controller-friendly** — full **gamepad navigation** and a **Big Picture**
  fullscreen mode for couch/arcade-cabinet use.
- **Global hotkey** to summon the launcher from anywhere.

### 1.3 Downloads & installation
- **Manifest-driven installs** — each server game install is described by a
  manifest of files with byte sizes, **SHA-256 hashes**, and download URLs.
- **Resumable downloads** — each file is pulled as a single resumable HTTP
  **byte-range GET**, streamed write-through to a `.part` file, then verified by
  full-file SHA-256 before commit. Path-traversal (`..`) entries are rejected
  before any write.
- **Background worker** — the UI never blocks on downloads; the install button
  queues a job. Games are **not** auto-launched on completion (started manually).
- **Steam-style Downloads view** — a topbar Downloads button with a count badge
  opens a status window with current download speed, disk-write speed, peak, a
  live throughput line graph, and the queue.
- **Download controls** — pause, resume, cancel, and a configurable **bandwidth
  limit**.
- **Periodic re-sync** — the server catalog is re-fetched every ~10 minutes and
  on window focus, preserving local install state.

### 1.4 Emulator management
- **First-launch wizard** + Settings can **auto-download emulators** straight
  from their GitHub Releases (e.g. RPCS3, Gopher64, Mesen2), extracting `.7z`
  in-process via the bundled LZMA SDK (falling back to external `7za.exe`/`7z.exe`
  if present). Assets shipped as bare `.exe` are used directly.
- **Update checker** — background checks of each emulator's latest GitHub release
  tag; Settings shows installed-vs-latest.
- **Custom libraries** — user-defined ROM/repack directories with per-library
  enable toggles (e.g. FitGirl repacks).

### 1.5 Metadata
- **IGDB integration** via Twitch OAuth — fuzzy-matches scanned titles to IGDB
  entries; ambiguous matches resolve through a manual picker dialog.
- **SteamGridDB cover art** downloaded per game.
- **Hero/banner art and screenshots** in the detail panel.
- **Manual edit** of game title and launch path; manual metadata re-pick.

### 1.6 Accounts & sign-in
- Username/password sign-in to the server at startup; a bearer token is issued
  and used for all subsequent API calls and downloads.
- **Account self-service** from inside the launcher: view/update profile, change
  password (off the UI thread, with real WinHTTP error surfacing), and
  enable/disable **TOTP two-factor** (the server returns an `otpauth://` URI the
  client renders as a QR code).
- Server URL is configured at sign-in; schemeless hosts are normalized
  automatically.

### 1.7 Social (in the launcher)
A Steam/Discord-style social panel, all over a single persistent WebSocket
gateway. See §3 for the full subsystem.
- Friends list with **friend requests** (inline Accept/Decline, send-feedback
  toasts).
- **Presence** — online / away / in-game, with the current game surfaced.
- **Direct messages** in a dedicated chat window, with typing indicators and
  history.
- **Voice calls** — real-time voice chat captured/rendered through WASAPI,
  relayed as PCM over the gateway's binary channel.
- **Notifications & toasts** — friend request/accepted/online/in-game, new
  message, voice invite, and system events, with a bell panel + capped history
  and hover-to-pause toasts.
- **Personalization** — favorites, nicknames, recent-interaction sort, and
  per-user sound/presence-alert preferences, all kept client-side and overlaid
  on the server-authoritative friend list.
- **Resilient connection** — automatic reconnect with exponential backoff, an
  application-level heartbeat, and on-resume reconciliation of the friend list
  and open conversations so nothing is missed while offline.

### 1.8 Cloud saves
- **Cloud Saves v1** — per-game save sync between the client and server
  endpoints, so progress follows you across machines.

### 1.9 Launch integration
- **Pre/post-launch hooks** per game (run a command before/after a game runs).
- **Process monitoring** — the launcher minimizes on launch (optional) and
  re-shows itself when the game process exits.
- **Discord Rich Presence** — shows what you're playing.
- **Per-game desktop / Start-menu shortcuts** with icons generated from the
  game's own cover art.
- **Windows Defender exclusion** toggle for PC game folders (elevated, opt-in).

### 1.10 Auto-update
- The launcher **updates itself on launch**: after a release is published it
  pulls and applies the new version with no manual MSI step.
- **Version lockstep** with the server: the client refuses to connect unless the
  server's **major.minor** matches its own (patch floats), preventing
  incompatible client/server pairings.

---

## 2. The Backend (server)

**Rust / axum 0.7 / tokio** service backed by **MariaDB** (`mysql_async`), with
Argon2 password hashing and TOTP. Serves the catalog API, the file downloads, the
social gateway, account management, and an HTML admin UI.

### 2.1 Catalog & content
- **Native filesystem scanner** catalogs known emulator ROM formats, Xbox 360
  GOD directories, and PC archive installs from a library root, then syncs them
  into MariaDB.
- Catalog content paths are stored **relative** to the library root (never
  absolute NAS paths). Stable game IDs are `<platform>-<sha1_short(relpath)>`.
- **Auto-rescan** on a background interval (default 30 min; configurable,
  `0` disables) that no-ops while a scan is running, so moved/renamed folders
  self-heal and stale rows can't linger. Manual rescans can be kicked from the
  admin UI with live per-platform hashing progress; a filesystem watcher also
  triggers syncs.
- **Per-game manifests** with relative path, byte size, SHA-256 hash, download
  URL, and the launch-target relative path.

### 2.2 File serving
- `GET /files/{id}/{relative-path}` streams game files with **HTTP byte-range**
  support for resumable launcher downloads.
- Missing backing files return a **404** with an admin-facing "rescan" message
  (not a 500), and a per-chunk fallback path exists for manifest files without a
  direct URL.

### 2.3 API surface
- `GET /api/health` — status + live server `VERSION`.
- `GET /api/catalog` — catalog entries (no per-file hashes).
- `GET /api/games/{id}/manifest` — full file manifest for one game.
- `GET /api/emulators` — flat list of server-known emulator runtimes and their
  firmware/BIOS blobs (with `kind`), so a client can auto-deploy required firmware
  (e.g. PS2 BIOS → PCSX2) without manual setup.
- `GET /files/{id}/{relative-path}` — ranged file download.
- `GET /api/saves/{id}` and `GET/PUT /api/saves/{id}/file?path=&mtime=` — per-game
  **cloud-save** listing and per-file get/put (traversal-guarded, 50 MB/file cap).
- `GET /api/social/turn` — short-lived **TURN credentials** (HMAC of the coturn
  shared secret + TTL) for the unified client's WebRTC voice.
- `GET/POST /api/account`, `/api/account/password`,
  `/api/account/totp/{setup,enable,disable}` — launcher account self-service.
- `/api/social/*` + `/ws/social` — the social subsystem (see §3).

### 2.4 Accounts, auth & admin
- Username/password login for both admin and launcher clients; **Argon2** hashing
  and **TOTP** two-factor.
- **Bearer-token auth** — clients send `Authorization: Bearer <token>`; tokens
  can be a global `ARCADE_AUTH_TOKEN` or named per-user tokens minted in the
  admin UI.
- **HTML admin UI** (`/admin`) can:
  - create, rotate, and delete named launcher/user bearer tokens;
  - show library root, catalog paths, and per-platform game counts;
  - trigger an async catalog rescan with live per-platform hashing status;
  - sign in with username/email + password;
  - issue password-reset links by email;
  - force a user to change their password on next login.
- **Account management page** (`/admin/accounts`) — full account administration on
  its own page (off the main dashboard): full-text-filterable user cards (edit
  email/role/status, reset password, toggle 2FA, force password change, delete),
  create-user form, issued-token table, and a **Pending Account Requests** table
  that lets an admin **Approve** (creates a standard non-admin account) or **Deny**
  each outstanding self-service signup — the in-app counterpart to the emailed
  Accept/Deny links.
- **Game-request triage page** (`/admin/requests`) — set each community request's
  status (pending / approved / fulfilled / declined) or delete it. This replaces
  the inline admin status dropdown that used to live on the client Requests board.
- **New-signup notifications** email **every enabled admin** (each admin's own
  address) with HTML **Accept / Deny** buttons.
- **First-boot bootstrap** of an admin account from environment variables when no
  admin exists.
- **Password-reset email** via optional SMTP settings; if SMTP is unconfigured, a
  temporary reset URL is shown for LAN/recovery use.

### 2.5 Versioning & releases
- A single `VERSION` file is the source of truth, reported through `/api/health`.
- Pushing to `main` triggers GitHub Actions, which auto-bumps the version (patch
  by default; `[minor]`/`[major]` keywords), tags `server-vX.Y.Z`, and publishes
  a release. `[minor]` is the rule for any change that breaks client↔server
  compatibility (API shape, auth flow, manifest/catalog format).

---

## 3. Social subsystem (client + server)

A persistent, full-duplex social layer shared between the launcher's
`src/Social/` module and the server's `social_api.rs`.

### 3.1 Transport — the `/ws/social` gateway
- One **persistent WebSocket per signed-in client**, authenticated by `?token=`
  query param (or `Authorization: Bearer`) **before** the upgrade.
- **Text channel** carries JSON control frames: presence diffs, chat delivery,
  typing indicators, and friend request/accept events. On connect the server
  sends a `hello{selfId}` frame.
- **Binary channel** carries voice PCM, kept on a separate path from control/chat
  JSON.
- The client side uses the WinHTTP WebSocket API on its own worker thread; the
  server uses axum's native WebSocket.

### 3.2 Features over the gateway
- **Friends & requests** — send/accept/decline friend requests, block, and a REST
  friend list + DM history (`/api/social/*`) used for initial load and reconnect
  reconciliation. `POST /api/social/friends/respond` carries `{userId, action}`
  where `action` ∈ `accept | decline | cancel | remove | ignore` (`ignore`
  silently drops an incoming request without notifying the sender); clients
  surface this as a pending-requests tab.
- **Presence** — online / away / in-game broadcast as diffs.
- **Direct messages** — delivered live, persisted as history server-side. The
  unified client adds **edit/delete, emoji reactions, replies, and file
  attachments** (presigned `POST /api/social/attachments/presign` → object-store
  PUT → `GET /api/social/attachments/{id}`), plus **profiles** (banner/bio/level-XP),
  **privacy** policies, and a per-user **ignore** list.
- **Voice** — two relay models. The **native** launcher relays raw **PCM over the
  binary channel**. The **unified client** instead does **peer-to-peer WebRTC**,
  using the gateway only to relay `voice_signal` offer/answer/ICE; media flows P2P
  via **STUN + a deployed coturn TURN server**, with TTL-limited TURN credentials
  served from `GET /api/social/turn`. (The two voice models don't interop — fine
  post-cutover since the unified client is now the sole shipping client.)

### 3.3 Keepalive & resilience (the hard-won bits)
- **Server-side control Ping every 25s** keeps proxies/timeouts happy.
- **Application-level heartbeat** — because WinHTTP answers WS *control* pings
  internally and never wakes a blocked receive, the client also sends an
  application `{"type":"ping"}` every 20s and the server replies with a
  **data-frame** `{"type":"pong"}` that actually wakes the receive loop. The
  client's WS receive timeout is 45s. This pairing is load-bearing — without it
  an idle connection silently dies and reconnect-loops.
- **Reconnect with backoff** (1s → cap 30s) and post-reconnect reconciliation of
  friends + open conversations.
- **Scheme handling** — the client maps `wss://`→`https://` (and `ws://`→`http://`)
  before parsing the URL, and normalizes schemeless base URLs, so the upgrade
  request is formed correctly. (These were the root causes of the long-standing
  "Reconnecting…" bug, now fixed.)

### 3.4 Infrastructure note
- Behind nginx, the upgrade headers (`proxy_http_version 1.1`, `Upgrade`,
  `Connection "upgrade"`, long read/send timeouts) are scoped to a dedicated
  `location /ws/` block — never applied globally, so long multi-file download
  sequences keep their HTTP keep-alive.

---

## 4. Companion service — Game Requests

A separate binary, **ArcadeLauncher-Requests**, runs alongside the server and
provides a community request board:
- Logged-in launcher users **request game releases** to be added to the catalog,
  **search IGDB** to pick the exact release, and **upvote** each other's
  requests.
- Admins triage the board: **approve / fulfill / decline** — now from the main
  server admin UI (`/admin/requests`), which updates the shared `game_requests`
  table directly (the client board no longer carries an inline admin dropdown).
- Intentionally decoupled: it shares the same MariaDB and launcher accounts
  (authenticating against the server's `admin_users` table and reading IGDB
  credentials from `server_settings` — no duplicated secrets), owns only its own
  three tables, runs on its own port (`8723`) with its own session cookie, and
  emails the admin on each brand-new request.

---

## 5. Deployment & operations

- **Server** runs as a systemd service in a Proxmox CT, port `8721`, with the
  game library mounted at the library root. Deploy artifacts live in `deploy/`
  (`install-linux.sh`, service + env templates).
- **Reverse proxy** — nginx fronts the server at a public domain, terminating TLS
  and routing both the HTTP API and the `/ws/social` gateway to the upstream.
- **TURN/voice** — a **coturn** server (Docker, host networking) provides WebRTC
  TURN relay for symmetric-NAT / off-LAN voice; the app server mints time-limited
  credentials against coturn's shared secret via `GET /api/social/turn`. TURN media
  is its own UDP/TCP service (router-forwarded), not proxied through nginx.
- **Clients** connect to the public URL (works on-LAN and remotely); the raw LAN
  IP only works inside the home network.
- Both repos auto-version and publish GitHub releases on push to `main`; the
  client installer is downloaded/applied by the launcher's self-update. The
  **server and unified client share a version line** (currently **0.10.0**) so the
  `major.minor` lockstep stays satisfied — coordinated `x.x.0` bumps go to both.

---

## 6. Tech stack at a glance

| Layer | Tech |
|-------|------|
| Launcher UI | C++17, Win32, Direct2D, DirectWrite, WIC |
| Launcher networking | WinHTTP (REST + WebSocket), bearer tokens |
| Launcher voice | WASAPI capture/render, PCM over WS binary frames |
| Launcher packaging | WiX v4 MSI, GitHub Actions, self-update |
| Backend | Rust, axum 0.7, tokio |
| Database | MariaDB via `mysql_async` |
| Auth | Argon2 passwords, TOTP 2FA, bearer tokens |
| Metadata | IGDB (Twitch OAuth) + SteamGridDB |
| Proxy / hosting | nginx + TLS, Proxmox CT, systemd |
| Requests service | Separate Rust binary, shared DB |
