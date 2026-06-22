// models.rs - split out of main.rs and re-assembled via include! (crate-root scope).


#[derive(Clone)]
struct AppState {
    cfg: Arc<Config>,
    db: Pool,
    scan: Arc<std::sync::Mutex<ScanStatus>>,
    // username -> (nonce, expires_at) for single-use challenge-response login.
    challenges: Arc<std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>>,
    // admin-login brute-force throttle: key -> (consecutive_failures, locked_until_epoch).
    login_throttle: Arc<std::sync::Mutex<std::collections::HashMap<String, (u32, i64)>>>,
}

// Admin login lockout policy.
const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_LOCKOUT_SECS: i64 = 300;

impl AppState {
    // Returns Some(seconds_remaining) if the key is currently locked out.
    fn login_locked(&self, key: &str) -> Option<i64> {
        let guard = self.login_throttle.lock().ok()?;
        let (fails, until) = guard.get(key)?;
        if *fails >= LOGIN_MAX_FAILURES && *until > now() {
            Some(*until - now())
        } else {
            None
        }
    }
    fn record_login_failure(&self, key: &str) {
        if let Ok(mut g) = self.login_throttle.lock() {
            let entry = g.entry(key.to_string()).or_insert((0, 0));
            entry.0 += 1;
            if entry.0 >= LOGIN_MAX_FAILURES {
                entry.1 = now() + LOGIN_LOCKOUT_SECS;
            }
        }
    }
    fn clear_login_failures(&self, key: &str) {
        if let Ok(mut g) = self.login_throttle.lock() {
            g.remove(key);
        }
    }
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
    #[serde(default)]
    current_file: String, // file currently being hashed within `current`
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
    admin_secure_cookie: bool,
    db_host: String,
    db_port: u16,
    db_name: String,
    db_user: String,
    db_password: String,
    // Optional Redis URL (e.g. redis://10.0.0.x:6379). When set, the social
    // gateway fans real-time events out across instances via pub/sub and tracks a
    // cross-instance online registry. Unset ⇒ single-instance in-process behavior.
    redis_url: Option<String>,
    // Optional S3/MinIO config for DM attachments (ROADMAP 1.3). Some(...) only
    // when endpoint+bucket+access+secret are all set; otherwise attachment
    // endpoints return 503 and the feature is dormant (like Redis above).
    s3: Option<S3Config>,
    // Optional TURN/STUN config for WebRTC voice (ROADMAP T9g). Some(...) only
    // when the shared secret + at least one TURN URL are set; otherwise
    // /api/social/turn returns STUN-only and voice falls back to STUN.
    turn: Option<TurnConfig>,
    // Self-service registration (ROADMAP TSd). Closed by default: deploying the
    // binary never silently opens signup — flip ARCADE_REGISTRATION_OPEN=true.
    registration_open: bool,
    // Optional SMTP for sending those approval emails. Some(...) only when host +
    // From are set; otherwise the request is logged with the approve/deny links.
    smtp: Option<SmtpConfig>,
}

// Outbound SMTP for admin notifications (registration approvals). Auth is
// optional (username empty ⇒ unauthenticated relay). STARTTLS by default
// (port 587); set ARCADE_SMTP_STARTTLS=false for implicit-TLS submission.
#[derive(Clone)]
struct SmtpConfig {
    host: String,
    port: u16,
    username: String,
    password: String,
    from: String,
    starttls: bool,
}

// coturn "static-auth-secret" (TURN REST API) settings. The server mints
// short-lived credentials per call: username = "<expiry>:<userId>", credential =
// base64(HMAC-SHA1(secret, username)). The secret is shared with the coturn
// process (its `static-auth-secret`) and never leaves the server.
#[derive(Clone)]
struct TurnConfig {
    secret: String,
    // Fully-formed ICE URLs, e.g. ["turn:turn.example.net:3478?transport=udp",
    //  "turns:turn.example.net:5349?transport=tcp"].
    urls: Vec<String>,
    // Optional extra STUN-only URLs advertised alongside TURN.
    stun_urls: Vec<String>,
    // Credential lifetime in seconds.
    ttl: i64,
}

