// auth.rs - split out of main.rs and re-assembled via include! (crate-root scope).

// Release version from the VERSION file (the workflow's single source of truth;
// Cargo.toml's version is not kept in sync). Clients compare major.minor against
// their own and refuse to connect on mismatch.
const SERVER_VERSION: &str = include_str!("../VERSION");

async fn api_health(State(_): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true, "schemaVersion": 1, "version": SERVER_VERSION.trim(), "backend": "rust"}))
}

// Public, non-secret client configuration handed to every launcher at startup.
// Currently just the launcher's own Discord application id (the same for all
// users) used for Rich Presence. Anonymous like /api/health; the app id is not
// sensitive, and the client degrades to "no presence" when it's empty/absent.
async fn api_client_config(State(st): State<AppState>) -> Json<serde_json::Value> {
    let app_id = setting_value(&st.db, DISCORD_APP_ID_KEY)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    Json(serde_json::json!({ "discordAppId": app_id }))
}

// Best-effort client IP from proxy headers (nginx sets X-Forwarded-For). Falls
// back to X-Real-IP. Returns None for direct/unknown connections.
fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            headers
                .get("x-real-ip")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
}

// Append a security audit row (ROADMAP 0.6). Best-effort: a logging failure must
// never block the auth flow itself.
async fn audit(
    db: &Pool,
    user_id: Option<u64>,
    username: Option<&str>,
    event: &str,
    ip: Option<&str>,
    detail: Option<&str>,
) {
    let Ok(mut c) = db.get_conn().await else {
        return;
    };
    let _ = c
        .exec_drop(
            "INSERT INTO auth_audit (user_id, username, event, ip, detail, created_at) VALUES (:u,:n,:e,:i,:d,:t)",
            params! {"u" => user_id, "n" => username, "e" => event, "i" => ip, "d" => detail, "t" => now()},
        )
        .await;
}

