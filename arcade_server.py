#!/usr/bin/env python3
"""
ArcadeLauncher private library server.

Dependency-free prototype:
  - catalog API
  - generated install manifests with SHA-256 hashes
  - range-capable file downloads for resumable launcher installs
"""

from __future__ import annotations

import argparse
import base64
import hmac
import html
import hashlib
import json
import mimetypes
import os
import secrets
import smtplib
import subprocess
import sys
import time
from email.message import EmailMessage
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path, PurePosixPath
from urllib.parse import parse_qs, quote, unquote, urlparse


CHUNK_SIZE = 1024 * 1024
SESSION_COOKIE = "AL_ADMIN_SESSION"
SESSION_TTL_SECONDS = 12 * 60 * 60
RESET_TTL_SECONDS = 60 * 60


class Library:
    def __init__(self, root: Path, auth_store: "MariaDbAuthStore | None" = None):
        self.root = root.resolve()
        self.catalog_path = self.root / "catalog.json"
        self.hash_cache_path = self.root / "hash_cache.json"
        self.auth_store = auth_store

    def read_catalog(self) -> dict:
        if self.auth_store:
            games = self.auth_store.list_games()
            if games:
                return {"schemaVersion": 1, "generatedBy": "mariadb", "games": games}
        if not self.catalog_path.exists():
            return {"schemaVersion": 1, "games": []}
        with self.catalog_path.open("r", encoding="utf-8") as f:
            data = json.load(f)
        if "games" not in data or not isinstance(data["games"], list):
            raise ValueError("catalog.json must contain a games array")
        return data

    def find_game(self, game_id: str) -> dict | None:
        if self.auth_store:
            game = self.auth_store.find_game(game_id)
            if game:
                return game
        for game in self.read_catalog().get("games", []):
            if game.get("id") == game_id:
                return game
        return None

    def content_path_for(self, game: dict) -> Path:
        content_path = game.get("contentPath", "")
        if not isinstance(content_path, str) or not content_path:
            raise ValueError("game contentPath is required")
        root = safe_join(self.root, content_path)
        if not root.exists():
            raise FileNotFoundError(f"contentPath not found: {content_path}")
        return root

    def manifest_for(self, base_url: str, game: dict) -> dict:
        content_root = self.content_path_for(game)
        files = []
        if content_root.is_file():
            paths = [content_root]
            rel_root = content_root.parent
        else:
            paths = sorted(path for path in content_root.rglob("*") if path.is_file())
            rel_root = content_root

        for path in paths:
            rel = path.relative_to(rel_root).as_posix()
            files.append(
                {
                    "path": rel,
                    "size": path.stat().st_size,
                    "sha256": self.sha256_cached(path),
                    "url": f"{base_url}/files/{quote(game['id'])}/{quote(rel, safe='/')}",
                }
            )

        launch = game.get("launch", {})
        return {
            "schemaVersion": 1,
            "id": game.get("id", ""),
            "title": game.get("title", ""),
            "platform": game.get("platform", ""),
            "installType": game.get("installType", "emulator_rom"),
            "version": game.get("version", ""),
            "coverArtUrl": game.get("coverArtUrl", ""),
            "igdbId": game.get("igdbId", 0),
            "launch": {
                "target": launch.get("target", ""),
                "arguments": launch.get("arguments", "{rom}"),
            },
            "files": files,
        }

    def sha256_cached(self, path: Path) -> str:
        stat = path.stat()
        rel = path.resolve().relative_to(self.root).as_posix()
        cache_key = f"{rel}|{stat.st_size}|{int(stat.st_mtime)}"
        cache = self.read_hash_cache()
        cached = cache.get(cache_key)
        if isinstance(cached, str) and len(cached) == 64:
            return cached

        digest = sha256_file(path)
        cache[cache_key] = digest
        self.write_hash_cache(cache)
        return digest

    def read_hash_cache(self) -> dict:
        try:
            if self.hash_cache_path.exists():
                with self.hash_cache_path.open("r", encoding="utf-8") as f:
                    data = json.load(f)
                if isinstance(data, dict):
                    return data
        except Exception:
            pass
        return {}

    def write_hash_cache(self, cache: dict) -> None:
        try:
            tmp = self.hash_cache_path.with_suffix(".json.tmp")
            with tmp.open("w", encoding="utf-8") as f:
                json.dump(cache, f, indent=2)
                f.write("\n")
            tmp.chmod(0o644)
            tmp.replace(self.hash_cache_path)
        except Exception:
            pass

    def catalog_summary(self) -> dict:
        if self.auth_store:
            game_count, by_platform = self.auth_store.catalog_counts()
        else:
            catalog = self.read_catalog()
            by_platform = {}
            for game in catalog.get("games", []):
                platform = str(game.get("platform", "Unknown"))
                by_platform[platform] = by_platform.get(platform, 0) + 1
            game_count = len(catalog.get("games", []))
        return {
            "gameCount": game_count,
            "byPlatform": dict(sorted(by_platform.items())),
            "catalogPath": str(self.catalog_path),
            "hashCachePath": str(self.hash_cache_path),
        }

    def rescan_catalog(self) -> str:
        script = Path(__file__).with_name("generate_catalog.py")
        if not script.exists():
            raise FileNotFoundError("generate_catalog.py not found")
        completed = subprocess.run(
            [sys.executable, str(script), "--library-root", str(self.root)],
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=300,
        )
        if completed.returncode != 0:
            raise RuntimeError(completed.stdout.strip() or "catalog generation failed")
        return completed.stdout.strip()


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while True:
            chunk = f.read(CHUNK_SIZE)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def safe_join(root: Path, relative: str) -> Path:
    posix = PurePosixPath(unquote(relative).replace("\\", "/"))
    if posix.is_absolute() or ".." in posix.parts:
        raise ValueError("invalid path")
    result = (root / Path(*posix.parts)).resolve()
    if result != root and root not in result.parents:
        raise ValueError("path escapes library root")
    return result


def parse_range(header: str | None, size: int) -> tuple[int, int] | None:
    if not header:
        return None
    if not header.startswith("bytes="):
        raise ValueError("unsupported range unit")
    spec = header[6:].split(",", 1)[0].strip()
    if "-" not in spec:
        raise ValueError("invalid range")
    start_s, end_s = spec.split("-", 1)
    if start_s == "":
        suffix = int(end_s)
        if suffix <= 0:
            raise ValueError("invalid suffix range")
        return max(size - suffix, 0), size - 1
    start = int(start_s)
    end = int(end_s) if end_s else size - 1
    if start < 0 or start >= size or end < start:
        raise ValueError("range not satisfiable")
    return start, min(end, size - 1)


