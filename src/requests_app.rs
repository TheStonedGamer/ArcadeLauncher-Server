// ArcadeLauncher Requests — folded into the catalog server as a sub-app mounted
// under `/requests` on the public router (formerly a standalone binary on :8723).
// Kept as a real `mod` so its many same-named helpers (`now`, `constant_eq`,
// `Config`, `User`, `Session`, `igdb_*`, `health`, …) don't collide with the
// crate-root items the server assembles via `include!`. It reuses the server's
// MariaDB pool and only ever writes its own request_* / game_requests tables;
// it authenticates against the same `admin_users` accounts and launcher_tokens,
// and reads IGDB creds from the shared `server_settings` table.
//
// Lets logged-in launcher users search game releases (via IGDB) and request the
// ones they want added to the catalog.

// Reusing the server pool leaves Config's DB fields + database_url() unused, and
// a few helpers are only reachable on some code paths — all intentional.
#![allow(dead_code)]

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use hmac::{Hmac, Mac};
use mysql_async::{params, prelude::*, Pool, Row};
use rand::{distributions::Alphanumeric, Rng};
use reqwest::Client;
use serde::Deserialize;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::env;

const IGDB_CLIENT_ID_KEY: &str = "igdb.client_id";
const IGDB_CLIENT_SECRET_KEY: &str = "igdb.client_secret";
const SESSION_TTL_SECONDS: i64 = 60 * 60 * 24 * 30; // 30 days
const SESSION_COOKIE: &str = "arq_session";

#[derive(Clone)]
struct AppState {
    db: Pool,
    cfg: Arc<Config>,
}

struct Config {
    host: String,
    port: u16,
    secure_cookie: bool,
    db_host: String,
    db_port: u16,
    db_name: String,
    db_user: String,
    db_password: String,
    // SMTP notification (new-request emails). Disabled if host or to-address is blank.
    smtp_host: String,
    smtp_port: u16,
    smtp_user: String,
    smtp_password: String,
    smtp_starttls: bool,
    notify_from: String,
    notify_to: String,
    public_url: String,
}

impl Config {
    fn from_env() -> Self {
        Self {
            host: env_string("ARCADE_REQ_HOST", "127.0.0.1"),
            port: env_u16("ARCADE_REQ_PORT", 8723),
            secure_cookie: env_string("ARCADE_REQ_SECURE_COOKIE", "false") == "true",
            db_host: env_string("ARCADE_DB_HOST", "127.0.0.1"),
            db_port: env_u16("ARCADE_DB_PORT", 3306),
            db_name: env_string("ARCADE_DB_NAME", "arcadelauncher"),
            db_user: env_string("ARCADE_DB_USER", "arcade"),
            db_password: env_string("ARCADE_DB_PASSWORD", ""),
            smtp_host: env_string("ARCADE_REQ_SMTP_HOST", ""),
            smtp_port: env_u16("ARCADE_REQ_SMTP_PORT", 587),
            smtp_user: env_string("ARCADE_REQ_SMTP_USER", ""),
            smtp_password: env_string("ARCADE_REQ_SMTP_PASSWORD", ""),
            smtp_starttls: env_string("ARCADE_REQ_SMTP_STARTTLS", "true") != "false",
            notify_from: env_string("ARCADE_REQ_NOTIFY_FROM", ""),
            notify_to: env_string("ARCADE_REQ_NOTIFY_TO", "shinyjeesus@gmail.com"),
            public_url: env_string("ARCADE_REQ_PUBLIC_URL", ""),
        }
    }

    fn email_enabled(&self) -> bool {
        !self.smtp_host.is_empty() && !self.notify_to.is_empty()
    }

    // From-address falls back to the SMTP user, then a sensible default.
    fn effective_from(&self) -> String {
        if !self.notify_from.is_empty() {
            self.notify_from.clone()
        } else if !self.smtp_user.is_empty() && self.smtp_user.contains('@') {
            self.smtp_user.clone()
        } else {
            "arcadelauncher-requests@localhost".to_string()
        }
    }

    fn database_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            urlencoding::encode(&self.db_user),
            urlencoding::encode(&self.db_password),
            self.db_host,
            self.db_port,
            self.db_name
        )
    }
}

