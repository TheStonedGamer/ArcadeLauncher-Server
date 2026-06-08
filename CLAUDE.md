# ArcadeLauncher Server — working notes for Claude

Single-file **Rust / axum 0.7 / tokio** backend (`src/main.rs`, ~3300 lines) with
a **MariaDB** (`mysql_async`) catalog, Argon2/TOTP auth, and an admin HTML UI
rendered via `format!` (literal braces are doubled `{{`/`}}`). Read
[`README.md`](README.md) for deployment.

## Build & test
- `cargo build --release` — single binary.
- `cargo test --release` — unit tests live in `#[cfg(test)] mod tests` at the end
  of `main.rs`, covering pure helpers (`parse_range`, `constant_eq`,
  `encode_path`, `clean_igdb_title`, `normalize_title`, `is_pc_primary_archive`,
  `stable_id`, `sha1_short`, `clean_title`, `igdb_platform_ids`).
- Repo: `github.com/TheStonedGamer/ArcadeLauncher-Server`.

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

## nginx gotcha (resolved)
- The proxy originally set `Upgrade: $http_upgrade` / `Connection: "upgrade"`
  unconditionally on all requests with no WebSocket map. This broke keep-alive on
  long multi-file download sequences (HTTP 401/501). The app has no WebSockets;
  fix was removing those two headers from the site config and reloading nginx.

## Conventions
- Rescans run automatically on an interval (see Auto-rescan above) and can also
  be kicked manually from the admin UI.
- Catalog content paths are stored **relative** to the library root; never store
  absolute NAS/server paths.
- Do not read production `*.env` files or query MariaDB for credentials/tokens
  (classifier-blocked, and correctly so).