def hash_password(password: str, salt: str | None = None) -> str:
    salt_bytes = base64.urlsafe_b64decode(salt.encode("ascii")) if salt else secrets.token_bytes(16)
    digest = hashlib.scrypt(
        password.encode("utf-8"),
        salt=salt_bytes,
        n=2**14,
        r=8,
        p=1,
        dklen=64,
        maxmem=64 * 1024 * 1024,
    )
    return "scrypt$n=16384,r=8,p=1${}${}".format(
        base64.urlsafe_b64encode(salt_bytes).decode("ascii"),
        base64.urlsafe_b64encode(digest).decode("ascii"),
    )


def verify_password(password: str, stored: str) -> bool:
    try:
        algo, params, salt, expected = stored.split("$", 3)
        if algo != "scrypt":
            return False
        actual = hash_password(password, salt).split("$", 3)[3]
        return hmac.compare_digest(actual, expected)
    except Exception:
        return False


def email_config_from_env() -> dict:
    return {
        "host": os.environ.get("ARCADE_SMTP_HOST", ""),
        "port": int(os.environ.get("ARCADE_SMTP_PORT", "587") or "587"),
        "username": os.environ.get("ARCADE_SMTP_USERNAME", ""),
        "password": os.environ.get("ARCADE_SMTP_PASSWORD", ""),
        "from": os.environ.get("ARCADE_SMTP_FROM", ""),
        "tls": os.environ.get("ARCADE_SMTP_TLS", "1") != "0",
    }


def send_reset_email(to_addr: str, reset_url: str) -> str:
    cfg = email_config_from_env()
    if not cfg["host"] or not cfg["from"]:
        return "SMTP is not configured; reset URL was not emailed."

    msg = EmailMessage()
    msg["Subject"] = "ArcadeLauncher password reset"
    msg["From"] = cfg["from"]
    msg["To"] = to_addr
    msg.set_content(
        "Use this link to reset your ArcadeLauncher Server password. "
        f"It expires in 60 minutes.\n\n{reset_url}\n"
    )

    with smtplib.SMTP(cfg["host"], cfg["port"], timeout=20) as smtp:
        if cfg["tls"]:
            smtp.starttls()
        if cfg["username"]:
            smtp.login(cfg["username"], cfg["password"])
        smtp.send_message(msg)
    return "Password reset email sent."


