use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
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

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    db: Pool,
    scan: Arc<std::sync::Mutex<ScanStatus>>,
    // username -> (nonce, expires_at) for single-use challenge-response login.
    challenges: Arc<std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>>,
}

// Live progress for the background catalog scan/hash/enrich task.
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct ScanStatus {
    running: bool,
    phase: String, // idle | scanning | hashing | igdb | done | error
    message: String,
    total: usize,
    processed: usize,
    #[serde(default)]
    active: usize, // games currently being hashed in parallel (hashing phase)
    current: String,
    started_at: i64, // epoch seconds when the current/last run began (0 = never)
    updated_at: i64, // epoch seconds of the last progress update
    #[serde(default)]
    per_platform: BTreeMap<String, PlatformProgress>, // hashing progress per platform
}

// Per-platform hashing progress, surfaced live in the admin scan-status UI.
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct PlatformProgress {
    total: usize,
    processed: usize,
}

impl AppState {
    fn set_scan<F: FnOnce(&mut ScanStatus)>(&self, f: F) {
        if let Ok(mut s) = self.scan.lock() {
            f(&mut s);
            s.updated_at = now();
        }
    }
    fn scan_snapshot(&self) -> ScanStatus {
        self.scan.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[derive(Clone)]
struct Config {
    host: String,
    port: u16,
    admin_host: String,
    admin_port: u16,
    library_root: PathBuf,
    auto_rescan_secs: u64,
    auth_token: String,
    admin_username: String,
    admin_email: String,
    admin_password: String,
    db_host: String,
    db_port: u16,
    db_name: String,
    db_user: String,
    db_password: String,
}

impl Config {
    fn from_env() -> Result<Self> {
        let args: Vec<String> = env::args().collect();
        let mut host = env_string("ARCADE_HOST", "0.0.0.0");
        let mut port = env_u16("ARCADE_PORT", 8721);
        let mut admin_host = env_string("ARCADE_ADMIN_HOST", "127.0.0.1");
        let mut admin_port = env_u16("ARCADE_ADMIN_PORT", 8722);
        let mut library_root = PathBuf::from(env_string("ARCADE_LIBRARY_ROOT", "/srv/arcade-library"));
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--host" if i + 1 < args.len() => {
                    host = args[i + 1].clone();
                    i += 2;
                }
                "--port" if i + 1 < args.len() => {
                    port = args[i + 1].parse().context("invalid --port")?;
                    i += 2;
                }
                "--admin-host" if i + 1 < args.len() => {
                    admin_host = args[i + 1].clone();
                    i += 2;
                }
                "--admin-port" if i + 1 < args.len() => {
                    admin_port = args[i + 1].parse().context("invalid --admin-port")?;
                    i += 2;
                }
                "--library-root" if i + 1 < args.len() => {
                    library_root = PathBuf::from(&args[i + 1]);
                    i += 2;
                }
                _ => i += 1,
            }
        }
        Ok(Self {
            host,
            port,
            admin_host,
            admin_port,
            library_root,
            auto_rescan_secs: env_u64("ARCADE_AUTO_RESCAN_SECS", 1800),
            auth_token: env_string("ARCADE_AUTH_TOKEN", ""),
            admin_username: env_string("ARCADE_ADMIN_USERNAME", "admin"),
            admin_email: env_string("ARCADE_ADMIN_EMAIL", ""),
            admin_password: env_string("ARCADE_ADMIN_PASSWORD", ""),
            db_host: env_string("ARCADE_DB_HOST", "127.0.0.1"),
            db_port: env_u16("ARCADE_DB_PORT", 3306),
            db_name: env_string("ARCADE_DB_NAME", "arcadelauncher"),
            db_user: env_string("ARCADE_DB_USER", "arcade"),
            db_password: env_string("ARCADE_DB_PASSWORD", ""),
        })
    }

    fn database_url(&self, with_db: bool) -> String {
        let db = if with_db { format!("/{}", self.db_name) } else { String::new() };
        format!(
            "mysql://{}:{}@{}:{}{}",
            urlencoding::encode(&self.db_user),
            urlencoding::encode(&self.db_password),
            self.db_host,
            self.db_port,
            db
        )
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Game {
    id: String,
    title: String,
    platform: String,
    install_type: String,
    version: String,
    content_path: String,
    cover_art_url: String,
    igdb_id: u64,
    summary: String,
    genres: String,
    igdb_rating: f64,
    release_date: i64,
    launch: Launch,
}

#[derive(Debug, Clone, Serialize)]
struct Launch {
    target: String,
    arguments: String,
}

#[derive(Debug, Clone)]
struct IgdbMatch {
    id: u64,
    name: String,
    summary: String,
    genres: String,
    rating: f64,
    release_date: i64,
    cover_image_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    schema_version: u8,
    generated_by: String,
    games: Vec<Game>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u8,
    id: String,
    title: String,
    platform: String,
    install_type: String,
    version: String,
    cover_art_url: String,
    igdb_id: u64,
    launch: Launch,
    files: Vec<ManifestFile>,
}

#[derive(Serialize)]
struct ManifestFile {
    path: String,
    size: u64,
    sha256: String,
    url: String,
    chunk_size: usize,
    chunks: Vec<ManifestChunk>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManifestChunk {
    index: usize,
    offset: u64,
    size: u64,
    sha256: String,
    compression: String,
    url: String,
}

// Precomputed, URL-free manifest data persisted in `game_manifests` during scan.
// URLs depend on the request's Host header, so they are filled in at request time.
#[derive(Serialize, Deserialize, Clone)]
struct StoredChunk {
    index: usize,
    offset: u64,
    size: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct StoredFile {
    path: String,
    size: u64,
    sha256: String,
    chunks: Vec<StoredChunk>,
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    #[serde(default, alias = "totpCode")]
    totp_code: String,
}

#[derive(Deserialize)]
struct AdminForm {
    action: String,
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
    is_admin: Option<String>,
    user_id: Option<u64>,
    setting_key: Option<String>,
    setting_value: Option<String>,
    service_name: Option<String>,
    totp_code: Option<String>,
    game_id: Option<String>,
    search_query: Option<String>,
    igdb_id: Option<u64>,
}

#[derive(Default, Deserialize)]
struct MetadataQuery {
    game_id: Option<String>,
    search_query: Option<String>,
}

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
    };
    let public_app = Router::new()
        .route("/api/login", post(api_login))
        .route("/api/auth/challenge", get(api_auth_challenge))
        .route("/api/auth/verify", post(api_auth_verify))
        .route("/api/health", get(api_health))
        .route("/api/catalog", get(api_catalog))
        .route("/api/games/:id/manifest", get(api_manifest))
        .route("/art/:id", get(download_art))
        .route("/emulators/*rel", get(download_emulator))
        .route("/files/:id/*rel", get(download_file))
        .route("/chunks/:id/:file_index/:chunk_index/*rel", get(download_chunk))
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
    // Periodic auto-rescan so moving/renaming folders can't leave stale catalog
    // rows for long. spawn_rescan() no-ops while a scan is already running, so an
    // in-progress manual rescan is never disturbed. 0 disables the timer.
    if auto_rescan_state.cfg.auto_rescan_secs > 0 {
        let st = auto_rescan_state;
        let period = st.cfg.auto_rescan_secs;
        info!("auto-rescan enabled every {}s", period);
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

    let public_server = axum::serve(listener, public_app);
    let admin_server = axum::serve(admin_listener, admin_app);
    tokio::try_join!(public_server, admin_server)?;
    Ok(())
}

async fn ensure_database(cfg: &Config) -> Result<()> {
    validate_db_identifier(&cfg.db_name)?;
    let pool = Pool::new(cfg.database_url(false).as_str());
    let mut conn = pool.get_conn().await?;
    conn.query_drop(format!(
        "CREATE DATABASE IF NOT EXISTS `{}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        cfg.db_name.replace('`', "``")
    ))
    .await?;
    drop(conn);
    pool.disconnect().await?;
    Ok(())
}

async fn ensure_schema(db: &Pool) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS admin_users (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          username VARCHAR(80) NOT NULL UNIQUE,
          email VARCHAR(255) NOT NULL UNIQUE,
          password_hash VARCHAR(255) NOT NULL,
          is_admin BOOLEAN NOT NULL DEFAULT TRUE,
          enabled BOOLEAN NOT NULL DEFAULT TRUE,
          created_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT TRUE").await;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN totp_secret VARCHAR(64) NULL").await;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN totp_enabled BOOLEAN NOT NULL DEFAULT FALSE").await;
    // Password-derived shared key for client challenge-response auth (hex SHA-256).
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN auth_key CHAR(64) NULL").await;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS launcher_tokens (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          name VARCHAR(160) NOT NULL,
          user_id BIGINT UNSIGNED NULL,
          token_hash CHAR(64) NOT NULL UNIQUE,
          token_plain TEXT NULL,
          enabled BOOLEAN NOT NULL DEFAULT TRUE,
          created_at BIGINT NOT NULL,
          INDEX (user_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    let _ = c.query_drop("ALTER TABLE launcher_tokens ADD COLUMN user_id BIGINT UNSIGNED NULL").await;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS admin_sessions (
          token_hash CHAR(64) NOT NULL PRIMARY KEY,
          admin_id BIGINT UNSIGNED NOT NULL,
          expires_at BIGINT NOT NULL,
          created_at BIGINT NOT NULL,
          INDEX (admin_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS password_resets (
          token_hash CHAR(64) NOT NULL PRIMARY KEY,
          admin_id BIGINT UNSIGNED NOT NULL,
          expires_at BIGINT NOT NULL,
          created_at BIGINT NOT NULL,
          INDEX (admin_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS games (
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
          summary TEXT NULL,
          genres TEXT NULL,
          igdb_rating DOUBLE NOT NULL DEFAULT 0,
          release_date BIGINT NOT NULL DEFAULT 0,
          updated_at BIGINT NOT NULL,
          INDEX idx_games_platform_title (platform, title)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN summary TEXT NULL").await;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN genres TEXT NULL").await;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN igdb_rating DOUBLE NOT NULL DEFAULT 0").await;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN release_date BIGINT NOT NULL DEFAULT 0").await;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS server_settings (
          setting_key VARCHAR(120) NOT NULL PRIMARY KEY,
          setting_value TEXT NOT NULL,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS game_manifests (
          game_id VARCHAR(96) NOT NULL PRIMARY KEY,
          version VARCHAR(80) NOT NULL,
          files_json LONGTEXT NOT NULL,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    Ok(())
}

async fn ensure_bootstrap_admin(db: &Pool, cfg: &Config) -> Result<()> {
    if cfg.admin_username.is_empty() || cfg.admin_email.is_empty() || cfg.admin_password.is_empty() {
        return Ok(());
    }
    let mut c = db.get_conn().await?;
    let count: Option<u64> = c.query_first("SELECT COUNT(*) FROM admin_users").await?;
    if count.unwrap_or(0) == 0 {
        let hash = hash_password_argon2(&cfg.admin_password)?;
        let auth_key = derive_auth_key(&cfg.admin_username, &cfg.admin_password);
        c.exec_drop(
            "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at,auth_key) VALUES (:u,:e,:p,TRUE,TRUE,:t,:k)",
            params! {"u" => &cfg.admin_username, "e" => &cfg.admin_email, "p" => hash, "t" => now(), "k" => auth_key},
        )
        .await?;
    }
    Ok(())
}

async fn api_health(State(_): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true, "schemaVersion": 1, "version": env!("CARGO_PKG_VERSION"), "backend": "rust"}))
}

async fn api_login(State(st): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    match find_user(&st.db, &form.username).await {
        Ok(Some(user)) if verify_password_any(&form.password, &user.password_hash)
            && verify_user_totp(&user, &form.totp_code) => {
            match issue_user_token(&st.db, user.id, &user.username).await {
                Ok(token) => Json(serde_json::json!({"token": token, "username": user.username, "isAdmin": user.is_admin})).into_response(),
                Err(e) => server_error(e),
            }
        }
        _ => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid username, password, or 2FA code"}))).into_response(),
    }
}

#[derive(Deserialize)]
struct ChallengeQuery {
    username: String,
}

#[derive(Deserialize)]
struct VerifyForm {
    username: String,
    proof: String,
    #[serde(default, alias = "totpCode")]
    totp_code: String,
}

// Password-derived shared secret, identical on client and server.
// key = SHA-256( lowercase(username) || 0x1f || password )
fn derive_auth_key(username: &str, password: &str) -> String {
    let mut h = Sha256::new();
    h.update(username.trim().to_lowercase().as_bytes());
    h.update([0x1fu8]);
    h.update(password.as_bytes());
    hex::encode(h.finalize())
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256>>::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

// HMAC-SHA256 counter-mode keystream XOR. Symmetric: encrypt == decrypt.
fn hmac_ctr_xor(key: &[u8], iv: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut counter: u32 = 0;
    let mut block: Vec<u8> = Vec::new();
    let mut bi = 32usize;
    for &b in data {
        if bi >= 32 {
            let mut msg = Vec::with_capacity(iv.len() + 4);
            msg.extend_from_slice(iv);
            msg.extend_from_slice(&counter.to_be_bytes());
            block = hmac_sha256(key, &msg);
            counter = counter.wrapping_add(1);
            bi = 0;
        }
        out.push(b ^ block[bi]);
        bi += 1;
    }
    out
}

async fn api_auth_challenge(State(st): State<AppState>, Query(q): Query<ChallengeQuery>) -> Response {
    let nonce = random_token(24);
    let username = q.username.trim().to_lowercase();
    if let Ok(mut map) = st.challenges.lock() {
        let cutoff = now();
        map.retain(|_, (_, exp)| *exp > cutoff);
        map.insert(username, (nonce.clone(), now() + 120));
    }
    Json(serde_json::json!({"nonce": nonce})).into_response()
}

async fn api_auth_verify(State(st): State<AppState>, Form(form): Form<VerifyForm>) -> Response {
    let username = form.username.trim().to_lowercase();
    // Pop the nonce (single use).
    let nonce = match st.challenges.lock() {
        Ok(mut map) => match map.remove(&username) {
            Some((n, exp)) if exp > now() => n,
            _ => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "challenge expired; request a new one"}))).into_response(),
        },
        Err(_) => return server_error(anyhow!("challenge store poisoned")),
    };
    let user = match find_user(&st.db, &form.username).await {
        Ok(Some(u)) => u,
        _ => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid credentials"}))).into_response(),
    };
    let auth_key_hex = match user_auth_key(&st.db, user.id).await {
        Ok(Some(k)) if !k.is_empty() => k,
        _ => return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "challenge-response not available for this account; use password login"}))).into_response(),
    };
    let Ok(key_bytes) = hex::decode(&auth_key_hex) else {
        return server_error(anyhow!("corrupt auth key"));
    };
    let expected = hex::encode(hmac_sha256(&key_bytes, nonce.as_bytes()));
    if !constant_eq(expected.as_bytes(), form.proof.trim().as_bytes()) || !verify_user_totp(&user, &form.totp_code) {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid credentials or 2FA code"}))).into_response();
    }
    let token = match issue_user_token(&st.db, user.id, &user.username).await {
        Ok(t) => t,
        Err(e) => return server_error(e),
    };
    // Encrypt the token so it never travels in cleartext: HMAC-CTR keyed by the
    // password-derived secret, with a fresh random IV per response.
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let ciphertext = hmac_ctr_xor(&key_bytes, &iv, token.as_bytes());
    Json(serde_json::json!({
        "iv": hex::encode(iv),
        "token": hex::encode(ciphertext),
        "username": user.username,
        "isAdmin": user.is_admin,
    })).into_response()
}

