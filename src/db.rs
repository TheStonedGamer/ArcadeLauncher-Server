// db.rs - split out of main.rs and re-assembled via include! (crate-root scope).

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

async fn find_user_by_id(db: &Pool, id: u64) -> Result<Option<User>> {
    let mut c = db.get_conn().await?;
    let row: Option<Row> = c
        .exec_first(
            "SELECT id, username, email, password_hash, is_admin, enabled, totp_secret, totp_enabled FROM admin_users WHERE id = :id LIMIT 1",
            params! {"id" => id},
        )
        .await?;
    Ok(row.map(user_from_row))
}

// Resolve the launcher account behind a Bearer token. Unlike `authorized_api`
// (which also accepts the static service token), account endpoints require a
// real per-user token so they can act on that user's row.
async fn launcher_user(st: &AppState, headers: &HeaderMap) -> Option<User> {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    let token = auth.strip_prefix("Bearer ").map(str::trim)?;
    let hash = sha256_hex(token.as_bytes());
    let mut c = st.db.get_conn().await.ok()?;
    let uid: Option<u64> = c
        .exec_first(
            "SELECT user_id FROM launcher_tokens WHERE token_hash=:h AND enabled=TRUE LIMIT 1",
            params! {"h" => hash},
        )
        .await
        .ok()
        .flatten();
    find_user_by_id(&st.db, uid?).await.ok().flatten()
}

// Profile picture (avatar) accessors. Stored inline on the user row; bytes are
// only loaded on demand by the avatar GET endpoint, never in the hot user fetch.
async fn get_user_avatar(db: &Pool, user_id: u64) -> Result<Option<(Vec<u8>, String)>> {
    let mut c = db.get_conn().await?;
    let row: Option<(Option<Vec<u8>>, Option<String>)> = c
        .exec_first(
            "SELECT avatar, avatar_mime FROM admin_users WHERE id=:id",
            params! {"id" => user_id},
        )
        .await?;
    Ok(row.and_then(|(bytes, mime)| match (bytes, mime) {
        (Some(b), Some(m)) if !b.is_empty() => Some((b, m)),
        _ => None,
    }))
}

async fn get_user_avatar_version(db: &Pool, user_id: u64) -> i64 {
    let Ok(mut c) = db.get_conn().await else { return 0; };
    let row: Result<Option<i64>, _> = c
        .exec_first("SELECT avatar_updated FROM admin_users WHERE id=:id", params! {"id" => user_id})
        .await;
    row.ok().flatten().unwrap_or(0)
}

async fn set_user_avatar(db: &Pool, user_id: u64, bytes: &[u8], mime: &str) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE admin_users SET avatar=:a, avatar_mime=:m, avatar_updated=:t WHERE id=:id",
        params! {"a" => bytes.to_vec(), "m" => mime, "t" => now(), "id" => user_id},
    )
    .await?;
    Ok(())
}

async fn clear_user_avatar(db: &Pool, user_id: u64) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE admin_users SET avatar=NULL, avatar_mime=NULL, avatar_updated=:t WHERE id=:id",
        params! {"t" => now(), "id" => user_id},
    )
    .await?;
    Ok(())
}

