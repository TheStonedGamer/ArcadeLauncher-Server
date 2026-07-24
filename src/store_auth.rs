// store_auth.rs - browser cookie-session auth for the public storefront.
// Assembled via include! at crate-root scope, so it shares AppState, User,
// find_user/find_user_by_id, verify_password_any, verify_user_totp, audit,
// random_token, sha256_hex, now, client_ip with the rest of the crate.
//
// This is DELIBERATELY separate from the native launcher's Bearer-token flow
// (auth.rs / launcher_tokens) and from the admin UI's AL_ADMIN_SESSION cookie.
// The storefront runs in a browser at arcade.orlandoaio.net, same-origin with
// /api (edge nginx proxies it), so a plain HttpOnly session cookie is the right
// tool. We store only the SHA-256 of the opaque token; the cookie carries the
// token itself.

const STORE_SESSION_COOKIE: &str = "AL_STORE_SESSION";
const STORE_SESSION_TTL_SECONDS: i64 = 60 * 60 * 24 * 30; // 30 days

#[derive(Deserialize)]
struct StoreLoginForm {
    username: String,
    password: String,
    #[serde(default, alias = "totpCode", alias = "totp")]
    totp_code: String,
}

// Build the Set-Cookie header value. `secure` is added behind TLS (the edge
// terminates HTTPS and sets X-Forwarded-Proto=https). SameSite=Lax so top-level
// navigations to the store carry the session while still blocking CSRF POSTs.
fn store_cookie(token: &str, secure: bool, max_age: i64) -> String {
    let secure_attr = if secure { "; Secure" } else { "" };
    format!(
        "{STORE_SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age}{secure_attr}"
    )
}

fn request_is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|p| p.eq_ignore_ascii_case("https"))
        .unwrap_or(false)
}

// Resolve the storefront account behind the session cookie, or None. Also lazily
// evicts expired rows. This is the guard the library endpoints call.
async fn web_user(st: &AppState, headers: &HeaderMap) -> Option<User> {
    let token = cookie_value(headers, STORE_SESSION_COOKIE)?;
    let hash = sha256_hex(token.as_bytes());
    let mut c = st.db.get_conn().await.ok()?;
    let ts = now();
    let _ = c
        .exec_drop(
            "DELETE FROM user_sessions WHERE expires_at <= :t",
            params! {"t" => ts},
        )
        .await;
    let uid: Option<u64> = c
        .exec_first(
            "SELECT user_id FROM user_sessions WHERE token_hash=:h AND expires_at > :t LIMIT 1",
            params! {"h" => &hash, "t" => ts},
        )
        .await
        .ok()
        .flatten();
    find_user_by_id(&st.db, uid?).await.ok().flatten()
}

// POST /api/store/auth/login — password (+ optional TOTP) → session cookie.
async fn store_login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<StoreLoginForm>,
) -> Response {
    let username = form.username.trim().to_lowercase();
    let ip = client_ip(&headers);
    if let Some(secs) = st.login_locked(&username) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": format!("too many attempts; try again in {secs}s")})),
        )
            .into_response();
    }
    let bad = || {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "invalid credentials"})),
        )
            .into_response()
    };
    let user = match find_user(&st.db, &form.username).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            st.record_login_failure(&username);
            audit(&st.db, None, Some(&form.username), "login_failed", ip.as_deref(), Some("store")).await;
            return bad();
        }
        Err(e) => return server_error(e),
    };
    if !verify_password_any(&form.password, &user.password_hash) {
        st.record_login_failure(&username);
        audit(&st.db, Some(user.id), Some(&user.username), "login_failed", ip.as_deref(), Some("store")).await;
        return bad();
    }
    // Second factor. Unlike the launcher there is no phone-push fallback here; a
    // TOTP-protected account must type its code to sign in on the web.
    if user.totp_enabled && !verify_user_totp(&user, form.totp_code.trim()) {
        st.record_login_failure(&username);
        audit(&st.db, Some(user.id), Some(&user.username), "login_failed", ip.as_deref(), Some("store 2fa")).await;
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "two-factor code required", "totpRequired": true})),
        )
            .into_response();
    }

    st.clear_login_failures(&username);
    let token = random_token(36);
    let hash = sha256_hex(token.as_bytes());
    let ts = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            "INSERT INTO user_sessions (token_hash,user_id,expires_at,created_at) VALUES (:h,:u,:e,:t)",
            params! {"h" => &hash, "u" => user.id, "e" => ts + STORE_SESSION_TTL_SECONDS, "t" => ts},
        )
        .await
    {
        return server_error(e);
    }
    audit(&st.db, Some(user.id), Some(&user.username), "login", ip.as_deref(), Some("store")).await;

    let cookie = store_cookie(&token, request_is_secure(&headers), STORE_SESSION_TTL_SECONDS);
    let body = Json(serde_json::json!({
        "username": user.username,
        "email": user.email,
        "isAdmin": user.is_admin,
    }));
    ([(header::SET_COOKIE, cookie)], body).into_response()
}

// POST /api/store/auth/logout — drop the session row and expire the cookie.
async fn store_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, STORE_SESSION_COOKIE) {
        let hash = sha256_hex(token.as_bytes());
        if let Ok(mut c) = st.db.get_conn().await {
            let _ = c
                .exec_drop(
                    "DELETE FROM user_sessions WHERE token_hash=:h",
                    params! {"h" => hash},
                )
                .await;
        }
    }
    let expire = store_cookie("", request_is_secure(&headers), 0);
    ([(header::SET_COOKIE, expire)], Json(serde_json::json!({"ok": true}))).into_response()
}

// GET /api/store/auth/me — who is signed in on this browser, or 401.
async fn store_me(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match web_user(&st, &headers).await {
        Some(user) => Json(serde_json::json!({
            "username": user.username,
            "email": user.email,
            "isAdmin": user.is_admin,
        }))
        .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "not signed in"})),
        )
            .into_response(),
    }
}