fn env_string(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_u16(key: &str, default: u16) -> u16 {
    env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Build the Requests sub-app, ready to mount under `/requests` on the server's
/// public router. Reuses the server's MariaDB pool (`db`) rather than opening a
/// second one; the `ARCADE_REQ_*` env still configures cookie security, the
/// new-request SMTP notification, and the public board URL. Returns a fully
/// state-applied `Router<()>` so it can be nested via `nest_service`.
pub async fn router(db: Pool) -> Result<Router> {
    let cfg = Arc::new(Config::from_env());
    ensure_schema(&db).await.context("requests schema init failed")?;
    let state = AppState { db, cfg };

    Ok(Router::new()
        .route("/", get(index_page))
        .route("/health", get(health))
        .route("/login", post(login_post))
        .route("/logout", post(logout_post))
        .route("/api/me", get(api_me))
        .route("/api/search", get(api_search))
        .route("/api/requests", get(api_list_requests).post(api_create_request))
        .route("/api/requests/:id/vote", post(api_vote))
        .route("/api/requests/:id/rating", post(api_rate))
        .route("/api/requests/:id/status", post(api_status))
        .with_state(state))
}

// ── Schema (only our own tables; admin_users / server_settings are owned by the
// catalog server and only read here) ────────────────────────────────────────
async fn ensure_schema(db: &Pool) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS game_requests (
            id BIGINT AUTO_INCREMENT PRIMARY KEY,
            igdb_id BIGINT NOT NULL DEFAULT 0,
            title VARCHAR(255) NOT NULL,
            platform VARCHAR(80) NOT NULL DEFAULT '',
            cover_url VARCHAR(512) NOT NULL DEFAULT '',
            release_date BIGINT NOT NULL DEFAULT 0,
            summary TEXT NULL,
            requested_by BIGINT NOT NULL,
            requested_by_name VARCHAR(190) NOT NULL DEFAULT '',
            note VARCHAR(500) NOT NULL DEFAULT '',
            status VARCHAR(20) NOT NULL DEFAULT 'pending',
            votes INT NOT NULL DEFAULT 1,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            UNIQUE KEY uniq_igdb (igdb_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS request_votes (
            request_id BIGINT NOT NULL,
            user_id BIGINT NOT NULL,
            created_at BIGINT NOT NULL,
            PRIMARY KEY (request_id, user_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS request_ratings (
            request_id BIGINT NOT NULL,
            user_id BIGINT NOT NULL,
            stars TINYINT NOT NULL,
            created_at BIGINT NOT NULL,
            updated_at BIGINT NOT NULL,
            PRIMARY KEY (request_id, user_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS request_sessions (
            token CHAR(64) PRIMARY KEY,
            user_id BIGINT NOT NULL,
            username VARCHAR(190) NOT NULL,
            is_admin BOOLEAN NOT NULL DEFAULT FALSE,
            created_at BIGINT NOT NULL,
            expires_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    Ok(())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true, "service": "requests", "version": env!("CARGO_PKG_VERSION")}))
}

// ── Users (read the catalog server's admin_users table) ──────────────────────
struct User {
    id: u64,
    username: String,
    password_hash: String,
    is_admin: bool,
    totp_secret: Option<String>,
    totp_enabled: bool,
}

async fn find_user(db: &Pool, key: &str) -> Result<Option<User>> {
    let mut c = db.get_conn().await?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT id, username, password_hash, is_admin, totp_secret, totp_enabled \
             FROM admin_users WHERE enabled = TRUE AND (username = :k OR email = :k) LIMIT 1",
            params! {"k" => key.trim()},
        )
        .await?;
    Ok(row.map(|row| {
        let (id, username, password_hash, is_admin, totp_secret, totp_enabled): (
            u64,
            String,
            String,
            bool,
            Option<String>,
            bool,
        ) = mysql_async::from_row(row);
        User { id, username, password_hash, is_admin, totp_secret, totp_enabled }
    }))
}

// ── Sessions ─────────────────────────────────────────────────────────────────
struct Session {
    user_id: u64,
    username: String,
    is_admin: bool,
}

async fn create_session(db: &Pool, user: &User) -> Result<String> {
    let token = random_token(32);
    let token_store = sha256_hex(token.as_bytes());
    let now = now();
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "INSERT INTO request_sessions (token, user_id, username, is_admin, created_at, expires_at) \
         VALUES (:t, :u, :n, :a, :c, :e)",
        params! {
            "t" => &token_store,
            "u" => user.id,
            "n" => &user.username,
            "a" => user.is_admin,
            "c" => now,
            "e" => now + SESSION_TTL_SECONDS,
        },
    )
    .await?;
    Ok(token)
}

async fn session_from_headers(db: &Pool, headers: &HeaderMap) -> Option<Session> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    let token = raw
        .split(';')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == SESSION_COOKIE).then(|| v.trim().to_string())
        })
        .next()?;
    let token_store = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await.ok()?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT user_id, username, is_admin, expires_at FROM request_sessions WHERE token = :t LIMIT 1",
            params! {"t" => token_store},
        )
        .await
        .ok()?;
    let row = row?;
    let (user_id, username, is_admin, expires_at): (u64, String, bool, i64) = mysql_async::from_row(row);
    if expires_at < now() {
        return None;
    }
    Some(Session { user_id, username, is_admin })
}