async fn user_auth_key(db: &Pool, user_id: u64) -> Result<Option<String>> {
    let mut c = db.get_conn().await?;
    let row: Option<Option<String>> = c
        .exec_first("SELECT auth_key FROM admin_users WHERE id=:id", params! {"id" => user_id})
        .await?;
    Ok(row.flatten())
}

async fn api_catalog(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    match list_games(&st.db).await {
        Ok(mut games) => {
            let base = public_base_url(&st, &headers).await;
            for game in &mut games {
                hydrate_server_art_url(&st, &base, game).await;
            }
            Json(Catalog { schema_version: 1, generated_by: "mariadb-rust".into(), games }).into_response()
        },
        Err(e) => server_error(e),
    }
}

async fn api_manifest(State(st): State<AppState>, headers: HeaderMap, AxumPath(id): AxumPath<String>) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    let game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "game not found"}))).into_response(),
        Err(e) => return server_error(e),
    };
    match manifest_for(&st, &headers, &game).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => {
            // A stale catalog row whose files moved/were deleted surfaces as an
            // io NotFound while hashing on demand. Return a clear 404 (not a raw
            // 500) so the client can tell the user to ask for a rescan.
            if e.downcast_ref::<std::io::Error>()
                .map(|io| io.kind() == std::io::ErrorKind::NotFound)
                .unwrap_or(false)
            {
                warn!("manifest_for: content missing on disk for {} -> {e}", game.id);
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "error": "game files are missing on the server; ask the admin to rescan the catalog"
                    })),
                )
                    .into_response();
            }
            server_error(e)
        }
    }
}

async fn download_file(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath((id, rel)): AxumPath<(String, String)>,
) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    let game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "game not found").into_response(),
        Err(e) => return server_error(e),
    };
    let file_path = match file_path_for(&st.cfg, &game, &rel).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    if !fs::metadata(&file_path).await.map(|m| m.is_file()).unwrap_or(false) {
        warn!("download_file: missing on disk for {} -> {}", game.id, file_path.display());
        return (StatusCode::NOT_FOUND, "file no longer exists on the server; ask the admin to rescan the catalog").into_response();
    }
    match stream_file(file_path, headers.get(header::RANGE)).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn download_chunk(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath((id, file_index, chunk_index, rel)): AxumPath<(String, usize, usize, String)>,
) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    let game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "game not found").into_response(),
        Err(e) => return server_error(e),
    };
    let file_path = match file_path_for(&st.cfg, &game, &rel).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    if !fs::metadata(&file_path).await.map(|m| m.is_file()).unwrap_or(false) {
        warn!("download_chunk: missing on disk for {} -> {}", game.id, file_path.display());
        return (StatusCode::NOT_FOUND, "file no longer exists on the server; ask the admin to rescan the catalog").into_response();
    }
    match stream_chunk(file_path, file_index, chunk_index).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn admin_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(admin)) => Html(admin_html(&st, Some(admin), "", "", "").await.unwrap_or_else(|e| format!("error: {e}"))).into_response(),
        _ => Html(login_html("")).into_response(),
    }
}

async fn admin_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, SESSION_COOKIE) {
        let _ = delete_session(&st.db, &token).await;
    }
    let mut r = Redirect::to("/admin/login").into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("AL_ADMIN_SESSION=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0"),
    );
    r
}

async fn admin_metadata_page(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<MetadataQuery>,
) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(admin)) => {
            Html(metadata_page_html(
                &st,
                Some(admin),
                "",
                q.game_id.as_deref().unwrap_or(""),
                q.search_query.as_deref().unwrap_or(""),
            ).await.unwrap_or_else(|e| format!("error: {e}"))).into_response()
        }
        _ => Html(login_html("Please sign in first.")).into_response(),
    }
}

async fn admin_metadata_post(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<AdminForm>) -> Response {
    let admin = match current_admin(&st.db, &headers).await {
        Ok(Some(a)) => a,
        _ => return Html(login_html("Please sign in first.")).into_response(),
    };
    let game_id = form.game_id.clone().unwrap_or_default();
    let search_query = form.search_query.clone().unwrap_or_default();
    let msg = match form.action.as_str() {
        "igdb_search" => "Search complete. Choose a match below.".to_string(),
        "igdb_apply" => match form.igdb_id {
            Some(igdb_id) => match apply_manual_igdb_match(&st, &game_id, igdb_id).await {
                Ok(title) => format!("Applied IGDB metadata to {title}."),
                Err(e) => e.to_string(),
            },
            None => "missing IGDB match id".to_string(),
        },
        _ => "No action taken.".to_string(),
    };
    Html(metadata_page_html(&st, Some(admin), &msg, &game_id, &search_query).await.unwrap_or_else(|e| format!("error: {e}"))).into_response()
}

async fn admin_post(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<AdminForm>) -> Response {
    if form.action == "login" {
        let username = form.username.unwrap_or_default();
        let password = form.password.unwrap_or_default();
        match find_user(&st.db, &username).await {
            Ok(Some(user)) if user.is_admin && verify_password_any(&password, &user.password_hash)
                && verify_user_totp(&user, form.totp_code.as_deref().unwrap_or("")) => {
                match create_session(&st.db, user.id).await {
                    Ok(token) => {
                        let mut r = Redirect::to("/admin").into_response();
                        if let Ok(cookie) = HeaderValue::from_str(&session_cookie_value(&token)) {
                            r.headers_mut().insert(header::SET_COOKIE, cookie);
                        }
                        r
                    }
                    Err(e) => server_error(e),
                }
            }
            _ => Html(login_html("Invalid username, password, or 2FA code.")).into_response(),
        }
    } else {
        let admin = match current_admin(&st.db, &headers).await {
            Ok(Some(a)) => a,
            _ => return Html(login_html("Please sign in first.")).into_response(),
        };
        let matcher_game_id = form.game_id.clone().unwrap_or_default();
        let matcher_query = form.search_query.clone().unwrap_or_default();
        let msg = match form.action.as_str() {
            "add_user" => {
                let username = form.username.unwrap_or_default();
                let email = form.email.unwrap_or_default();
                let password = form.password.unwrap_or_default();
                if username.is_empty() || email.is_empty() || password.len() < 10 {
                    "username, email, and a 10+ character password are required".to_string()
                } else {
                    match create_user(&st.db, &username, &email, &password, form.is_admin.as_deref() == Some("1")).await {
                        Ok(_) => format!("Created user {username}."),
                        Err(e) => e.to_string(),
                    }
                }
            }
            "rotate_user" => match form.user_id {
                Some(id) => rotate_launcher_token(&st.db, id).await.map(|_| "Rotated user token.".to_string()).unwrap_or_else(|e| e.to_string()),
                None => "missing token id".to_string(),
            },
            "delete_user" => match form.user_id {
                Some(id) => delete_launcher_token(&st.db, id).await.map(|_| "Deleted user token.".to_string()).unwrap_or_else(|e| e.to_string()),
                None => "missing token id".to_string(),
            },
            "enable_totp" => match form.user_id {
                Some(id) => enable_user_totp(&st.db, id).await.unwrap_or_else(|e| e.to_string()),
                None => "missing user id".to_string(),
            },
            "disable_totp" => match form.user_id {
                Some(id) => disable_user_totp(&st.db, id).await.map(|_| "Disabled 2FA.".to_string()).unwrap_or_else(|e| e.to_string()),
                None => "missing user id".to_string(),
            },
            "rescan" => spawn_rescan(&st),
            "igdb_enrich" => match enrich_catalog_from_igdb(&st, false).await {
                Ok(out) => out,
                Err(e) => e.to_string(),
            },
            "igdb_refresh" => match enrich_catalog_from_igdb(&st, true).await {
                Ok(out) => out,
                Err(e) => e.to_string(),
            },
            "igdb_search" => "IGDB search results are shown in Metadata Matcher.".to_string(),
            "igdb_apply" => {
                let game_id = form.game_id.unwrap_or_default();
                match form.igdb_id {
                    Some(igdb_id) => match apply_manual_igdb_match(&st, &game_id, igdb_id).await {
                        Ok(title) => format!("Applied IGDB metadata to {title}."),
                        Err(e) => e.to_string(),
                    },
                    None => "missing IGDB match id".to_string(),
                }
            },
            "validate_games" => match validate_games(&st).await {
                Ok(report) => report.to_message(),
                Err(e) => e.to_string(),
            },
            "restart_service" => {
                let name = form.service_name.unwrap_or_default();
                match restart_service(&name).await {
                    Ok(msg) => msg,
                    Err(e) => e.to_string(),
                }
            },
            "save_setting" => {
                let key = form.setting_key.unwrap_or_default();
                let value = form.setting_value.unwrap_or_default();
                match save_server_setting(&st.db, &key, &value).await {
                    Ok(_) => format!("Saved setting {key}. Some runtime/env settings may require a service restart."),
                    Err(e) => e.to_string(),
                }
            },
            _ => "No action taken.".to_string(),
        };
        Html(admin_html(&st, Some(admin), &msg, &matcher_game_id, &matcher_query).await.unwrap_or_else(|e| format!("error: {e}"))).into_response()
    }
}

#[derive(Debug)]
struct User {
    id: u64,
    username: String,
    email: String,
    password_hash: String,
    is_admin: bool,
    enabled: bool,
    totp_secret: Option<String>,
    totp_enabled: bool,
}

fn user_from_row(row: Row) -> User {
    let (id, username, email, password_hash, is_admin, enabled, totp_secret, totp_enabled): (u64, String, String, String, bool, bool, Option<String>, bool) = mysql_async::from_row(row);
    User { id, username, email, password_hash, is_admin, enabled, totp_secret, totp_enabled }
}

async fn find_user(db: &Pool, key: &str) -> Result<Option<User>> {
    let mut c = db.get_conn().await?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT id, username, email, password_hash, is_admin, enabled, totp_secret, totp_enabled FROM admin_users WHERE enabled = TRUE AND (username = :k OR email = :k) LIMIT 1",
            params! {"k" => key.trim()},
        )
        .await?;
    Ok(row.map(user_from_row))
}

async fn list_users(db: &Pool) -> Result<Vec<User>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<Row> = c.query("SELECT id, username, email, password_hash, is_admin, enabled, totp_secret, totp_enabled FROM admin_users ORDER BY username").await?;
    Ok(rows.into_iter().map(user_from_row).collect())
}

async fn create_user(db: &Pool, username: &str, email: &str, password: &str, is_admin: bool) -> Result<()> {
    let mut c = db.get_conn().await?;
    let hash = hash_password_argon2(password)?;
    let auth_key = derive_auth_key(username, password);
    c.exec_drop(
        "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at,auth_key) VALUES (:u,:e,:p,:a,TRUE,:t,:k)",
        params! {"u" => username, "e" => email, "p" => hash, "a" => is_admin, "t" => now(), "k" => auth_key},
    )
    .await?;
    Ok(())
}

async fn issue_user_token(db: &Pool, user_id: u64, username: &str) -> Result<String> {
    let token = random_token(36);
    let token_hash = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await?;
    let existing: Option<u64> = c.exec_first("SELECT id FROM launcher_tokens WHERE user_id = :id LIMIT 1", params! {"id" => user_id}).await?;
    if let Some(id) = existing {
        c.exec_drop(
            "UPDATE launcher_tokens SET name=:n, token_hash=:h, token_plain=:p, enabled=TRUE, created_at=:t WHERE id=:id",
            params! {"n" => username, "h" => token_hash, "p" => &token, "t" => now(), "id" => id},
        )
        .await?;
    } else {
        c.exec_drop(
            "INSERT INTO launcher_tokens (name,user_id,token_hash,token_plain,enabled,created_at) VALUES (:n,:u,:h,:p,TRUE,:t)",
            params! {"n" => username, "u" => user_id, "h" => token_hash, "p" => &token, "t" => now()},
        )
        .await?;
    }
    Ok(token)
}

async fn validate_launcher_token(db: &Pool, token: &str) -> bool {
    let hash = sha256_hex(token.as_bytes());
    let Ok(mut c) = db.get_conn().await else { return false; };
    let row: Result<Option<u64>, _> = c.exec_first("SELECT id FROM launcher_tokens WHERE token_hash=:h AND enabled=TRUE LIMIT 1", params! {"h" => hash}).await;
    row.ok().flatten().is_some()
}

async fn rotate_launcher_token(db: &Pool, id: u64) -> Result<()> {
    let token = random_token(36);
    let hash = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE launcher_tokens SET token_hash=:h, token_plain=:p, created_at=:t WHERE id=:id",
        params! {"h" => hash, "p" => token, "t" => now(), "id" => id},
    )
    .await?;
    Ok(())
}

async fn delete_launcher_token(db: &Pool, id: u64) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop("DELETE FROM launcher_tokens WHERE id=:id", params! {"id" => id}).await?;
    Ok(())
}

async fn create_session(db: &Pool, admin_id: u64) -> Result<String> {
    let token = random_token(36);
    let hash = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await?;
    let ts = now();
    c.exec_drop("DELETE FROM admin_sessions WHERE expires_at <= :t", params! {"t" => ts}).await?;
    c.exec_drop(
        "INSERT INTO admin_sessions (token_hash,admin_id,expires_at,created_at) VALUES (:h,:a,:e,:t)",
        params! {"h" => hash, "a" => admin_id, "e" => ts + SESSION_TTL_SECONDS, "t" => ts},
    )
    .await?;
    Ok(token)
}

async fn current_admin(db: &Pool, headers: &HeaderMap) -> Result<Option<User>> {
    let Some(token) = cookie_value(headers, SESSION_COOKIE) else { return Ok(None); };
    let hash = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await?;
    let ts = now();
    c.exec_drop("DELETE FROM admin_sessions WHERE expires_at <= :t", params! {"t" => ts}).await?;
    let row: Option<Row> = c
        .exec_first(
            r#"SELECT a.id,a.username,a.email,a.password_hash,a.is_admin,a.enabled,a.totp_secret,a.totp_enabled
               FROM admin_sessions s JOIN admin_users a ON a.id=s.admin_id
               WHERE s.token_hash=:h AND s.expires_at > :t AND a.enabled=TRUE AND a.is_admin=TRUE LIMIT 1"#,
            params! {"h" => hash, "t" => ts},
        )
        .await?;
    Ok(row.map(user_from_row))
}

async fn delete_session(db: &Pool, token: &str) -> Result<()> {
    let hash = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await?;
    c.exec_drop("DELETE FROM admin_sessions WHERE token_hash=:h", params! {"h" => hash}).await?;
    Ok(())
}

async fn list_launcher_tokens(db: &Pool) -> Result<Vec<(u64, String, String, bool)>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<Row> = c.query("SELECT id,name,COALESCE(token_plain,''),enabled FROM launcher_tokens ORDER BY name").await?;
    Ok(rows.into_iter().map(mysql_async::from_row).collect())
}