// S3-compatible object store (MinIO) connection for DM attachments. Path-style
// addressing; presigned URLs are generated in-process (src/s3.rs) so clients
// upload/download directly to the store and bytes never transit this server.
#[derive(Clone)]
struct S3Config {
    endpoint: String,    // e.g. http://10.0.0.220:9000 (no trailing slash)
    region: String,      // e.g. us-east-1 (MinIO ignores but SigV4 needs it)
    bucket: String,      // e.g. arcade-attachments
    access_key: String,
    secret_key: String,
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
            admin_secure_cookie: env_string("ARCADE_ADMIN_SECURE_COOKIE", "false") == "true",
            db_host: env_string("ARCADE_DB_HOST", "127.0.0.1"),
            db_port: env_u16("ARCADE_DB_PORT", 3306),
            db_name: env_string("ARCADE_DB_NAME", "arcadelauncher"),
            db_user: env_string("ARCADE_DB_USER", "arcade"),
            db_password: env_string("ARCADE_DB_PASSWORD", ""),
            redis_url: {
                let u = env_string("ARCADE_REDIS_URL", "");
                if u.is_empty() { None } else { Some(u) }
            },
            s3: {
                let endpoint = env_string("ARCADE_S3_ENDPOINT", "");
                let bucket = env_string("ARCADE_S3_BUCKET", "");
                let access_key = env_string("ARCADE_S3_ACCESS_KEY", "");
                let secret_key = env_string("ARCADE_S3_SECRET_KEY", "");
                if endpoint.is_empty() || bucket.is_empty() || access_key.is_empty() || secret_key.is_empty() {
                    None
                } else {
                    Some(S3Config {
                        endpoint: endpoint.trim_end_matches('/').to_string(),
                        region: env_string("ARCADE_S3_REGION", "us-east-1"),
                        bucket,
                        access_key,
                        secret_key,
                    })
                }
            },
            turn: {
                let secret = env_string("ARCADE_TURN_SECRET", "");
                // Comma-separated TURN URLs (turn:/turns:) and optional STUN URLs.
                let urls: Vec<String> = env_string("ARCADE_TURN_URLS", "")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let stun_urls: Vec<String> = env_string("ARCADE_STUN_URLS", "")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if secret.is_empty() || urls.is_empty() {
                    None
                } else {
                    Some(TurnConfig {
                        secret,
                        urls,
                        stun_urls,
                        ttl: (env_u64("ARCADE_TURN_TTL", 3600) as i64).max(60),
                    })
                }
            },
            registration_open: env_string("ARCADE_REGISTRATION_OPEN", "false") == "true",
            smtp: {
                let host = env_string("ARCADE_SMTP_HOST", "");
                let from = env_string("ARCADE_SMTP_FROM", "");
                if host.is_empty() || from.is_empty() {
                    None
                } else {
                    Some(SmtpConfig {
                        host,
                        port: env_u16("ARCADE_SMTP_PORT", 587),
                        username: env_string("ARCADE_SMTP_USER", ""),
                        password: env_string("ARCADE_SMTP_PASS", ""),
                        from,
                        starttls: env_string("ARCADE_SMTP_STARTTLS", "true") != "false",
                    })
                }
            },
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
    // Screenshot / artwork image URLs (IGDB), newest-first as returned by IGDB.
    // Stored newline-joined in the `screenshots` TEXT column; surfaced to the
    // client as a JSON string array.
    #[serde(default)]
    screenshots: Vec<String>,
    // Company / franchise metadata (IGDB). Stored in the developer/publisher/
    // franchise TEXT columns; surfaced to the client as plain strings.
    #[serde(default)]
    developer: String,
    #[serde(default)]
    publisher: String,
    #[serde(default)]
    franchise: String,
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
    screenshots: Vec<String>,
    developer: String,
    publisher: String,
    franchise: String,
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
    // Present for GameCube/Wii games that have a sibling `<rom>.textures.zip`
    // custom-texture pack on the NAS. The client extracts it into Dolphin's
    // Load/Textures folder. Omitted entirely when there is no pack.
    #[serde(skip_serializing_if = "Option::is_none")]
    texture_pack: Option<TexturePack>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TexturePack {
    size: u64,
    sha256: String,
    url: String,
}

// Reserved stored-manifest path marking the Dolphin texture-pack zip. It is
// filtered out of the manifest's `files` (so the client never installs it into
// the game root) and promoted to the `texture_pack` field instead.
const TEXTURE_PACK_SENTINEL: &str = "::dolphin-texture-pack::";

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
    enabled: Option<String>,
    user_id: Option<u64>,
    setting_key: Option<String>,
    setting_value: Option<String>,
    service_name: Option<String>,
    totp_code: Option<String>,
    game_id: Option<String>,
    search_query: Option<String>,
    igdb_id: Option<u64>,
    // Changelog authoring fields (add_changelog / delete_changelog actions).
    changelog_id: Option<u64>,
    changelog_version: Option<String>,
    changelog_title: Option<String>,
    changelog_body: Option<String>,
    // Which admin page a POST should re-render ("accounts" / "requests"); empty
    // → the main dashboard. Lets the shared admin_post dispatch serve all pages.
    return_to: Option<String>,
    // Pending-signup approval (approve_pending / deny_pending actions).
    pending_id: Option<u64>,
    // Game-request triage (set_request_status / delete_request actions).
    request_id: Option<u64>,
    request_status: Option<String>,
    // Social test harness (bot_* actions on the Social Test admin page).
    target_id: Option<u64>,
    bot_id: Option<u64>,
    bot_name: Option<String>,
    presence_state: Option<String>,
    status_text: Option<String>,
    game_title: Option<String>,
    activity_kind: Option<String>,
    activity_value: Option<i64>,
    message_body: Option<String>,
}

#[derive(Default, Deserialize)]
struct MetadataQuery {
    game_id: Option<String>,
    search_query: Option<String>,
}