/// Resolve an `admin_users` row by id (used after a Bearer token maps to a
/// `user_id`). Mirrors `find_user` but keyed on the numeric id.
async fn find_user_by_id(db: &Pool, id: u64) -> Option<(String, bool)> {
    let mut c = db.get_conn().await.ok()?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT username, is_admin FROM admin_users WHERE id = :id AND enabled = TRUE LIMIT 1",
            params! {"id" => id},
        )
        .await
        .ok()?;
    let (username, is_admin): (String, bool) = mysql_async::from_row(row?);
    Some((username, is_admin))
}

/// Resolve a session from an `Authorization: Bearer <token>` header, validating
/// the token against the catalog server's `launcher_tokens` table (the SAME
/// per-user token the launcher already holds after `session_login`). This lets
/// the in-client board authenticate with the launcher's bearer token instead of
/// a separate cookie login — no password re-entry, consistent with the rest of
/// the client. The token is sha256-hashed before lookup, exactly as the catalog
/// server stores and validates it.
async fn session_from_bearer(db: &Pool, headers: &HeaderMap) -> Option<Session> {
    let auth = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = auth.strip_prefix("Bearer ").map(str::trim)?;
    if token.is_empty() {
        return None;
    }
    let hash = sha256_hex(token.as_bytes());
    let mut c = db.get_conn().await.ok()?;
    // user_id is nullable (the static service token has none); deserializing a
    // NULL into Option<u64> yields None, so flatten covers both "no row" and
    // "service token with no user".
    let uid: Option<u64> = c
        .exec_first(
            "SELECT user_id FROM launcher_tokens WHERE token_hash = :h AND enabled = TRUE LIMIT 1",
            params! {"h" => hash},
        )
        .await
        .ok()
        .flatten();
    let uid = uid?;
    let (username, is_admin) = find_user_by_id(db, uid).await?;
    Some(Session { user_id: uid, username, is_admin })
}

/// The signed-in session for a request, accepting EITHER the `arq_session`
/// cookie (the web UI) OR a launcher Bearer token (the in-client board). Cookie
/// is tried first so the existing web flow is unchanged.
async fn current_session(db: &Pool, headers: &HeaderMap) -> Option<Session> {
    if let Some(s) = session_from_headers(db, headers).await {
        return Some(s);
    }
    session_from_bearer(db, headers).await
}

async fn delete_session(db: &Pool, headers: &HeaderMap) {
    if let Some(raw) = headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) {
        if let Some(token) = raw.split(';').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k.trim() == SESSION_COOKIE).then(|| v.trim().to_string())
        }) {
            if let Ok(mut c) = db.get_conn().await {
                let _ = c
                    .exec_drop(
                        "DELETE FROM request_sessions WHERE token = :t",
                        params! {"t" => sha256_hex(token.as_bytes())},
                    )
                    .await;
            }
        }
    }
}

fn cookie_header(token: &str, secure: bool, max_age: i64) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age={max_age}{secure}"
    )
}

// ── Auth crypto (mirrors the catalog server exactly) ─────────────────────────
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
    use rust_scrypt::{scrypt, Params as ScryptParams};
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

fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer = 0u32;
    let mut bits = 0u8;
    for c in s.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let val = match c.to_ascii_uppercase() {
            'A'..='Z' => c.to_ascii_uppercase() as u32 - 'A' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return Err(anyhow!("invalid base32 character")),
        };
        buffer = (buffer << 5) | val;
        bits += 5;
        if bits >= 8 {
            out.push((buffer >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    Ok(out)
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
    chrono::Utc::now().timestamp()
}

// ── IGDB (reads credentials from the shared server_settings table) ───────────
async fn setting_value(db: &Pool, key: &str) -> Result<Option<String>> {
    let mut c = db.get_conn().await?;
    // The catalog server owns this table; its columns are setting_key /
    // setting_value (NOT name / value). Querying the wrong column names raises a
    // MariaDB "Unknown column" error that surfaced to the client as a 503 on
    // search ("IGDB credentials are not configured").
    let row: Option<Row> = c
        .exec_first(
            "SELECT setting_value FROM server_settings WHERE setting_key = :k LIMIT 1",
            params! {"k" => key},
        )
        .await?;
    Ok(row.map(|r| mysql_async::from_row::<(String,)>(r).0))
}

async fn igdb_credentials(db: &Pool) -> Result<(String, String)> {
    let client_id = setting_value(db, IGDB_CLIENT_ID_KEY).await?.unwrap_or_default();
    let client_secret = setting_value(db, IGDB_CLIENT_SECRET_KEY).await?.unwrap_or_default();
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(anyhow!("IGDB credentials are not configured on the catalog server"));
    }
    Ok((client_id, client_secret))
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

#[derive(serde::Serialize)]
struct SearchResult {
    igdb_id: u64,
    name: String,
    summary: String,
    platforms: String,
    cover_url: String,
    release_date: i64,
}

async fn igdb_search(http: &Client, client_id: &str, token: &str, query: &str) -> Result<Vec<SearchResult>> {
    let escaped = query.replace('\\', "\\\\").replace('"', "\\\"");
    let body = format!(
        "search \"{escaped}\";\
         fields id,name,summary,first_release_date,cover.image_id,platforms.abbreviation,platforms.name;\
         limit 50;"
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
    let Some(items) = value.as_array() else { return Ok(Vec::new()); };
    Ok(items.iter().filter_map(parse_search_result).collect())
}

fn parse_search_result(v: &serde_json::Value) -> Option<SearchResult> {
    let igdb_id = v.get("id")?.as_u64()?;
    let name = v.get("name")?.as_str()?.to_string();
    let summary = v.get("summary").and_then(|s| s.as_str()).unwrap_or_default().to_string();
    let release_date = v.get("first_release_date").and_then(|d| d.as_i64()).unwrap_or(0);
    let cover_url = v
        .get("cover")
        .and_then(|c| c.get("image_id"))
        .and_then(|i| i.as_str())
        .map(igdb_cover_url)
        .unwrap_or_default();
    let platforms = v
        .get("platforms")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    p.get("abbreviation")
                        .and_then(|a| a.as_str())
                        .or_else(|| p.get("name").and_then(|n| n.as_str()))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some(SearchResult { igdb_id, name, summary, platforms, cover_url, release_date })
}

fn igdb_cover_url(image_id: &str) -> String {
    format!("https://images.igdb.com/igdb/image/upload/t_cover_big/{image_id}.jpg")
}

// ── Route handlers ───────────────────────────────────────────────────────────
fn json_error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({"error": msg.into()}))).into_response()
}

async fn require_session(st: &AppState, headers: &HeaderMap) -> Result<Session, Response> {
    current_session(&st.db, headers)
        .await
        .ok_or_else(|| json_error(StatusCode::UNAUTHORIZED, "not signed in"))
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
    #[serde(default)]
    totp_code: String,
}

