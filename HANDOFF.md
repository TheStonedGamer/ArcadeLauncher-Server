# ArcadeLauncher Server — Session Handoff

_Last updated: 2026-06-21_

**Rust / axum 0.7 / tokio** backend with a **MariaDB** (`mysql_async`) catalog, Argon2/TOTP auth, and an
admin HTML UI rendered via `format!`. Serves the `ArcadeLauncher-Client` Windows launcher. See the
client's `HANDOFF.md` for the full cross-cutting picture; this file is server-/infra-focused.

## Recent changes (2026-06-21, prod 0.10.9)
- **Self-service registration + email** shipped: `registration.rs` (POST `/api/auth/register`,
  GET `/api/auth/approve|deny`) and `password_reset.rs` (forgot/reset). New-signup emails go to **every
  enabled admin** (queried from `admin_users`) as a **multipart HTML message with Accept/Deny buttons**;
  the old single `ARCADE_REGISTRATION_NOTIFY_EMAIL` config was **removed**. SMTP via `lettre` from
  `ARCADE_SMTP_HOST/PORT/USER/PASS/FROM/STARTTLS` (note the names — `USER`/`PASS`/`STARTTLS`, not
  `USERNAME`/`PASSWORD`/`TLS`).
- **New admin pages** in `admin_extra.rs` (included after `admin_html.rs`):
  - `/admin/accounts` — account management moved off the dashboard: user cards, create-user, tokens, and
    a **Pending Account Requests** table (approve → creates non-admin account / deny).
  - `/admin/requests` — game-request triage (set status / delete), writing the shared `game_requests`
    table directly. The client board's inline admin dropdown was removed (client v0.10.15).
  - All three admin pages share `admin_post`; POSTs carry a `return_to` field so the handler re-renders
    the right page. New `AdminForm` fields: `return_to`, `pending_id`, `request_id`, `request_status`.
- **Deploy is now git-pull, not scp:** push → on CT `cd /root/build-arcade && git pull --ff-only` →
  `cargo build --release` → install → restart. `/root/build-arcade` is a git clone of the (public) repo.
  Pull picks up the CI `VERSION` bump automatically. **Startup binds ~25s after restart** (schema + fs
  watcher) — poll `/api/health` for ~30s; don't mistake the gap for a failure.
- **Companion Requests service 502 fix** (separate repo `ArcadeLauncher-Requests`): board load panicked
  reading a NULL `AVG(stars)` into `f64`; now read via `Option<f64>` + `CAST(... AS DOUBLE)`. That repo is
  **private** and the CT has no GitHub creds, so it still deploys via **scp** (no git-pull there yet).

## Repos / hosts
- Server: `C:\Users\BrianTheMint\source\repos\ArcadeLauncher-Server` — `github.com/TheStonedGamer/ArcadeLauncher-Server`
- Client: `C:\Users\BrianTheMint\source\repos\ArcadeLauncher-Client` — `github.com/TheStonedGamer/ArcadeLauncher-Client`
- **App host:** `10.0.0.210` (root login), Proxmox CT, systemd service `arcadelauncher-server`, port `8721`.
- **Reverse proxy:** nginx on `10.0.0.203` (login as **brian**, not root) → `arcade.orlandoaio.net` → upstream `10.0.0.210:8721`.
- **Public URL:** `https://arcade.orlandoaio.net` (works on-LAN and remotely). LAN IP `10.0.0.210:8721` is internal-only.
- Library / served assets root on the app host: `/srv/arcade-library/` (emulators at `/srv/arcade-library/emulators/`,
  served at `https://arcade.orlandoaio.net/emulators/...`, ranged GET / HTTP 206 supported).