async fn list_server_settings(db: &Pool) -> Result<Vec<(String, String)>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<Row> = c.query("SELECT setting_key,setting_value FROM server_settings ORDER BY setting_key").await?;
    Ok(rows.into_iter().map(mysql_async::from_row).collect())
}

async fn setting_value(db: &Pool, key: &str) -> Result<Option<String>> {
    let mut c = db.get_conn().await?;
    Ok(c.exec_first("SELECT setting_value FROM server_settings WHERE setting_key=:k", params! {"k" => key}).await?)
}

async fn save_server_setting(db: &Pool, key: &str, value: &str) -> Result<()> {
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() {
        return Err(anyhow!("setting key is required"));
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return Err(anyhow!("setting key may only contain letters, numbers, dash, underscore, or dot"));
    }
    if is_sensitive_key(key) && value.is_empty() {
        return Err(anyhow!("blank sensitive settings are not saved"));
    }
    let mut c = db.get_conn().await?;
    c.exec_drop(
        r#"INSERT INTO server_settings (setting_key,setting_value,updated_at)
           VALUES (:k,:v,:t)
           ON DUPLICATE KEY UPDATE setting_value=:v, updated_at=:t"#,
        params! {"k" => key, "v" => value, "t" => now()},
    )
    .await?;
    Ok(())
}

async fn list_games(db: &Pool) -> Result<Vec<Game>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<Row> = c
        .query(
            "SELECT id,title,platform,install_type,version,content_path,launch_target,launch_arguments,COALESCE(cover_art_url,''),igdb_id,COALESCE(summary,''),COALESCE(genres,''),igdb_rating,release_date FROM games ORDER BY platform,title,id",
        )
        .await?;
    Ok(rows.into_iter().map(game_from_row).collect())
}

async fn find_game(db: &Pool, id: &str) -> Result<Option<Game>> {
    let mut c = db.get_conn().await?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT id,title,platform,install_type,version,content_path,launch_target,launch_arguments,COALESCE(cover_art_url,''),igdb_id,COALESCE(summary,''),COALESCE(genres,''),igdb_rating,release_date FROM games WHERE id=:id",
            params! {"id" => id},
        )
        .await?;
    Ok(row.map(game_from_row))
}

fn game_from_row(row: Row) -> Game {
    let id = row.get::<String, _>(0).unwrap_or_default();
    let title = row.get::<String, _>(1).unwrap_or_default();
    let platform = row.get::<String, _>(2).unwrap_or_default();
    let install_type = row.get::<String, _>(3).unwrap_or_default();
    let version = row.get::<String, _>(4).unwrap_or_default();
    let content_path = row.get::<String, _>(5).unwrap_or_default();
    let launch_target = row.get::<String, _>(6).unwrap_or_default();
    let launch_arguments = row.get::<String, _>(7).unwrap_or_default();
    let cover_art_url = row.get::<String, _>(8).unwrap_or_default();
    let igdb_id = row.get::<u64, _>(9).unwrap_or_default();
    let summary = row.get::<String, _>(10).unwrap_or_default();
    let genres = row.get::<String, _>(11).unwrap_or_default();
    let igdb_rating = row.get::<f64, _>(12).unwrap_or_default();
    let release_date = row.get::<i64, _>(13).unwrap_or_default();
    Game {
        id,
        title,
        platform,
        install_type,
        version,
        content_path,
        cover_art_url,
        igdb_id,
        summary,
        genres,
        igdb_rating,
        release_date,
        launch: Launch { target: launch_target, arguments: if launch_arguments.is_empty() { "{rom}".into() } else { launch_arguments } },
    }
}

async fn download_art(
    State(st): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "game not found").into_response(),
        Err(e) => return server_error(e),
    };
    let root = match content_path_for(&st.cfg, &game).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let cached = server_cover_path(&st.cfg.library_root, &game.id);
    let path = if fs::metadata(&cached).await.map(|m| m.is_file()).unwrap_or(false) {
        Some(cached)
    } else {
        find_local_cover(&root).await
    };
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "art not found").into_response();
    };
    match stream_file(path, None).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn download_emulator(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    let path = match safe_join(&st.cfg.library_root.join("emulators"), &rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match stream_file(path, headers.get(header::RANGE)).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn authorized_api(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = auth.strip_prefix("Bearer ").map(str::trim) else {
        return false;
    };
    (!st.cfg.auth_token.is_empty() && constant_eq(token.as_bytes(), st.cfg.auth_token.as_bytes()))
        || validate_launcher_token(&st.db, token).await
}

async fn manifest_for(st: &AppState, headers: &HeaderMap, game: &Game) -> Result<Manifest> {
    let mut game = game.clone();
    let base = public_base_url(st, headers).await;
    hydrate_server_art_url(st, &base, &mut game).await;

    // Use precomputed hashes from the DB when they match the current version.
    // Fall back to hashing on demand (and persist) only when missing/stale —
    // this avoids re-hashing tens of GB on every manifest request.
    let stored = match load_stored_manifest(&st.db, &game.id, &game.version).await {
        Ok(Some(files)) => files,
        _ => {
            let files = compute_stored_files(&st.cfg, &game).await?;
            if let Err(e) = store_manifest(&st.db, &game.id, &game.version, &files).await {
                error!("failed to persist manifest for {}: {e}", game.id);
            }
            files
        }
    };

    let mut manifest_files = Vec::new();
    for (file_index, sf) in stored.iter().enumerate() {
        let rel = sf.path.clone();
        let chunks = sf
            .chunks
            .iter()
            .map(|c| ManifestChunk {
                index: c.index,
                offset: c.offset,
                size: c.size,
                sha256: c.sha256.clone(),
                compression: "none".into(),
                url: format!(
                    "{}/chunks/{}/{}/{}/{}",
                    base,
                    urlencoding::encode(&game.id),
                    file_index,
                    c.index,
                    encode_path(&rel)
                ),
            })
            .collect();
        manifest_files.push(ManifestFile {
            path: rel.clone(),
            size: sf.size,
            sha256: sf.sha256.clone(),
            url: format!("{}/files/{}/{}", base, urlencoding::encode(&game.id), encode_path(&rel)),
            chunk_size: CHUNK_SIZE,
            chunks,
        });
    }
    Ok(Manifest {
        schema_version: 1,
        id: game.id.clone(),
        title: game.title.clone(),
        platform: game.platform.clone(),
        install_type: game.install_type.clone(),
        version: game.version.clone(),
        cover_art_url: game.cover_art_url.clone(),
        igdb_id: game.igdb_id,
        launch: game.launch.clone(),
        files: manifest_files,
    })
}

// Hash every file of a game once, producing the full-file sha256 and per-chunk
// hashes in a single read pass. This is the expensive work that must happen
// during scan/sync, never inside a manifest request.
async fn compute_stored_files(cfg: &Config, game: &Game) -> Result<Vec<StoredFile>> {
    let root = content_path_for(cfg, game).await?;
    let (files, rel_root) = if fs::metadata(&root).await?.is_file() {
        (vec![root.clone()], root.parent().unwrap_or(&cfg.library_root).to_path_buf())
    } else {
        (walk_files(&root).await?, root.clone())
    };
    // Hash files in parallel across blocking threads. SHA-256 over many GB is
    // CPU-bound, so we fan out one spawn_blocking task per file instead of
    // hashing every file serially on one core. A process-wide semaphore
    // (`hash_semaphore`) caps the total in-flight hashes to the host's
    // parallelism, so this composes safely with the game-level fan-out in
    // `ensure_manifests` without oversubscribing the CPU.
    let concurrency = hash_concurrency();
    let mut out: Vec<StoredFile> = futures_util::stream::iter(files.into_iter().map(|path| {
        let rel_root = rel_root.clone();
        async move {
            let _permit = hash_semaphore().acquire().await.expect("hash semaphore closed");
            tokio::task::spawn_blocking(move || hash_file_blocking(&path, &rel_root))
                .await
                .map_err(|e| anyhow!("hash task panicked: {e}"))?
        }
    }))
    .buffer_unordered(concurrency)
    .try_collect()
    .await?;
    // buffer_unordered yields in completion order; sort by path so file_index is
    // deterministic across rescans.
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

// Number of files to hash concurrently. Capped so a giant single library scan
// can't oversubscribe the box; falls back to 4 if parallelism is unknown.
fn hash_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

// Process-wide cap on concurrent file hashes. Shared by the per-file fan-out in
// `compute_stored_files` and the per-game fan-out in `ensure_manifests` so the
// two nested layers never schedule more CPU-bound hashes than the box has cores.
fn hash_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(hash_concurrency()))
}

// Synchronous single-file hash: full-file sha256 plus per-chunk sha256 in one
// sequential read pass. Runs inside spawn_blocking so it never stalls the async
// runtime. Missing files surface as io::Error(NotFound), which callers map to a
// 404 "rescan" hint rather than a 500.
fn hash_file_blocking(path: &Path, rel_root: &Path) -> Result<StoredFile> {
    use std::io::Read;
    let rel = path
        .strip_prefix(rel_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut file_hasher = Sha256::new();
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut offset = 0u64;
    let mut index = 0usize;
    while offset < size {
        let want = ((size - offset).min(CHUNK_SIZE as u64)) as usize;
        file.read_exact(&mut buf[..want])?;
        file_hasher.update(&buf[..want]);
        let mut ch = Sha256::new();
        ch.update(&buf[..want]);
        chunks.push(StoredChunk {
            index,
            offset,
            size: want as u64,
            sha256: hex::encode(ch.finalize()),
        });
        offset += want as u64;
        index += 1;
    }
    Ok(StoredFile {
        path: rel,
        size,
        sha256: hex::encode(file_hasher.finalize()),
        chunks,
    })
}

async fn load_stored_manifest(db: &Pool, game_id: &str, version: &str) -> Result<Option<Vec<StoredFile>>> {
    let mut c = db.get_conn().await?;
    let row: Option<(String, String)> = c
        .exec_first(
            "SELECT version, files_json FROM game_manifests WHERE game_id=:id",
            params! {"id" => game_id},
        )
        .await?;
    match row {
        Some((v, json)) if v == version => Ok(Some(serde_json::from_str(&json)?)),
        _ => Ok(None),
    }
}

async fn store_manifest(db: &Pool, game_id: &str, version: &str, files: &[StoredFile]) -> Result<()> {
    let json = serde_json::to_string(files)?;
    let mut c = db.get_conn().await?;
    c.exec_drop(
        r#"INSERT INTO game_manifests (game_id, version, files_json, updated_at)
           VALUES (:id, :version, :json, :ts)
           ON DUPLICATE KEY UPDATE version=VALUES(version), files_json=VALUES(files_json), updated_at=VALUES(updated_at)"#,
        params! {"id" => game_id, "version" => version, "json" => json, "ts" => now()},
    )
    .await?;
    Ok(())
}

// Precompute (or refresh) stored manifests for every game whose version changed.
// Called from the scan/sync path so manifest requests are instant.
async fn ensure_manifests(st: &AppState, games: &[Game]) -> Result<()> {
    let mut platform_totals = BTreeMap::<String, PlatformProgress>::new();
    for game in games {
        platform_totals.entry(game.platform.clone()).or_default().total += 1;
    }
    st.set_scan(|s| {
        s.phase = "hashing".to_string();
        s.total = games.len();
        s.processed = 0;
        s.active = 0;
        s.current = String::new();
        s.message = "Generating file hashes…".to_string();
        s.per_platform = platform_totals.clone();
    });
    // Hash games concurrently. The shared `hash_semaphore` bounds total CPU work,
    // so single-file ISOs (one file each) still saturate every core because
    // multiple games hash at once, while multi-file repacks fan out internally.
    let game_concurrency = hash_concurrency();
    futures_util::stream::iter(games.iter())
        .for_each_concurrent(game_concurrency, |game| async move {
            // Only games whose stored manifest is missing/stale need hashing. We
            // report `current`/`active` for those alone, so the status reflects
            // real hashing work instead of flickering through instant cache hits.
            let needs_hash = !matches!(
                load_stored_manifest(&st.db, &game.id, &game.version).await,
                Ok(Some(_))
            );
            if needs_hash {
                st.set_scan(|s| {
                    s.active += 1;
                    s.current = game.title.clone();
                });
                match compute_stored_files(&st.cfg, game).await {
                    Ok(files) => {
                        if let Err(e) = store_manifest(&st.db, &game.id, &game.version, &files).await {
                            error!("failed to persist manifest for {}: {e}", game.id);
                        }
                    }
                    Err(e) => error!("failed to hash files for {}: {e}", game.id),
                }
                st.set_scan(|s| { s.active = s.active.saturating_sub(1); });
            }
            let platform = game.platform.clone();
            st.set_scan(|s| {
                s.processed += 1;
                s.per_platform.entry(platform).or_default().processed += 1;
                if s.active == 0 {
                    s.current = String::new();
                }
            });
        })
        .await;
    // Drop stale manifests for games no longer present.
    let ids: HashSet<&str> = games.iter().map(|g| g.id.as_str()).collect();
    let mut c = st.db.get_conn().await?;
    let existing: Vec<String> = c.query("SELECT game_id FROM game_manifests").await?;
    for id in existing {
        if !ids.contains(id.as_str()) {
            c.exec_drop("DELETE FROM game_manifests WHERE game_id=:id", params! {"id" => id}).await?;
        }
    }
    Ok(())
}

async fn content_path_for(cfg: &Config, game: &Game) -> Result<PathBuf> {
    safe_join(&cfg.library_root, &game.content_path)
}

async fn file_path_for(cfg: &Config, game: &Game, rel: &str) -> Result<PathBuf> {
    let root = content_path_for(cfg, game).await?;
    if fs::metadata(&root).await?.is_file() {
        let requested = Path::new(rel).file_name().and_then(|s| s.to_str()).unwrap_or("");
        let actual = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if requested != actual {
            return Err(anyhow!("invalid file path"));
        }
        Ok(root)
    } else {
        safe_join(&root, rel)
    }
}

async fn stream_file(path: PathBuf, range: Option<&HeaderValue>) -> Result<Response> {
    let meta = fs::metadata(&path).await?;
    if !meta.is_file() {
        return Err(anyhow!("file not found"));
    }
    let size = meta.len();
    let parsed = parse_range(range.and_then(|h| h.to_str().ok()), size)?;
    let (start, end, status) = if let Some((s, e)) = parsed {
        (s, e, StatusCode::PARTIAL_CONTENT)
    } else {
        (0, size.saturating_sub(1), StatusCode::OK)
    };
    let len = end.saturating_sub(start).saturating_add(1);
    let mut file = File::open(&path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let stream = ReaderStream::new(file.take(len)).map_ok(Bytes::from);
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime_guess::from_path(&path).first_or_octet_stream().as_ref())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    Ok(resp.body(Body::from_stream(stream))?)
}

async fn stream_chunk(path: PathBuf, _file_index: usize, chunk_index: usize) -> Result<Response> {
    let meta = fs::metadata(&path).await?;
    if !meta.is_file() {
        return Err(anyhow!("file not found"));
    }
    let size = meta.len();
    let start = (chunk_index as u64).saturating_mul(CHUNK_SIZE as u64);
    if start >= size {
        return Err(anyhow!("chunk out of range"));
    }
    let len = ((CHUNK_SIZE as u64).min(size - start)) as u64;
    let mut file = File::open(&path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let stream = ReaderStream::new(file.take(len)).map_ok(Bytes::from);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len.to_string())
        .body(Body::from_stream(stream))?)
}

#[allow(dead_code)]
async fn chunks_for_file(
    path: &Path,
    base: &str,
    game_id: &str,
    file_index: usize,
    rel: &str,
    size: u64,
) -> Result<Vec<ManifestChunk>> {
    let mut out = Vec::new();
    let mut file = File::open(path).await?;
    let mut offset = 0u64;
    let mut index = 0usize;
    let mut buf = vec![0u8; CHUNK_SIZE];
    while offset < size {
        let want = ((size - offset).min(CHUNK_SIZE as u64)) as usize;
        file.read_exact(&mut buf[..want]).await?;
        let mut hasher = Sha256::new();
        hasher.update(&buf[..want]);
        out.push(ManifestChunk {
            index,
            offset,
            size: want as u64,
            sha256: hex::encode(hasher.finalize()),
            compression: "none".into(),
            url: format!(
                "{}/chunks/{}/{}/{}/{}",
                base,
                urlencoding::encode(game_id),
                file_index,
                index,
                encode_path(rel)
            ),
        });
        offset += want as u64;
        index += 1;
    }
    Ok(out)
}

fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>> {
    let Some(header) = header else { return Ok(None); };
    let Some(spec) = header.strip_prefix("bytes=") else { return Err(anyhow!("unsupported range unit")); };
    let spec = spec.split(',').next().unwrap_or("").trim();
    let Some((start_s, end_s)) = spec.split_once('-') else { return Err(anyhow!("invalid range")); };
    if start_s.is_empty() {
        let suffix: u64 = end_s.parse()?;
        if suffix == 0 {
            return Err(anyhow!("invalid suffix range"));
        }
        return Ok(Some((size.saturating_sub(suffix), size.saturating_sub(1))));
    }
    let start: u64 = start_s.parse()?;
    let end = if end_s.is_empty() { size.saturating_sub(1) } else { end_s.parse()? };
    if start >= size || end < start {
        return Err(anyhow!("range not satisfiable"));
    }
    Ok(Some((start, end.min(size.saturating_sub(1)))))
}

async fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

async fn walk_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                out.push(path.clone());
                stack.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[allow(dead_code)]
async fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = urlencoding::decode(relative)?.replace('\\', "/");
    let rel_path = Path::new(&rel);
    if rel_path.is_absolute() {
        return Err(anyhow!("invalid path"));
    }
    let mut out = root.to_path_buf();
    for c in rel_path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return Err(anyhow!("invalid path")),
        }
    }
    Ok(out)
}

