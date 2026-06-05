# ArcadeLauncher Server

Backend service for a private ArcadeLauncher game library.

The primary backend is a Rust/Linux service:

- `axum`/`tokio` HTTP API and admin UI.
- MariaDB-backed users, bearer tokens, sessions, and game catalog.
- Username/password login for both admin and launcher clients.
- Per-game install manifests with SHA-256 hashes.
- HTTP byte-range file serving for resumable launcher downloads.
- Native Rust filesystem catalog scanner and MariaDB sync.

## Run

```bash
cargo run --release -- --host 127.0.0.1 --port 8721 --library-root /srv/arcade-library
```

Open:

```text
http://127.0.0.1:8721/api/health
http://127.0.0.1:8721/api/catalog
http://127.0.0.1:8721/api/games/<game-id>/manifest
```

## Library Layout

`catalog.json` lives at the library root:

```text
D:\ArcadeLibrary\
  catalog.json
  games\
    assassins-creed-ii-x360-god\
      5553083B\
        00007000\
          C3698B02515E795C2B2F
          C3698B02515E795C2B2F.data\
            Data0000
            Data0001
```

Example `catalog.json`:

```json
{
  "schemaVersion": 1,
  "games": [
    {
      "id": "assassins-creed-ii-x360-god",
      "title": "Assassin's Creed II",
      "platform": "Xbox360",
      "installType": "emulator_rom",
      "version": "1",
      "contentPath": "games/assassins-creed-ii-x360-god",
      "launch": {
        "target": "5553083B/00007000/C3698B02515E795C2B2F",
        "arguments": "{rom}"
      }
    }
  ]
}
```

Keep `contentPath` relative to the library root. Do not put absolute NAS/server paths in the catalog.

`contentPath` may point to either a game directory, such as an Xbox 360 GOD folder, or a single ROM file. For mounted ROM libraries, use the admin panel's `Rescan Filesystem and Sync DB` action.

The native scanner catalogs known emulator ROM formats, Xbox 360 GOD directories, and PC archive installs.

## Proxmox CT Deployment

Recommended shape:

- Proxmox unprivileged Debian/Ubuntu CT.
- Mount your game storage into the CT at `/srv/arcade-library`.
- Run this server as a systemd service on port `8721`.
- Windows clients connect to `http://<ct-ip>:8721`.

Inside the CT:

```bash
apt-get update
apt-get install -y git
git clone https://github.com/TheStonedGamer/ArcadeLauncher-Server.git /opt/arcadelauncher-server
cd /opt/arcadelauncher-server
sudo deploy/install-linux.sh
```

If the game library is mounted from the Proxmox host, make sure the CT can read it:

```text
/srv/arcade-library/
  catalog.json
  games/
```

Service files:

- `deploy/arcadelauncher-server.service`
- `deploy/arcadelauncher-server.env.example`
- `deploy/install-linux.sh`

Useful commands:

```bash
systemctl status arcadelauncher-server
journalctl -u arcadelauncher-server -f
curl http://127.0.0.1:8721/api/health
```

For LAN-only use, plain HTTP is fine to start. For remote access, put it behind a VPN such as WireGuard/Tailscale or a reverse proxy with TLS and authentication.

Set `ARCADE_AUTH_TOKEN` in `/etc/arcadelauncher-server.env` to require clients to send:

```text
Authorization: Bearer <token>
```

The web admin UI is available at:

```text
http://<ct-ip>:8721/admin
```

On first boot, if no admin account exists, the server creates one from:

```text
ARCADE_ADMIN_USERNAME
ARCADE_ADMIN_EMAIL
ARCADE_ADMIN_PASSWORD
```

The admin UI can:

- create, rotate, and delete named launcher/user bearer tokens
- show library root, catalog paths, and per-platform game counts
- trigger a catalog rescan
- sign in with username/email and password
- request password reset links by email

Launcher clients may authenticate with either `ARCADE_AUTH_TOKEN` or a named token created in the admin UI. Keep admin access LAN/VPN-only unless you add TLS and stronger authentication.

Password reset emails use these optional SMTP settings:

```text
ARCADE_SMTP_HOST=smtp.example.com
ARCADE_SMTP_PORT=587
ARCADE_SMTP_USERNAME=...
ARCADE_SMTP_PASSWORD=...
ARCADE_SMTP_FROM=arcadelauncher@example.com
ARCADE_SMTP_TLS=1
```

If SMTP is not configured, the reset page still creates a temporary reset URL and displays it after request. That fallback is for LAN/recovery use only.

## API

`GET /api/health`

Returns server status.

`GET /api/catalog`

Returns the catalog entries without per-file manifests.

`GET /api/games/{id}/manifest`

Returns the file list for one game:

- relative path
- byte size
- SHA-256 hash
- download URL
- launch target relative path

`GET /files/{id}/{relative-path}`

Downloads a game file. Supports `Range: bytes=start-end`.

## Security Notes

This is a LAN/private prototype. Before exposing it outside your network, add real auth, TLS, signed URLs, audit logging, and per-user entitlements.