async fn login_post(State(st): State<AppState>, Form(form): Form<LoginForm>) -> Response {
    let user = match find_user(&st.db, &form.username).await {
        Ok(Some(u)) => u,
        Ok(None) => return login_failed(),
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if !verify_password_any(&form.password, &user.password_hash) || !verify_user_totp(&user, &form.totp_code) {
        return login_failed();
    }
    match create_session(&st.db, &user).await {
        Ok(token) => {
            let mut resp = Json(serde_json::json!({"ok": true})).into_response();
            resp.headers_mut().insert(
                header::SET_COOKIE,
                cookie_header(&token, st.cfg.secure_cookie, SESSION_TTL_SECONDS).parse().unwrap(),
            );
            resp
        }
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn login_failed() -> Response {
    json_error(StatusCode::UNAUTHORIZED, "Invalid credentials or 2FA code")
}

async fn logout_post(State(st): State<AppState>, headers: HeaderMap) -> Response {
    delete_session(&st.db, &headers).await;
    let mut resp = Json(serde_json::json!({"ok": true})).into_response();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        cookie_header("", st.cfg.secure_cookie, 0).parse().unwrap(),
    );
    resp
}

async fn api_me(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_session(&st.db, &headers).await {
        Some(s) => Json(serde_json::json!({
            "signedIn": true,
            "username": s.username,
            "isAdmin": s.is_admin
        }))
        .into_response(),
        None => Json(serde_json::json!({"signedIn": false})).into_response(),
    }
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    /// Optional platform filter (abbreviation or name, e.g. "PS2"); blank = any.
    #[serde(default)]
    platform: String,
}

/// Case-insensitive check that an IGDB `platforms` display string (comma-joined
/// abbreviations/names) covers the requested platform. An empty filter matches
/// everything. Matches a whole comma-part exactly, or as a substring so "PS2"
/// hits "PlayStation 2" only when the abbreviation is present (IGDB returns the
/// abbreviation when it has one). Pure → unit-tested.
fn platform_matches(platforms: &str, filter: &str) -> bool {
    let f = filter.trim().to_ascii_lowercase();
    if f.is_empty() {
        return true;
    }
    platforms.split(',').map(|p| p.trim().to_ascii_lowercase()).any(|p| p == f || p.contains(&f))
}

async fn api_search(State(st): State<AppState>, headers: HeaderMap, Query(q): Query<SearchQuery>) -> Response {
    if let Err(resp) = require_session(&st, &headers).await {
        return resp;
    }
    let query = q.q.trim();
    if query.len() < 2 {
        return Json(serde_json::json!({"results": []})).into_response();
    }
    let (client_id, client_secret) = match igdb_credentials(&st.db).await {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::SERVICE_UNAVAILABLE, e.to_string()),
    };
    let http = match Client::builder().user_agent("ArcadeLauncher-Requests/1.0").build() {
        Ok(h) => h,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let token = match igdb_authenticate(&http, &client_id, &client_secret).await {
        Ok(t) => t,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, e.to_string()),
    };
    match igdb_search(&http, &client_id, &token, query).await {
        Ok(results) => {
            let filtered: Vec<SearchResult> = results
                .into_iter()
                .filter(|r| platform_matches(&r.platforms, &q.platform))
                .collect();
            Json(serde_json::json!({"results": filtered})).into_response()
        }
        Err(e) => json_error(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    igdb_id: u64,
    title: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    cover_url: String,
    #[serde(default)]
    release_date: i64,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    note: String,
}

async fn api_create_request(State(st): State<AppState>, headers: HeaderMap, Json(req): Json<CreateRequest>) -> Response {
    let session = match require_session(&st, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if req.title.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "title is required");
    }
    let note: String = req.note.chars().take(500).collect();
    let now = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    // A request for the same IGDB id already on the board becomes an upvote
    // instead of a duplicate row.
    if req.igdb_id != 0 {
        let existing: Option<u64> = match c
            .exec_first(
                "SELECT id FROM game_requests WHERE igdb_id = :g LIMIT 1",
                params! {"g" => req.igdb_id},
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        if let Some(id) = existing {
            return add_vote(&st, id, &session).await;
        }
    }

    let res = c
        .exec_drop(
            "INSERT INTO game_requests \
             (igdb_id, title, platform, cover_url, release_date, summary, requested_by, requested_by_name, note, status, votes, created_at, updated_at) \
             VALUES (:g, :t, :p, :c, :r, :s, :u, :n, :note, 'pending', 1, :now, :now)",
            params! {
                "g" => req.igdb_id,
                "t" => req.title.trim(),
                "p" => req.platform.trim(),
                "c" => req.cover_url.trim(),
                "r" => req.release_date,
                "s" => req.summary.trim(),
                "u" => session.user_id,
                "n" => &session.username,
                "note" => note,
                "now" => now,
            },
        )
        .await;
    if let Err(e) = res {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }
    let new_id = c.last_insert_id().unwrap_or(0);
    let _ = c
        .exec_drop(
            "INSERT IGNORE INTO request_votes (request_id, user_id, created_at) VALUES (:r, :u, :now)",
            params! {"r" => new_id, "u" => session.user_id, "now" => now},
        )
        .await;

    // Fire-and-forget admin notification; never blocks or fails the request.
    if st.cfg.email_enabled() {
        let cfg = st.cfg.clone();
        let mail = RequestEmail {
            title: req.title.trim().to_string(),
            platform: req.platform.trim().to_string(),
            release_date: req.release_date,
            cover_url: req.cover_url.trim().to_string(),
            summary: req.summary.trim().to_string(),
            note: req.note.trim().to_string(),
            requester: session.username.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = send_request_email(&cfg, &mail).await {
                tracing::warn!("failed to send request notification email: {e}");
            }
        });
    }

    Json(serde_json::json!({"ok": true, "id": new_id})).into_response()
}

async fn add_vote(st: &AppState, request_id: u64, session: &Session) -> Response {
    let now = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let inserted = c
        .exec_iter(
            "INSERT IGNORE INTO request_votes (request_id, user_id, created_at) VALUES (:r, :u, :now)",
            params! {"r" => request_id, "u" => session.user_id, "now" => now},
        )
        .await
        .map(|r| r.affected_rows())
        .unwrap_or(0);
    if inserted > 0 {
        let _ = c
            .exec_drop(
                "UPDATE game_requests SET votes = votes + 1, updated_at = :now WHERE id = :r",
                params! {"r" => request_id, "now" => now},
            )
            .await;
    }
    Json(serde_json::json!({"ok": true, "id": request_id, "voted": inserted > 0})).into_response()
}

async fn api_vote(State(st): State<AppState>, headers: HeaderMap, AxumPath(id): AxumPath<u64>) -> Response {
    let session = match require_session(&st, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    add_vote(&st, id, &session).await
}

/// Clamp an incoming star value to the accepted 1..=5 range. Pure → unit-tested.
fn clamp_stars(stars: i64) -> Option<u8> {
    if (1..=5).contains(&stars) {
        Some(stars as u8)
    } else {
        None
    }
}

#[derive(Deserialize)]
struct RateBody {
    stars: i64,
}

/// Upsert the signed-in user's 1–5 star rating for a request (community game
/// rating, one vote per user, changeable). Returns the fresh average + count so
/// the UI can update without a full board refetch.
async fn api_rate(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<u64>,
    Json(body): Json<RateBody>,
) -> Response {
    let session = match require_session(&st, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let Some(stars) = clamp_stars(body.stars) else {
        return json_error(StatusCode::BAD_REQUEST, "stars must be between 1 and 5");
    };
    let now = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };

    // Only rate a request that actually exists (no FK on the ratings table).
    let exists: Option<u64> = match c
        .exec_first("SELECT id FROM game_requests WHERE id = :r", params! {"r" => id})
        .await
    {
        Ok(v) => v,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if exists.is_none() {
        return json_error(StatusCode::NOT_FOUND, "no such request");
    }

    let res = c
        .exec_drop(
            "INSERT INTO request_ratings (request_id, user_id, stars, created_at, updated_at) \
             VALUES (:r, :u, :s, :now, :now) \
             ON DUPLICATE KEY UPDATE stars = :s, updated_at = :now",
            params! {"r" => id, "u" => session.user_id, "s" => stars, "now" => now},
        )
        .await;
    if let Err(e) = res {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
    }

    // Fresh aggregate for an immediate UI update.
    let agg: Option<Row> = c
        .exec_first(
            "SELECT CAST(AVG(stars) AS DOUBLE) AS a, COUNT(*) AS c FROM request_ratings WHERE request_id = :r",
            params! {"r" => id},
        )
        .await
        .unwrap_or(None);
    let (rating_avg, rating_count) = match agg {
        Some(mut row) => (
            // NULL avg (no ratings) read straight into f64 would panic; go via Option.
            row.take::<Option<f64>, _>("a").unwrap_or(None).unwrap_or(0.0),
            row.take::<i64, _>("c").unwrap_or(0),
        ),
        None => (0.0, 0),
    };

    Json(serde_json::json!({
        "ok": true,
        "id": id,
        "myRating": stars,
        "ratingAvg": rating_avg,
        "ratingCount": rating_count,
    }))
    .into_response()
}

async fn api_list_requests(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let session = match require_session(&st, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let rows: Vec<Row> = match c
        .exec(
            "SELECT r.id, r.igdb_id, r.title, r.platform, r.cover_url, r.release_date, r.summary, \
                    r.requested_by_name, r.note, r.status, r.votes, r.created_at, \
                    (SELECT COUNT(*) FROM request_votes v WHERE v.request_id = r.id AND v.user_id = :me) AS mine, \
                    (SELECT CAST(AVG(g.stars) AS DOUBLE) FROM request_ratings g WHERE g.request_id = r.id) AS rating_avg, \
                    (SELECT COUNT(*) FROM request_ratings g WHERE g.request_id = r.id) AS rating_count, \
                    (SELECT g.stars FROM request_ratings g WHERE g.request_id = r.id AND g.user_id = :me) AS my_rating \
             FROM game_requests r \
             ORDER BY FIELD(r.status,'pending','approved','fulfilled','declined'), r.votes DESC, r.created_at ASC",
            params! {"me" => session.user_id},
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|mut row| {
            let id: u64 = row.take("id").unwrap_or(0);
            let igdb_id: u64 = row.take("igdb_id").unwrap_or(0);
            let title: String = row.take("title").unwrap_or_default();
            let platform: String = row.take("platform").unwrap_or_default();
            let cover_url: String = row.take("cover_url").unwrap_or_default();
            let release_date: i64 = row.take("release_date").unwrap_or(0);
            let summary: Option<String> = row.take("summary").unwrap_or_default();
            let by: String = row.take("requested_by_name").unwrap_or_default();
            let note: String = row.take("note").unwrap_or_default();
            let status: String = row.take("status").unwrap_or_default();
            let votes: i64 = row.take("votes").unwrap_or(0);
            let created_at: i64 = row.take("created_at").unwrap_or(0);
            let mine: i64 = row.take("mine").unwrap_or(0);
            // AVG() is NULL in MariaDB when a request has no ratings. Reading a
            // NULL straight into f64 PANICS (kills the worker → nginx 502), so
            // read through Option<f64> and default to 0.0. (SQL also CASTs the
            // AVG to DOUBLE so it isn't a DECIMAL.)
            let rating_avg: f64 = row.take::<Option<f64>, _>("rating_avg").unwrap_or(None).unwrap_or(0.0);
            let rating_count: i64 = row.take("rating_count").unwrap_or(0);
            // my_rating is NULL when the caller hasn't rated → 0.
            let my_rating: Option<i64> = row.take("my_rating").unwrap_or(None);
            serde_json::json!({
                "id": id,
                "igdbId": igdb_id,
                "title": title,
                "platform": platform,
                "coverUrl": cover_url,
                "releaseDate": release_date,
                "summary": summary.unwrap_or_default(),
                "requestedBy": by,
                "note": note,
                "status": status,
                "votes": votes,
                "createdAt": created_at,
                "votedByMe": mine > 0,
                "ratingAvg": rating_avg,
                "ratingCount": rating_count,
                "myRating": my_rating.unwrap_or(0),
            })
        })
        .collect();
    Json(serde_json::json!({"requests": items, "isAdmin": session.is_admin})).into_response()
}

#[derive(Deserialize)]
struct StatusUpdate {
    status: String,
}

async fn api_status(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<u64>,
    Json(body): Json<StatusUpdate>,
) -> Response {
    let session = match require_session(&st, &headers).await {
        Ok(s) => s,
        Err(resp) => return resp,
    };
    if !session.is_admin {
        return json_error(StatusCode::FORBIDDEN, "admin only");
    }
    let status = body.status.as_str();
    if !matches!(status, "pending" | "approved" | "fulfilled" | "declined") {
        return json_error(StatusCode::BAD_REQUEST, "invalid status");
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let res = c
        .exec_drop(
            "UPDATE game_requests SET status = :s, updated_at = :now WHERE id = :id",
            params! {"s" => status, "now" => now(), "id" => id},
        )
        .await;
    match res {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => json_error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

// ── Email notification ────────────────────────────────────────────────────────
struct RequestEmail {
    title: String,
    platform: String,
    release_date: i64,
    cover_url: String,
    summary: String,
    note: String,
    requester: String,
}

fn release_year(ts: i64) -> Option<i64> {
    if ts <= 0 {
        return None;
    }
    use chrono::{DateTime, Datelike, Utc};
    DateTime::<Utc>::from_timestamp(ts, 0).map(|d| d.year() as i64)
}

async fn send_request_email(cfg: &Config, mail: &RequestEmail) -> Result<()> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let from: Mailbox = cfg
        .effective_from()
        .parse()
        .context("invalid ARCADE_REQ_NOTIFY_FROM / SMTP from address")?;
    let to: Mailbox = cfg
        .notify_to
        .parse()
        .context("invalid ARCADE_REQ_NOTIFY_TO address")?;

    let year = release_year(mail.release_date)
        .map(|y| y.to_string())
        .unwrap_or_default();
    let meta = [year.as_str(), mail.platform.as_str()]
        .iter()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    let subject = format!("New game request: {}", mail.title);

    // Plain-text body (always present).
    let mut text = format!("{}\n", mail.title);
    if !meta.is_empty() {
        text.push_str(&format!("{meta}\n"));
    }
    text.push_str(&format!("Requested by: {}\n", mail.requester));
    if !mail.note.is_empty() {
        text.push_str(&format!("Note: {}\n", mail.note));
    }
    if !mail.summary.is_empty() {
        text.push_str(&format!("\n{}\n", mail.summary));
    }
    if !cfg.public_url.is_empty() {
        text.push_str(&format!("\nReview the board: {}\n", cfg.public_url));
    }

    // HTML body with the cover art.
    let cover_html = if mail.cover_url.is_empty() {
        String::new()
    } else {
        format!(
            "<img src=\"{}\" alt=\"\" style=\"width:120px;border-radius:8px;float:left;margin:0 16px 8px 0\"/>",
            html_escape(&mail.cover_url)
        )
    };
    let note_html = if mail.note.is_empty() {
        String::new()
    } else {
        format!("<p style=\"margin:8px 0\"><em>{}</em></p>", html_escape(&mail.note))
    };
    let summary_html = if mail.summary.is_empty() {
        String::new()
    } else {
        format!("<p style=\"margin:8px 0;color:#444\">{}</p>", html_escape(&mail.summary))
    };
    let link_html = if cfg.public_url.is_empty() {
        String::new()
    } else {
        format!(
            "<p style=\"margin:16px 0 0\"><a href=\"{0}\">Review the request board</a></p>",
            html_escape(&cfg.public_url)
        )
    };
    let html = format!(
        "<div style=\"font:14px/1.5 -apple-system,Segoe UI,sans-serif;color:#111\">\
           {cover}\
           <h2 style=\"margin:0 0 2px\">{title}</h2>\
           <p style=\"margin:0 0 8px;color:#666\">{meta}</p>\
           <p style=\"margin:0\">Requested by <strong>{by}</strong></p>\
           {note}{summary}{link}\
           <div style=\"clear:both\"></div>\
         </div>",
        cover = cover_html,
        title = html_escape(&mail.title),
        meta = html_escape(&meta),
        by = html_escape(&mail.requester),
        note = note_html,
        summary = summary_html,
        link = link_html,
    );

    let email = Message::builder()
        .from(from)
        .to(to)
        .subject(subject)
        .multipart(
            MultiPart::alternative()
                .singlepart(SinglePart::builder().header(ContentType::TEXT_PLAIN).body(text))
                .singlepart(SinglePart::builder().header(ContentType::TEXT_HTML).body(html)),
        )?;

    let builder = if cfg.smtp_starttls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.smtp_host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.smtp_host)?
    };
    let mut builder = builder.port(cfg.smtp_port);
    if !cfg.smtp_user.is_empty() {
        builder = builder.credentials(Credentials::new(
            cfg.smtp_user.clone(),
            cfg.smtp_password.clone(),
        ));
    }
    let mailer = builder.build();
    mailer.send(email).await?;
    Ok(())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = include_str!("requests_index.html");

#[cfg(test)]
mod tests {
    use super::{clamp_stars, platform_matches};

    #[test]
    fn empty_platform_filter_matches_everything() {
        assert!(platform_matches("PC, Switch", ""));
        assert!(platform_matches("", "  "));
    }

    #[test]
    fn platform_filter_matches_a_comma_part_case_insensitively() {
        assert!(platform_matches("PC, Switch", "switch"));
        assert!(platform_matches("PS2, PS3", "PS2"));
        assert!(!platform_matches("PC, Switch", "PS2"));
    }

    #[test]
    fn platform_filter_matches_as_substring() {
        // Full platform name present (no abbreviation) still matches a fragment.
        assert!(platform_matches("PlayStation 2", "PlayStation"));
        assert!(!platform_matches("PlayStation 2", "Xbox"));
    }

    #[test]
    fn clamp_stars_accepts_one_to_five() {
        for s in 1..=5 {
            assert_eq!(clamp_stars(s), Some(s as u8));
        }
    }

    #[test]
    fn clamp_stars_rejects_out_of_range() {
        assert_eq!(clamp_stars(0), None);
        assert_eq!(clamp_stars(6), None);
        assert_eq!(clamp_stars(-3), None);
    }
}
