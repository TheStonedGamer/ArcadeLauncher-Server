use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post, put},
    Form, Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use bytes::Bytes;
use cookie::Cookie;
use futures_util::{StreamExt, TryStreamExt};
use hmac::{Hmac, Mac};
use mysql_async::{params, prelude::Queryable, Pool, Row};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use reqwest::Client;
use rust_scrypt::{scrypt, Params as ScryptParams};
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncSeekExt, SeekFrom},
    process::Command,
};
use tokio_util::io::ReaderStream;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

const CHUNK_SIZE: usize = 1024 * 1024;
const SESSION_COOKIE: &str = "AL_ADMIN_SESSION";
const SESSION_TTL_SECONDS: i64 = 12 * 60 * 60;
const IGDB_CLIENT_ID_KEY: &str = "igdb.client_id";
const IGDB_CLIENT_SECRET_KEY: &str = "igdb.client_secret";
const PUBLIC_BASE_URL_KEY: &str = "server.public_base_url";
const DISCORD_WEBHOOK_KEY: &str = "discord.webhook_url";

// ============================================================================
// Source split into modules and re-assembled via include! macros below. Each
// included file lives in this crate-root scope (no use/visibility changes), so
// the compiled binary is identical to the original single-file main.rs.
// ============================================================================
include!("models.rs");
include!("db_setup.rs");
include!("auth.rs");
include!("handlers.rs");
include!("db.rs");
include!("manifest.rs");
include!("files.rs");
include!("crypto.rs");
include!("scan_jobs.rs");
include!("igdb.rs");
include!("scan.rs");
include!("admin_html.rs");
include!("discord.rs");
include!("users_api.rs");

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(env::var("RUST_LOG").unwrap_or_else(|_| "info,tower_http=warn".into()))
        .init();

    let cfg = Arc::new(Config::from_env()?);
    ensure_database(&cfg).await?;
    let db = Pool::new(cfg.database_url(true).as_str());
    ensure_schema(&db).await?;
    ensure_bootstrap_admin(&db, &cfg).await?;

    let state = AppState {
        cfg: cfg.clone(),
        db,
        scan: Arc::new(std::sync::Mutex::new(ScanStatus::default())),
        challenges: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        login_throttle: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    let public_app = Router::new()
        .route("/api/login", post(api_login))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
        .route("/api/users", get(api_users_list).post(api_users_create))
        .route("/api/users/:id", put(api_users_update).delete(api_users_delete))
        .route("/api/auth/challenge", get(api_auth_challenge))
        .route("/api/auth/verify", post(api_auth_verify))
        .route("/api/account", get(api_account))
        .route("/api/account/password", post(api_account_password))
        .route("/api/account/totp/setup", post(api_account_totp_setup))
        .route("/api/account/totp/enable", post(api_account_totp_enable))
        .route("/api/account/totp/disable", post(api_account_totp_disable))
        .route(
            "/api/account/avatar",
            get(api_account_avatar)
                .post(api_account_avatar_upload)
                .delete(api_account_avatar_delete)
                .layer(DefaultBodyLimit::max(6 * 1024 * 1024)),
        )
        .route("/api/health", get(api_health))
        .route("/api/saves/:id", get(api_saves_list))
        .route("/api/saves/:id/file", get(api_saves_get).put(api_saves_put))
        .route("/api/catalog", get(api_catalog))
        .route("/api/games/:id/manifest", get(api_manifest))
        .route("/api/games/:id/changelogs", get(api_changelogs))
        .route("/art/:id", get(download_art))
        .route("/emulators/*rel", get(download_emulator))
        .route("/files/:id/*rel", get(download_file))
        .route("/chunks/:id/:file_index/:chunk_index/*rel", get(download_chunk))
        .route("/textures/:id", get(download_texture))
        .layer(TraceLayer::new_for_http())
        .with_state(state.clone());

    let auto_rescan_state = state.clone();
    let admin_app = Router::new()
        .route("/", get(admin_page))
        .route("/admin", get(admin_page).post(admin_post))
        .route("/admin/metadata", get(admin_metadata_page).post(admin_metadata_post))
        .route("/admin/login", get(admin_page).post(admin_post))
        .route("/admin/logout", get(admin_logout))
        .route("/admin/scan-status", get(admin_scan_status))
        .route("/art/:id", get(download_art))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    let admin_addr: SocketAddr = format!("{}:{}", cfg.admin_host, cfg.admin_port).parse()?;
    info!("ArcadeLauncher API listening on http://{}", addr);
    info!("ArcadeLauncher admin listening on http://{}", admin_addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let admin_listener = tokio::net::TcpListener::bind(admin_addr).await?;
    // Event-driven catalog refresh via a filesystem watcher (replaces interval
    // polling). spawn_rescan() no-ops while a scan is already running, so an
    // in-progress manual rescan is never disturbed. If the watcher can't be set
    // up we fall back to the old interval poll (auto_rescan_secs; 0 disables).
    if !start_library_watcher(auto_rescan_state.clone()) {
        if auto_rescan_state.cfg.auto_rescan_secs > 0 {
            let st = auto_rescan_state;
            let period = st.cfg.auto_rescan_secs;
            info!("filesystem watcher unavailable; auto-rescan fallback every {}s", period);
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(period));
                // Skip the immediate first tick so startup isn't slammed with a scan.
                ticker.tick().await;
                loop {
                    ticker.tick().await;
                    let msg = spawn_rescan(&st);
                    info!("auto-rescan tick: {msg}");
                }
            });
        }
    }

    let public_server = axum::serve(listener, public_app);
    let admin_server = axum::serve(admin_listener, admin_app);
    tokio::try_join!(public_server, admin_server)?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parse_range_none_when_absent() {
        assert!(parse_range(None, 100).unwrap().is_none());
    }

    #[test]
    fn parse_range_open_ended() {
        assert_eq!(parse_range(Some("bytes=0-"), 100).unwrap(), Some((0, 99)));
    }

    #[test]
    fn parse_range_closed() {
        assert_eq!(parse_range(Some("bytes=10-19"), 100).unwrap(), Some((10, 19)));
    }

    #[test]
    fn parse_range_suffix() {
        assert_eq!(parse_range(Some("bytes=-20"), 100).unwrap(), Some((80, 99)));
    }

    #[test]
    fn parse_range_clamps_end_to_size() {
        assert_eq!(parse_range(Some("bytes=10-9999"), 100).unwrap(), Some((10, 99)));
    }

    #[test]
    fn parse_range_uses_first_of_multiple() {
        assert_eq!(parse_range(Some("bytes=0-9,20-29"), 100).unwrap(), Some((0, 9)));
    }

    #[test]
    fn parse_range_rejects_bad_unit() {
        assert!(parse_range(Some("items=0-9"), 100).is_err());
    }

    #[test]
    fn parse_range_rejects_start_past_end_of_file() {
        assert!(parse_range(Some("bytes=200-300"), 100).is_err());
    }

    #[test]
    fn parse_range_rejects_inverted() {
        assert!(parse_range(Some("bytes=50-10"), 100).is_err());
    }

    #[test]
    fn parse_range_rejects_zero_suffix() {
        assert!(parse_range(Some("bytes=-0"), 100).is_err());
    }

    #[test]
    fn constant_eq_basics() {
        assert!(constant_eq(b"abc", b"abc"));
        assert!(!constant_eq(b"abc", b"abd"));
        assert!(!constant_eq(b"abc", b"ab"));
        assert!(constant_eq(b"", b""));
    }

    #[test]
    fn encode_path_preserves_separators_and_escapes() {
        assert_eq!(encode_path("games/PC/Half Life 2"), "games/PC/Half%20Life%202");
        assert_eq!(encode_path("a/b/c"), "a/b/c");
    }

    #[test]
    fn clean_igdb_title_strips_brackets_and_separators() {
        assert_eq!(clean_igdb_title("Halo (USA) [Disc 1]"), "Halo");
        assert_eq!(clean_igdb_title("Some_Game-Title"), "Some Game Title");
    }

    #[test]
    fn normalize_title_lowercases_and_strips_punct() {
        assert_eq!(normalize_title("Grand Theft Auto: V!"), "grand theft auto v");
        assert_eq!(normalize_title("  Mega   Man  "), "mega man");
    }

    #[test]
    fn is_pc_primary_archive_accepts_archives() {
        assert!(is_pc_primary_archive(Path::new("Game.zip")));
        assert!(is_pc_primary_archive(Path::new("Game.7z")));
        assert!(is_pc_primary_archive(Path::new("Game.rar")));
        assert!(is_pc_primary_archive(Path::new("Game.7z.001")));
    }

    #[test]
    fn is_pc_primary_archive_rejects_non_archives_and_continuations() {
        assert!(!is_pc_primary_archive(Path::new("Game.exe")));
        assert!(!is_pc_primary_archive(Path::new("Game.7z.002")));
        assert!(!is_pc_primary_archive(Path::new("Game.part2.rar")));
        assert!(is_pc_primary_archive(Path::new("Game.part1.rar")));
    }

    #[test]
    fn split_segments_round_trips_and_bounds_size() {
        // Mixed ASCII + multibyte so boundary backoff is exercised.
        let s = "héllo wörld ".repeat(5000);
        // At realistic sizes (>= max UTF-8 char width) the byte bound holds exactly.
        for max in [4usize, 7, 64, 4096] {
            let segs = split_segments(&s, max);
            assert!(segs.iter().all(|seg| seg.len() <= max), "segment exceeded max={max}");
            assert_eq!(segs.concat(), s, "round-trip failed for max={max}");
        }
        // Degenerate max smaller than a char must still terminate and round-trip.
        let tiny = split_segments(&s, 1);
        assert_eq!(tiny.concat(), s);
        assert!(tiny.iter().all(|seg| !seg.is_empty()));
        // Empty input yields no segments.
        assert!(split_segments("", 16).is_empty());
    }

    #[test]
    fn stable_id_is_deterministic_and_platform_lowercased() {
        let a = stable_id("Xbox360", Path::new("games/Xbox360/Halo.iso"));
        let b = stable_id("Xbox360", Path::new("games/Xbox360/Halo.iso"));
        assert_eq!(a, b);
        assert!(a.starts_with("xbox360-"));
    }

    #[test]
    fn stable_id_normalizes_backslashes() {
        let fwd = stable_id("PC", Path::new("games/PC/Doom"));
        let back = stable_id("PC", Path::new(r"games\PC\Doom"));
        assert_eq!(fwd, back);
    }

    #[test]
    fn match_key_drops_noise_and_normalizes_separators() {
        // Dot/underscore separators and scene/region tags collapse to the core
        // title so the Jaccard pass scores a clean hit against the IGDB name.
        assert_eq!(match_key("Super.Mario.64 USA v1"), "super mario 64");
        assert_eq!(match_key("Halo_Combat_Evolved (PAL) REPACK"), "halo combat evolved");
        assert!(title_similarity(&match_key("Super.Mario.64 (USA)"), &match_key("Super Mario 64")) >= 0.60);
    }

    #[test]
    fn sha1_short_is_twelve_hex_chars() {
        let h = sha1_short("hello");
        assert_eq!(h.len(), 12);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(sha1_short("hello"), sha1_short("hello"));
    }

    #[test]
    fn clean_title_strips_tags_and_underscores() {
        assert_eq!(clean_title("Super_Mario_64 [USA] (v1.0).z64"), "Super Mario 64");
        assert_eq!(clean_title("Plain Name.iso"), "Plain Name");
    }

    #[test]
    fn human_size_scales_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
        assert_eq!(human_size(45_412_745_543), "42.3 GB");
    }

    #[test]
    fn role_str_maps_admin_flag() {
        assert_eq!(role_str(true), "Admin");
        assert_eq!(role_str(false), "User");
    }

    #[test]
    fn igdb_backoff_grows_and_caps() {
        // Monotonic-ish growth ignoring the ±1s jitter, capped near 30s.
        assert!(igdb_backoff_secs(1) <= 2);
        assert!(igdb_backoff_secs(3) >= 4 && igdb_backoff_secs(3) <= 5);
        assert!(igdb_backoff_secs(10) <= 31);
    }

    #[test]
    fn igdb_platform_ids_known_and_unknown() {
        assert_eq!(igdb_platform_ids("GameCube"), &[21]);
        assert_eq!(igdb_platform_ids("Wii"), &[5]);
        assert_eq!(igdb_platform_ids("Xbox360"), &[12]);
        assert!(igdb_platform_ids("Unknown").is_empty());
    }
}
