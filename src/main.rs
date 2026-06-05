use anyhow::{anyhow, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE, Engine};
use bytes::Bytes;
use cookie::Cookie;
use futures_util::TryStreamExt;
use mysql_async::{params, prelude::Queryable, Pool, Row};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use rust_scrypt::{scrypt, Params as ScryptParams};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
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
use tracing::{error, info};

const CHUNK_SIZE: usize = 1024 * 1024;
const SESSION_COOKIE: &str = "AL_ADMIN_SESSION";
const SESSION_TTL_SECONDS: i64 = 12 * 60 * 60;

#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    db: Pool,
}

#[derive(Clone)]
struct Config {
    host: String,
    port: u16,
    library_root: PathBuf,
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
            library_root,
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
    launch: Launch,
}

#[derive(Debug, Clone, Serialize)]
struct Launch {
    target: String,
    arguments: String,
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
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
struct AdminForm {
    action: String,
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
    is_admin: Option<String>,
    user_id: Option<u64>,
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

    let state = AppState { cfg: cfg.clone(), db };
    let app = Router::new()
        .route("/", get(admin_page))
        .route("/admin", get(admin_page).post(admin_post))
        .route("/admin/login", get(admin_page).post(admin_post))
        .route("/admin/logout", get(admin_logout))
        .route("/api/login", post(api_login))
        .route("/api/health", get(api_health))
        .route("/api/catalog", get(api_catalog))
        .route("/api/games/:id/manifest", get(api_manifest))
        .route("/files/:id/*rel", get(download_file))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port).parse()?;
    info!("ArcadeLauncher Rust server listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ensure_database(cfg: &Config) -> Result<()> {
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
          updated_at BIGINT NOT NULL,
          INDEX idx_games_platform_title (platform, title)
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
        let hash = hash_password_scrypt(&cfg.admin_password)?;
        c.exec_drop(
            "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at) VALUES (:u,:e,:p,TRUE,TRUE,:t)",
            params! {"u" => &cfg.admin_username, "e" => &cfg.admin_email, "p" => hash, "t" => now()},
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
        Ok(Some(user)) if verify_password_any(&form.password, &user.password_hash) => {
            match issue_user_token(&st.db, user.id, &user.username).await {
                Ok(token) => Json(serde_json::json!({"token": token, "username": user.username, "isAdmin": user.is_admin})).into_response(),
                Err(e) => server_error(e),
            }
        }
        _ => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid username or password"}))).into_response(),
    }
}

async fn api_catalog(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    match list_games(&st.db).await {
        Ok(games) => Json(Catalog { schema_version: 1, generated_by: "mariadb-rust".into(), games }).into_response(),
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
        Err(e) => server_error(e),
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
    match stream_file(file_path, headers.get(header::RANGE)).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn admin_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(admin)) => Html(admin_html(&st, Some(admin), "").await.unwrap_or_else(|e| format!("error: {e}"))).into_response(),
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

async fn admin_post(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<AdminForm>) -> Response {
    if form.action == "login" {
        let username = form.username.unwrap_or_default();
        let password = form.password.unwrap_or_default();
        match find_user(&st.db, &username).await {
            Ok(Some(user)) if user.is_admin && verify_password_any(&password, &user.password_hash) => {
                match create_session(&st.db, user.id).await {
                    Ok(token) => {
                        let mut r = Redirect::to("/admin").into_response();
                        let cookie = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={SESSION_TTL_SECONDS}");
                        r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
                        r
                    }
                    Err(e) => server_error(e),
                }
            }
            _ => Html(login_html("Invalid username or password.")).into_response(),
        }
    } else {
        let admin = match current_admin(&st.db, &headers).await {
            Ok(Some(a)) => a,
            _ => return Html(login_html("Please sign in first.")).into_response(),
        };
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
            "rescan" => match rescan_catalog(&st.cfg).await {
                Ok(out) => format!("Catalog rescan complete.\n{out}"),
                Err(e) => e.to_string(),
            },
            _ => "No action taken.".to_string(),
        };
        Html(admin_html(&st, Some(admin), &msg).await.unwrap_or_else(|e| format!("error: {e}"))).into_response()
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
}

fn user_from_row(row: Row) -> User {
    let (id, username, email, password_hash, is_admin, enabled): (u64, String, String, String, bool, bool) = mysql_async::from_row(row);
    User { id, username, email, password_hash, is_admin, enabled }
}

async fn find_user(db: &Pool, key: &str) -> Result<Option<User>> {
    let mut c = db.get_conn().await?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT id, username, email, password_hash, is_admin, enabled FROM admin_users WHERE enabled = TRUE AND (username = :k OR email = :k) LIMIT 1",
            params! {"k" => key.trim()},
        )
        .await?;
    Ok(row.map(user_from_row))
}

async fn list_users(db: &Pool) -> Result<Vec<User>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<Row> = c.query("SELECT id, username, email, password_hash, is_admin, enabled FROM admin_users ORDER BY username").await?;
    Ok(rows.into_iter().map(user_from_row).collect())
}

