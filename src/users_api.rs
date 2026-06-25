// users_api.rs - JSON user-management & RBAC API (crate-root scope).
//
// Extends the existing username/password + launcher-token auth (no second JWT
// stack). Roles are derived from the admin_users.is_admin column:
//   Admin -> full read/write to settings, paths, webhooks, and user management
//   User  -> read-only catalog + own account
// Admin-only routes are guarded by require_admin(); PUT /api/users/:id also
// allows a user to edit their own email/password (but never role/enabled).

fn role_str(is_admin: bool) -> &'static str {
    if is_admin {
        "Admin"
    } else {
        "User"
    }
}

// Shared by the admin web panel's per-user "Save Changes" action. Applies an
// email/password/role/enabled update, but refuses any change that would strip
// the last remaining admin (demotion or disable) so the panel can't lock out.
async fn apply_user_update(
    st: &AppState,
    target: &User,
    email: Option<&str>,
    password: Option<&str>,
    new_admin: bool,
    new_enabled: bool,
    removes_admin: bool,
) -> String {
    if removes_admin {
        match enabled_admin_count(&st.db).await {
            Ok(n) if n <= 1 => {
                return "Cannot demote or disable the last remaining admin account.".to_string()
            }
            Err(e) => return e.to_string(),
            _ => {}
        }
    }
    match update_user(&st.db, target, email, password, Some(new_admin), Some(new_enabled)).await {
        Ok(_) => {
            let mut notes = Vec::new();
            if password.is_some() {
                notes.push("password reset");
            }
            if !new_enabled {
                notes.push("account disabled");
            }
            if new_admin != target.is_admin {
                notes.push(if new_admin { "promoted to admin" } else { "demoted to client" });
            }
            let extra = if notes.is_empty() { String::new() } else { format!(" ({})", notes.join(", ")) };
            format!("Updated {}{}.", target.username, extra)
        }
        Err(e) => e.to_string(),
    }
}

fn user_json(u: &User) -> serde_json::Value {
    serde_json::json!({
        "id": u.id,
        "username": u.username,
        "email": u.email,
        "role": role_str(u.is_admin),
        "isAdmin": u.is_admin,
        "enabled": u.enabled,
        "totpEnabled": u.totp_enabled,
    })
}

// Resolve the caller's account from the Bearer token and require Admin. Returns
// the caller on success, or a ready-to-send error Response otherwise.
async fn require_admin(st: &AppState, headers: &HeaderMap) -> std::result::Result<User, Response> {
    match launcher_user(st, headers).await {
        Some(u) if u.is_admin => Ok(u),
        Some(_) => Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin access required"})),
        )
            .into_response()),
        None => Err(unauthorized()),
    }
}

#[derive(Deserialize)]
struct ApiLoginBody {
    username: String,
    password: String,
    #[serde(default, alias = "totpCode")]
    totp_code: String,
}

// POST /api/auth/login — JSON credential login returning a bearer token + the
// authenticated user (including role). Shares the brute-force throttle with the
// admin web login.
async fn api_auth_login(State(st): State<AppState>, Json(body): Json<ApiLoginBody>) -> Response {
    let key = body.username.trim().to_lowercase();
    if let Some(secs) = st.login_locked(&key) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": format!("too many attempts; retry in {}s", secs.max(1))})),
        )
            .into_response();
    }
    match find_user(&st.db, &body.username).await {
        Ok(Some(user))
            if verify_password_any(&body.password, &user.password_hash)
                && verify_user_totp(&user, &body.totp_code) =>
        {
            st.clear_login_failures(&key);
            match issue_user_token(&st.db, user.id, &user.username).await {
                Ok(token) => {
                    let must_change = user_must_change_password(&st.db, user.id).await;
                    Json(serde_json::json!({
                        "token": token,
                        "mustChangePassword": must_change,
                        "user": user_json(&user),
                    }))
                    .into_response()
                }
                Err(e) => server_error(e),
            }
        }
        _ => {
            st.record_login_failure(&key);
            (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "invalid username, password, or 2FA code"})),
            )
                .into_response()
        }
    }
}

// POST /api/auth/logout — revoke the caller's launcher token. A subsequent login
// re-mints a token on the same row, so this only ends the current session(s).
async fn api_auth_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    if let Ok(mut c) = st.db.get_conn().await {
        let _ = c
            .exec_drop(
                "UPDATE launcher_tokens SET enabled=FALSE WHERE user_id=:id",
                params! {"id" => user.id},
            )
            .await;
    }
    audit(&st.db, Some(user.id), Some(&user.username), "logout", client_ip(&headers).as_deref(), None).await;
    Json(serde_json::json!({"ok": true})).into_response()
}

// GET /api/users — list all accounts (Admin only).
async fn api_users_list(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_admin(&st, &headers).await {
        return r;
    }
    match list_users(&st.db).await {
        Ok(users) => Json(serde_json::json!({
            "users": users.iter().map(user_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct CreateUserBody {
    username: String,
    email: String,
    password: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default, alias = "isAdmin")]
    is_admin: Option<bool>,
}

// POST /api/users — create an account (Admin only).
async fn api_users_create(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateUserBody>,
) -> Response {
    if let Err(r) = require_admin(&st, &headers).await {
        return r;
    }
    let is_admin = body.is_admin.unwrap_or(false) || body.role.as_deref() == Some("Admin");
    if body.username.trim().is_empty() || body.email.trim().is_empty() || body.password.len() < 6 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "username, email, and a 6+ character password are required"})),
        )
            .into_response();
    }
    match create_user(&st.db, body.username.trim(), body.email.trim(), &body.password, is_admin).await {
        Ok(_) => (StatusCode::CREATED, Json(serde_json::json!({"ok": true}))).into_response(),
        Err(e) => server_error(e),
    }
}

#[derive(Deserialize)]
struct UpdateUserBody {
    email: Option<String>,
    password: Option<String>,
    role: Option<String>,
    #[serde(alias = "isAdmin")]
    is_admin: Option<bool>,
    enabled: Option<bool>,
}

// PUT /api/users/:id — modify a user. Admins may change anything; a non-admin
// may edit only their own email/password (never role or enabled state).
async fn api_users_update(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<u64>,
    Json(body): Json<UpdateUserBody>,
) -> Response {
    let Some(caller) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let is_self = caller.id == id;
    if !caller.is_admin && !is_self {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin or self only"})),
        )
            .into_response();
    }
    let wants_privilege = body.is_admin.is_some() || body.role.is_some() || body.enabled.is_some();
    if wants_privilege && !caller.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "only admins can change role or enabled state"})),
        )
            .into_response();
    }
    if let Some(pw) = &body.password {
        if pw.len() < 6 {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "password must be at least 6 characters"})),
            )
                .into_response();
        }
    }
    let target = match find_user_by_id(&st.db, id).await {
        Ok(Some(u)) => u,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "user not found"})),
            )
                .into_response()
        }
        Err(e) => return server_error(e),
    };
    let is_admin = body.is_admin.or_else(|| body.role.as_ref().map(|r| r == "Admin"));
    match update_user(
        &st.db,
        &target,
        body.email.as_deref(),
        body.password.as_deref(),
        is_admin,
        body.enabled,
    )
    .await
    {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

// DELETE /api/users/:id — delete an account (Admin only). Refuses to remove the
// last remaining admin so the panel can't be locked out.
async fn api_users_delete(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    if let Err(r) = require_admin(&st, &headers).await {
        return r;
    }
    match delete_user_account(&st.db, id).await {
        Ok(_) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e.to_string()}))).into_response(),
    }
}
