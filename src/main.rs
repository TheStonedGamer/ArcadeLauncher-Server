use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{delete, get, post, put},
    Form, Json, Router,
};
use base64::{engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE, Engine};
use bytes::Bytes;
use cookie::Cookie;
use futures_util::{StreamExt, TryStreamExt};
use hmac::{Hmac, Mac};
use lettre::{
    message::{header::ContentType, MultiPart, SinglePart}, transport::smtp::authentication::Credentials, AsyncSmtpTransport,
    AsyncTransport, Message, Tokio1Executor,
};
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
const DISCORD_APP_ID_KEY: &str = "discord.app_id";

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
include!("admin_extra.rs");
include!("discord.rs");
include!("users_api.rs");
include!("fanout.rs");
include!("s3.rs");
include!("devices.rs");
include!("social_api.rs");
include!("registration.rs");
include!("password_reset.rs");

// The former standalone Requests service, folded in as a real module (NOT an
// include!) so its same-named helpers stay namespaced. Mounted under /requests
// on the public app in main(). See requests_app.rs.
mod requests_app;

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

    // Optional cross-instance fan-out (ROADMAP 0.4). No-op when ARCADE_REDIS_URL
    // is unset — the gateway then runs single-instance as before.
    if let Some(url) = cfg.redis_url.clone() {
        init_fanout(&url).await;
    }

    let state = AppState {
        cfg: cfg.clone(),
        db,
        scan: Arc::new(std::sync::Mutex::new(ScanStatus::default())),
        challenges: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        login_throttle: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };
    // Build the folded-in Requests sub-app (reuses the server's DB pool) and
    // mount it under /requests. nest_service is used because the sub-app already
    // has its own state applied (Router<()>). nginx routes external /requests
    // here, so the old :8723 binary can be retired after deploy.
    let requests_router = requests_app::router(state.db.clone()).await?;

    let public_app = Router::new()
        .nest_service("/requests", requests_router)
        .route("/api/login", post(api_login))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
        .route("/api/users", get(api_users_list).post(api_users_create))
        .route("/api/users/:id", put(api_users_update).delete(api_users_delete))
        .route("/api/auth/challenge", get(api_auth_challenge))
        .route("/api/auth/verify", post(api_auth_verify))
        .route("/api/auth/register", post(api_auth_register))
        .route("/api/auth/approve", get(api_auth_approve))
        .route("/api/auth/deny", get(api_auth_deny))
        .route("/api/auth/forgot", post(api_auth_forgot))
        .route("/api/auth/reset", get(api_auth_reset_page).post(api_auth_reset_submit))
        .route("/api/account", get(api_account))
        .route("/api/account/security", get(api_account_security))
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
        .route("/api/metrics", get(api_metrics))
        .route("/api/feature-flags", get(api_feature_flags))
        .route("/api/games/search", get(api_games_search))
        .route("/api/client-config", get(api_client_config))
        .route("/api/saves/:id", get(api_saves_list))
        .route("/api/saves/:id/file", get(api_saves_get).put(api_saves_put))
        .route("/api/saves/:id/versions", get(api_saves_versions))
        .route("/api/saves/:id/version", get(api_saves_version_get))
        .route("/api/saves/:id/restore", post(api_saves_restore))
        .route("/api/config", get(api_config_all))
        .route("/api/config/:ns", get(api_config_get).post(api_config_put))
        .route("/api/catalog", get(api_catalog))
        .route("/api/games/:id/manifest", get(api_manifest))
        .route("/api/games/:id/changelogs", get(api_changelogs))
        .route("/api/social/friends", get(api_social_friends))
        .route("/api/social/friends/request", post(api_social_request))
        .route("/api/social/friends/respond", post(api_social_respond))
        .route("/api/social/friends/block", post(api_social_block))
        .route(
            "/api/social/privacy",
            get(api_social_privacy_get).put(api_social_privacy_put),
        )
        .route(
            "/api/social/ignores",
            get(api_social_ignores_get).post(api_social_ignores_post),
        )
        .route("/api/social/profile/:id", get(api_social_profile_get))
        .route("/api/social/profile", put(api_social_profile_put))
        .route(
            "/api/social/friendmeta",
            get(api_social_friendmeta_get).put(api_social_friendmeta_put),
        )
        .route("/api/social/search", get(api_social_search))
        .route("/api/social/review", put(api_social_review_put))
        .route("/api/social/review/:id", delete(api_social_review_delete))
        .route("/api/social/reviews/:id", get(api_social_reviews_get))
        .route("/api/social/activity", get(api_social_activity))
        .route("/api/social/screenshot", post(api_social_screenshot_create))
        .route("/api/social/screenshot/:id", delete(api_social_screenshot_delete))
        .route("/api/social/screenshots/:id", get(api_social_screenshots_get))
        .route("/api/social/turn", get(api_social_turn))
        .route("/api/social/attachments/presign", post(api_social_attachment_presign))
        .route("/api/social/attachments/:id", get(api_social_attachment_get))
        .route("/api/social/messages/:id", get(api_social_history))
        .route("/api/social/notifications", get(api_social_notifications))
        .route(
            "/api/social/notifications/read",
            post(api_social_notifications_read),
        )
        .route(
            "/api/social/prefs",
            get(api_social_prefs_get).post(api_social_prefs_put),
        )
        .route("/api/library/stats", get(api_library_stats))
        .route("/api/library/playtime", post(api_library_playtime))
        .route("/api/library/rating", post(api_library_rating))
        .route("/api/library/meta", post(api_library_meta))
        .route("/api/library/duplicates", get(api_library_duplicates))
        .route(
            "/api/library/launch-profiles",
            get(api_library_launch_profiles),
        )
        .route(
            "/api/library/launch-profile",
            post(api_library_launch_profile_put),
        )
        .route(
            "/api/library/collections",
            get(api_library_collections).post(api_library_collection_create),
        )
        .route(
            "/api/library/collections/:id",
            put(api_library_collection_update).delete(api_library_collection_delete),
        )
        .route(
            "/api/library/collections/:id/items",
            post(api_library_collection_items),
        )
        .route("/ws/social", get(ws_social))
        .route("/api/emulators", get(api_emulators))
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
        .route("/admin/accounts", get(admin_accounts_page).post(admin_post))
        .route("/admin/requests", get(admin_requests_page).post(admin_post))
        .route("/admin/social-test", get(admin_social_test_page).post(admin_post))
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

    #[test]
    fn registration_validation_rules() {
        // Happy path.
        assert!(validate_registration("player_one", "p1@example.com", "longenough1").is_ok());
        // Username length bounds.
        assert!(validate_registration("ab", "a@b.co", "longenough1").is_err());
        assert!(validate_registration(&"x".repeat(33), "a@b.co", "longenough1").is_err());
        // Must start alphanumeric and only allowed chars.
        assert!(validate_registration("_leading", "a@b.co", "longenough1").is_err());
        assert!(validate_registration("has space", "a@b.co", "longenough1").is_err());
        assert!(validate_registration("bad$char", "a@b.co", "longenough1").is_err());
        // Dotted/underscored/hyphenated names allowed.
        assert!(validate_registration("a.b-c_d", "a@b.co", "longenough1").is_ok());
        // Email shape.
        assert!(validate_registration("good", "no-at-sign", "longenough1").is_err());
        assert!(validate_registration("good", "two@@x.com", "longenough1").is_err());
        assert!(validate_registration("good", "nodot@localhost", "longenough1").is_err());
        // Password minimum length.
        assert!(validate_registration("good", "a@b.co", "short").is_err());
    }

    #[test]
    fn email_plausibility() {
        assert!(is_plausible_email("a@b.co"));
        assert!(is_plausible_email("orlandb204567@outlook.com"));
        assert!(!is_plausible_email("a@b"));
        assert!(!is_plausible_email("@b.co"));
        assert!(!is_plausible_email("a@.co"));
        // Surrounding whitespace is trimmed, so a trailing space is accepted.
        assert!(is_plausible_email("a@b.co "));
        // Internal whitespace is rejected.
        assert!(!is_plausible_email("a b@c.co"));
        assert!(!is_plausible_email(""));
    }

    #[test]
    fn registration_email_carries_both_links() {
        let (subject, body, _html) = registration_email(
            "newbie",
            "n@example.com",
            "1.2.3.4",
            "https://arcade.example/api/auth/approve?token=AAA",
            "https://arcade.example/api/auth/deny?token=AAA",
        );
        assert!(subject.contains("newbie"));
        assert!(body.contains("n@example.com"));
        assert!(body.contains("approve?token=AAA"));
        assert!(body.contains("deny?token=AAA"));
        assert!(body.contains("1.2.3.4"));
        // IP line is omitted when unknown.
        let (_, body2, _) = registration_email("x", "x@y.zz", "", "http://a", "http://d");
        assert!(!body2.contains("From IP:"));
    }

    #[test]
    fn username_normalization() {
        assert_eq!(normalize_username("  PlayerOne "), "playerone");
    }

    #[test]
    fn turn_credentials_match_coturn_rest_format() {
        // username = "<expiry>:<userId>"; credential = base64(HMAC-SHA1(secret, username)).
        let (user, cred) = turn_credentials("s3cr3t", 42, 1_700_000_000);
        assert_eq!(user, "1700000000:42");
        // Stable, recomputable value (verified against an independent HMAC-SHA1).
        assert_eq!(cred, "4m+gopKcl0HXw6KRLkDWn63gfQE=");
        // Same inputs are deterministic; a different expiry changes the credential.
        assert_eq!(turn_credentials("s3cr3t", 42, 1_700_000_000), (user, cred));
        assert_ne!(turn_credentials("s3cr3t", 42, 1_700_000_001).1, "4m+gopKcl0HXw6KRLkDWn63gfQE=");
    }
}
