// crypto.rs - split out of main.rs and re-assembled via include! (crate-root scope).

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

async fn set_must_change_password(db: &Pool, user_id: u64, value: bool) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE admin_users SET must_change_password=:v WHERE id=:id",
        params! {"v" => value, "id" => user_id},
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

fn session_cookie_value(token: &str, secure: bool) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/{secure_attr}; Max-Age={SESSION_TTL_SECONDS}")
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
        .replace('-', " ")
        // Dot-separated scene names (Super.Mario.64) become spaced words so the
        // IGDB search and similarity pass treat them as real word boundaries.
        .replace('.', " ");
    without_brackets.split_whitespace().collect::<Vec<_>>().join(" ")
}

// Tokens commonly appended to ROM/repack filenames (region, dump, and scene-group
// markers) that never appear in an IGDB game name. Dropping them before the
// Jaccard comparison stops them from dragging the similarity score below the
// match threshold for otherwise-correct titles.
fn is_noise_token(t: &str) -> bool {
    const NOISE: &[&str] = &[
        "usa", "eur", "europe", "jpn", "jap", "japan", "ntsc", "pal", "world",
        "proper", "repack", "readnfo", "multi", "disc", "disk", "cd", "dvd",
        "iso", "decrypted", "encrypted", "demo", "beta", "unl", "rev", "fitgirl",
        "dodi", "elamigos", "razor1911", "codex", "plaza", "skidrow",
    ];
    if NOISE.contains(&t) {
        return true;
    }
    // Pure version markers like v1, v10, v1_0 -> "v1", "v10".
    t.len() > 1 && t.starts_with('v') && t[1..].chars().all(|c| c.is_ascii_digit())
}

// Normalized, noise-filtered token string used for fuzzy title matching. Built on
// normalize_title (lowercase + punctuation->space) then strips the noise tokens.
fn match_key(title: &str) -> String {
    normalize_title(title)
        .split_whitespace()
        .filter(|t| !is_noise_token(t))
        .collect::<Vec<_>>()
        .join(" ")
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
// Quiet-period (seconds) the filesystem watcher waits after the last change
// before kicking a rescan, so a multi-file copy collapses into one rescan.
const LIBRARY_WATCH_DEBOUNCE_SECS: u64 = 5;

// Event-driven catalog refresh that replaces fixed-interval polling. A `notify`
// watcher on <library_root>/games fires on Create/Modify/Remove/Rename events; a
// debounced async task collapses bursts into a single incremental rescan once
// the filesystem goes quiet. Because manifests are cached per (id, version),
// unchanged games are no-ops, so the triggered rescan only re-hashes what
// actually changed. Returns false if the watcher couldn't be initialised, so the
// caller can fall back to interval polling.