fn hash_password_argon2(password: &str) -> Result<String> {
    use argon2::{
        password_hash::{PasswordHasher, SaltString},
        Argon2,
    };
    let salt = SaltString::generate(&mut rand::thread_rng());
    Ok(Argon2::default().hash_password(password.as_bytes(), &salt)?.to_string())
}

fn verify_password_any(password: &str, stored: &str) -> bool {
    if stored.starts_with("scrypt$") {
        verify_scrypt(password, stored)
    } else if stored.starts_with("$argon2") {
        use argon2::{password_hash::PasswordHash, Argon2, PasswordVerifier};
        PasswordHash::new(stored)
            .ok()
            .and_then(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).ok())
            .is_some()
    } else {
        false
    }
}

async fn hydrate_server_art_url(st: &AppState, base: &str, game: &mut Game) {
    if game.cover_art_url.starts_with("http://") || game.cover_art_url.starts_with("https://") {
        return;
    }
    if game.cover_art_url == "local" || game.cover_art_url.is_empty() {
        if fs::metadata(server_cover_path(&st.cfg.library_root, &game.id)).await.map(|m| m.is_file()).unwrap_or(false) {
            game.cover_art_url = format!("{}/art/{}", base, urlencoding::encode(&game.id));
            return;
        }
        if let Ok(root) = content_path_for(&st.cfg, game).await {
            if find_local_cover(&root).await.is_some() {
                game.cover_art_url = format!("{}/art/{}", base, urlencoding::encode(&game.id));
            }
        }
    }
}

async fn find_local_cover(root: &Path) -> Option<PathBuf> {
    let dir = if fs::metadata(root).await.ok()?.is_file() {
        root.parent()?.to_path_buf()
    } else {
        root.to_path_buf()
    };
    const COVER_NAMES: &[&str] = &[
        "cover.jpg", "cover.jpeg", "cover.png", "cover.webp",
        "folder.jpg", "folder.jpeg", "folder.png", "folder.webp",
        "poster.jpg", "poster.jpeg", "poster.png", "poster.webp",
        "boxart.jpg", "boxart.jpeg", "boxart.png", "boxart.webp",
    ];
    for name in COVER_NAMES {
        let p = dir.join(name);
        if fs::metadata(&p).await.map(|m| m.is_file()).unwrap_or(false) {
            return Some(p);
        }
    }
    let mut stack = vec![dir];
    let mut scanned = 0usize;
    while let Some(current) = stack.pop() {
        scanned += 1;
        if scanned > 200 {
            break;
        }
        let Ok(mut entries) = fs::read_dir(&current).await else { continue; };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            let Ok(meta) = entry.metadata().await else { continue; };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
            let lower = name.to_ascii_lowercase();
            if COVER_NAMES.iter().any(|candidate| *candidate == lower) {
                return Some(path);
            }
        }
    }
    None
}

async fn apply_sidecar_metadata(root: &Path, game: &mut Game) {
    let dir = match fs::metadata(root).await {
        Ok(m) if m.is_file() => root.parent().map(Path::to_path_buf),
        Ok(_) => Some(root.to_path_buf()),
        Err(_) => None,
    };
    let Some(dir) = dir else { return; };
    for name in ["arcadelauncher.metadata.json", "metadata.json"] {
        let path = dir.join(name);
        let Ok(text) = fs::read_to_string(&path).await else { continue; };
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else { continue; };
        if let Some(v) = json.get("coverArtUrl").or_else(|| json.get("cover_art_url")).and_then(|v| v.as_str()) {
            game.cover_art_url = v.to_string();
        }
        if let Some(v) = json.get("summary").and_then(|v| v.as_str()) {
            game.summary = v.to_string();
        }
        if let Some(v) = json.get("genres").and_then(|v| v.as_str()) {
            game.genres = v.to_string();
        } else if let Some(arr) = json.get("genres").and_then(|v| v.as_array()) {
            game.genres = arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
        }
        if let Some(v) = json.get("igdbRating").or_else(|| json.get("igdb_rating")).and_then(|v| v.as_f64()) {
            game.igdb_rating = v;
        }
        if let Some(v) = json.get("releaseDate").or_else(|| json.get("release_date")).and_then(|v| v.as_i64()) {
            game.release_date = v;
        }
        if let Some(v) = json.get("igdbId").or_else(|| json.get("igdb_id")).and_then(|v| v.as_u64()) {
            game.igdb_id = v;
        }
        if let Some(v) = json.get("launchTarget").or_else(|| json.get("launch_target")).and_then(|v| v.as_str()) {
            game.launch.target = v.replace('\\', "/");
        }
        if let Some(v) = json.get("launchArguments").or_else(|| json.get("launch_arguments")).and_then(|v| v.as_str()) {
            game.launch.arguments = v.to_string();
        }
        break;
    }
    if game.cover_art_url.is_empty() && find_local_cover(root).await.is_some() {
        game.cover_art_url = "local".into();
    }
}

fn verify_user_totp(user: &User, code: &str) -> bool {
    if !user.totp_enabled {
        return true;
    }
    let Some(secret) = user.totp_secret.as_deref() else { return false; };
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() != 6 {
        return false;
    }
    let now_step = now() / 30;
    for step in [now_step - 1, now_step, now_step + 1] {
        if matches!(totp_code(secret, step as u64), Ok(expected) if expected == digits) {
            return true;
        }
    }
    false
}

fn totp_code(secret_b32: &str, step: u64) -> Result<String> {
    type HmacSha1 = Hmac<Sha1>;
    let key = base32_decode(secret_b32)?;
    let mut msg = [0u8; 8];
    msg.copy_from_slice(&step.to_be_bytes());
    let mut mac = HmacSha1::new_from_slice(&key).map_err(|_| anyhow!("invalid TOTP key"))?;
    mac.update(&msg);
    let digest = mac.finalize().into_bytes();
    let offset = (digest[19] & 0x0f) as usize;
    let bin = (((digest[offset] & 0x7f) as u32) << 24)
        | ((digest[offset + 1] as u32) << 16)
        | ((digest[offset + 2] as u32) << 8)
        | (digest[offset + 3] as u32);
    Ok(format!("{:06}", bin % 1_000_000))
}

fn base32_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            let idx = ((buffer >> (bits - 5)) & 31) as usize;
            out.push(ALPHABET[idx] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        let idx = ((buffer << (5 - bits)) & 31) as usize;
        out.push(ALPHABET[idx] as char);
    }
    out
}

fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = match c.to_ascii_uppercase() {
            'A'..='Z' => c.to_ascii_uppercase() as u8 - b'A',
            '2'..='7' => c as u8 - b'2' + 26,
            _ => return Err(anyhow!("invalid base32 secret")),
        } as u32;
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buffer >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Ok(out)
}

async fn enable_user_totp(db: &Pool, user_id: u64) -> Result<String> {
    let mut secret = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut secret);
    let encoded = base32_encode(&secret);
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE admin_users SET totp_secret=:s, totp_enabled=TRUE WHERE id=:id",
        params! {"s" => &encoded, "id" => user_id},
    ).await?;
    let user: Option<(String, String)> = c.exec_first(
        "SELECT username,email FROM admin_users WHERE id=:id",
        params! {"id" => user_id},
    ).await?;
    let account = user.map(|(u, e)| if e.is_empty() { u } else { e }).unwrap_or_else(|| user_id.to_string());
    let uri = format!(
        "otpauth://totp/ArcadeLauncher:{}?secret={}&issuer=ArcadeLauncher&algorithm=SHA1&digits=6&period=30",
        urlencoding::encode(&account),
        encoded
    );
    Ok(format!("Enabled 2FA. Add this authenticator URI:\n{uri}"))
}

async fn disable_user_totp(db: &Pool, user_id: u64) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE admin_users SET totp_secret=NULL, totp_enabled=FALSE WHERE id=:id",
        params! {"id" => user_id},
    ).await?;
    Ok(())
}

fn verify_scrypt(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 4 || parts[0] != "scrypt" {
        return false;
    }
    let Ok(salt) = URL_SAFE.decode(parts[2]) else { return false; };
    let Ok(expected) = URL_SAFE.decode(parts[3]) else { return false; };
    let Ok(params) = ScryptParams::new(14, 8, 1, expected.len()) else { return false; };
    let mut out = vec![0u8; expected.len()];
    if scrypt(password.as_bytes(), &salt, &params, &mut out).is_err() {
        return false;
    }
    constant_eq(&out, &expected)
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn random_token(len: usize) -> String {
    rand::thread_rng().sample_iter(&Alphanumeric).take(len * 2).map(char::from).collect()
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let cookie = Cookie::parse(part.trim().to_string()).ok()?;
        if cookie.name() == name {
            return Some(cookie.value().to_string());
        }
    }
    None
}

fn session_cookie_value(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECONDS}")
}

fn base_url(headers: &HeaderMap, cfg: &Config) -> String {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    let proto = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    if host == "localhost" {
        format!("http://{}:{}", cfg.host, cfg.port)
    } else {
        format!("{proto}://{host}")
    }
}

async fn public_base_url(st: &AppState, headers: &HeaderMap) -> String {
    if let Ok(Some(value)) = setting_value(&st.db, PUBLIC_BASE_URL_KEY).await {
        let value = value.trim().trim_end_matches('/');
        if value.starts_with("https://") || value.starts_with("http://") {
            return value.to_string();
        }
    }
    base_url(headers, &st.cfg)
}

fn encode_path(path: &str) -> String {
    path.split('/').map(urlencoding::encode).collect::<Vec<_>>().join("/")
}

fn server_cover_path(library_root: &Path, game_id: &str) -> PathBuf {
    library_root.join(".arcadelauncher").join("art").join(format!("{}.jpg", safe_file_part(game_id)))
}

fn igdb_cover_url(image_id: &str) -> String {
    format!("https://images.igdb.com/igdb/image/upload/t_cover_big/{image_id}.jpg")
}

fn admin_cover_src(game: &Game) -> String {
    if game.cover_art_url == "local" {
        format!("/art/{}", urlencoding::encode(&game.id))
    } else {
        game.cover_art_url.clone()
    }
}

fn igdb_platform_ids(platform: &str) -> &'static [i32] {
    match platform {
        "Dolphin" => &[21, 5],
        "GameCube" => &[21],
        "Wii" => &[5],
        "Ryujinx" => &[130],
        "RPCS3" => &[9],
        "N64" => &[4],
        "NES" => &[18],
        "SNES" => &[19],
        "PS1" => &[7],
        "PS2" => &[8],
        "Xbox360" => &[12],
        "Xbox" => &[11],
        _ => &[],
    }
}

fn clean_igdb_title(title: &str) -> String {
    let without_brackets = title
        .split(['(', '['])
        .next()
        .unwrap_or(title)
        .replace('_', " ")
        .replace('-', " ");
    without_brackets.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn normalize_title(title: &str) -> String {
    title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_similarity(a: &str, b: &str) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    if a == b {
        return 1.0;
    }
    if a.contains(b) || b.contains(a) {
        return 0.85;
    }
    let aw: HashSet<&str> = a.split_whitespace().collect();
    let bw: HashSet<&str> = b.split_whitespace().collect();
    let common = aw.intersection(&bw).count() as f64;
    let total = aw.union(&bw).count() as f64;
    if total == 0.0 { 0.0 } else { common / total }
}

fn safe_file_part(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn validate_db_identifier(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 64 {
        return Err(anyhow!("database name must be 1-64 characters"));
    }
    if !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(anyhow!("database name may only contain ASCII letters, numbers, and underscore"));
    }
    Ok(())
}

fn sanitize_search_query(query: &str) -> Result<String> {
    let cleaned = query
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        return Err(anyhow!("search query is required"));
    }
    if cleaned.chars().count() > 120 {
        return Err(anyhow!("search query is too long"));
    }
    Ok(cleaned)
}

fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    k.contains("secret") || k.contains("password") || k.contains("token")
}

fn masked_value(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let len = value.chars().count();
    if len <= 8 {
        return "********".into();
    }
    let head = value.chars().take(4).collect::<String>();
    let tail = value.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    format!("{head}...{tail}")
}

fn env_string(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_u16(name: &str, default: u16) -> u16 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response()
}