## Source layout (split via `include!`)
`src/main.rs` keeps only the top-level `use` block, shared `const`s, `fn main`, and `#[cfg(test)] mod tests`,
then `include!`s the rest — every included file lives in **crate-root scope** (no `mod`/`use`/`pub` boundaries).
- `models.rs` — structs/impls/consts (`AppState`, `Config`, `Game`, …)
- `db_setup.rs` — `ensure_database` / `ensure_schema` / bootstrap admin
- `auth.rs` — login, challenge/verify, account, TOTP handlers (incl. `POST /api/account/password`)
- `handlers.rs` — catalog/manifest/download + admin POST handlers
- `db.rs` — user/token/session/settings/games queries
- `manifest.rs` — manifest build, stored-file hashing, changelogs, `ensure_manifests`
- `files.rs` — content paths, byte-range streaming, dir walks, `safe_join`
- `crypto.rs` — password/TOTP/scrypt/base32 (`hash_password_argon2` = `Argon2::default()`, fast)
- `scan_jobs.rs` — fs watcher, `spawn_rescan`, `rescan_catalog`, scan-status
- `igdb.rs` — IGDB auth/search/match/enrich
- `scan.rs` — catalog scanners, `game_entry`, `sync_catalog_db`, validation
- `admin_html.rs` — admin/metadata HTML builders + `CSS`

## Work completed 2026-06-12 (pushed to `main`, commit `aa629d1`; CT deploy PENDING)
1. **Discord webhooks (`discord.rs`, new).** New-game catalog notifications fired from
   `rescan_catalog` via `notify_new_game` → `tokio::spawn` + `reqwest` rich embed
   (Title "🚀 New Game Added to ArcadeLauncher!", fields Platform / Game Name / Size in
   human-readable units). Webhook URL stored as `server_settings` key `discord.webhook_url`
   (validated to a real discord.com webhook host). `sync_catalog_db` now returns the
   newly-added games and **suppresses the initial bulk import** (empty catalog) so a fresh
   DB doesn't blast one message per game. Admin config UI gained a webhook field with
   **Save** + **Test Webhook** (`test_webhook` action → `test_discord_webhook`).
2. **IGDB resilience (`igdb.rs`).** `igdb_post_json` wraps all `api.igdb.com` POSTs with
   exponential backoff (1→30s, ±1s jitter) retrying 429 (honoring `Retry-After`) / 5xx /
   transient transport errors; Twitch auth got the same. Strict payload validation
   (`id>0 && !name`) before commit. New `apply_local_metadata_fallback` scrapes a sibling
   `.nfo` (Genre/Description) + local cover when IGDB can't match/errors, so the enrich
   queue never stalls. Deep logging now includes game id, platform, and raw status.
3. **User management / RBAC (`users_api.rs`, new; `db.rs`).** Extends the existing
   token+session auth (no JWT). Role derived from `admin_users.is_admin` (`Admin`/`User`).
   New JSON routes: `POST /api/auth/login`, `POST /api/auth/logout`, `GET`/`POST /api/users`
   (admin), `PUT /api/users/:id` (admin or self; privilege/enable changes admin-only),
   `DELETE /api/users/:id` (admin, refuses last remaining admin). DB helpers `update_user`
   / `delete_user_account`. **No schema migration** — reuses existing tables.
- Build clean; `cargo test --release` = **26 passed** (added `human_size`, `role_str`,
  `igdb_backoff` tests). **Deploy to the CT still pending explicit authorization.**

## Build / test / deploy
- `cargo build --release` (single binary). `cargo test --release` (pure-helper unit tests in `main.rs`).
- **Deploy (prod, authorization-gated):** scp `src/main.rs` → `cargo build --release` on the CT → install
  binary → `systemctl restart arcadelauncher-server`. Artifacts in `deploy/`.
- **No server code changed this session** — work was client-side + an nginx infra fix.

## Infra change deployed this session (nginx on 10.0.0.203)
The arcade site config (`/etc/nginx/sites-available/arcade.orlandoaio.net`) had **regressed** to set
unconditionally:
```nginx
proxy_set_header Upgrade $http_upgrade;
proxy_set_header Connection "upgrade";
```
This is the documented keep-alive / 401-501 gotcha — the app had **no WebSockets** *at that time*. Removed
both lines from the global scope (timestamped `.bak` saved beside the config), `nginx -t` passed (the
duplicate-server-name warnings are pre-existing/benign), `systemctl reload nginx`, verified GET
`/api/catalog` and POST `/api/auth/verify` respond in ~0.1s through the proxy.