async fn user_must_change_password(db: &Pool, id: u64) -> bool {
    let Ok(mut c) = db.get_conn().await else { return false; };
    let row: Result<Option<bool>, _> = c
        .exec_first("SELECT must_change_password FROM admin_users WHERE id=:id", params! {"id" => id})
        .await;
    row.ok().flatten().unwrap_or(false)
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

// Apply a partial update to an account. Each field is optional; password changes
// also refresh the challenge-response auth_key and clear must_change_password.
async fn update_user(
    db: &Pool,
    target: &User,
    email: Option<&str>,
    password: Option<&str>,
    is_admin: Option<bool>,
    enabled: Option<bool>,
) -> Result<()> {
    let mut c = db.get_conn().await?;
    if let Some(email) = email {
        let email = email.trim();
        if !email.is_empty() {
            c.exec_drop(
                "UPDATE admin_users SET email=:e WHERE id=:id",
                params! {"e" => email, "id" => target.id},
            )
            .await?;
        }
    }
    if let Some(pw) = password {
        let hash = hash_password_argon2(pw)?;
        let auth_key = derive_auth_key(&target.username, pw);
        c.exec_drop(
            "UPDATE admin_users SET password_hash=:p, auth_key=:k, must_change_password=FALSE WHERE id=:id",
            params! {"p" => hash, "k" => auth_key, "id" => target.id},
        )
        .await?;
    }
    if let Some(is_admin) = is_admin {
        c.exec_drop(
            "UPDATE admin_users SET is_admin=:a WHERE id=:id",
            params! {"a" => is_admin, "id" => target.id},
        )
        .await?;
    }
    if let Some(enabled) = enabled {
        c.exec_drop(
            "UPDATE admin_users SET enabled=:e WHERE id=:id",
            params! {"e" => enabled, "id" => target.id},
        )
        .await?;
    }
    Ok(())
}

// Delete a user account and its dependent token/session rows. Refuses to remove
// the final enabled admin so the admin panel can never be locked out.
async fn delete_user_account(db: &Pool, id: u64) -> Result<()> {
    let mut c = db.get_conn().await?;
    let target_is_admin: Option<bool> = c
        .exec_first("SELECT is_admin FROM admin_users WHERE id=:id", params! {"id" => id})
        .await?;
    if target_is_admin.is_none() {
        return Err(anyhow!("user not found"));
    }
    if target_is_admin == Some(true) {
        let admin_count: Option<u64> = c
            .query_first("SELECT COUNT(*) FROM admin_users WHERE is_admin=TRUE AND enabled=TRUE")
            .await?;
        if admin_count.unwrap_or(0) <= 1 {
            return Err(anyhow!("cannot delete the last remaining admin account"));
        }
    }
    c.exec_drop("DELETE FROM launcher_tokens WHERE user_id=:id", params! {"id" => id}).await?;
    c.exec_drop("DELETE FROM admin_sessions WHERE admin_id=:id", params! {"id" => id}).await?;
    c.exec_drop("DELETE FROM admin_users WHERE id=:id", params! {"id" => id}).await?;
    Ok(())
}

async fn issue_user_token(db: &Pool, user_id: u64, username: &str) -> Result<String> {
    let mut c = db.get_conn().await?;
    // Reuse an existing enabled token instead of minting a fresh one on every
    // login. The launcher runs several independent ServerClient instances (the
    // background download worker plus each periodic catalog re-sync), each of
    // which logs in on its own. Since we keep a single token row per user,
    // rotating the token here would invalidate an in-flight download the moment
    // a re-sync logs in — surfacing as a mid-download 401. Returning the stable
    // existing token lets all concurrent clients share one credential. Admins
    // can still force rotation via rotate_launcher_token.
    let existing: Option<(u64, Option<String>, bool)> = c
        .exec_first(
            "SELECT id, token_plain, enabled FROM launcher_tokens WHERE user_id = :id LIMIT 1",
            params! {"id" => user_id},
        )
        .await?;
    if let Some((_, plain_opt, enabled)) = &existing {
        if *enabled {
            if let Some(plain) = plain_opt {
                if !plain.is_empty() {
                    return Ok(plain.clone());
                }
            }
        }
    }

    let token = random_token(36);
    let token_hash = sha256_hex(token.as_bytes());
    match existing {
        Some((id, _, _)) => {
            c.exec_drop(
                "UPDATE launcher_tokens SET name=:n, token_hash=:h, token_plain=:p, enabled=TRUE, created_at=:t WHERE id=:id",
                params! {"n" => username, "h" => token_hash, "p" => &token, "t" => now(), "id" => id},
            )
            .await?;
        }
        None => {
            c.exec_drop(
                "INSERT INTO launcher_tokens (name,user_id,token_hash,token_plain,enabled,created_at) VALUES (:n,:u,:h,:p,TRUE,:t)",
                params! {"n" => username, "u" => user_id, "h" => token_hash, "p" => &token, "t" => now()},
            )
            .await?;
        }
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