fn server_error(e: impl std::fmt::Display) -> Response {
    error!("{e}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response()
}

// Kick off a catalog rescan on a background task. Returns immediately; progress
// is tracked in `st.scan` and exposed via GET /admin/scan-status.
fn spawn_rescan(st: &AppState) -> String {
    {
        let mut guard = match st.scan.lock() {
            Ok(g) => g,
            Err(_) => return "Scan status unavailable.".to_string(),
        };
        if guard.running {
            return "A rescan is already in progress.".to_string();
        }
        *guard = ScanStatus {
            running: true,
            phase: "scanning".to_string(),
            message: "Starting catalog scan…".to_string(),
            started_at: now(),
            updated_at: now(),
            ..Default::default()
        };
    }
    let st = st.clone();
    tokio::spawn(async move {
        match rescan_catalog(&st).await {
            Ok(msg) => st.set_scan(|s| {
                s.running = false;
                s.phase = "done".to_string();
                s.current = String::new();
                s.message = msg;
            }),
            Err(e) => st.set_scan(|s| {
                s.running = false;
                s.phase = "error".to_string();
                s.current = String::new();
                s.message = format!("Rescan failed: {e}");
            }),
        }
    });
    "Catalog rescan started in the background.".to_string()
}

async fn rescan_catalog(st: &AppState) -> Result<String> {
    st.set_scan(|s| { s.phase = "scanning".to_string(); s.message = "Scanning library…".to_string(); });
    let games = scan_catalog(&st.cfg.library_root).await?;
    sync_catalog_db(&st.db, &games).await?;
    if let Err(e) = ensure_manifests(st, &games).await {
        error!("manifest precompute failed: {e}");
    }
    st.set_scan(|s| { s.phase = "igdb".to_string(); s.current = String::new(); s.message = "Enriching metadata…".to_string(); });
    let enrichment = enrich_catalog_from_igdb(st, false).await.unwrap_or_else(|e| format!("IGDB enrichment skipped: {e}"));

    let mut by_platform = BTreeMap::<String, usize>::new();
    for game in &games {
        *by_platform.entry(game.platform.clone()).or_default() += 1;
    }
    let mut msg = format!(
        "Synced {} games to MariaDB.",
        games.len()
    );
    for (platform, count) in by_platform {
        msg.push_str(&format!("\n{platform}: {count}"));
    }
    msg.push_str(&format!("\n{enrichment}"));
    Ok(msg)
}

async fn admin_scan_status(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(_)) => Json(st.scan_snapshot()).into_response(),
        _ => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not signed in"}))).into_response(),
    }
}

async fn enrich_catalog_from_igdb(st: &AppState, force: bool) -> Result<String> {
    let client_id = setting_value(&st.db, IGDB_CLIENT_ID_KEY).await?.unwrap_or_default();
    let client_secret = setting_value(&st.db, IGDB_CLIENT_SECRET_KEY).await?.unwrap_or_default();
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(anyhow!("set {IGDB_CLIENT_ID_KEY} and {IGDB_CLIENT_SECRET_KEY} in Configuration first"));
    }

    let http = Client::builder().user_agent("ArcadeLauncher-Server/1.0").build()?;
    let token = igdb_authenticate(&http, &client_id, &client_secret).await?;
    let games = list_games(&st.db).await?;
    let mut matched = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for game in games {
        if !force && game.igdb_id > 0 && !game.summary.is_empty() && !game.cover_art_url.is_empty() {
            skipped += 1;
            continue;
        }
        match igdb_best_match(&http, &client_id, &token, &game).await {
            Ok(Some(meta)) => {
                let cover_art_url = if !meta.cover_image_id.is_empty() {
                    match cache_igdb_cover(&http, st, &game.id, &meta.cover_image_id).await {
                        Ok(true) => "local".to_string(),
                        _ => igdb_cover_url(&meta.cover_image_id),
                    }
                } else {
                    game.cover_art_url.clone()
                };
                save_game_metadata(&st.db, &game.id, &meta, &cover_art_url).await?;
                matched += 1;
                tokio::time::sleep(std::time::Duration::from_millis(260)).await;
            }
            Ok(None) => {
                failed += 1;
                tokio::time::sleep(std::time::Duration::from_millis(260)).await;
            }
            Err(e) => {
                failed += 1;
                error!("IGDB metadata failed for {}: {e}", game.title);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Ok(format!("IGDB enrichment complete. matched: {matched}, skipped: {skipped}, unmatched/failed: {failed}"))
}

async fn igdb_authenticate(http: &Client, client_id: &str, client_secret: &str) -> Result<String> {
    let json: serde_json::Value = http
        .post("https://id.twitch.tv/oauth2/token")
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "client_credentials"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    json.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Twitch auth response did not include access_token"))
}

async fn igdb_best_match(http: &Client, client_id: &str, token: &str, game: &Game) -> Result<Option<IgdbMatch>> {
    let title = clean_igdb_title(&game.title);
    let mut candidates = igdb_search(http, client_id, token, &title, igdb_platform_ids(&game.platform)).await?;
    if candidates.is_empty() && !igdb_platform_ids(&game.platform).is_empty() {
        candidates = igdb_search(http, client_id, token, &title, &[]).await?;
    }
    let norm_title = normalize_title(&title);
    let mut best: Option<(f64, IgdbMatch)> = None;
    for candidate in candidates {
        let score = title_similarity(&norm_title, &normalize_title(&candidate.name));
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, candidate));
        }
    }
    Ok(best.and_then(|(score, meta)| if score >= 0.60 { Some(meta) } else { None }))
}

async fn igdb_credentials(db: &Pool) -> Result<(String, String)> {
    let client_id = setting_value(db, IGDB_CLIENT_ID_KEY).await?.unwrap_or_default();
    let client_secret = setting_value(db, IGDB_CLIENT_SECRET_KEY).await?.unwrap_or_default();
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(anyhow!("set IGDB credentials in Configuration first"));
    }
    Ok((client_id, client_secret))
}

async fn igdb_search_for_game(st: &AppState, game: &Game, query: &str) -> Result<Vec<IgdbMatch>> {
    let query = sanitize_search_query(query)?;
    let platforms = igdb_platform_ids(&game.platform);
    if platforms.is_empty() {
        return Err(anyhow!("no IGDB platform mapping is configured for {}", game.platform));
    }
    let (client_id, client_secret) = igdb_credentials(&st.db).await?;
    let http = Client::builder().user_agent("ArcadeLauncher-Server/1.0").build()?;
    let token = igdb_authenticate(&http, &client_id, &client_secret).await?;
    igdb_search(&http, &client_id, &token, &query, platforms).await
}