> **Update (2026-06-13):** the app **now uses WebSockets** — the `/ws/social` gateway. The site config
> has since gained a dedicated `location /ws/` block that re-adds `Upgrade`/`Connection "upgrade"` +
> `proxy_http_version 1.1` + 3600s timeouts, scoped to `/ws/` only. The regression to watch for is now the
> *opposite*: don't let those upgrade headers leak back into the **global** scope or `location /` (that
> breaks downloads); keep them inside `/ws/`.

Config notes worth knowing: `client_max_body_size 153600m`, `proxy_buffering off`,
`proxy_request_buffering off`, `proxy_read_timeout/send_timeout 3600s`, `proxy_http_version 1.1`.

## Context for the friend's "connection timed out 12002" (resolved client-side)
Diagnosed via nginx logs: a LAN `POST /api/account/password` returned **200**; **zero** remote
ArcadeLauncher requests appeared in the access log. The friend's client was pointed at the **LAN IP**
default (`10.0.0.210:8721`), unreachable remotely. Fixed in the client by defaulting all server-URL
fields to `https://arcade.orlandoaio.net`. **Server/proxy were not the cause** (nginx header removal was a
separate latent-bug cleanup). The friend must log out/in and re-enter the public URL — their saved config
still has the LAN IP.

## Important server behaviors
- **Game IDs:** `<platform_lower>-<sha1_short(relative_path)>` (`stable_id`). Renaming a platform changes
  IDs; `sync_catalog_db` **prunes orphan rows** not present in a fresh scan.
- **Auto-rescan:** background tokio task runs `spawn_rescan` every `ARCADE_AUTO_RESCAN_SECS` (default 1800;
  `0` disables). No-ops while a scan runs (`st.scan` guard) → manual rescans never disturbed.
- `rescan_catalog`: scan → `sync_catalog_db` (prune) → `ensure_manifests` (hashing, per-platform progress)
  → IGDB enrich.
- Auth: challenge-response preferred (`/api/auth/challenge` + `/api/auth/verify`, password never on the
  wire); legacy `/api/login` fallback. `derive_auth_key(username, password)` must stay in sync with the
  client's `TryChallengeResponse` (SHA-256 of `lower(username) 0x1f password`).
- `download_file`/`download_chunk` return **404** (not 500) with a "rescan" message when a backing file is
  missing on disk.
- PC games tagged platform `"PC"`, layout `games/PC/<game folder>`. `igdb_platform_ids`: GameCube→[21], Wii→[5].
- Hosted emulator extras (for client per-launch self-heal): `scph1001.bin` (PS1 BIOS),
  `xemu-firmware/{bios.bin,mcpx.bin,hdd.qcow2}` (OG Xbox), `PS3UPDAT.PUP` (RPCS3 firmware). Ryujinx archive
  bundles Switch firmware + `prod.keys` (portable mode).

## Standing rules
- Prod deploy/SSH to `10.0.0.210` (root) requires **explicit per-turn authorization**. nginx `10.0.0.203` is user `brian`.
- Do **not** read prod `*.env` files or query MariaDB for credentials/tokens (classifier-blocked, correctly).
- Do **not** weaken PowerShell execution policy with `-ExecutionPolicy Bypass`.
- User runs catalog scans/rescans **manually** — do not trigger them.
- Catalog content paths stored **relative** to the library root; never store absolute NAS/server paths.
- Commit messages end with `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`.

## Quick verification
- Build: `cargo build --release` ; tests: `cargo test --release`
- Service: `ssh root@10.0.0.210 "systemctl status arcadelauncher-server"`
- Proxy health: `ssh brian@10.0.0.203 "curl -s -o /dev/null -w '%{http_code} %{time_total}s\n' https://arcade.orlandoaio.net/api/catalog"`
- Asset serving: `curl -sI https://arcade.orlandoaio.net/emulators/<file>` (expect 200 / ranged 206)