async fn api_login(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<LoginForm>) -> Response {
    let ip = client_ip(&headers);
    let key = form.username.trim().to_lowercase();
    if let Some(secs) = st.login_locked(&key) {
        audit(&st.db, None, Some(&form.username), "login_locked", ip.as_deref(),
              Some(&format!("{secs}s remaining"))).await;
        return (StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": format!("too many attempts; try again in {secs}s")}))).into_response();
    }
    match find_user(&st.db, &form.username).await {
        Ok(Some(user)) if verify_password_any(&form.password, &user.password_hash)
            && verify_user_totp(&user, &form.totp_code) => {
            st.clear_login_failures(&key);
            match issue_user_token(&st.db, user.id, &user.username).await {
                Ok(token) => {
                    audit(&st.db, Some(user.id), Some(&user.username), "login", ip.as_deref(), Some("password")).await;
                    let must_change = user_must_change_password(&st.db, user.id).await;
                    Json(serde_json::json!({"token": token, "username": user.username, "isAdmin": user.is_admin, "mustChangePassword": must_change})).into_response()
                }
                Err(e) => server_error(e),
            }
        }
        _ => {
            st.record_login_failure(&key);
            audit(&st.db, None, Some(&form.username), "login_failed", ip.as_deref(), None).await;
            (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid username, password, or 2FA code"}))).into_response()
        }
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
    // Push approval (0.14): the id of a request already raised on the owner's
    // phone. Empty on the first attempt; the client echoes it back while polling.
    #[serde(default, alias = "approvalId")]
    approval_id: String,
    // Shown on the phone so the owner can tell which machine is asking.
    #[serde(default, alias = "deviceName")]
    device_name: String,
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

// ---------------------------------------------------------------------------
// Sign-in approval store (glue for guard.rs)
// ---------------------------------------------------------------------------

// Kept in memory alongside the challenge nonces rather than in a table: an
// approval is answerable for two minutes, so a restart costs the user one
// retry, which is cheaper than a migration.
static APPROVALS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Approval>>> =
    std::sync::OnceLock::new();

fn approvals() -> &'static std::sync::Mutex<std::collections::HashMap<String, Approval>> {
    APPROVALS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

// What the login handler should do about a missing 2FA code.
enum GuardOutcome {
    // A phone already approved this sign-in; let it through.
    Allow,
    // Raised or still waiting — the client should re-verify with this id.
    Pending(String),
    // The owner said no.
    Denied,
    // No phone to ask. The user must type their code.
    Unavailable,
}

// Record a decision made on a phone. Returns the resulting state so the caller
// can tell the phone whether its tap actually landed.
fn guard_decide(user_id: u64, request_id: &str, action: &str) -> Option<ApprovalState> {
    let now = now() as u64;
    let mut map = approvals().lock().ok()?;
    let approval = map.get(request_id)?;
    // A phone may only answer its own account's requests.
    if approval.user_id != user_id {
        return None;
    }
    let next = decide(approval, action, now).ok()?;
    map.get_mut(request_id)?.state = next;
    Some(next)
}

// Decide what to do when a 2FA-enabled account logged in without a code.
async fn guard_gate(user: &User, approval_id: &str, ip: Option<&str>, device_name: &str) -> GuardOutcome {
    let now = now() as u64;

    // Polling an existing request.
    if !approval_id.is_empty() {
        let mut map = match approvals().lock() {
            Ok(m) => m,
            Err(_) => return GuardOutcome::Unavailable,
        };
        if let Some(existing) = map.get(approval_id) {
            if can_consume(existing, user.id, now) {
                // Spending removes it, so one approval buys exactly one login.
                map.remove(approval_id);
                return GuardOutcome::Allow;
            }
            match effective_state(existing, now) {
                ApprovalState::Pending => return GuardOutcome::Pending(approval_id.to_string()),
                ApprovalState::Denied => return GuardOutcome::Denied,
                // Expired or spent-by-someone-else: fall through and raise a
                // fresh request rather than stranding the user.
                _ => {}
            }
        }
    }

    // Raise a new one — but only to phones. A desktop must not be able to
    // approve a sign-in happening on a desktop, or the second factor is not a
    // second factor.
    let phones: Vec<Device> = social_hub()
        .devices_for(user.id)
        .into_iter()
        .filter(|d| d.kind == "mobile")
        .collect();
    if phones.is_empty() {
        return GuardOutcome::Unavailable;
    }

    let id = random_token(18);
    let approval = Approval {
        id: id.clone(),
        user_id: user.id,
        device_name: device_name.trim().to_string(),
        ip: ip.unwrap_or_default().to_string(),
        created_at: now,
        state: ApprovalState::Pending,
    };
    let prompt = approval_prompt(&approval.device_name, &approval.ip);
    if let Ok(mut map) = approvals().lock() {
        map.retain(|_, a| !is_evictable(a, now));
        map.insert(id.clone(), approval);
    } else {
        return GuardOutcome::Unavailable;
    }

    let msg = serde_json::json!({
        "type": "guard_request",
        "requestId": id,
        "prompt": prompt,
        "deviceName": if device_name.trim().is_empty() { "A PC" } else { device_name.trim() },
        "ip": display_ip(ip.unwrap_or_default()),
        "expiresIn": APPROVAL_TTL_SECONDS,
    })
    .to_string();
    for phone in &phones {
        social_hub().send_to_device(user.id, &phone.id, &msg);
    }
    GuardOutcome::Pending(id)
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

async fn api_auth_verify(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<VerifyForm>) -> Response {
    let username = form.username.trim().to_lowercase();
    let ip = client_ip(&headers);
    if let Some(secs) = st.login_locked(&username) {
        audit(&st.db, None, Some(&form.username), "login_locked", ip.as_deref(),
              Some(&format!("{secs}s remaining"))).await;
        return (StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": format!("too many attempts; try again in {secs}s")}))).into_response();
    }
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
    let bad_2fa = || {
        (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "invalid credentials or 2FA code"}))).into_response()
    };
    if !constant_eq(expected.as_bytes(), form.proof.trim().as_bytes()) {
        st.record_login_failure(&username);
        audit(&st.db, Some(user.id), Some(&user.username), "login_failed", ip.as_deref(), Some("challenge")).await;
        return bad_2fa();
    }
    // Second factor. A typed code still works exactly as before; if the code is
    // wrong or absent we fall back to asking the owner's phone. The push is an
    // ALTERNATIVE to the code, never an extra hurdle — a flat phone must not be
    // able to lock the owner out of their own launcher.
    if user.totp_enabled && !verify_user_totp(&user, &form.totp_code) {
        match guard_gate(&user, form.approval_id.trim(), ip.as_deref(), &form.device_name).await {
            GuardOutcome::Allow => {
                audit(&st.db, Some(user.id), Some(&user.username), "login_approved", ip.as_deref(), Some("guard push")).await;
            }
            GuardOutcome::Pending(id) => {
                // Not a failure: do not count it against the throttle, or a
                // slow tap on the phone would lock the account out.
                return (
                    StatusCode::ACCEPTED,
                    Json(serde_json::json!({
                        "approvalId": id,
                        "status": "pending",
                        "expiresIn": APPROVAL_TTL_SECONDS,
                    })),
                )
                    .into_response();
            }
            GuardOutcome::Denied => {
                st.record_login_failure(&username);
                audit(&st.db, Some(user.id), Some(&user.username), "login_denied", ip.as_deref(), Some("guard push")).await;
                return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "sign-in denied on your phone"}))).into_response();
            }
            GuardOutcome::Unavailable => {
                st.record_login_failure(&username);
                audit(&st.db, Some(user.id), Some(&user.username), "login_failed", ip.as_deref(), Some("2fa")).await;
                return bad_2fa();
            }
        }
    }
    st.clear_login_failures(&username);
    let token = match issue_user_token(&st.db, user.id, &user.username).await {
        Ok(t) => t,
        Err(e) => return server_error(e),
    };
    audit(&st.db, Some(user.id), Some(&user.username), "login", ip.as_deref(), Some("challenge")).await;
    // Encrypt the token so it never travels in cleartext: HMAC-CTR keyed by the
    // password-derived secret, with a fresh random IV per response.
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let ciphertext = hmac_ctr_xor(&key_bytes, &iv, token.as_bytes());
    let must_change = user_must_change_password(&st.db, user.id).await;
    Json(serde_json::json!({
        "iv": hex::encode(iv),
        "token": hex::encode(ciphertext),
        "username": user.username,
        "isAdmin": user.is_admin,
        "mustChangePassword": must_change,
    })).into_response()
}