async fn create_user(db: &Pool, username: &str, email: &str, password: &str, is_admin: bool) -> Result<()> {
    let mut c = db.get_conn().await?;
    let hash = hash_password_argon2(password)?;
    c.exec_drop(
        "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at) VALUES (:u,:e,:p,:a,TRUE,:t)",
        params! {"u" => username, "e" => email, "p" => hash, "a" => is_admin, "t" => now()},
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
            r#"SELECT a.id,a.username,a.email,a.password_hash,a.is_admin,a.enabled
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

async fn list_games(db: &Pool) -> Result<Vec<Game>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<Row> = c
        .query(
            "SELECT id,title,platform,install_type,version,content_path,launch_target,launch_arguments,COALESCE(cover_art_url,''),igdb_id FROM games ORDER BY platform,title,id",
        )
        .await?;
    Ok(rows.into_iter().map(game_from_row).collect())
}

async fn find_game(db: &Pool, id: &str) -> Result<Option<Game>> {
    let mut c = db.get_conn().await?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT id,title,platform,install_type,version,content_path,launch_target,launch_arguments,COALESCE(cover_art_url,''),igdb_id FROM games WHERE id=:id",
            params! {"id" => id},
        )
        .await?;
    Ok(row.map(game_from_row))
}

fn game_from_row(row: Row) -> Game {
    let (id, title, platform, install_type, version, content_path, launch_target, launch_arguments, cover_art_url, igdb_id): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        u64,
    ) = mysql_async::from_row(row);
    Game {
        id,
        title,
        platform,
        install_type,
        version,
        content_path,
        cover_art_url,
        igdb_id,
        launch: Launch { target: launch_target, arguments: if launch_arguments.is_empty() { "{rom}".into() } else { launch_arguments } },
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
    let root = content_path_for(&st.cfg, game).await?;
    let (files, rel_root) = if fs::metadata(&root).await?.is_file() {
        (vec![root.clone()], root.parent().unwrap_or(&st.cfg.library_root).to_path_buf())
    } else {
        (walk_files(&root).await?, root.clone())
    };
    let base = base_url(headers, &st.cfg);
    let mut manifest_files = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&rel_root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
        let meta = fs::metadata(&path).await?;
        manifest_files.push(ManifestFile {
            path: rel.clone(),
            size: meta.len(),
            sha256: sha256_file(&path).await?,
            url: format!("{}/files/{}/{}", base, urlencoding::encode(&game.id), encode_path(&rel)),
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

fn hash_password_scrypt(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let params = ScryptParams::new(14, 8, 1, 64)?;
    let mut out = [0u8; 64];
    scrypt(password.as_bytes(), &salt, &params, &mut out)?;
    Ok(format!("scrypt$n=16384,r=8,p=1${}${}", URL_SAFE.encode(salt), URL_SAFE.encode(out)))
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

fn base_url(headers: &HeaderMap, cfg: &Config) -> String {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    let proto = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    if host == "localhost" {
        format!("http://{}:{}", cfg.host, cfg.port)
    } else {
        format!("{proto}://{host}")
    }
}

fn encode_path(path: &str) -> String {
    path.split('/').map(urlencoding::encode).collect::<Vec<_>>().join("/")
}

fn env_string(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_u16(name: &str, default: u16) -> u16 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response()
}

fn server_error(e: impl std::fmt::Display) -> Response {
    error!("{e}");
    (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": e.to_string()}))).into_response()
}

async fn rescan_catalog(cfg: &Config) -> Result<String> {
    let out = Command::new("python3")
        .arg("/opt/arcadelauncher-server/generate_catalog.py")
        .arg("--library-root")
        .arg(&cfg.library_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        return Err(anyhow!(text));
    }
    Ok(text)
}

async fn admin_html(st: &AppState, admin: Option<User>, message: &str) -> Result<String> {
    let users = list_users(&st.db).await.unwrap_or_default();
    let tokens = list_launcher_tokens(&st.db).await.unwrap_or_default();
    let games = list_games(&st.db).await.unwrap_or_default();
    let mut by_platform = BTreeMap::<String, usize>::new();
    for g in &games {
        *by_platform.entry(g.platform.clone()).or_default() += 1;
    }
    let user_rows = users
        .iter()
        .map(|u| format!("<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&u.username), esc(&u.email), if u.is_admin { "Admin" } else { "Client" }, if u.enabled { "Enabled" } else { "Disabled" }))
        .collect::<String>();
    let token_rows = tokens
        .iter()
        .map(|(id, name, token, enabled)| {
            format!(
                "<tr><td>{}</td><td><code class='token'>{}</code></td><td>{}</td><td><form method='post' class='inline'><input type='hidden' name='user_id' value='{}'><button name='action' value='rotate_user'>Rotate</button><button name='action' value='delete_user' class='danger'>Delete</button></form></td></tr>",
                esc(name), esc(token), if *enabled { "Enabled" } else { "Disabled" }, id
            )
        })
        .collect::<String>();
    let platform_rows = by_platform
        .iter()
        .map(|(p, c)| format!("<div class='platform-row'><span>{}</span><strong>{}</strong></div>", esc(p), c))
        .collect::<String>();
    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          <aside class="sidebar"><div class="brand-block"><div class="brand-mark">AL</div><div><div class="brand-title">ArcadeLauncher</div><div class="brand-subtitle">Rust Server</div></div></div><nav><a href="#overview">Overview</a><a href="#library">Library</a><a href="#auth">Auth</a><a href="#config">Configuration</a></nav></aside>
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Private library server</div><h1>Server Administration</h1></div><div class="account-box"><span>Signed in as <strong>{}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {}
            <section id="overview" class="section"><div class="section-heading"><h2>Overview</h2><span class="muted">Rust backend, MariaDB catalog, local file delivery</span></div><div class="metric-grid"><div class="metric"><span>Total Games</span><strong>{}</strong></div><div class="metric"><span>Platforms</span><strong>{}</strong></div><div class="metric"><span>Issued Tokens</span><strong>{}</strong></div><div class="metric"><span>Users</span><strong>{}</strong></div></div></section>
            <section id="library" class="section split"><div><div class="section-heading"><h2>Library Setup</h2><span class="muted">Filesystem stores files; MariaDB stores lookup metadata.</span></div><dl class="kv"><dt>Library Root</dt><dd><code>{}</code></dd><dt>Backend</dt><dd><code>rust/axum</code></dd></dl><form method="post" class="row"><button name="action" value="rescan">Rescan Filesystem and Sync DB</button></form></div><div class="platform-card"><h3>Platform Counts</h3>{}</div></section>
            <section id="auth" class="section"><div class="section-heading"><h2>Auth Management</h2><span class="muted">All users sign in with username/password; bearer tokens are issued behind the scenes.</span></div><div class="two-col"><div><h3>Create User</h3><form method="post" class="row"><input name="username" placeholder="Username"><input name="email" type="email" placeholder="Email"><input name="password" type="password" placeholder="Password"><label class="checkline"><input type="checkbox" name="is_admin" value="1"> Admin</label><button name="action" value="add_user">Create User</button></form><h3>Users</h3><table><thead><tr><th>Username</th><th>Email</th><th>Role</th><th>Status</th></tr></thead><tbody>{}</tbody></table></div><div><h3>Issued Tokens</h3><table><thead><tr><th>Name</th><th>Bearer Token</th><th>Status</th><th>Actions</th></tr></thead><tbody>{}</tbody></table></div></div></section>
          </div>
        </div>
        "##,
        esc(&signed),
        notice(message),
        games.len(),
        by_platform.len(),
        tokens.len(),
        users.len(),
        esc(&st.cfg.library_root.display().to_string()),
        if platform_rows.is_empty() { "<p class='muted'>No cataloged platforms yet.</p>".into() } else { platform_rows },
        if user_rows.is_empty() { "<tr><td colspan='4'>No users yet.</td></tr>".into() } else { user_rows },
        if token_rows.is_empty() { "<tr><td colspan='4'>No issued tokens yet.</td></tr>".into() } else { token_rows },
    )))
}

fn login_html(message: &str) -> String {
    shell(&format!(
        r#"<section><h2>Sign In</h2>{}<form method="post" action="/admin/login" class="stack"><input name="username" placeholder="Username or email" autofocus required><input name="password" type="password" placeholder="Password" required><button name="action" value="login">Sign In</button></form></section>"#,
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
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>ArcadeLauncher Server</title><style>{}</style></head><body><main>{}</main></body></html>"#,
        CSS, body
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

static CSS: &str = r#"
:root{color-scheme:dark;--bg:#0f1115;--panel:#171b21;--panel2:#1d232b;--line:#2c3540;--text:#e8edf2;--muted:#9aa7b5;--accent:#4cc2ff;--bad:#ff6b6b}
*{box-sizing:border-box}body{margin:0;font:14px/1.45 "Segoe UI",sans-serif;background:var(--bg);color:var(--text)}main{width:100%;min-height:100vh}h1,h2,h3{margin:0;letter-spacing:0}h1{font-size:28px}h2{font-size:19px}h3{font-size:15px;margin:18px 0 10px}.admin-layout{display:grid;grid-template-columns:250px 1fr;min-height:100vh}.sidebar{background:#11161d;border-right:1px solid var(--line);padding:24px 18px}.brand-block{display:flex;gap:12px;align-items:center;margin-bottom:28px}.brand-mark{width:42px;height:42px;display:grid;place-items:center;background:var(--accent);color:#041019;font-weight:800;border-radius:8px}.brand-title{font-weight:700}.brand-subtitle,.muted,.eyebrow{color:var(--muted)}nav{display:flex;flex-direction:column;gap:6px}nav a,.buttonlink{color:var(--text);text-decoration:none;padding:9px 10px;border-radius:6px;border:1px solid transparent}nav a:hover,.buttonlink:hover{border-color:var(--line);background:var(--panel)}.content{padding:24px;min-width:0}.topbar{display:flex;justify-content:space-between;gap:16px;align-items:center;margin-bottom:20px}.account-box{display:flex;gap:12px;align-items:center;flex-wrap:wrap}.section{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:18px;margin-bottom:16px}.section-heading{display:flex;justify-content:space-between;gap:14px;align-items:end;margin-bottom:14px}.metric-grid{display:grid;grid-template-columns:repeat(4,minmax(120px,1fr));gap:12px}.metric{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px}.metric span{display:block;color:var(--muted)}.metric strong{font-size:26px}.split{display:grid;grid-template-columns:minmax(0,1fr) 320px;gap:20px}.platform-card{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px}.platform-row{display:flex;justify-content:space-between;border-bottom:1px solid var(--line);padding:7px 0}.kv{display:grid;grid-template-columns:120px minmax(0,1fr);gap:8px 12px}.kv dt{color:var(--muted)}.kv dd{margin:0;min-width:0}.row{display:flex;gap:10px;flex-wrap:wrap;align-items:center}.stack{display:flex;gap:10px;flex-direction:column;align-items:flex-start}.inline{display:flex;gap:8px;flex-wrap:wrap}.checkline{display:inline-flex;align-items:center;gap:6px;color:var(--muted)}input{background:#0c1015;color:var(--text);border:1px solid var(--line);border-radius:6px;padding:9px 10px;min-width:180px}button{background:var(--accent);color:#041019;border:0;border-radius:6px;padding:9px 12px;font-weight:700;cursor:pointer}.danger{background:var(--bad);color:#180406}table{width:100%;border-collapse:collapse;background:var(--panel2);border-radius:8px;overflow:hidden}th,td{text-align:left;border-bottom:1px solid var(--line);padding:9px;vertical-align:top}th{color:var(--muted);font-weight:600}code,.token{overflow-wrap:anywhere}.notice{white-space:pre-wrap;background:#102033;border:1px solid #285b86;padding:12px;border-radius:8px}@media(max-width:900px){.admin-layout{grid-template-columns:1fr}.sidebar{position:static}.metric-grid,.split{grid-template-columns:1fr}.topbar{align-items:flex-start;flex-direction:column}}
"#;