async fn igdb_fetch_by_id(http: &Client, client_id: &str, token: &str, igdb_id: u64) -> Result<IgdbMatch> {
    let body = format!(
        "fields id,name,summary,rating,first_release_date,cover.image_id,genres.name;where id = {igdb_id};limit 1;"
    );
    let value: serde_json::Value = http
        .post("https://api.igdb.com/v4/games")
        .header("Client-ID", client_id)
        .bearer_auth(token)
        .body(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    value
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(parse_igdb_match)
        .ok_or_else(|| anyhow!("IGDB game {igdb_id} was not found"))
}

async fn apply_manual_igdb_match(st: &AppState, game_id: &str, igdb_id: u64) -> Result<String> {
    let game = find_game(&st.db, game_id).await?.ok_or_else(|| anyhow!("game not found"))?;
    let (client_id, client_secret) = igdb_credentials(&st.db).await?;
    let http = Client::builder().user_agent("ArcadeLauncher-Server/1.0").build()?;
    let token = igdb_authenticate(&http, &client_id, &client_secret).await?;
    let meta = igdb_fetch_by_id(&http, &client_id, &token, igdb_id).await?;
    let cover_art_url = if !meta.cover_image_id.is_empty() {
        match cache_igdb_cover(&http, st, &game.id, &meta.cover_image_id).await {
            Ok(true) => "local".to_string(),
            _ => igdb_cover_url(&meta.cover_image_id),
        }
    } else {
        game.cover_art_url.clone()
    };
    save_game_metadata(&st.db, &game.id, &meta, &cover_art_url).await?;
    Ok(game.title)
}

async fn igdb_search(http: &Client, client_id: &str, token: &str, title: &str, platforms: &[i32]) -> Result<Vec<IgdbMatch>> {
    let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
    let mut body = format!(
        "search \"{escaped}\";fields id,name,summary,rating,first_release_date,cover.image_id,genres.name;"
    );
    if !platforms.is_empty() {
        body.push_str("where release_dates.platform = (");
        body.push_str(&platforms.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(","));
        body.push_str(");");
    }
    body.push_str("limit 8;");
    let value: serde_json::Value = http
        .post("https://api.igdb.com/v4/games")
        .header("Client-ID", client_id)
        .bearer_auth(token)
        .body(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(items) = value.as_array() else { return Ok(Vec::new()); };
    Ok(items.iter().filter_map(parse_igdb_match).collect())
}

fn parse_igdb_match(v: &serde_json::Value) -> Option<IgdbMatch> {
    let id = v.get("id")?.as_u64()?;
    let name = v.get("name")?.as_str()?.to_string();
    let genres = v
        .get("genres")
        .and_then(|g| g.as_array())
        .map(|arr| arr.iter().filter_map(|g| g.get("name").and_then(|n| n.as_str())).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    Some(IgdbMatch {
        id,
        name,
        summary: v.get("summary").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        genres,
        rating: v.get("rating").and_then(|r| r.as_f64()).unwrap_or_default(),
        release_date: v.get("first_release_date").and_then(|d| d.as_i64()).unwrap_or_default(),
        cover_image_id: v.get("cover").and_then(|c| c.get("image_id")).and_then(|i| i.as_str()).unwrap_or_default().to_string(),
    })
}

async fn cache_igdb_cover(http: &Client, st: &AppState, game_id: &str, image_id: &str) -> Result<bool> {
    let bytes = http.get(igdb_cover_url(image_id)).send().await?.error_for_status()?.bytes().await?;
    let path = server_cover_path(&st.cfg.library_root, game_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, bytes).await?;
    Ok(true)
}

async fn save_game_metadata(db: &Pool, game_id: &str, meta: &IgdbMatch, cover_art_url: &str) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop(
        r#"UPDATE games
           SET igdb_id=:igdb_id,
               summary=:summary,
               genres=:genres,
               igdb_rating=:igdb_rating,
               release_date=:release_date,
               cover_art_url=IF(:cover_art_url='',cover_art_url,:cover_art_url),
               updated_at=:updated_at
           WHERE id=:id"#,
        params! {
            "id" => game_id,
            "igdb_id" => meta.id,
            "summary" => &meta.summary,
            "genres" => &meta.genres,
            "igdb_rating" => meta.rating,
            "release_date" => meta.release_date,
            "cover_art_url" => cover_art_url,
            "updated_at" => now(),
        },
    )
    .await?;
    Ok(())
}

async fn scan_catalog(library_root: &Path) -> Result<Vec<Game>> {
    let mut games = Vec::new();
    games.extend(scan_single_file_platforms(library_root).await?);
    games.extend(scan_xbox360_god(library_root).await?);
    games.extend(scan_pc_games(library_root).await?);
    games.sort_by(|a, b| {
        (a.platform.as_str(), a.title.to_lowercase(), a.id.as_str())
            .cmp(&(b.platform.as_str(), b.title.to_lowercase(), b.id.as_str()))
    });
    Ok(games)
}

async fn scan_single_file_platforms(library_root: &Path) -> Result<Vec<Game>> {
    let specs: &[(&str, &str, &[&str])] = &[
        ("Nintendo/NES", "NES", &["nes", "fds", "unf", "unif"]),
        ("Nintendo/SNES", "SNES", &["sfc", "smc", "fig", "bs", "st", "zip", "7z"]),
        ("Nintendo/N64", "N64", &["z64", "n64", "v64", "rom"]),
        ("Nintendo/Switch", "Ryujinx", &["nsp", "xci", "nca", "nro"]),
        ("Nintendo/Gamecube", "GameCube", &["iso", "gcm", "rvz", "gcz"]),
        ("Nintendo/Wii", "Wii", &["iso", "rvz", "gcz", "wbfs", "dol", "elf"]),
    ];
    let skip: HashSet<&str> = ["sqlite", "db", "txt", "nfo", "jpg", "jpeg", "png", "webp"].into_iter().collect();
    let mut out = Vec::new();
    let games_root = library_root.join("games");
    for (relative_dir, platform, extensions) in specs {
        let platform_root = games_root.join(relative_dir);
        if fs::metadata(&platform_root).await.is_err() {
            continue;
        }
        let allowed: HashSet<&str> = extensions.iter().copied().collect();
        for path in walk_files(&platform_root).await? {
            let suffix = file_ext(&path);
            if suffix.is_empty() || skip.contains(suffix.as_str()) || !allowed.contains(suffix.as_str()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
            out.push(game_entry(
                library_root,
                &path,
                platform,
                &clean_title(name),
                Path::new(name),
                "emulator_rom",
                "{rom}",
            ).await?);
        }
    }
    Ok(out)
}

async fn scan_xbox360_god(library_root: &Path) -> Result<Vec<Game>> {
    let xbox_root = library_root.join("games").join("Microsoft").join("Xbox 360");
    if fs::metadata(&xbox_root).await.is_err() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut seen_roots = HashSet::<PathBuf>::new();
    let god_dirs = ["00007000", "0007000"];
    for dir in walk_dirs(&xbox_root).await? {
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else { continue; };
        if !god_dirs.contains(&name) {
            continue;
        }
        let Some(package) = find_god_package(&dir).await? else { continue; };
        let relative_god_dir = dir.strip_prefix(&xbox_root).unwrap_or(&dir);
        let Some(first) = relative_god_dir.components().next() else { continue; };
        let game_root = xbox_root.join(first.as_os_str());
        if !seen_roots.insert(game_root.clone()) {
            continue;
        }
        let target = package.strip_prefix(&game_root).unwrap_or(&package).to_path_buf();
        let title = game_root.file_name().and_then(|s| s.to_str()).map(clean_title).unwrap_or_else(|| "Xbox 360 Game".into());
        out.push(game_entry(library_root, &game_root, "Xbox360", &title, &target, "emulator_rom", "{rom}").await?);
    }
    Ok(out)
}

// True if `path` is the *primary* part of a repack archive that should become its
// own game. Recognises plain .zip/.7z/.rar and the first part of multi-part sets
// (.partN.rar where N==1, and split archives like name.7z.001 / name.rar.001).
// Crucially, a bare ".001" with no archive extension before it is NOT an archive —
// ".001" is a very common split-data extension inside installed game folders, and
// matching it blindly turned thousands of internal files into phantom games.
fn is_pc_primary_archive(path: &Path) -> bool {
    let name_l = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n.to_ascii_lowercase(),
        None => return false,
    };
    let ext = file_ext(path);
    let is_archive_ext = matches!(ext.as_str(), "zip" | "7z" | "rar");
    // Split archive first part: name.<7z|zip|rar>.001
    let is_split_first = ext == "001"
        && (name_l.ends_with(".7z.001") || name_l.ends_with(".zip.001") || name_l.ends_with(".rar.001"));
    if !is_archive_ext && !is_split_first {
        return false;
    }
    // Skip continuation parts of multi-part RAR sets (.partN.rar, N>1).
    if let Some(idx) = name_l.find(".part") {
        let digits: String = name_l[idx + 5..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.parse::<u32>().map(|n| n > 1).unwrap_or(false) {
            return false;
        }
    }
    true
}

async fn scan_pc_games(library_root: &Path) -> Result<Vec<Game>> {
    let pc_root = library_root.join("games").join("PC");
    if fs::metadata(&pc_root).await.is_err() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut rd = fs::read_dir(&pc_root).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if meta.is_file() {
            // Loose repack archive sitting directly in games/PC.
            if is_pc_primary_archive(&path) {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    out.push(game_entry(library_root, &path, "PC", &clean_title(name), Path::new(""), "pc_archive", "{exe}").await?);
                }
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
        if name.eq_ignore_ascii_case("steam") {
            let mut steam = fs::read_dir(&path).await?;
            while let Some(game_dir) = steam.next_entry().await? {
                let game_path = game_dir.path();
                if game_dir.metadata().await?.is_dir() {
                    if let Some(game) = pc_folder_entry(library_root, &game_path).await? {
                        out.push(game);
                    }
                }
            }
            continue;
        }
        // Recurse up to two levels so games nested under category folders aren't
        // missed — but stop the moment a folder is itself a game.
        collect_pc_folder_games(library_root, &path, 2, &mut out).await?;
    }
    Ok(out)
}

// Resolve `dir` as either ONE game (it has a launchable exe → the whole folder is
// the game; never descend into it) or a category folder (recurse into subfolders
// and pick up any loose repack archives sitting directly inside it).
async fn collect_pc_folder_games(library_root: &Path, dir: &Path, max_depth: usize, out: &mut Vec<Game>) -> Result<()> {
    // A folder with a launchable exe is a single game. Its internal archives and
    // subfolders are part of that game and must NOT be scanned as separate games.
    if let Some(game) = pc_folder_entry(library_root, dir).await? {
        out.push(game);
        return Ok(());
    }
    // Not a game itself — treat as a category. Collect loose archives here, then
    // recurse into subfolders.
    let mut subdirs = Vec::<PathBuf>::new();
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if meta.is_file() {
            if is_pc_primary_archive(&path) {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    out.push(game_entry(library_root, &path, "PC", &clean_title(name), Path::new(""), "pc_archive", "{exe}").await?);
                }
            }
        } else if meta.is_dir() {
            subdirs.push(path);
        }
    }
    if max_depth == 0 {
        if !subdirs.is_empty() {
            info!("PC scan: reached depth limit, not descending into {} subfolder(s) of {}", subdirs.len(), dir.display());
        }
        return Ok(());
    }
    for sub in subdirs {
        Box::pin(collect_pc_folder_games(library_root, &sub, max_depth - 1, out)).await?;
    }
    Ok(())
}

async fn pc_folder_entry(library_root: &Path, game_root: &Path) -> Result<Option<Game>> {
    let Some(title) = game_root.file_name().and_then(|s| s.to_str()).map(clean_title) else {
        return Ok(None);
    };
    let target = find_pc_launch_target(game_root).await?;
    let game = game_entry(library_root, game_root, "PC", &title, &target, "pc_folder", "{exe}").await?;
    if game.launch.target.is_empty() {
        return Ok(None);
    }
    Ok(Some(game))
}

async fn find_pc_launch_target(game_root: &Path) -> Result<PathBuf> {
    let mut candidates = Vec::<PathBuf>::new();
    let mut fallback = Vec::<PathBuf>::new();
    for path in walk_files(game_root).await? {
        if file_ext(&path) != "exe" {
            continue;
        }
        let rel = path.strip_prefix(game_root).unwrap_or(&path).to_path_buf();
        let lower = rel.to_string_lossy().to_ascii_lowercase();
        fallback.push(rel.clone());
        if lower.contains("unins") || lower.contains("setup") || lower.contains("redist") ||
           lower.contains("_commonredist") || lower.contains("crashreport") {
            continue;
        }
        candidates.push(rel);
    }
    // Prefer a "real" game exe, but fall back to any exe so a folder is never
    // silently dropped just because every exe matched an installer heuristic.
    let pick = if candidates.is_empty() { &mut fallback } else { &mut candidates };
    pick.sort_by_key(|p| {
        let depth = p.components().count();
        let len = p.to_string_lossy().len();
        (depth, len)
    });
    Ok(pick.first().cloned().unwrap_or_default())
}

async fn find_god_package(god_dir: &Path) -> Result<Option<PathBuf>> {
    let mut rd = fs::read_dir(god_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if !meta.is_file() || path.extension().is_some() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
        if fs::metadata(god_dir.join(format!("{name}.data"))).await.map(|m| m.is_dir()).unwrap_or(false) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

async fn game_entry(
    library_root: &Path,
    content_path: &Path,
    platform: &str,
    title: &str,
    target: &Path,
    install_type: &str,
    arguments: &str,
) -> Result<Game> {
    let relative_content = content_path.strip_prefix(library_root).unwrap_or(content_path);
    let mut game = Game {
        id: stable_id(platform, relative_content),
        title: title.to_string(),
        platform: platform.to_string(),
        install_type: install_type.to_string(),
        version: version_for(content_path).await?,
        content_path: relative_content.to_string_lossy().replace('\\', "/"),
        cover_art_url: String::new(),
        igdb_id: 0,
        summary: String::new(),
        genres: String::new(),
        igdb_rating: 0.0,
        release_date: 0,
        launch: Launch {
            target: target.to_string_lossy().replace('\\', "/"),
            arguments: arguments.to_string(),
        },
    };
    apply_sidecar_metadata(content_path, &mut game).await;
    Ok(game)
}

async fn sync_catalog_db(db: &Pool, games: &[Game]) -> Result<()> {
    let mut c = db.get_conn().await?;
    let ts = now();
    for game in games {
        c.exec_drop(
            r#"INSERT INTO games
              (id,title,platform,install_type,version,content_path,launch_target,launch_arguments,cover_art_url,igdb_id,summary,genres,igdb_rating,release_date,updated_at)
              VALUES (:id,:title,:platform,:install_type,:version,:content_path,:launch_target,:launch_arguments,:cover_art_url,:igdb_id,:summary,:genres,:igdb_rating,:release_date,:updated_at)
              ON DUPLICATE KEY UPDATE
                title=VALUES(title),
                platform=VALUES(platform),
                install_type=VALUES(install_type),
                version=VALUES(version),
                content_path=VALUES(content_path),
                launch_target=VALUES(launch_target),
                launch_arguments=VALUES(launch_arguments),
                cover_art_url=IF(VALUES(cover_art_url)='',cover_art_url,VALUES(cover_art_url)),
                igdb_id=IF(VALUES(igdb_id)=0,igdb_id,VALUES(igdb_id)),
                summary=IF(VALUES(summary)='',summary,VALUES(summary)),
                genres=IF(VALUES(genres)='',genres,VALUES(genres)),
                igdb_rating=IF(VALUES(igdb_rating)=0,igdb_rating,VALUES(igdb_rating)),
                release_date=IF(VALUES(release_date)=0,release_date,VALUES(release_date)),
                updated_at=VALUES(updated_at)"#,
            params! {
                "id" => &game.id,
                "title" => &game.title,
                "platform" => &game.platform,
                "install_type" => &game.install_type,
                "version" => &game.version,
                "content_path" => &game.content_path,
                "launch_target" => &game.launch.target,
                "launch_arguments" => &game.launch.arguments,
                "cover_art_url" => &game.cover_art_url,
                "igdb_id" => game.igdb_id,
                "summary" => &game.summary,
                "genres" => &game.genres,
                "igdb_rating" => game.igdb_rating,
                "release_date" => game.release_date,
                "updated_at" => ts,
            },
        )
        .await?;
    }
    let ids: HashSet<&str> = games.iter().map(|g| g.id.as_str()).collect();
    let existing: Vec<String> = c.query("SELECT id FROM games").await?;
    for id in existing {
        if !ids.contains(id.as_str()) {
            c.exec_drop("DELETE FROM games WHERE id=:id", params! {"id" => id}).await?;
        }
    }
    Ok(())
}

async fn version_for(path: &Path) -> Result<String> {
    let meta = fs::metadata(path).await?;
    if meta.is_file() {
        let modified = modified_secs(&meta);
        return Ok(sha1_short(&format!("{}:{}:{}", path.file_name().and_then(|s| s.to_str()).unwrap_or(""), meta.len(), modified)));
    }
    let mut h = Sha1::new();
    for file_path in walk_files(path).await? {
        let meta = fs::metadata(&file_path).await?;
        let rel = file_path.strip_prefix(path).unwrap_or(&file_path).to_string_lossy().replace('\\', "/");
        h.update(format!("{}:{}:{}\n", rel, meta.len(), modified_secs(&meta)).as_bytes());
    }
    Ok(hex::encode(h.finalize())[..12].to_string())
}

fn stable_id(platform: &str, relative: &Path) -> String {
    format!("{}-{}", platform.to_lowercase(), sha1_short(&relative.to_string_lossy().replace('\\', "/")))
}

fn sha1_short(text: &str) -> String {
    let mut h = Sha1::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())[..12].to_string()
}

fn modified_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn file_ext(path: &Path) -> String {
    path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn clean_title(name: &str) -> String {
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let mut out = String::new();
    let mut skip_square = 0u32;
    let mut skip_paren = 0u32;
    for ch in stem.chars() {
        match ch {
            '[' => skip_square += 1,
            ']' if skip_square > 0 => skip_square -= 1,
            '(' => skip_paren += 1,
            ')' if skip_paren > 0 => skip_paren -= 1,
            '_' if skip_square == 0 && skip_paren == 0 => out.push(' '),
            _ if skip_square == 0 && skip_paren == 0 => out.push(ch),
            _ => {}
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == ' ' || c == '.' || c == '-' || c == '_').to_string();
    if trimmed.is_empty() { stem.to_string() } else { trimmed }
}

struct GameValidationReport {
    total: usize,
    ok: usize,
    missing: Vec<String>,
    empty: Vec<String>,
    errors: Vec<String>,
    bytes: u64,
}

impl GameValidationReport {
    fn to_message(&self) -> String {
        let mut msg = format!(
            "Game validation complete.\n{} checked, {} OK, {} missing, {} empty, {} errors, {} total bytes.",
            self.total,
            self.ok,
            self.missing.len(),
            self.empty.len(),
            self.errors.len(),
            self.bytes
        );
        for (label, rows) in [("Missing", &self.missing), ("Empty", &self.empty), ("Errors", &self.errors)] {
            if !rows.is_empty() {
                msg.push_str(&format!("\n\n{label}:"));
                for row in rows.iter().take(50) {
                    msg.push_str("\n- ");
                    msg.push_str(row);
                }
                if rows.len() > 50 {
                    msg.push_str(&format!("\n- ... {} more", rows.len() - 50));
                }
            }
        }
        msg
    }
}

async fn validate_games(st: &AppState) -> Result<GameValidationReport> {
    let games = list_games(&st.db).await?;
    let mut report = GameValidationReport {
        total: games.len(),
        ok: 0,
        missing: Vec::new(),
        empty: Vec::new(),
        errors: Vec::new(),
        bytes: 0,
    };
    for game in games {
        let path = match content_path_for(&st.cfg, &game).await {
            Ok(p) => p,
            Err(e) => {
                report.errors.push(format!("{}: {}", game.title, e));
                continue;
            }
        };
        let meta = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                report.missing.push(format!("{} ({}) -> {}", game.title, game.platform, path.display()));
                continue;
            }
        };
        if meta.is_file() {
            if meta.len() == 0 {
                report.empty.push(format!("{} ({}) -> {}", game.title, game.platform, path.display()));
            } else {
                report.ok += 1;
                report.bytes += meta.len();
            }
            continue;
        }
        if meta.is_dir() {
            match dir_file_stats(&path).await {
                Ok((count, bytes)) if count > 0 && bytes > 0 => {
                    report.ok += 1;
                    report.bytes += bytes;
                }
                Ok(_) => report.empty.push(format!("{} ({}) -> {}", game.title, game.platform, path.display())),
                Err(e) => report.errors.push(format!("{}: {}", game.title, e)),
            }
        }
    }
    Ok(report)
}

async fn dir_file_stats(root: &Path) -> Result<(usize, u64)> {
    let mut count = 0usize;
    let mut bytes = 0u64;
    for path in walk_files(root).await? {
        let meta = fs::metadata(path).await?;
        count += 1;
        bytes += meta.len();
    }
    Ok((count, bytes))
}

async fn restart_service(name: &str) -> Result<String> {
    let service = match name {
        "arcadelauncher-server" => "arcadelauncher-server.service",
        "mariadb" => "mariadb.service",
        _ => return Err(anyhow!("service is not restartable from this panel")),
    };
    if service == "arcadelauncher-server.service" {
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            std::process::exit(1);
        });
        return Ok("Restarting ArcadeLauncher Server. Refresh the admin panel in a few seconds.".into());
    }
    let out = Command::new("sudo")
        .arg("/bin/systemctl")
        .arg("restart")
        .arg(service)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(if err.is_empty() { "service restart failed".into() } else { err }));
    }
    Ok(format!("Restarted {service}."))
}

