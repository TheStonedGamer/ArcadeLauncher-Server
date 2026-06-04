#!/usr/bin/env python3
"""Generate ArcadeLauncher catalog.json from a mounted ROM library."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import tempfile
import time
from pathlib import Path


PLATFORMS = [
    ("Nintendo/NES", "NES", {".nes", ".fds", ".unf", ".unif"}),
    ("Nintendo/SNES", "SNES", {".sfc", ".smc", ".fig", ".bs", ".st"}),
    ("Nintendo/N64", "N64", {".z64", ".n64", ".v64", ".rom"}),
    ("Nintendo/Switch", "Ryujinx", {".nsp", ".xci", ".nca", ".nro"}),
    ("Nintendo/Gamecube", "Dolphin", {".iso", ".gcm", ".rvz", ".gcz"}),
    ("Nintendo/Wii", "Dolphin", {".iso", ".rvz", ".gcz", ".wbfs", ".dol", ".elf"}),
]

XBOX360_ROOT = Path("Microsoft/Xbox 360")
XBOX360_GOD_DIRS = {"00007000", "0007000"}
PC_ARCHIVE_ROOTS = [Path("PC/Steam")]
PC_ARCHIVE_SUFFIXES = {".zip", ".7z", ".rar"}
SKIP_SUFFIXES = {".sqlite", ".db", ".txt", ".nfo", ".jpg", ".jpeg", ".png", ".webp"}


def clean_title(name: str) -> str:
    title = Path(name).stem
    title = re.sub(r"\[[^\]]*\]", "", title)
    title = re.sub(r"\([^)]*\)", "", title)
    title = re.sub(r"\s+", " ", title.replace("_", " ")).strip(" .-_")
    return title or Path(name).stem


def stable_id(platform: str, relative: Path) -> str:
    digest = hashlib.sha1(relative.as_posix().encode("utf-8")).hexdigest()[:12]
    return f"{platform.lower()}-{digest}"


def version_for(path: Path) -> str:
    if path.is_file():
        stat = path.stat()
        return hashlib.sha1(f"{path.name}:{stat.st_size}:{int(stat.st_mtime)}".encode()).hexdigest()[:12]

    h = hashlib.sha1()
    for file_path in sorted(path.rglob("*")):
        if not file_path.is_file():
            continue
        stat = file_path.stat()
        rel = file_path.relative_to(path).as_posix()
        h.update(f"{rel}:{stat.st_size}:{int(stat.st_mtime)}\n".encode("utf-8"))
    return h.hexdigest()[:12]


def game_entry(
    library_root: Path,
    content_path: Path,
    platform: str,
    title: str,
    target: Path,
    install_type: str = "emulator_rom",
    arguments: str = "{rom}",
) -> dict:
    relative_content = content_path.relative_to(library_root)
    return {
        "id": stable_id(platform, relative_content),
        "title": title,
        "platform": platform,
        "installType": install_type,
        "version": version_for(content_path),
        "contentPath": relative_content.as_posix(),
        "launch": {
            "target": target.as_posix(),
            "arguments": arguments,
        },
    }


def scan_single_file_platforms(library_root: Path) -> list[dict]:
    games = []
    games_root = library_root / "games"
    for relative_dir, platform, extensions in PLATFORMS:
        platform_root = games_root / relative_dir
        if not platform_root.exists():
            continue
        for path in sorted(platform_root.rglob("*")):
            if not path.is_file():
                continue
            suffix = path.suffix.lower()
            if suffix in SKIP_SUFFIXES or suffix not in extensions:
                continue
            games.append(game_entry(library_root, path, platform, clean_title(path.name), Path(path.name)))
    return games


def find_god_package(god_dir: Path) -> Path | None:
    for child in sorted(god_dir.iterdir()):
        if not child.is_file() or child.suffix:
            continue
        if (god_dir / f"{child.name}.data").is_dir():
            return child
    return None


def scan_xbox360_god(library_root: Path) -> list[dict]:
    games = []
    xbox_root = library_root / "games" / XBOX360_ROOT
    if not xbox_root.exists():
        return games

    seen_roots: set[Path] = set()
    for god_dir in sorted(path for path in xbox_root.rglob("*") if path.is_dir() and path.name in XBOX360_GOD_DIRS):
        package = find_god_package(god_dir)
        if not package:
            continue

        relative_god_dir = god_dir.relative_to(xbox_root)
        game_root = xbox_root / relative_god_dir.parts[0]
        if game_root in seen_roots:
            continue
        seen_roots.add(game_root)

        target = package.relative_to(game_root)
        games.append(game_entry(library_root, game_root, "Xbox360", clean_title(game_root.name), target))
    return games


def scan_pc_archives(library_root: Path) -> list[dict]:
    games = []
    games_root = library_root / "games"
    for relative_dir in PC_ARCHIVE_ROOTS:
        archive_root = games_root / relative_dir
        if not archive_root.exists():
            continue
        for path in sorted(archive_root.rglob("*")):
            if not path.is_file() or path.suffix.lower() not in PC_ARCHIVE_SUFFIXES:
                continue
            games.append(
                game_entry(
                    library_root,
                    path,
                    "Repacks",
                    clean_title(path.name),
                    Path(""),
                    "pc_archive",
                    "{exe}",
                )
            )
    return games


def write_catalog(path: Path, games: list[dict]) -> None:
    catalog = {
        "schemaVersion": 1,
        "generatedBy": "server/generate_catalog.py",
        "games": sorted(games, key=lambda g: (g["platform"], g["title"].lower(), g["id"])),
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile("w", encoding="utf-8", dir=path.parent, delete=False) as tmp:
        json.dump(catalog, tmp, indent=2)
        tmp.write("\n")
        tmp_path = Path(tmp.name)
    tmp_path.chmod(0o644)
    tmp_path.replace(path)


def sync_mariadb(games: list[dict]) -> bool:
    try:
        import pymysql
    except ImportError:
        return False

    host = os.environ.get("ARCADE_DB_HOST", "127.0.0.1")
    port = int(os.environ.get("ARCADE_DB_PORT", "3306") or "3306")
    database = os.environ.get("ARCADE_DB_NAME", "arcadelauncher")
    user = os.environ.get("ARCADE_DB_USER", "arcade")
    password = os.environ.get("ARCADE_DB_PASSWORD", "")
    conn = pymysql.connect(
        host=host,
        port=port,
        user=user,
        password=password,
        database=database,
        charset="utf8mb4",
        autocommit=True,
    )
    now = int(time.time())
    seen = [game["id"] for game in games]
    with conn:
        with conn.cursor() as cur:
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
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate ArcadeLauncher catalog.json")
    parser.add_argument("--library-root", required=True, type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    library_root = args.library_root.resolve()
    output = args.output or library_root / "catalog.json"
    games = scan_single_file_platforms(library_root)
    games.extend(scan_xbox360_god(library_root))
    games.extend(scan_pc_archives(library_root))
    write_catalog(output, games)
    db_synced = sync_mariadb(games)

    by_platform: dict[str, int] = {}
    for game in games:
        by_platform[game["platform"]] = by_platform.get(game["platform"], 0) + 1
    print(f"Wrote {len(games)} games to {output}")
    print(f"MariaDB sync: {'yes' if db_synced else 'no'}")
    for platform, count in sorted(by_platform.items()):
        print(f"{platform}: {count}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