class MariaDbAuthStore:
    def __init__(self) -> None:
        self.host = os.environ.get("ARCADE_DB_HOST", "127.0.0.1")
        self.port = int(os.environ.get("ARCADE_DB_PORT", "3306") or "3306")
        self.database = os.environ.get("ARCADE_DB_NAME", "arcadelauncher")
        self.user = os.environ.get("ARCADE_DB_USER", "arcade")
        self.password = os.environ.get("ARCADE_DB_PASSWORD", "")

    def connect(self, database: str | None = None):
        try:
            import pymysql
        except ImportError as exc:
            raise RuntimeError("python3-pymysql is required for MariaDB auth storage") from exc
        return pymysql.connect(
            host=self.host,
            port=self.port,
            user=self.user,
            password=self.password,
            database=database if database is not None else self.database,
            charset="utf8mb4",
            cursorclass=pymysql.cursors.DictCursor,
            autocommit=True,
        )

    def ensure_schema(self) -> None:
        with self.connect(database=None) as conn:
            with conn.cursor() as cur:
                cur.execute(
                    f"CREATE DATABASE IF NOT EXISTS `{self.database}` "
                    "CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci"
                )
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS admin_users (
                      id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
                      username VARCHAR(80) NOT NULL UNIQUE,
                      email VARCHAR(255) NOT NULL UNIQUE,
                      password_hash VARCHAR(255) NOT NULL,
                      is_admin BOOLEAN NOT NULL DEFAULT TRUE,
                      enabled BOOLEAN NOT NULL DEFAULT TRUE,
                      created_at BIGINT NOT NULL
                    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
                    """
                )
                try:
                    cur.execute("ALTER TABLE admin_users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT TRUE")
                except Exception:
                    pass
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS launcher_tokens (
                      id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
                      name VARCHAR(160) NOT NULL,
                      user_id BIGINT UNSIGNED NULL,
                      token_hash CHAR(64) NOT NULL UNIQUE,
                      token_plain TEXT NULL,
                      enabled BOOLEAN NOT NULL DEFAULT TRUE,
                      created_at BIGINT NOT NULL,
                      INDEX (user_id)
                    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
                    """
                )
                try:
                    cur.execute("ALTER TABLE launcher_tokens ADD COLUMN user_id BIGINT UNSIGNED NULL")
                except Exception:
                    pass
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS admin_sessions (
                      token_hash CHAR(64) NOT NULL PRIMARY KEY,
                      admin_id BIGINT UNSIGNED NOT NULL,
                      expires_at BIGINT NOT NULL,
                      created_at BIGINT NOT NULL,
                      INDEX (admin_id),
                      CONSTRAINT fk_admin_sessions_admin
                        FOREIGN KEY (admin_id) REFERENCES admin_users(id)
                        ON DELETE CASCADE
                    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS password_resets (
                      token_hash CHAR(64) NOT NULL PRIMARY KEY,
                      admin_id BIGINT UNSIGNED NOT NULL,
                      expires_at BIGINT NOT NULL,
                      created_at BIGINT NOT NULL,
                      INDEX (admin_id),
                      CONSTRAINT fk_password_resets_admin
                        FOREIGN KEY (admin_id) REFERENCES admin_users(id)
                        ON DELETE CASCADE
                    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
                    """
                )
                cur.execute(
                    """
                    CREATE TABLE IF NOT EXISTS games (
                      id VARCHAR(96) NOT NULL PRIMARY KEY,
                      title VARCHAR(512) NOT NULL,
                      platform VARCHAR(80) NOT NULL,
                      install_type VARCHAR(80) NOT NULL,
                      version VARCHAR(80) NOT NULL,
                      content_path TEXT NOT NULL,
                      launch_target TEXT NOT NULL,
                      launch_arguments TEXT NOT NULL,
                      cover_art_url TEXT NULL,
                      igdb_id BIGINT NOT NULL DEFAULT 0,
                      updated_at BIGINT NOT NULL,
                      INDEX idx_games_platform_title (platform, title)
                    ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
                    """
                )

    def token_hash(self, token: str) -> str:
        return hashlib.sha256(token.encode("utf-8")).hexdigest()

    def ensure_bootstrap_admin(self, username: str, email: str, password: str) -> None:
        if not username or not email or not password:
            return
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("SELECT COUNT(*) AS c FROM admin_users")
                if int(cur.fetchone()["c"]) > 0:
                    return
                cur.execute(
                    """
                    INSERT INTO admin_users (username, email, password_hash, is_admin, enabled, created_at)
                    VALUES (%s, %s, %s, TRUE, TRUE, %s)
                    """,
                    (username, email, hash_password(password), int(time.time())),
                )

    def find_admin(self, username_or_email: str) -> dict | None:
        key = username_or_email.strip()
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    SELECT id, username, email, password_hash AS passwordHash, is_admin AS isAdmin, enabled
                    FROM admin_users
                    WHERE enabled = TRUE AND (username = %s OR email = %s)
                    LIMIT 1
                    """,
                    (key, key),
                )
                return cur.fetchone()

    def get_admin_by_id(self, admin_id: int) -> dict | None:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    SELECT id, username, email, password_hash AS passwordHash, is_admin AS isAdmin, enabled
                    FROM admin_users WHERE id = %s AND enabled = TRUE
                    """,
                    (admin_id,),
                )
                return cur.fetchone()

    def list_admins(self) -> list[dict]:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("SELECT id, username, email, is_admin AS isAdmin, enabled, created_at AS createdAt FROM admin_users ORDER BY username")
                return list(cur.fetchall())

    def create_user(self, username: str, email: str, password: str, is_admin: bool = False) -> None:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO admin_users (username, email, password_hash, is_admin, enabled, created_at)
                    VALUES (%s, %s, %s, %s, TRUE, %s)
                    """,
                    (username, email, hash_password(password), bool(is_admin), int(time.time())),
                )

    def list_launcher_tokens(self) -> list[dict]:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    SELECT id, name, user_id AS userId, token_plain AS token, enabled, created_at AS createdAt
                    FROM launcher_tokens ORDER BY name
                    """
                )
                return list(cur.fetchall())

    def create_launcher_token(self, name: str) -> str:
        token = secrets.token_urlsafe(36)
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    INSERT INTO launcher_tokens (name, token_hash, token_plain, enabled, created_at)
                    VALUES (%s, %s, %s, TRUE, %s)
                    """,
                    (name, self.token_hash(token), token, int(time.time())),
                )
        return token

    def issue_user_token(self, user_id: int, username: str) -> str:
        token = secrets.token_urlsafe(36)
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("SELECT id FROM launcher_tokens WHERE user_id = %s LIMIT 1", (user_id,))
                existing = cur.fetchone()
                if existing:
                    cur.execute(
                        """
                        UPDATE launcher_tokens
                        SET name = %s, token_hash = %s, token_plain = %s, enabled = TRUE, created_at = %s
                        WHERE id = %s
                        """,
                        (username, self.token_hash(token), token, int(time.time()), existing["id"]),
                    )
                else:
                    cur.execute(
                        """
                        INSERT INTO launcher_tokens (name, user_id, token_hash, token_plain, enabled, created_at)
                        VALUES (%s, %s, %s, %s, TRUE, %s)
                        """,
                        (username, user_id, self.token_hash(token), token, int(time.time())),
                    )
        return token

    def delete_launcher_token(self, token_id: int) -> None:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("DELETE FROM launcher_tokens WHERE id = %s", (token_id,))

    def rotate_launcher_token(self, token_id: int) -> str:
        token = secrets.token_urlsafe(36)
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "UPDATE launcher_tokens SET token_hash = %s, token_plain = %s, created_at = %s WHERE id = %s",
                    (self.token_hash(token), token, int(time.time()), token_id),
                )
        return token

    def validate_launcher_token(self, token: str) -> bool:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "SELECT id FROM launcher_tokens WHERE token_hash = %s AND enabled = TRUE LIMIT 1",
                    (self.token_hash(token),),
                )
                return cur.fetchone() is not None

    def create_session(self, admin_id: int) -> str:
        token = secrets.token_urlsafe(36)
        with self.connect() as conn:
            with conn.cursor() as cur:
                now = int(time.time())
                cur.execute("DELETE FROM admin_sessions WHERE expires_at <= %s", (now,))
                cur.execute(
                    """
                    INSERT INTO admin_sessions (token_hash, admin_id, expires_at, created_at)
                    VALUES (%s, %s, %s, %s)
                    """,
                    (self.token_hash(token), admin_id, now + SESSION_TTL_SECONDS, now),
                )
        return token

    def admin_for_session(self, token: str) -> dict | None:
        if not token:
            return None
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("DELETE FROM admin_sessions WHERE expires_at <= %s", (int(time.time()),))
                cur.execute(
                    """
                    SELECT a.id, a.username, a.email, a.enabled
                    FROM admin_sessions s
                    JOIN admin_users a ON a.id = s.admin_id
                    WHERE s.token_hash = %s AND s.expires_at > %s AND a.enabled = TRUE
                    LIMIT 1
                    """,
                    (self.token_hash(token), int(time.time())),
                )
                return cur.fetchone()

    def delete_session(self, token: str) -> None:
        if not token:
            return
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("DELETE FROM admin_sessions WHERE token_hash = %s", (self.token_hash(token),))

    def create_password_reset(self, admin_id: int) -> str:
        token = secrets.token_urlsafe(36)
        now = int(time.time())
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("DELETE FROM password_resets WHERE expires_at <= %s", (now,))
                cur.execute(
                    """
                    INSERT INTO password_resets (token_hash, admin_id, expires_at, created_at)
                    VALUES (%s, %s, %s, %s)
                    """,
                    (self.token_hash(token), admin_id, now + RESET_TTL_SECONDS, now),
                )
        return token

    def reset_password(self, token: str, password: str) -> bool:
        token_hash = self.token_hash(token)
        now = int(time.time())
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    "SELECT admin_id FROM password_resets WHERE token_hash = %s AND expires_at > %s",
                    (token_hash, now),
                )
                row = cur.fetchone()
                if not row:
                    return False
                admin_id = int(row["admin_id"])
                cur.execute("UPDATE admin_users SET password_hash = %s WHERE id = %s", (hash_password(password), admin_id))
                cur.execute("DELETE FROM password_resets WHERE token_hash = %s", (token_hash,))
                cur.execute("DELETE FROM admin_sessions WHERE admin_id = %s", (admin_id,))
                return True

    def list_games(self) -> list[dict]:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    SELECT id, title, platform, install_type, version, content_path,
                           launch_target, launch_arguments, cover_art_url, igdb_id
                    FROM games ORDER BY platform, title, id
                    """
                )
                return [self.row_to_game(row) for row in cur.fetchall()]

    def find_game(self, game_id: str) -> dict | None:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute(
                    """
                    SELECT id, title, platform, install_type, version, content_path,
                           launch_target, launch_arguments, cover_art_url, igdb_id
                    FROM games WHERE id = %s
                    """,
                    (game_id,),
                )
                row = cur.fetchone()
                return self.row_to_game(row) if row else None

    def catalog_counts(self) -> tuple[int, dict[str, int]]:
        with self.connect() as conn:
            with conn.cursor() as cur:
                cur.execute("SELECT platform, COUNT(*) AS c FROM games GROUP BY platform ORDER BY platform")
                rows = cur.fetchall()
        counts = {str(row["platform"]): int(row["c"]) for row in rows}
        return sum(counts.values()), counts

    def sync_games(self, games: list[dict]) -> None:
        now = int(time.time())
        seen = [game["id"] for game in games]
        with self.connect() as conn:
            with conn.cursor() as cur:
                for game in games:
                    launch = game.get("launch", {})
                    cur.execute(
                        """
                        INSERT INTO games
                          (id, title, platform, install_type, version, content_path,
                           launch_target, launch_arguments, cover_art_url, igdb_id, updated_at)
                        VALUES (%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s)
                        ON DUPLICATE KEY UPDATE
                          title=VALUES(title),
                          platform=VALUES(platform),
                          install_type=VALUES(install_type),
                          version=VALUES(version),
                          content_path=VALUES(content_path),
                          launch_target=VALUES(launch_target),
                          launch_arguments=VALUES(launch_arguments),
                          cover_art_url=VALUES(cover_art_url),
                          igdb_id=VALUES(igdb_id),
                          updated_at=VALUES(updated_at)
                        """,
                        (
                            game.get("id", ""),
                            game.get("title", ""),
                            game.get("platform", ""),
                            game.get("installType", "emulator_rom"),
                            game.get("version", ""),
                            game.get("contentPath", ""),
                            launch.get("target", ""),
                            launch.get("arguments", "{rom}"),
                            game.get("coverArtUrl", ""),
                            int(game.get("igdbId", 0) or 0),
                            now,
                        ),
                    )
                if seen:
                    placeholders = ",".join(["%s"] * len(seen))
                    cur.execute(f"DELETE FROM games WHERE id NOT IN ({placeholders})", seen)
                else:
                    cur.execute("DELETE FROM games")

    def row_to_game(self, row: dict) -> dict:
        return {
            "id": row.get("id", ""),
            "title": row.get("title", ""),
            "platform": row.get("platform", ""),
            "installType": row.get("install_type", "emulator_rom"),
            "version": row.get("version", ""),
            "contentPath": row.get("content_path", ""),
            "coverArtUrl": row.get("cover_art_url", "") or "",
            "igdbId": int(row.get("igdb_id", 0) or 0),
            "launch": {
                "target": row.get("launch_target", "") or "",
                "arguments": row.get("launch_arguments", "{rom}") or "{rom}",
            },
        }


class ArcadeHandler(BaseHTTPRequestHandler):
    server_version = "ArcadeLauncherServer/0.1"

    @property
    def library(self) -> Library:
        return self.server.library  # type: ignore[attr-defined]

    @property
    def auth_token(self) -> str:
        return self.server.auth_token  # type: ignore[attr-defined]

    @property
    def admin_token(self) -> str:
        return self.server.admin_token  # type: ignore[attr-defined]

    @property
    def auth_store(self) -> MariaDbAuthStore:
        return self.server.auth_store  # type: ignore[attr-defined]

    def do_GET(self) -> None:
        try:
            parsed = urlparse(self.path)
            if parsed.path in ("/", "/admin", "/admin/login", "/admin/reset", "/admin/logout"):
                if parsed.path == "/admin/logout":
                    self.clear_session()
                    self.redirect("/admin/login")
                    return
                self.send_admin_page()
                return

            if not self.authorized_api():
                self.send_json({"error": "unauthorized"}, HTTPStatus.UNAUTHORIZED)
                return
            self.route_get()
        except FileNotFoundError as exc:
            self.send_json({"error": str(exc)}, HTTPStatus.NOT_FOUND)
        except ValueError as exc:
            self.send_json({"error": str(exc)}, HTTPStatus.BAD_REQUEST)
        except Exception as exc:
            self.send_json({"error": str(exc)}, HTTPStatus.INTERNAL_SERVER_ERROR)

    def do_POST(self) -> None:
        try:
            parsed = urlparse(self.path)
            if parsed.path == "/api/login":
                self.handle_api_login()
                return
            if parsed.path in ("/admin", "/admin/login", "/admin/reset"):
                self.handle_admin_post()
                return
            self.send_json({"error": "not found"}, HTTPStatus.NOT_FOUND)
        except Exception as exc:
            if self.path.startswith("/api/"):
                self.send_json({"error": str(exc)}, HTTPStatus.INTERNAL_SERVER_ERROR)
            else:
                self.send_admin_page(str(exc), error=True)

    def route_get(self) -> None:
        parsed = urlparse(self.path)
        path = parsed.path

        if path == "/api/health":
            self.send_json({"ok": True, "schemaVersion": 1, "version": "0.1"})
            return

        if path == "/api/catalog":
            self.send_json(self.library.read_catalog())
            return

        if path.startswith("/api/games/") and path.endswith("/manifest"):
            game_id = unquote(path[len("/api/games/") : -len("/manifest")]).strip("/")
            game = self.library.find_game(game_id)
            if not game:
                raise FileNotFoundError(f"game not found: {game_id}")
            self.send_json(self.library.manifest_for(self.base_url(), game))
            return

        if path.startswith("/files/"):
            self.send_file(path[len("/files/") :])
            return

        self.send_json({"error": "not found"}, HTTPStatus.NOT_FOUND)

    def handle_api_login(self) -> None:
        form = self.read_form()
        username = form.get("username", [""])[0]
        password = form.get("password", [""])[0]
        user = self.auth_store.find_admin(username)
        if not user or not verify_password(password, str(user.get("passwordHash", ""))):
            self.send_json({"error": "invalid username or password"}, HTTPStatus.UNAUTHORIZED)
            return
        token = self.auth_store.issue_user_token(int(user["id"]), str(user.get("username", username)))
        self.send_json({
            "token": token,
            "username": str(user.get("username", "")),
            "isAdmin": bool(user.get("isAdmin", False)),
        })

    def handle_admin_post(self) -> None:
        form = self.read_form()
        action = form.get("action", [""])[0]

        if action == "login":
            self.handle_login(form)
            return

        if action == "forgot_password":
            self.handle_forgot_password(form)
            return

        if action == "reset_password":
            self.handle_reset_password(form)
            return

        admin = self.current_admin()
        if not admin:
            self.send_admin_page("Please sign in first.", error=True)
            return

        if action == "add_user":
            username = form.get("username", [""])[0].strip()
            email = form.get("email", [""])[0].strip()
            password = form.get("password", [""])[0]
            is_admin = form.get("is_admin", [""])[0] == "1"
            if not username or not email or len(password) < 10:
                raise ValueError("username, email, and a 10+ character password are required")
            self.auth_store.create_user(username, email, password, is_admin)
            self.send_admin_page(f"Created user {username}.")
            return

        if action == "delete_user":
            user_id = int(form.get("user_id", ["0"])[0] or "0")
            self.auth_store.delete_launcher_token(user_id)
            self.send_admin_page("Deleted user token.")
            return

        if action == "rotate_user":
            user_id = int(form.get("user_id", ["0"])[0] or "0")
            self.auth_store.rotate_launcher_token(user_id)
            self.send_admin_page("Rotated user token.")
            return

        if action == "rescan":
            output = self.library.rescan_catalog()
            self.send_admin_page("Catalog rescan complete.\n" + output)
            return

        self.send_admin_page("No action taken.")

    def handle_login(self, form: dict[str, list[str]]) -> None:
        username = form.get("username", [""])[0]
        password = form.get("password", [""])[0]
        admin = self.auth_store.find_admin(username)
        if not admin or not admin.get("isAdmin", False) or not verify_password(password, str(admin.get("passwordHash", ""))):
            self.send_admin_page("Invalid username or password.", error=True)
            return
        session_token = self.auth_store.create_session(int(admin["id"]))
        self.redirect("/admin", cookie=f"{SESSION_COOKIE}={session_token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECONDS}")

    def handle_forgot_password(self, form: dict[str, list[str]]) -> None:
        email = form.get("email", [""])[0].strip()
        admin = self.auth_store.find_admin(email)
        message = "If that email exists, a reset link has been sent."
        if admin:
            token = self.auth_store.create_password_reset(int(admin["id"]))
            reset_url = f"{self.base_url()}/admin/reset?token={quote(token)}"
            try:
                email_status = send_reset_email(str(admin.get("email", "")), reset_url)
                if "not configured" in email_status:
                    message += "\n\nSMTP is not configured yet. Temporary reset URL:\n" + reset_url
            except Exception as exc:
                message += "\n\nEmail failed. Temporary reset URL:\n" + reset_url + f"\n\n{exc}"
        self.send_admin_page(message)

    def handle_reset_password(self, form: dict[str, list[str]]) -> None:
        token = form.get("reset_token", [""])[0]
        password = form.get("password", [""])[0]
        confirm = form.get("confirm", [""])[0]
        if len(password) < 10:
            self.send_admin_page("Password must be at least 10 characters.", error=True)
            return
        if password != confirm:
            self.send_admin_page("Passwords do not match.", error=True)
            return
        if not self.auth_store.reset_password(token, password):
            self.send_admin_page("Reset link is invalid or expired.", error=True)
            return
        self.send_admin_page("Password changed. You can sign in now.")

    def read_form(self) -> dict[str, list[str]]:
        length = int(self.headers.get("Content-Length", "0") or "0")
        raw = self.rfile.read(length).decode("utf-8") if length > 0 else ""
        return parse_qs(raw)

    def cookie_value(self, name: str) -> str:
        header = self.headers.get("Cookie", "")
        for part in header.split(";"):
            key, sep, value = part.strip().partition("=")
            if sep and key == name:
                return value
        return ""

    def current_admin(self) -> dict | None:
        token = self.cookie_value(SESSION_COOKIE)
        if not token:
            return None
        return self.auth_store.admin_for_session(token)

    def clear_session(self) -> None:
        token = self.cookie_value(SESSION_COOKIE)
        if token:
            self.auth_store.delete_session(token)

    def redirect(self, location: str, cookie: str = "") -> None:
        self.send_response(HTTPStatus.SEE_OTHER)
        self.send_header("Location", location)
        if cookie:
            self.send_header("Set-Cookie", cookie)
        elif location == "/admin/login":
            self.send_header("Set-Cookie", f"{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
        self.send_header("Content-Length", "0")
        self.end_headers()

    def send_file(self, file_route: str) -> None:
        game_id, sep, rel = file_route.partition("/")
        if not sep:
            raise ValueError("file path missing")
        game = self.library.find_game(unquote(game_id))
        if not game:
            raise FileNotFoundError(f"game not found: {game_id}")

        content_root = self.library.content_path_for(game)
        if content_root.is_file():
            requested = PurePosixPath(unquote(rel).replace("\\", "/"))
            if requested.name != content_root.name or len(requested.parts) != 1:
                raise ValueError("invalid file path")
            file_path = content_root
        else:
            file_path = safe_join(content_root, rel)
        if not file_path.is_file():
            raise FileNotFoundError("file not found")

        size = file_path.stat().st_size
        range_header = self.headers.get("Range")
        byte_range = parse_range(range_header, size)
        content_type = mimetypes.guess_type(file_path.name)[0] or "application/octet-stream"

        if byte_range:
            start, end = byte_range
            self.send_response(HTTPStatus.PARTIAL_CONTENT)
            self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        else:
            start, end = 0, size - 1
            self.send_response(HTTPStatus.OK)

        length = end - start + 1
        self.send_header("Content-Type", content_type)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(length))
        self.end_headers()

        with file_path.open("rb") as f:
            f.seek(start)
            remaining = length
            while remaining > 0:
                chunk = f.read(min(CHUNK_SIZE, remaining))
                if not chunk:
                    break
                self.wfile.write(chunk)
                remaining -= len(chunk)

    def send_json(self, data: dict, status: HTTPStatus = HTTPStatus.OK) -> None:
        body = json.dumps(data, indent=2).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_html(self, html_text: str, status: HTTPStatus = HTTPStatus.OK) -> None:
        body = html_text.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_admin_page(self, message: str = "", error: bool = False) -> None:
        parsed = urlparse(self.path)
        query = parse_qs(parsed.query)
        admin = self.current_admin()
        authed = admin is not None
        summary = self.library.catalog_summary() if authed else {}
        launcher_tokens = self.auth_store.list_launcher_tokens() if authed else []
        admins = self.auth_store.list_admins() if authed else []
        smtp_cfg = email_config_from_env()
        db_summary = {
            "host": self.auth_store.host,
            "port": self.auth_store.port,
            "database": self.auth_store.database,
            "user": self.auth_store.user,
        }
        service_summary = {
            "baseUrl": self.base_url(),
            "libraryRoot": str(self.library.root),
            "catalogPath": str(self.library.catalog_path),
            "hashCachePath": str(self.library.hash_cache_path),
            "authMode": "Bearer tokens + admin sessions",
            "smtp": "Configured" if smtp_cfg.get("host") and smtp_cfg.get("from") else "Not configured",
        }
        message_html = ""
        if message:
            cls = "error" if error else "notice"
            message_html = f"<pre class='{cls}'>{html.escape(message)}</pre>"

        if parsed.path == "/admin/reset":
            reset_token = html.escape(query.get("token", [""])[0])
            admin_body = f"""
              <section>
                <h2>Reset Password</h2>
                <form method="post" action="/admin/reset" class="stack">
                  <input type="hidden" name="reset_token" value="{reset_token}">
                  <input name="password" type="password" placeholder="New password" required>
                  <input name="confirm" type="password" placeholder="Confirm password" required>
                  <button name="action" value="reset_password">Change Password</button>
                </form>
                <p><a href="/admin/login">Back to sign in</a></p>
              </section>
            """
            self.send_admin_shell(message_html, admin_body)
            return

        if not authed:
            admin_body = """
              <section>
                <h2>Sign In</h2>
                <form method="post" action="/admin/login" class="stack">
                  <input name="username" placeholder="Username or email" autofocus required>
                  <input name="password" type="password" placeholder="Password" required>
                  <button name="action" value="login">Sign In</button>
                </form>
              </section>
              <section>
                <h2>Password Reset</h2>
                <form method="post" action="/admin/login" class="row">
                  <input name="email" type="email" placeholder="Email address">
                  <button name="action" value="forgot_password">Email Reset Link</button>
                </form>
              </section>
            """
            self.send_admin_shell(message_html, admin_body)
            return

        tokens_html = ""
        for user in launcher_tokens:
            tokens_html += f"""
            <tr>
              <td>{html.escape(str(user.get('name', '')))}</td>
              <td><code class="token">{html.escape(str(user.get('token', '')))}</code></td>
              <td>{'Enabled' if user.get('enabled', True) else 'Disabled'}</td>
              <td>
                <form method="post" class="inline">
                  <input type="hidden" name="user_id" value="{html.escape(str(user.get('id', '')))}">
                  <button name="action" value="rotate_user">Rotate</button>
                  <button name="action" value="delete_user" class="danger">Delete</button>
                </form>
              </td>
            </tr>"""

        platforms_html = ""
        for platform, count in summary.get("byPlatform", {}).items():
            platforms_html += f"<div class='platform-row'><span>{html.escape(platform)}</span><strong>{count}</strong></div>"

        admins_html = ""
        for item in admins:
            admins_html += f"""
            <tr>
              <td>{html.escape(str(item.get('username', '')))}</td>
              <td>{html.escape(str(item.get('email', '')))}</td>
              <td>{'Admin' if item.get('isAdmin', False) else 'Client'}</td>
              <td>{'Enabled' if item.get('enabled', True) else 'Disabled'}</td>
            </tr>"""

        admin_body = f"""
          <div class="admin-layout">
            <aside class="sidebar">
              <div class="brand-block">
                <div class="brand-mark">AL</div>
                <div>
                  <div class="brand-title">ArcadeLauncher</div>
                  <div class="brand-subtitle">Server Console</div>
                </div>
              </div>
              <nav>
                <a href="#overview">Overview</a>
                <a href="#library">Library</a>
                <a href="#auth">Auth</a>
                <a href="#config">Configuration</a>
                <a href="#maintenance">Maintenance</a>
              </nav>
            </aside>
            <div class="content">
              <section class="topbar">
                <div>
                  <div class="eyebrow">Private library server</div>
                  <h1>Server Administration</h1>
                </div>
                <div class="account-box">
                  <span>Signed in as <strong>{html.escape(str(admin.get('username', '')))}</strong></span>
                  <a class="buttonlink" href="/admin/logout">Sign Out</a>
                </div>
              </section>

              <section id="overview" class="section">
                <div class="section-heading">
                  <h2>Overview</h2>
                  <span class="muted">Live status from MariaDB and the mounted library</span>
                </div>
                <div class="metric-grid">
                  <div class="metric"><span>Total Games</span><strong>{summary.get('gameCount', 0)}</strong></div>
                  <div class="metric"><span>Platforms</span><strong>{len(summary.get('byPlatform', {}))}</strong></div>
                  <div class="metric"><span>Issued Tokens</span><strong>{len(launcher_tokens)}</strong></div>
                  <div class="metric"><span>Users</span><strong>{len(admins)}</strong></div>
                </div>
              </section>

              <section id="library" class="section split">
                <div>
                  <div class="section-heading">
                    <h2>Library Setup</h2>
                    <span class="muted">Filesystem stores game files; MariaDB stores lookup metadata.</span>
                  </div>
                  <dl class="kv">
                    <dt>Library Root</dt><dd><code>{html.escape(str(service_summary['libraryRoot']))}</code></dd>
                    <dt>Catalog File</dt><dd><code>{html.escape(str(service_summary['catalogPath']))}</code></dd>
                    <dt>Hash Cache</dt><dd><code>{html.escape(str(service_summary['hashCachePath']))}</code></dd>
                  </dl>
                  <form method="post" class="row">
                    <button name="action" value="rescan">Rescan Filesystem and Sync DB</button>
                  </form>
                </div>
                <div class="platform-card">
                  <h3>Platform Counts</h3>
                  {platforms_html or '<p class="muted">No cataloged platforms yet.</p>'}
                </div>
              </section>

              <section id="auth" class="section">
                <div class="section-heading">
                  <h2>Auth Management</h2>
                  <span class="muted">All users sign in with username/password; bearer tokens are issued behind the scenes.</span>
                </div>
                <div class="two-col">
                  <div>
                    <h3>Create User</h3>
                    <form method="post" class="row">
                      <input name="username" placeholder="Username">
                      <input name="email" type="email" placeholder="Email">
                      <input name="password" type="password" placeholder="Password">
                      <label class="checkline"><input type="checkbox" name="is_admin" value="1"> Admin</label>
                      <button name="action" value="add_user">Create User</button>
                    </form>
                    <h3>Users</h3>
                    <table>
                      <thead><tr><th>Username</th><th>Email</th><th>Role</th><th>Status</th></tr></thead>
                      <tbody>{admins_html or '<tr><td colspan="4">No users yet.</td></tr>'}</tbody>
                    </table>
                  </div>
                  <div>
                    <h3>Issued Tokens</h3>
                    <table>
                      <thead><tr><th>Name</th><th>Bearer Token</th><th>Status</th><th>Actions</th></tr></thead>
                      <tbody>{tokens_html or '<tr><td colspan="4">No issued tokens yet.</td></tr>'}</tbody>
                    </table>
                    <p class="muted">Clients receive these tokens automatically after username/password login.</p>
                  </div>
                </div>
              </section>

              <section id="config" class="section">
                <div class="section-heading">
                  <h2>Server Configuration</h2>
                  <span class="muted">Read-only view of the active service environment.</span>
                </div>
                <div class="config-grid">
                  <div class="config-card">
                    <h3>HTTP/API</h3>
                    <dl class="kv compact">
                      <dt>Base URL</dt><dd><code>{html.escape(str(service_summary['baseUrl']))}</code></dd>
                      <dt>Auth</dt><dd>{html.escape(str(service_summary['authMode']))}</dd>
                    </dl>
                  </div>
                  <div class="config-card">
                    <h3>MariaDB</h3>
                    <dl class="kv compact">
                      <dt>Host</dt><dd><code>{html.escape(str(db_summary['host']))}:{db_summary['port']}</code></dd>
                      <dt>Database</dt><dd><code>{html.escape(str(db_summary['database']))}</code></dd>
                      <dt>User</dt><dd><code>{html.escape(str(db_summary['user']))}</code></dd>
                    </dl>
                  </div>
                  <div class="config-card">
                    <h3>Email / Password Reset</h3>
                    <dl class="kv compact">
                      <dt>SMTP</dt><dd>{html.escape(str(service_summary['smtp']))}</dd>
                      <dt>From</dt><dd><code>{html.escape(str(smtp_cfg.get('from') or 'not set'))}</code></dd>
                      <dt>Host</dt><dd><code>{html.escape(str(smtp_cfg.get('host') or 'not set'))}</code></dd>
                    </dl>
                  </div>
                </div>
              </section>

              <section id="maintenance" class="section">
                <div class="section-heading">
                  <h2>Maintenance</h2>
                  <span class="muted">Useful checks and operational notes.</span>
                </div>
                <div class="callout">
                  <strong>Configuration source:</strong>
                  edit <code>/etc/arcadelauncher-server.env</code> in the CT, then restart <code>arcadelauncher-server</code>.
                  Web editing is intentionally disabled for now so secrets and service-level settings stay controlled by the host.
                </div>
              </section>
            </div>
          </div>
        """
        self.send_admin_shell(message_html, admin_body)

    def send_admin_shell(self, message_html: str, admin_body: str) -> None:
        self.send_html(f"""<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>ArcadeLauncher Server</title>
  <style>
    :root {{ color-scheme: dark; --bg:#0f1115; --panel:#171b21; --panel2:#1d232b; --line:#2c3540; --text:#e8edf2; --muted:#9aa7b5; --accent:#4cc2ff; --good:#64d28a; --bad:#ff6b6b; }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; font: 14px/1.45 "Segoe UI", sans-serif; background: var(--bg); color: var(--text); }}
    header {{ display:none; }}
    main {{ width: 100%; min-height: 100vh; }}
    h1, h2, h3 {{ margin: 0; letter-spacing: 0; }}
    h1 {{ font-size: 28px; }}
    h2 {{ font-size: 20px; }}
    h3 {{ font-size: 15px; margin-bottom: 12px; color: #d8e1ea; }}
    input {{ background: #10141a; color: var(--text); border: 1px solid #3a4552; border-radius: 6px; padding: 9px 10px; min-width: 240px; }}
    button {{ background: var(--accent); color: #061018; border: 0; border-radius: 6px; padding: 9px 12px; font-weight: 700; cursor: pointer; }}
    button.danger {{ background: var(--bad); }}
    a {{ color: #8bd8ff; }}
    code {{ background: #10141a; border: 1px solid var(--line); border-radius: 4px; padding: 2px 5px; word-break: break-all; }}
    table {{ width: 100%; border-collapse: collapse; margin-top: 12px; }}
    th, td {{ text-align: left; border-bottom: 1px solid var(--line); padding: 10px 8px; vertical-align: top; }}
    th {{ color: var(--muted); font-weight: 600; font-size: 12px; text-transform: uppercase; }}
    .admin-layout {{ display: grid; grid-template-columns: 250px minmax(0, 1fr); min-height: 100vh; }}
    .sidebar {{ position: sticky; top: 0; height: 100vh; padding: 22px 16px; background: #13171d; border-right: 1px solid var(--line); }}
    .brand-block {{ display:flex; gap: 12px; align-items:center; margin-bottom: 26px; }}
    .brand-mark {{ width: 40px; height: 40px; display:grid; place-items:center; border-radius: 8px; background: var(--accent); color:#061018; font-weight: 800; }}
    .brand-title {{ font-weight: 800; }}
    .brand-subtitle, .muted, .eyebrow {{ color: var(--muted); }}
    .eyebrow {{ font-size: 12px; text-transform: uppercase; letter-spacing: .08em; }}
    nav {{ display:flex; flex-direction:column; gap: 6px; }}
    nav a {{ color: #cfdae6; text-decoration:none; padding: 10px 11px; border-radius: 6px; }}
    nav a:hover {{ background: var(--panel2); }}
    .content {{ padding: 24px; max-width: 1440px; width: 100%; }}
    .section {{ background: var(--panel); border: 1px solid var(--line); border-radius: 8px; padding: 18px; margin-bottom: 16px; }}
    .section-heading {{ display:flex; justify-content:space-between; gap: 12px; align-items:flex-start; margin-bottom: 16px; }}
    .topbar {{ display:flex; justify-content:space-between; align-items:center; background: transparent; border: 0; padding: 0 0 8px; }}
    .account-box {{ display:flex; gap: 12px; align-items:center; color: var(--muted); }}
    .buttonlink {{ background: var(--panel2); color: var(--text); border: 1px solid var(--line); border-radius: 6px; padding: 8px 12px; text-decoration: none; }}
    .metric-grid {{ display:grid; grid-template-columns: repeat(4, minmax(150px, 1fr)); gap: 12px; }}
    .metric {{ background: var(--panel2); border: 1px solid var(--line); border-radius: 8px; padding: 16px; }}
    .metric span {{ color: var(--muted); display:block; margin-bottom: 8px; }}
    .metric strong {{ font-size: 30px; line-height: 1; }}
    .split {{ display:grid; grid-template-columns: minmax(0, 1.4fr) minmax(280px, .7fr); gap: 18px; }}
    .two-col {{ display:grid; grid-template-columns: minmax(0, 1.2fr) minmax(320px, .8fr); gap: 18px; }}
    .config-grid {{ display:grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 14px; }}
    .config-card, .platform-card {{ background: var(--panel2); border: 1px solid var(--line); border-radius: 8px; padding: 15px; }}
    .platform-row {{ display:flex; justify-content:space-between; gap: 12px; padding: 8px 0; border-bottom: 1px solid var(--line); }}
    .platform-row:last-child {{ border-bottom: 0; }}
    .kv {{ display:grid; grid-template-columns: 130px minmax(0, 1fr); gap: 9px 12px; margin: 0 0 16px; }}
    .kv.compact {{ grid-template-columns: 90px minmax(0, 1fr); margin-bottom: 0; }}
    .kv dt {{ color: var(--muted); }}
    .kv dd {{ margin: 0; min-width: 0; }}
    .row {{ display: flex; gap: 10px; flex-wrap: wrap; align-items: center; }}
    .checkline {{ display:inline-flex; align-items:center; gap: 6px; color: var(--muted); }}
    .checkline input {{ min-width: 0; }}
    .stack {{ display: flex; gap: 10px; flex-direction: column; align-items: flex-start; }}
    .inline {{ display: flex; gap: 8px; flex-wrap: wrap; }}
    .token {{ max-width: 420px; display:inline-block; overflow-wrap:anywhere; }}
    .callout {{ border-left: 3px solid var(--accent); background: var(--panel2); padding: 13px 14px; border-radius: 6px; color: #dce6ef; }}
    .notice, .error {{ white-space: pre-wrap; border-radius: 6px; padding: 12px; margin: 16px 24px 0; }}
    .notice {{ background: #12331f; border: 1px solid #2c7a46; }}
    .error {{ background: #3a1618; border: 1px solid #a83c44; }}
    @media (max-width: 960px) {{
      .admin-layout, .split, .two-col, .config-grid, .metric-grid {{ grid-template-columns: 1fr; }}
      .sidebar {{ position: static; height: auto; }}
      .content {{ padding: 16px; }}
      .section-heading, .topbar, .account-box {{ flex-direction: column; align-items: flex-start; }}
    }}
  </style>
</head>
<body>
  <header><h1>ArcadeLauncher Server</h1></header>
  <main>{message_html}{admin_body}</main>
</body>
</html>""")

    def authorized_api(self) -> bool:
        if not self.auth_token:
            return True
        header = self.headers.get("Authorization", "")
        if header == f"Bearer {self.auth_token}":
            return True
        if header.startswith("Bearer "):
            token = header[len("Bearer ") :]
            return self.auth_store.validate_launcher_token(token)
        return False

    def authorized_admin(self, token: str = "") -> bool:
        token = token or self.headers.get("Authorization", "").removeprefix("Bearer ").strip()
        expected = self.admin_token or self.auth_token
        return bool(expected) and secrets.compare_digest(token, expected)

    def base_url(self) -> str:
        host = self.headers.get("Host") or f"{self.server.server_address[0]}:{self.server.server_address[1]}"
        return f"http://{host}"

    def log_message(self, fmt: str, *args: object) -> None:
        print(f"{self.address_string()} - {fmt % args}")


def main() -> int:
    parser = argparse.ArgumentParser(description="ArcadeLauncher private library server")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8721)
    parser.add_argument("--library-root", required=True, type=Path)
    parser.add_argument("--auth-token", default=os.environ.get("ARCADE_AUTH_TOKEN", ""))
    parser.add_argument("--admin-token", default=os.environ.get("ARCADE_ADMIN_TOKEN", ""))
    parser.add_argument("--admin-username", default=os.environ.get("ARCADE_ADMIN_USERNAME", "admin"))
    parser.add_argument("--admin-email", default=os.environ.get("ARCADE_ADMIN_EMAIL", ""))
    parser.add_argument("--admin-password", default=os.environ.get("ARCADE_ADMIN_PASSWORD", ""))
    args = parser.parse_args()

    auth_store = MariaDbAuthStore()
    auth_store.ensure_schema()
    auth_store.ensure_bootstrap_admin(args.admin_username, args.admin_email, args.admin_password or args.admin_token)
    library = Library(args.library_root, auth_store)
    server = ThreadingHTTPServer((args.host, args.port), ArcadeHandler)
    server.library = library  # type: ignore[attr-defined]
    server.auth_store = auth_store  # type: ignore[attr-defined]
    server.auth_token = args.auth_token  # type: ignore[attr-defined]
    server.admin_token = args.admin_token  # type: ignore[attr-defined]

    print(f"ArcadeLauncher server listening on http://{args.host}:{args.port}")
    print(f"Library root: {library.root}")
    print(f"Auth enabled: {'yes' if args.auth_token else 'no'}")
    print(f"Admin UI: http://{args.host}:{args.port}/admin")
    server.serve_forever()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
