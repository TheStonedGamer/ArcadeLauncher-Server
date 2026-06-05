#!/usr/bin/env bash
set -euo pipefail

if [[ "${EUID}" -ne 0 ]]; then
  echo "Run as root inside the Proxmox CT." >&2
  exit 1
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
SERVER_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

apt-get update
apt-get install -y mariadb-server python3 python3-pymysql ca-certificates

id -u arcade >/dev/null 2>&1 || useradd --system --home-dir /nonexistent --shell /usr/sbin/nologin arcade

install -d -o root -g root -m 0755 /opt/arcadelauncher-server
if [[ -f "${SERVER_DIR}/arcadelauncher-server" ]]; then
  install -m 0755 "${SERVER_DIR}/arcadelauncher-server" /opt/arcadelauncher-server/arcadelauncher-server
elif [[ -f "${SERVER_DIR}/target/release/arcadelauncher-server" ]]; then
  install -m 0755 "${SERVER_DIR}/target/release/arcadelauncher-server" /opt/arcadelauncher-server/arcadelauncher-server
else
  echo "Missing Rust server binary: arcadelauncher-server" >&2
  exit 1
fi
install -m 0644 "${SERVER_DIR}/arcade_server.py" /opt/arcadelauncher-server/arcade_server.py
install -m 0755 "${SERVER_DIR}/generate_catalog.py" /opt/arcadelauncher-server/generate_catalog.py

install -d -o arcade -g arcade -m 0755 /srv/arcade-library
if [[ ! -f /srv/arcade-library/catalog.json ]]; then
  install -o arcade -g arcade -m 0644 "${SERVER_DIR}/catalog.example.json" /srv/arcade-library/catalog.json
fi

if [[ ! -f /etc/arcadelauncher-server.env ]]; then
  install -m 0644 "${SCRIPT_DIR}/arcadelauncher-server.env.example" /etc/arcadelauncher-server.env
fi

set -a
# shellcheck disable=SC1091
source /etc/arcadelauncher-server.env
set +a

systemctl enable --now mariadb
mariadb -uroot <<SQL
CREATE DATABASE IF NOT EXISTS \`${ARCADE_DB_NAME}\` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;
CREATE USER IF NOT EXISTS '${ARCADE_DB_USER}'@'localhost' IDENTIFIED BY '${ARCADE_DB_PASSWORD}';
GRANT ALL PRIVILEGES ON \`${ARCADE_DB_NAME}\`.* TO '${ARCADE_DB_USER}'@'localhost';
FLUSH PRIVILEGES;
SQL

install -m 0644 "${SCRIPT_DIR}/arcadelauncher-server.service" /etc/systemd/system/arcadelauncher-server.service
systemctl daemon-reload
systemctl enable --now arcadelauncher-server.service

echo "ArcadeLauncher server installed."
echo "Edit /etc/arcadelauncher-server.env if needed, then run:"
echo "  systemctl restart arcadelauncher-server"
