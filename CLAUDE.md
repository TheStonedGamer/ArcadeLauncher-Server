# ArcadeLauncher Server — working notes for Claude

**Rust / axum 0.7 / tokio** backend with a **MariaDB** (`mysql_async`) catalog,
Argon2/TOTP auth, and an admin HTML UI rendered via `format!` (literal braces are
doubled `{{`/`}}`). Read [`README.md`](README.md) for deployment.

### Source layout (split via `include!`)
`src/main.rs` keeps only the top-level `use` block, the shared `const`s, `fn main`,
and `#[cfg(test)] mod tests`, then `include!`s the rest. Every included file lives
in the **crate-root scope** — there are no `mod`/`use`/`pub` boundaries between
them, so an item defined in one file is callable from any other with no qualifier,
exactly as when it was one file. To find code, open the file by topic:
- `models.rs` — structs, impls, constants (`AppState`, `Config`, `Game`, …)
- `db_setup.rs` — `ensure_database` / `ensure_schema` / bootstrap admin
- `auth.rs` — login, challenge/verify, account, TOTP HTTP handlers
- `handlers.rs` — `api_catalog`/`api_manifest`/download + admin POST handlers
- `db.rs` — user/token/session/settings/games DB queries
- `manifest.rs` — manifest build, stored-file hashing, changelogs, `ensure_manifests`
- `files.rs` — content paths, byte-range streaming, dir walks, `safe_join`
- `crypto.rs` — password/TOTP/scrypt/base32, art helpers, small util fns
- `scan_jobs.rs` — filesystem watcher, `spawn_rescan`, `rescan_catalog`, scan-status
- `igdb.rs` — IGDB auth/search/match/enrich
- `scan.rs` — catalog scanners, `game_entry`, `sync_catalog_db`, validation
- `admin_html.rs` — admin/metadata HTML builders + `CSS`
- `social_api.rs` — social subsystem: friends/requests/block REST, DM history, and
  the `/ws/social` WebSocket gateway (presence diffs, chat delivery, typing, voice
  relay). `ws_social` authenticates the `?token=` query param (or `Authorization:
  Bearer`) **before** `ws.on_upgrade`; `social_socket` runs the in/out pump.

When editing, prefer the relevant file; only edits to the `use` block, shared
consts, `main`, or tests touch `main.rs`. The compiled binary is identical to the
former single-file build.

## Build & test
- `cargo build --release` — single binary.
- `cargo test --release` — unit tests live in `#[cfg(test)] mod tests` at the end
  of `main.rs`, covering pure helpers (`parse_range`, `constant_eq`,
  `encode_path`, `clean_igdb_title`, `normalize_title`, `is_pc_primary_archive`,
  `stable_id`, `sha1_short`, `clean_title`, `igdb_platform_ids`).
- Repo: `github.com/TheStonedGamer/ArcadeLauncher-Server`.

## Versioning & releases
- The `VERSION` file is the single source of truth (Cargo.toml's version is NOT
  kept in sync). `/api/health` reports it via `SERVER_VERSION`
  (`include_str!("../VERSION")` in `auth.rs`).
- **Pushing to `main` triggers GitHub Actions**, which auto-bumps `VERSION`
  (patch by default; `[minor]`/`[major]` in the commit subject for bigger bumps —
  **rule: `[minor]` for any change that breaks client↔server compatibility**,
  e.g. API shape, auth flow, or manifest/catalog format; compatible changes stay
  patch),
  tags `server-vX.Y.Z`, and publishes a GitHub release. The bot's bump commit
  means local pushes often need `git pull --rebase origin main` first.
- **Version lockstep**: the client refuses to connect unless **major.minor**
  matches its own (patch floats). After a minor/major bump, deploy the server
  before clients update or they are locked out. Coordinated releases = push
  matching `[minor]`/`[major]` commits to BOTH repos.

## Deployment (production)
- Runs in a Proxmox CT at **`10.0.0.210`** (root login) as systemd service
  `arcadelauncher-server` on port `8721`. Deploy artifacts in `deploy/`.
- Deploy path: scp `src/main.rs` → `cargo build --release` on the CT → install
  binary → `systemctl restart arcadelauncher-server`. **Requires explicit user
  authorization** (production access is classifier-gated).
- Reverse proxy: nginx on **`10.0.0.203`** (login as user `brian`, not root) at
  `arcade.orlandoaio.net` → upstream `10.0.0.210:8721`.

## Important behaviors
- **Game IDs**: `<platform_lower>-<sha1_short(relative_path)>` (`stable_id`).
  Renaming a platform changes IDs; `sync_catalog_db` **prunes orphan rows** not
  present in a fresh scan, so a rescan clears stale entries.
- **Auto-rescan**: a background tokio task runs `spawn_rescan` every
  `ARCADE_AUTO_RESCAN_SECS` (default 1800; `0` disables). It no-ops while a scan
  is already running (the `st.scan` guard), so manual rescans are never
  disturbed. This means moved/renamed folders self-heal within one interval —
  stale catalog rows can't linger indefinitely.
- `rescan_catalog` order: scan → `sync_catalog_db` (prunes) → `ensure_manifests`
  (hashing; per-platform progress reported to the admin scan-status UI; hash
  errors are logged, don't abort) → IGDB enrich.
- PC games are tagged platform `"PC"`; layout is `games/PC/<game folder>` where
  the folder name is the game name. `igdb_platform_ids`: GameCube→[21], Wii→[5].
- `download_file` / `download_chunk` return **404** (not 500) with an
  admin-facing "rescan" message when the backing file is missing on disk.
- **Social gateway** (`/ws/social`, `social_api.rs`): a persistent WebSocket per
  signed-in client. The outbound pump emits a WS **control Ping every 25s**; the
  inbound loop also answers an application `{"type":"ping"}` with a data-frame
  `{"type":"pong"}`. The data-frame pong is load-bearing — the WinHTTP client can't
  see the control Ping, so without the app ping/pong an idle client would time out
  and reconnect-loop. Don't remove the `"ping"` handler.

## nginx (WebSocket-aware — do not strip Upgrade globally)
- History: the proxy once set `Upgrade: $http_upgrade` / `Connection: "upgrade"`
  **unconditionally on all requests**, which broke keep-alive on long multi-file
  download sequences (HTTP 401/501). At that time the app had no WebSockets, so the
  fix was to remove those headers.
- **Now the app DOES use WebSockets** (the `/ws/social` gateway). The current site
  config scopes the upgrade headers to a dedicated `location /ws/` block
  (`proxy_http_version 1.1`, `Upgrade $http_upgrade`, `Connection "upgrade"`,
  `proxy_read_timeout/proxy_send_timeout 3600s`) while `location /` stays plain.
  **Never** move the upgrade headers back to a global scope — keep them inside
  `/ws/` so downloads and the gateway both work.

## Conventions
- Rescans run automatically on an interval (see Auto-rescan above) and can also
  be kicked manually from the admin UI.
- Catalog content paths are stored **relative** to the library root; never store
  absolute NAS/server paths.
- Do not read production `*.env` files or query MariaDB for credentials/tokens
  (classifier-blocked, and correctly so).