async fn admin_html(st: &AppState, admin: Option<User>, message: &str, matcher_game_id: &str, matcher_query: &str) -> Result<String> {
    let users = list_users(&st.db).await.unwrap_or_default();
    let tokens = list_launcher_tokens(&st.db).await.unwrap_or_default();
    let games = list_games(&st.db).await.unwrap_or_default();
    let settings = list_server_settings(&st.db).await.unwrap_or_default();
    let service_rows = service_status_rows(st, games.len(), users.len(), tokens.len()).await;
    let validation_summary = validation_summary_rows(st, &games).await;
    let mut by_platform = BTreeMap::<String, usize>::new();
    for g in &games {
        *by_platform.entry(g.platform.clone()).or_default() += 1;
    }
    let user_rows = users
        .iter()
        .map(|u| {
            let twofa = if u.totp_enabled { "Enabled" } else { "Disabled" };
            let action = if u.totp_enabled { "disable_totp" } else { "enable_totp" };
            let label = if u.totp_enabled { "Disable 2FA" } else { "Enable 2FA" };
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td><form method='post' class='inline'><input type='hidden' name='user_id' value='{}'><button name='action' value='{}'>{}</button></form></td></tr>",
                esc(&u.username), esc(&u.email), if u.is_admin { "Admin" } else { "Client" }, if u.enabled { "Enabled" } else { "Disabled" }, twofa, u.id, action, label
            )
        })
        .collect::<String>();
    let token_rows = tokens
        .iter()
        .map(|(id, name, token, enabled)| {
            format!(
                "<tr><td>{}</td><td><code class='token'>{}</code></td><td>{}</td><td><form method='post' class='inline'><input type='hidden' name='user_id' value='{}'><button name='action' value='rotate_user'>Rotate</button><button name='action' value='delete_user' class='danger'>Delete</button></form></td></tr>",
                esc(name), esc(&masked_value(token)), if *enabled { "Enabled" } else { "Disabled" }, id
            )
        })
        .collect::<String>();
    let platform_rows = by_platform
        .iter()
        .map(|(p, c)| format!("<div class='platform-row'><span>{}</span><strong>{}</strong></div>", esc(p), c))
        .collect::<String>();
    let settings_rows = settings
        .iter()
        .map(|(k, v)| {
            let sensitive = is_sensitive_key(k);
            let value = if sensitive { String::new() } else { v.clone() };
            let masked = if sensitive { format!("<span class='muted'>{}</span>", esc(&masked_value(v))) } else { String::new() };
            format!(
                "<tr><td><code>{}</code></td><td><form method='post' class='inline'><input type='hidden' name='setting_key' value='{}'><input name='setting_value' value='{}'>{}<button name='action' value='save_setting'>Save</button></form></td></tr>",
                esc(k), esc(k), esc(&value), masked
            )
        })
        .collect::<String>();
    let igdb_client_id = settings.iter().find(|(k, _)| k == IGDB_CLIENT_ID_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let igdb_client_secret = settings.iter().find(|(k, _)| k == IGDB_CLIENT_SECRET_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let public_base_url = settings.iter().find(|(k, _)| k == PUBLIC_BASE_URL_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let matcher_html = metadata_matcher_html(st, &games, matcher_game_id, matcher_query).await;
    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          <aside class="sidebar"><div class="brand-block"><div class="brand-mark">AL</div><div><div class="brand-title">ArcadeLauncher</div><div class="brand-subtitle">Rust Server</div></div></div><nav><a href="#overview">Overview</a><a href="#services">Services</a><a href="#library">Library</a><a href="/admin/metadata">Metadata</a><a href="#auth">Auth</a><a href="#config">Configuration</a></nav></aside>
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Private library server</div><h1>Server Administration</h1></div><div class="account-box"><span>Signed in as <strong>{}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {}
            <section id="overview" class="section"><div class="section-heading"><h2>Overview</h2><span class="muted">Rust backend, MariaDB catalog, local file delivery</span></div><div class="metric-grid"><div class="metric"><span>Total Games</span><strong>{}</strong></div><div class="metric"><span>Platforms</span><strong>{}</strong></div><div class="metric"><span>Issued Tokens</span><strong>{}</strong></div><div class="metric"><span>Users</span><strong>{}</strong></div></div></section>
            <section id="services" class="section"><div class="section-heading"><h2>Backend Services</h2><span class="muted">Live checks from the running server process</span></div><table><thead><tr><th>Service</th><th>Status</th><th>Details</th><th>Action</th></tr></thead><tbody>{}</tbody></table></section>
            <section id="library" class="section split"><div><div class="section-heading"><h2>Library Setup</h2><span class="muted">Filesystem stores files; MariaDB stores lookup metadata and IGDB art.</span></div><dl class="kv"><dt>Library Root</dt><dd><code>{}</code></dd><dt>Backend</dt><dd><code>rust/axum</code></dd><dt>Art Cache</dt><dd><code>{}</code></dd></dl><form method="post" class="row"><button name="action" value="rescan">Rescan Filesystem and Sync DB</button><button name="action" value="igdb_enrich">Sync IGDB Metadata</button><button name="action" value="igdb_refresh">Force Refresh IGDB Metadata</button><button name="action" value="validate_games">Validate Games</button></form><div id="scan-status" class="scanbox"><div class="scanhead"><span id="sc-phase" class="scanphase idle"><span id="sc-spin" class="scanspin"></span><span id="sc-phtext">Idle</span></span><span class="scanmeta"><strong id="sc-count">0/0</strong> &middot; <span id="sc-pct">0%</span> &middot; elapsed <span id="sc-elapsed">0s</span></span></div><div id="sc-bar" class="scanbar"><i id="sc-fill"></i></div><div id="sc-cur" class="scancur"></div><div id="sc-plat" class="scanplat"></div><div id="sc-msg" class="scanmsg"></div></div><script>(function(){{var box=document.getElementById('scan-status');if(!box)return;var phEl=document.getElementById('sc-phase'),phTx=document.getElementById('sc-phtext'),spin=document.getElementById('sc-spin'),cnt=document.getElementById('sc-count'),pctEl=document.getElementById('sc-pct'),elEl=document.getElementById('sc-elapsed'),bar=document.getElementById('sc-bar'),fill=document.getElementById('sc-fill'),cur=document.getElementById('sc-cur'),plat=document.getElementById('sc-plat'),msg=document.getElementById('sc-msg');var names={{scanning:'Scanning library',hashing:'Hashing files',igdb:'Enriching metadata',done:'Completed',error:'Failed',idle:'Idle'}};function fmt(sec){{if(sec<60)return sec+'s';var m=Math.floor(sec/60),s=sec%60;return m+'m '+(s<10?'0':'')+s+'s';}}function render(s){{var phase=s.phase||'idle';box.classList.add('active');phEl.className='scanphase '+(phase==='done'?'done':phase==='error'?'error':phase==='idle'?'idle':'');phTx.textContent=names[phase]||phase;spin.style.display=s.running?'inline-block':'none';var total=s.total||0,proc=s.processed||0;var pct=total?Math.floor(proc*100/total):0;cnt.textContent=proc+'/'+total;pctEl.textContent=pct+'%';var nowSec=Math.floor(Date.now()/1000);var srv=s.startedAt?((s.updatedAt||nowSec)-s.startedAt):0;var elapsed=s.running?Math.max(srv,nowSec-(s.startedAt||nowSec)):srv;elEl.textContent=fmt(Math.max(0,elapsed));if(s.running&&!total){{bar.className='scanbar indet';fill.style.width='40%';}}else{{bar.className='scanbar';fill.style.width=pct+'%';}}var act=s.active||0;cur.textContent=s.current?('Hashing: '+s.current+(act>1?(' (+'+(act-1)+' more in parallel)'):'')):(act>0?(act+' hashing in parallel'):'');var pp=s.perPlatform||{{}};var keys=Object.keys(pp).sort();var ph='';for(var i=0;i<keys.length;i++){{var k=keys[i],v=pp[k]||{{}},t=v.total||0,p=v.processed||0,w=t?Math.floor(p*100/t):0,dn=(t&&p>=t);ph+='<div class="scanplat-row"><span class="scanplat-name">'+k+'</span><span class="scanplat-bar'+(dn?' done':'')+'"><i style="width:'+w+'%"></i></span><span class="scanplat-num">'+p+'/'+t+'</span></div>';}}plat.innerHTML=((phase==='hashing'||phase==='done')&&keys.length)?ph:'';msg.textContent=(phase==='done'||phase==='error')?(s.message||''):'';}}function poll(){{fetch('/admin/scan-status').then(function(r){{return r.json();}}).then(function(s){{if(!s)return;if(s.running||s.phase==='done'||s.phase==='error'){{render(s);}}else{{box.classList.remove('active');}}}}).catch(function(){{}});setTimeout(poll,1000);}}poll();}})();</script>{}<h3>Game Validation</h3><table><thead><tr><th>Check</th><th>Status</th><th>Details</th></tr></thead><tbody>{}</tbody></table></div><div class="platform-card"><h3>Platform Counts</h3>{}</div></section>
            <section id="auth" class="section"><div class="section-heading"><h2>Auth Management</h2><span class="muted">All users sign in with username/password; bearer tokens are issued behind the scenes.</span></div><div class="two-col"><div><h3>Create User</h3><form method="post" class="row"><input name="username" placeholder="Username"><input name="email" type="email" placeholder="Email"><input name="password" type="password" placeholder="Password"><label class="checkline"><input type="checkbox" name="is_admin" value="1"> Admin</label><button name="action" value="add_user">Create User</button></form><h3>Users</h3><table><thead><tr><th>Username</th><th>Email</th><th>Role</th><th>Status</th><th>2FA</th><th>Actions</th></tr></thead><tbody>{}</tbody></table></div><div><h3>Issued Tokens</h3><table><thead><tr><th>Name</th><th>Bearer Token</th><th>Status</th><th>Actions</th></tr></thead><tbody>{}</tbody></table></div></div></section>
            <section id="config" class="section"><div class="section-heading"><h2>Configuration</h2><span class="muted">Runtime env is read-only here; managed settings are stored in MariaDB.</span></div><div class="two-col"><div><h3>Runtime</h3><dl class="kv"><dt>API Listen</dt><dd><code>{}:{}</code></dd><dt>Admin Listen</dt><dd><code>{}:{}</code></dd><dt>Library</dt><dd><code>{}</code></dd><dt>Database</dt><dd><code>{}:{} / {}</code></dd><dt>Chunking</dt><dd><code>{} byte raw chunks; full-file fallback retained</code></dd></dl></div><div><h3>Backend URL</h3><form method="post" class="stack"><input type="hidden" name="setting_key" value="server.public_base_url"><input name="setting_value" type="url" placeholder="https://arcade.orlandoaio.net" value="{}"><button name="action" value="save_setting">Save Backend URL</button></form><h3>IGDB Credentials</h3><form method="post" class="stack"><input type="hidden" name="setting_key" value="igdb.client_id"><input name="setting_value" placeholder="IGDB/Twitch Client ID" value="{}"><button name="action" value="save_setting">Save Client ID</button></form><form method="post" class="stack credential-form"><input type="hidden" name="setting_key" value="igdb.client_secret"><input name="setting_value" type="password" placeholder="{}"><button name="action" value="save_setting">Save Client Secret</button></form><form method="post" class="row new-setting"><button name="action" value="igdb_enrich">Sync IGDB Metadata</button></form><h3>Managed Settings</h3><table><thead><tr><th>Key</th><th>Value</th></tr></thead><tbody>{}</tbody></table><form method="post" class="row new-setting"><input name="setting_key" placeholder="setting.key"><input name="setting_value" placeholder="value"><button name="action" value="save_setting">Add / Save</button></form></div></div></section>
          </div>
        </div>
        "##,
        esc(&signed),
        notice(message),
        games.len(),
        by_platform.len(),
        tokens.len(),
        users.len(),
        service_rows,
        esc(&st.cfg.library_root.display().to_string()),
        esc(&st.cfg.library_root.join(".arcadelauncher").join("art").display().to_string()),
        matcher_html,
        validation_summary,
        if platform_rows.is_empty() { "<p class='muted'>No cataloged platforms yet.</p>".into() } else { platform_rows },
        if user_rows.is_empty() { "<tr><td colspan='6'>No users yet.</td></tr>".into() } else { user_rows },
        if token_rows.is_empty() { "<tr><td colspan='4'>No issued tokens yet.</td></tr>".into() } else { token_rows },
        esc(&st.cfg.host),
        st.cfg.port,
        esc(&st.cfg.admin_host),
        st.cfg.admin_port,
        esc(&st.cfg.library_root.display().to_string()),
        esc(&st.cfg.db_host),
        st.cfg.db_port,
        esc(&st.cfg.db_name),
        CHUNK_SIZE,
        esc(public_base_url),
        esc(igdb_client_id),
        if igdb_client_secret.is_empty() { "IGDB/Twitch Client Secret".into() } else { format!("Saved ({})", masked_value(igdb_client_secret)) },
        if settings_rows.is_empty() { "<tr><td colspan='2'>No managed settings saved yet.</td></tr>".into() } else { settings_rows },
    )))
}

async fn metadata_page_html(st: &AppState, admin: Option<User>, message: &str, selected_id: &str, query: &str) -> Result<String> {
    let games = list_games(&st.db).await?;
    let selected = games
        .iter()
        .find(|g| g.id == selected_id)
        .or_else(|| games.iter().find(|g| g.igdb_id == 0))
        .or_else(|| games.first());
    let selected_id = selected.map(|g| g.id.as_str()).unwrap_or("");
    let search_value = if query.trim().is_empty() {
        selected.map(|g| g.title.as_str()).unwrap_or("")
    } else {
        query
    };
    let options = games
        .iter()
        .map(|g| {
            let sel = if g.id == selected_id { " selected" } else { "" };
            let marker = if g.igdb_id == 0 { " *" } else { "" };
            format!("<option value=\"{}\"{}>{} - {}{}</option>", esc(&g.id), sel, esc(&g.platform), esc(&g.title), marker)
        })
        .collect::<String>();

    let mut result_cards = String::new();
    if let Some(game) = selected {
        if !query.trim().is_empty() {
            match igdb_search_for_game(st, game, query.trim()).await {
                Ok(results) if results.is_empty() => {
                    result_cards = format!("<div class='empty-state'>No IGDB matches found for {}.</div>", esc(&game.platform));
                }
                Ok(results) => {
                    result_cards = results
                        .into_iter()
                        .map(|m| {
                            let year = if m.release_date > 0 {
                                chrono::DateTime::from_timestamp(m.release_date, 0).map(|d| d.format("%Y").to_string()).unwrap_or_default()
                            } else {
                                String::new()
                            };
                            let cover = if m.cover_image_id.is_empty() {
                                "<div class='match-cover placeholder'>No Art</div>".to_string()
                            } else {
                                format!("<img class='match-cover' src='{}' alt=''>", esc(&igdb_cover_url(&m.cover_image_id)))
                            };
                            format!(
                                "<article class='match-card'>{}<div class='match-body'><div class='match-title'>{}</div><div class='match-meta'>{} · {:.0} · {}</div><p>{}</p><form method='post' action='/admin/metadata'><input type='hidden' name='game_id' value='{}'><input type='hidden' name='search_query' value='{}'><input type='hidden' name='igdb_id' value='{}'><button name='action' value='igdb_apply'>Apply Match</button></form></div></article>",
                                cover,
                                esc(&m.name),
                                esc(&year),
                                m.rating,
                                esc(&m.genres),
                                esc(&m.summary),
                                esc(&game.id),
                                esc(query),
                                m.id
                            )
                        })
                        .collect();
                }
                Err(e) => {
                    result_cards = format!("<div class='empty-state'>{}</div>", esc(&e.to_string()));
                }
            }
        }
    }
    if result_cards.is_empty() {
        result_cards = "<div class='empty-state'>Search IGDB to choose a metadata match. Platform filtering is applied automatically.</div>".into();
    }

    let current = selected
        .map(|g| {
            let cover_src = admin_cover_src(g);
            let cover = if cover_src.is_empty() {
                "<div class='current-cover placeholder'>No Art</div>".to_string()
            } else {
                format!("<img class='current-cover' src='{}' alt=''>", esc(&cover_src))
            };
            format!(
                "<div class='current-game'>{}<div><h2>{}</h2><dl class='kv'><dt>Platform</dt><dd>{}</dd><dt>IGDB ID</dt><dd>{}</dd><dt>Rating</dt><dd>{:.0}</dd><dt>Genres</dt><dd>{}</dd><dt>Summary</dt><dd>{}</dd></dl></div></div>",
                cover,
                esc(&g.title),
                esc(&g.platform),
                if g.igdb_id == 0 { "Unmatched".into() } else { g.igdb_id.to_string() },
                g.igdb_rating,
                esc(&g.genres),
                esc(&g.summary)
            )
        })
        .unwrap_or_else(|| "<div class='empty-state'>No games are currently cataloged.</div>".into());
    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          <aside class="sidebar"><div class="brand-block"><div class="brand-mark">AL</div><div><div class="brand-title">ArcadeLauncher</div><div class="brand-subtitle">Rust Server</div></div></div><nav><a href="/admin">Dashboard</a><a href="/admin/metadata">Metadata</a><a href="/admin#library">Library</a><a href="/admin#auth">Auth</a><a href="/admin#config">Configuration</a></nav></aside>
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Metadata management</div><h1>IGDB Match Search</h1></div><div class="account-box"><span>Signed in as <strong>{}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {}
            <section class="section metadata-shell">
              <div class="section-heading"><h2>Selected Game</h2><span class="muted">Games marked with * need a manual match</span></div>
              <form method="post" action="/admin/metadata" class="row matcher-form"><select name="game_id">{}</select><input name="search_query" value="{}" placeholder="Search title"><button name="action" value="igdb_search">Search IGDB</button></form>
              {}
            </section>
            <section class="section"><div class="section-heading"><h2>Search Results</h2><span class="muted">Filtered to the selected game's platform</span></div><div class="match-grid">{}</div></section>
          </div>
        </div>
        "##,
        esc(&signed),
        notice(message),
        options,
        esc(search_value),
        current,
        result_cards
    )))
}