#[derive(Deserialize)]
struct PasswordChangeForm {
    #[serde(alias = "currentPassword", alias = "current_password")]
    current_password: String,
    #[serde(alias = "newPassword", alias = "new_password")]
    new_password: String,
}

#[derive(Deserialize)]
struct TotpSetupForm {
    password: String,
}

#[derive(Deserialize)]
struct TotpEnableForm {
    code: String,
}

#[derive(Deserialize)]
struct TotpDisableForm {
    #[serde(default)]
    password: String,
    #[serde(default)]
    code: String,
}

// GET /api/account — current launcher account state for the client UI.
async fn api_account(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not authenticated"}))).into_response();
    };
    let must_change = user_must_change_password(&st.db, user.id).await;
    let avatar_version = get_user_avatar_version(&st.db, user.id).await;
    Json(serde_json::json!({
        "username": user.username,
        "email": user.email,
        "isAdmin": user.is_admin,
        "totpEnabled": user.totp_enabled,
        "mustChangePassword": must_change,
        "avatarVersion": avatar_version,
    }))
    .into_response()
}

// Sniff a supported raster image type from magic bytes, falling back to a
// declared image/* content type. Returns None for anything we won't store.
fn detect_image_mime(data: &[u8], content_type: &str) -> Option<String> {
    if data.len() >= 8 && data[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png".into());
    }
    if data.len() >= 3 && data[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg".into());
    }
    if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        return Some("image/webp".into());
    }
    if data.len() >= 6 && (&data[..6] == b"GIF87a" || &data[..6] == b"GIF89a") {
        return Some("image/gif".into());
    }
    let ct = content_type.split(';').next().unwrap_or("").trim();
    if ct.starts_with("image/") && ct.len() <= 64 {
        return Some(ct.to_string());
    }
    None
}

// GET /api/account/avatar — the current user's profile picture bytes.
async fn api_account_avatar(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    match get_user_avatar(&st.db, user.id).await {
        Ok(Some((bytes, mime))) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "no avatar").into_response(),
        Err(e) => server_error(e),
    }
}

// POST /api/account/avatar — upload/replace the profile picture. Raw image bytes
// in the body; type is sniffed from magic bytes (Content-Type is only a hint).
async fn api_account_avatar_upload(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if body.is_empty() || body.len() > 4 * 1024 * 1024 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "image must be 1 byte..4 MB"}))).into_response();
    }
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let Some(mime) = detect_image_mime(&body, ct) else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "unsupported image type"}))).into_response();
    };
    if let Err(e) = set_user_avatar(&st.db, user.id, &body, &mime).await {
        return server_error(e);
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

// DELETE /api/account/avatar — remove the profile picture.
async fn api_account_avatar_delete(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if let Err(e) = clear_user_avatar(&st.db, user.id).await {
        return server_error(e);
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

// POST /api/account/password — change own password. Verifies the current
// password, updates the Argon2 hash + the challenge-response auth_key, and
// clears any admin-forced must_change flag.
async fn api_account_password(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<PasswordChangeForm>) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not authenticated"}))).into_response();
    };
    if !verify_password_any(&form.current_password, &user.password_hash) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "current password is incorrect"}))).into_response();
    }
    if form.new_password.len() < 8 {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "new password must be at least 8 characters"}))).into_response();
    }
    let hash = match hash_password_argon2(&form.new_password) {
        Ok(h) => h,
        Err(e) => return server_error(e),
    };
    let auth_key = derive_auth_key(&user.username, &form.new_password);
    let mut c = match st.db.get_conn().await { Ok(c) => c, Err(e) => return server_error(e) };
    if let Err(e) = c.exec_drop(
        "UPDATE admin_users SET password_hash=:p, auth_key=:k, must_change_password=FALSE WHERE id=:id",
        params! {"p" => hash, "k" => auth_key, "id" => user.id},
    ).await {
        return server_error(e);
    }
    audit(&st.db, Some(user.id), Some(&user.username), "password_change", client_ip(&headers).as_deref(), None).await;
    Json(serde_json::json!({"ok": true})).into_response()
}

// POST /api/account/totp/setup — begin enrolling an authenticator. Requires the
// account password, generates a fresh secret (stored but NOT yet enabled), and
// returns the base32 secret + otpauth URI so the client can render a QR code.
async fn api_account_totp_setup(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<TotpSetupForm>) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not authenticated"}))).into_response();
    };
    if !verify_password_any(&form.password, &user.password_hash) {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "password is incorrect"}))).into_response();
    }
    let mut secret = [0u8; 20];
    rand::thread_rng().fill_bytes(&mut secret);
    let encoded = base32_encode(&secret);
    let mut c = match st.db.get_conn().await { Ok(c) => c, Err(e) => return server_error(e) };
    // Store the pending secret but keep 2FA disabled until the user confirms a
    // code, so a half-finished enrollment never locks them out.
    if let Err(e) = c.exec_drop(
        "UPDATE admin_users SET totp_secret=:s, totp_enabled=FALSE WHERE id=:id",
        params! {"s" => &encoded, "id" => user.id},
    ).await {
        return server_error(e);
    }
    let account = if user.email.is_empty() { user.username.clone() } else { user.email.clone() };
    let uri = format!(
        "otpauth://totp/ArcadeLauncher:{}?secret={}&issuer=ArcadeLauncher&algorithm=SHA1&digits=6&period=30",
        urlencoding::encode(&account),
        encoded
    );
    Json(serde_json::json!({"secret": encoded, "otpauthUri": uri})).into_response()
}