async fn metadata_matcher_html(st: &AppState, games: &[Game], selected_id: &str, query: &str) -> String {
    let selected = games
        .iter()
        .find(|g| g.id == selected_id)
        .or_else(|| games.first());
    let selected_id = selected.map(|g| g.id.as_str()).unwrap_or("");
    let default_query = if query.trim().is_empty() {
        selected.map(|g| g.title.as_str()).unwrap_or("")
    } else {
        query
    };
    let options = games
        .iter()
        .map(|g| {
            let sel = if g.id == selected_id { " selected" } else { "" };
            format!("<option value=\"{}\"{}>{} - {}</option>", esc(&g.id), sel, esc(&g.platform), esc(&g.title))
        })
        .collect::<String>();
    let mut result_rows = String::new();
    if let Some(game) = selected {
        if !query.trim().is_empty() {
            match igdb_search_for_game(st, game, query.trim()).await {
                Ok(results) if results.is_empty() => {
                    result_rows = format!("<tr><td colspan='6'>No IGDB matches found for {}.</td></tr>", esc(&game.platform));
                }
                Ok(results) => {
                    result_rows = results
                        .into_iter()
                        .map(|m| {
                            let year = if m.release_date > 0 {
                                chrono::DateTime::from_timestamp(m.release_date, 0).map(|d| d.format("%Y").to_string()).unwrap_or_default()
                            } else {
                                String::new()
                            };
                            let summary = if m.summary.chars().count() > 180 {
                                format!("{}...", m.summary.chars().take(180).collect::<String>())
                            } else {
                                m.summary.clone()
                            };
                            format!(
                                "<tr><td>{}</td><td>{}</td><td>{:.0}</td><td>{}</td><td>{}</td><td><form method='post'><input type='hidden' name='game_id' value='{}'><input type='hidden' name='search_query' value='{}'><input type='hidden' name='igdb_id' value='{}'><button name='action' value='igdb_apply'>Apply</button></form></td></tr>",
                                esc(&m.name), esc(&year), m.rating, esc(&m.genres), esc(&summary), esc(&game.id), esc(query), m.id
                            )
                        })
                        .collect();
                }
                Err(e) => {
                    result_rows = format!("<tr><td colspan='6'>{}</td></tr>", esc(&e.to_string()));
                }
            }
        }
    }
    if result_rows.is_empty() {
        result_rows = "<tr><td colspan='6'>Search IGDB to choose a metadata match.</td></tr>".into();
    }
    format!(
        r#"<h3>Metadata Matcher <span class="muted">({})</span></h3><form method="post" action="/admin/metadata" class="row matcher-form"><select name="game_id">{}</select><input name="search_query" value="{}" placeholder="Search title"><button name="action" value="igdb_search">Open Full Search</button></form><table class="matcher-results"><thead><tr><th>IGDB Title</th><th>Year</th><th>Rating</th><th>Genres</th><th>Summary</th><th>Action</th></tr></thead><tbody>{}</tbody></table>"#,
        esc(selected.map(|g| g.platform.as_str()).unwrap_or("No platform")),
        options,
        esc(default_query),
        result_rows
    )
}

async fn service_status_rows(st: &AppState, game_count: usize, user_count: usize, token_count: usize) -> String {
    let mut rows = Vec::new();
    rows.push(status_row(
        "ArcadeLauncher Server",
        true,
        &format!("Rust process listening on {}:{}", st.cfg.host, st.cfg.port),
        Some("arcadelauncher-server"),
    ));

    let db_ok = db_ping(&st.db).await;
    rows.push(status_row(
        "MariaDB",
        db_ok,
        &format!(
            "{}:{} / {} as {}",
            st.cfg.db_host, st.cfg.db_port, st.cfg.db_name, st.cfg.db_user
        ),
        Some("mariadb"),
    ));

    rows.push(status_row(
        "Catalog Database",
        game_count > 0,
        &format!("{game_count} games, {user_count} users, {token_count} issued tokens"),
        None,
    ));

    let library_meta = fs::metadata(&st.cfg.library_root).await;
    rows.push(status_row(
        "Library Root",
        library_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        &st.cfg.library_root.display().to_string(),
        None,
    ));

    let games_path = st.cfg.library_root.join("games");
    let games_meta = fs::metadata(&games_path).await;
    rows.push(status_row(
        "Game Storage",
        games_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        &games_path.display().to_string(),
        None,
    ));

    rows.push(status_row(
        "Catalog Generator",
        true,
        "Native Rust scanner/upserter",
        None,
    ));

    let mount_detail = command_output("findmnt", &["-T", st.cfg.library_root.to_str().unwrap_or("")]).await;
    rows.push(status_row(
        "Library Mount",
        mount_detail.is_ok(),
        mount_detail.as_deref().unwrap_or("mount lookup unavailable"),
        None,
    ));

    let disk_detail = command_output("df", &["-h", st.cfg.library_root.to_str().unwrap_or("")]).await;
    rows.push(status_row(
        "Disk Space",
        disk_detail.is_ok(),
        disk_detail.as_deref().unwrap_or("disk usage unavailable"),
        None,
    ));

    rows.join("")
}

async fn db_ping(db: &Pool) -> bool {
    let Ok(mut conn) = db.get_conn().await else { return false; };
    conn.query_drop("SELECT 1").await.is_ok()
}

async fn command_output(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if !out.status.success() {
        return Err(anyhow!(text));
    }
    Ok(text)
}

async fn validation_summary_rows(st: &AppState, games: &[Game]) -> String {
    let mut missing = 0usize;
    let mut present = 0usize;
    for game in games {
        if let Ok(path) = content_path_for(&st.cfg, game).await {
            if fs::metadata(&path).await.is_ok() {
                present += 1;
            } else {
                missing += 1;
            }
        } else {
            missing += 1;
        }
    }
    let ok = missing == 0 && !games.is_empty();
    format!(
        "<tr><td>Catalog Paths</td><td><span class='status {}'>{}</span></td><td><code>{} present, {} missing. Click Validate Games for file/byte details.</code></td></tr>",
        if ok { "ok" } else { "bad" },
        if ok { "OK" } else { "Needs Attention" },
        present,
        missing
    )
}

fn status_row(name: &str, ok: bool, details: &str, restart: Option<&str>) -> String {
    let action = restart
        .map(|svc| {
            format!(
                "<form method='post' class='inline'><input type='hidden' name='service_name' value='{}'><button name='action' value='restart_service'>Restart</button></form>",
                esc(svc)
            )
        })
        .unwrap_or_else(|| "<span class='muted'>Not restartable</span>".into());
    format!(
        "<tr><td>{}</td><td><span class='status {}'>{}</span></td><td><code>{}</code></td><td>{}</td></tr>",
        esc(name),
        if ok { "ok" } else { "bad" },
        if ok { "Online" } else { "Needs Attention" },
        esc(details),
        action
    )
}

fn login_html(message: &str) -> String {
    shell(&format!(
        r#"<section><h2>Sign In</h2>{}<form method="post" action="/admin/login" class="stack"><input name="username" placeholder="Username or email" autofocus required><input name="password" type="password" placeholder="Password" required><input name="totp_code" inputmode="numeric" autocomplete="one-time-code" placeholder="2FA code, if enabled"><button name="action" value="login">Sign In</button></form></section>"#,
        notice(message)
    ))
}

fn notice(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!("<pre class='notice'>{}</pre>", esc(message))
    }
}

fn shell(body: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>ArcadeLauncher Server</title><link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' rx='20' fill='%237c5cff'/><text x='50' y='72' font-size='64' text-anchor='middle' fill='white'>%F0%9F%8E%AE</text></svg>"><style>{}</style></head><body><main>{}</main></body></html>"#,
        CSS, body
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

static CSS: &str = r#"
:root{color-scheme:dark;--bg:#0f1115;--panel:#171b21;--panel2:#1d232b;--line:#2c3540;--text:#e8edf2;--muted:#9aa7b5;--accent:#4cc2ff;--bad:#ff6b6b}
*{box-sizing:border-box}body{margin:0;font:14px/1.45 "Segoe UI",sans-serif;background:var(--bg);color:var(--text)}main{width:100%;min-height:100vh}h1,h2,h3{margin:0;letter-spacing:0}h1{font-size:28px}h2{font-size:19px}h3{font-size:15px;margin:18px 0 10px}.admin-layout{display:grid;grid-template-columns:250px 1fr;min-height:100vh}.sidebar{background:#11161d;border-right:1px solid var(--line);padding:24px 18px}.brand-block{display:flex;gap:12px;align-items:center;margin-bottom:28px}.brand-mark{width:42px;height:42px;display:grid;place-items:center;background:var(--accent);color:#041019;font-weight:800;border-radius:8px}.brand-title{font-weight:700}.brand-subtitle,.muted,.eyebrow{color:var(--muted)}nav{display:flex;flex-direction:column;gap:6px}nav a,.buttonlink{color:var(--text);text-decoration:none;padding:9px 10px;border-radius:6px;border:1px solid transparent}nav a:hover,.buttonlink:hover{border-color:var(--line);background:var(--panel)}.content{padding:24px;min-width:0}.topbar{display:flex;justify-content:space-between;gap:16px;align-items:center;margin-bottom:20px}.account-box{display:flex;gap:12px;align-items:center;flex-wrap:wrap}.section{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:18px;margin-bottom:16px}.section-heading{display:flex;justify-content:space-between;gap:14px;align-items:end;margin-bottom:14px}.metric-grid{display:grid;grid-template-columns:repeat(4,minmax(120px,1fr));gap:12px}.metric{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px}.metric span{display:block;color:var(--muted)}.metric strong{font-size:26px}.split,.two-col{display:grid;grid-template-columns:minmax(0,1fr) 320px;gap:20px}.two-col{grid-template-columns:repeat(2,minmax(0,1fr))}.platform-card{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px}.platform-row{display:flex;justify-content:space-between;border-bottom:1px solid var(--line);padding:7px 0}.kv{display:grid;grid-template-columns:120px minmax(0,1fr);gap:8px 12px}.kv dt{color:var(--muted)}.kv dd{margin:0;min-width:0}.row{display:flex;gap:10px;flex-wrap:wrap;align-items:center}.stack{display:flex;gap:10px;flex-direction:column;align-items:flex-start}.inline{display:flex;gap:8px;flex-wrap:wrap}.new-setting{margin-top:12px}.matcher-form{margin:10px 0}.matcher-form select{max-width:420px}.matcher-results{margin-bottom:14px}.checkline{display:inline-flex;align-items:center;gap:6px;color:var(--muted)}input,select{background:#0c1015;color:var(--text);border:1px solid var(--line);border-radius:6px;padding:9px 10px;min-width:180px}button{background:var(--accent);color:#041019;border:0;border-radius:6px;padding:9px 12px;font-weight:700;cursor:pointer}.danger{background:var(--bad);color:#180406}table{width:100%;border-collapse:collapse;background:var(--panel2);border-radius:8px;overflow:hidden}th,td{text-align:left;border-bottom:1px solid var(--line);padding:9px;vertical-align:top}th{color:var(--muted);font-weight:600}code,.token{overflow-wrap:anywhere;white-space:pre-wrap}.status{display:inline-flex;align-items:center;border-radius:999px;padding:4px 9px;font-weight:700;font-size:12px}.status.ok{background:#10351f;color:#74e19a}.status.bad{background:#3d1518;color:#ff8b8b}.notice{white-space:pre-wrap;background:#102033;border:1px solid #285b86;padding:12px;border-radius:8px}@media(max-width:900px){.admin-layout{grid-template-columns:1fr}.sidebar{position:static}.metric-grid,.split,.two-col{grid-template-columns:1fr}.topbar{align-items:flex-start;flex-direction:column}}
.metadata-shell{overflow:hidden}.current-game{display:grid;grid-template-columns:180px minmax(0,1fr);gap:18px;align-items:start}.current-cover,.match-cover{width:100%;aspect-ratio:3/4;object-fit:cover;border-radius:8px;background:#0c1015;border:1px solid var(--line)}.current-cover.placeholder,.match-cover.placeholder{display:grid;place-items:center;color:var(--muted);font-weight:700}.match-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(360px,1fr));gap:14px}.match-card{display:grid;grid-template-columns:104px minmax(0,1fr);gap:14px;background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:12px}.match-title{font-weight:800;font-size:16px}.match-meta{color:var(--muted);margin:3px 0 8px}.match-body p{margin:0 0 12px;color:#c9d3dd}.empty-state{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:16px;color:var(--muted)}@media(max-width:900px){.current-game,.match-card{grid-template-columns:1fr}.match-grid{grid-template-columns:1fr}}
.scanbox{margin:12px 0;background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px;display:none}.scanbox.active{display:block}.scanhead{display:flex;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:8px}.scanphase{display:inline-flex;align-items:center;gap:7px;border-radius:999px;padding:4px 11px;font-weight:700;font-size:12px;background:#11283a;color:var(--accent)}.scanphase.done{background:#10351f;color:#74e19a}.scanphase.error{background:#3d1518;color:#ff8b8b}.scanphase.idle{background:#22282f;color:var(--muted)}.scanspin{width:11px;height:11px;border-radius:50%;border:2px solid rgba(76,194,255,.35);border-top-color:var(--accent);animation:scanspin .8s linear infinite}@keyframes scanspin{to{transform:rotate(360deg)}}.scanmeta{color:var(--muted);font-size:13px}.scanmeta strong{color:var(--text)}.scanbar{height:10px;border-radius:999px;background:#0c1015;border:1px solid var(--line);overflow:hidden;margin:8px 0}.scanbar>i{display:block;height:100%;width:0;background:linear-gradient(90deg,#4cc2ff,#7c5cff);transition:width .4s ease}.scanbar.indet>i{width:40%;animation:scanindet 1.2s ease-in-out infinite}@keyframes scanindet{0%{margin-left:-40%}100%{margin-left:100%}}.scancur{font-size:13px;color:#c9d3dd;overflow-wrap:anywhere;min-height:18px}.scanmsg{margin-top:8px;color:var(--muted);white-space:pre-wrap;font-size:13px}.scanplat{margin-top:10px;display:flex;flex-direction:column;gap:6px}.scanplat-row{display:grid;grid-template-columns:120px minmax(0,1fr) 74px;gap:10px;align-items:center;font-size:12px}.scanplat-name{color:var(--text);font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.scanplat-bar{height:8px;border-radius:999px;background:#0c1015;border:1px solid var(--line);overflow:hidden}.scanplat-bar>i{display:block;height:100%;width:0;background:linear-gradient(90deg,#4cc2ff,#7c5cff);transition:width .4s ease}.scanplat-bar.done>i{background:linear-gradient(90deg,#39d98a,#74e19a)}.scanplat-num{color:var(--muted);text-align:right;font-variant-numeric:tabular-nums}
"#;

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
    fn igdb_platform_ids_known_and_unknown() {
        assert_eq!(igdb_platform_ids("GameCube"), &[21]);
        assert_eq!(igdb_platform_ids("Wii"), &[5]);
        assert_eq!(igdb_platform_ids("Xbox360"), &[12]);
        assert!(igdb_platform_ids("Unknown").is_empty());
    }
}