// POST /api/account/totp/enable — confirm enrollment by verifying a code
// against the pending secret, then flip 2FA on.
async fn api_account_totp_enable(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<TotpEnableForm>) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not authenticated"}))).into_response();
    };
    let Some(secret) = user.totp_secret.as_deref() else {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "no pending 2FA enrollment; start setup first"}))).into_response();
    };
    // Verify the code against the pending secret directly (the user row still has
    // totp_enabled=false, so verify_user_totp would short-circuit to true).
    let digits: String = form.code.chars().filter(|c| c.is_ascii_digit()).collect();
    let now_step = now() / 30;
    let ok = digits.len() == 6 && [now_step - 1, now_step, now_step + 1].iter().any(|step| {
        matches!(totp_code(secret, *step as u64), Ok(expected) if expected == digits)
    });
    if !ok {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "incorrect code"}))).into_response();
    }
    let mut c = match st.db.get_conn().await { Ok(c) => c, Err(e) => return server_error(e) };
    if let Err(e) = c.exec_drop(
        "UPDATE admin_users SET totp_enabled=TRUE WHERE id=:id",
        params! {"id" => user.id},
    ).await {
        return server_error(e);
    }
    audit(&st.db, Some(user.id), Some(&user.username), "totp_enabled", client_ip(&headers).as_deref(), None).await;
    Json(serde_json::json!({"ok": true})).into_response()
}

// POST /api/account/totp/disable — turn 2FA off after verifying either the
// account password or a current TOTP code.
async fn api_account_totp_disable(State(st): State<AppState>, headers: HeaderMap, Form(form): Form<TotpDisableForm>) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not authenticated"}))).into_response();
    };
    let by_password = !form.password.is_empty() && verify_password_any(&form.password, &user.password_hash);
    let by_code = user.totp_enabled && verify_user_totp(&user, &form.code);
    if !by_password && !by_code {
        return (StatusCode::FORBIDDEN, Json(serde_json::json!({"error": "provide your password or a current 2FA code"}))).into_response();
    }
    if let Err(e) = disable_user_totp(&st.db, user.id).await {
        return server_error(e);
    }
    audit(&st.db, Some(user.id), Some(&user.username), "totp_disabled", client_ip(&headers).as_deref(), None).await;
    Json(serde_json::json!({"ok": true})).into_response()
}

// GET /api/account/security — recent security events for the signed-in account
// (ROADMAP 0.6). Powers an account "recent activity / where am I logged in" view.
async fn api_account_security(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let rows: Vec<(u64, String, Option<String>, Option<String>, i64)> = match c
        .exec(
            r#"SELECT id, event, ip, detail, created_at FROM auth_audit
               WHERE user_id=:u ORDER BY id DESC LIMIT 100"#,
            params! {"u" => user.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let events: Vec<_> = rows
        .into_iter()
        .map(|(id, event, ip, detail, ts)| {
            serde_json::json!({
                "id": id,
                "event": event,
                "ip": ip,
                "detail": detail,
                "timestamp": ts,
            })
        })
        .collect();
    Json(serde_json::json!({ "events": events })).into_response()
}

async fn user_auth_key(db: &Pool, user_id: u64) -> Result<Option<String>> {
    let mut c = db.get_conn().await?;
    let row: Option<Option<String>> = c
        .exec_first("SELECT auth_key FROM admin_users WHERE id=:id", params! {"id" => user_id})
        .await?;
    Ok(row.flatten())
}

