// handlers.rs - split out of main.rs and re-assembled via include! (crate-root scope).

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

// Serve the optional Dolphin custom-texture pack for a GameCube/Wii game as a
// single resumable ranged GET. The pack lives next to the ROM as
// `<rom-stem>.textures.zip`; its hash/size are advertised in the manifest's
// `texture_pack` field. The client extracts it into Dolphin's Load/Textures.
async fn download_texture(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    let game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "game not found").into_response(),
        Err(e) => return server_error(e),
    };
    let path = match texture_pack_path(&st.cfg, &game).await {
        Some(p) => p,
        None => {
            warn!("download_texture: no texture pack for {}", game.id);
            return (StatusCode::NOT_FOUND, "no texture pack for this game; ask the admin to rescan the catalog").into_response();
        }
    };
    match stream_file(path, headers.get(header::RANGE)).await {
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
        let throttle_key = username.trim().to_lowercase();
        if let Some(secs) = st.login_locked(&throttle_key) {
            return Html(login_html(&format!(
                "Too many failed attempts. Try again in {} seconds.",
                secs.max(1)
            )))
            .into_response();
        }
        match find_user(&st.db, &username).await {
            Ok(Some(user)) if user.is_admin && verify_password_any(&password, &user.password_hash)
                && verify_user_totp(&user, form.totp_code.as_deref().unwrap_or("")) => {
                st.clear_login_failures(&throttle_key);
                match create_session(&st.db, user.id).await {
                    Ok(token) => {
                        let mut r = Redirect::to("/admin").into_response();
                        if let Ok(cookie) = HeaderValue::from_str(&session_cookie_value(&token, st.cfg.admin_secure_cookie)) {
                            r.headers_mut().insert(header::SET_COOKIE, cookie);
                        }
                        r
                    }
                    Err(e) => server_error(e),
                }
            }
            _ => {
                st.record_login_failure(&throttle_key);
                Html(login_html("Invalid username, password, or 2FA code.")).into_response()
            }
        }
    } else {
        let admin = match current_admin(&st.db, &headers).await {
            Ok(Some(a)) => a,
            _ => return Html(login_html("Please sign in first.")).into_response(),
        };
        let matcher_game_id = form.game_id.clone().unwrap_or_default();
        let matcher_query = form.search_query.clone().unwrap_or_default();
        let return_to = form.return_to.clone().unwrap_or_default();
        let msg = match form.action.as_str() {
            "approve_pending" => match form.pending_id {
                Some(id) => approve_pending_registration(&st.db, id).await.unwrap_or_else(|e| e.to_string()),
                None => "missing pending id".to_string(),
            },
            "deny_pending" => match form.pending_id {
                Some(id) => deny_pending_registration(&st.db, id).await.unwrap_or_else(|e| e.to_string()),
                None => "missing pending id".to_string(),
            },
            "set_request_status" => match form.request_id {
                Some(id) => set_game_request_status(&st.db, id, form.request_status.as_deref().unwrap_or("")).await.unwrap_or_else(|e| e.to_string()),
                None => "missing request id".to_string(),
            },
            "delete_request" => match form.request_id {
                Some(id) => delete_game_request(&st.db, id).await.unwrap_or_else(|e| e.to_string()),
                None => "missing request id".to_string(),
            },
            "add_user" => {
                let username = form.username.unwrap_or_default();
                let email = form.email.unwrap_or_default();
                let password = form.password.unwrap_or_default();
                if username.is_empty() || email.is_empty() || password.len() < 8 {
                    "username, email, and a 8+ character password are required".to_string()
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
            "force_password_change" => match form.user_id {
                Some(id) => set_must_change_password(&st.db, id, true).await
                    .map(|_| "User must set a new password on next sign-in.".to_string())
                    .unwrap_or_else(|e| e.to_string()),
                None => "missing user id".to_string(),
            },
            "update_user" => match form.user_id {
                Some(id) => match find_user_by_id(&st.db, id).await {
                    Ok(Some(target)) => {
                        let new_admin = form.is_admin.as_deref() == Some("1");
                        let new_enabled = form.enabled.as_deref() != Some("0");
                        let removes_admin = target.is_admin && target.enabled && (!new_admin || !new_enabled);
                        let pw = form.password.as_deref().map(str::trim).filter(|p| !p.is_empty());
                        if let Some(p) = pw {
                            if p.len() < 8 {
                                "Password must be at least 8 characters.".to_string()
                            } else {
                                apply_user_update(&st, &target, form.email.as_deref(), Some(p), new_admin, new_enabled, removes_admin).await
                            }
                        } else {
                            apply_user_update(&st, &target, form.email.as_deref(), None, new_admin, new_enabled, removes_admin).await
                        }
                    }
                    Ok(None) => "User not found.".to_string(),
                    Err(e) => e.to_string(),
                },
                None => "missing user id".to_string(),
            },
            "delete_account" => match form.user_id {
                Some(id) => delete_user_account(&st.db, id).await
                    .map(|_| "Deleted user account and all of its tokens/sessions.".to_string())
                    .unwrap_or_else(|e| e.to_string()),
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
            "test_webhook" => match test_discord_webhook(&st.db).await {
                Ok(msg) => msg,
                Err(e) => e.to_string(),
            },
            "add_changelog" => {
                let game_id = form.game_id.clone().unwrap_or_default();
                let body = form.changelog_body.clone().unwrap_or_default();
                if game_id.is_empty() || body.trim().is_empty() {
                    "A game and a non-empty changelog body are required.".to_string()
                } else {
                    match add_changelog(
                        &st.db,
                        &game_id,
                        form.changelog_version.as_deref().unwrap_or(""),
                        form.changelog_title.as_deref().unwrap_or(""),
                        &body,
                    ).await {
                        Ok(_) => "Added changelog entry.".to_string(),
                        Err(e) => e.to_string(),
                    }
                }
            },
            "delete_changelog" => match form.changelog_id {
                Some(id) => delete_changelog(&st.db, id).await
                    .map(|_| "Deleted changelog entry.".to_string())
                    .unwrap_or_else(|e| e.to_string()),
                None => "missing changelog id".to_string(),
            },
            // ── Social test harness (Social Test admin page) ──
            "bot_spawn" => match form.target_id {
                Some(target) => st_spawn_bot(&st, target, form.bot_name.as_deref().unwrap_or("")).await,
                None => "missing target".to_string(),
            },
            "bot_set_status" => match form.bot_id {
                Some(bot) => st_set_status(
                    &st,
                    bot,
                    form.presence_state.as_deref().unwrap_or("online"),
                    form.status_text.as_deref().unwrap_or(""),
                    form.game_id.as_deref().unwrap_or(""),
                    form.game_title.as_deref().unwrap_or(""),
                ).await,
                None => "missing bot".to_string(),
            },
            "bot_post_activity" => match form.bot_id {
                Some(bot) => st_post_activity(
                    &st,
                    bot,
                    form.activity_kind.as_deref().unwrap_or("played"),
                    form.game_id.as_deref().unwrap_or(""),
                    form.activity_value.unwrap_or(0),
                ).await,
                None => "missing bot".to_string(),
            },
            "bot_send_dm" => match (form.bot_id, form.target_id) {
                (Some(bot), Some(target)) => st_send_dm(&st, bot, target, form.message_body.as_deref().unwrap_or("")).await,
                _ => "missing bot or target".to_string(),
            },
            "bot_send_request" => match (form.bot_id, form.target_id) {
                (Some(bot), Some(target)) => st_send_request(&st, bot, target).await,
                _ => "missing bot or target".to_string(),
            },
            "bot_cleanup" => st_cleanup_bots(&st).await,
            _ => "No action taken.".to_string(),
        };
        match return_to.as_str() {
            "accounts" => Html(accounts_page_html(&st, Some(admin), &msg).await.unwrap_or_else(|e| format!("error: {e}"))).into_response(),
            "requests" => Html(requests_page_html(&st, Some(admin), &msg).await.unwrap_or_else(|e| format!("error: {e}"))).into_response(),
            "social-test" => Html(social_test_page_html(&st, Some(admin), &msg).await.unwrap_or_else(|e| format!("error: {e}"))).into_response(),
            _ => Html(admin_html(&st, Some(admin), &msg, &matcher_game_id, &matcher_query).await.unwrap_or_else(|e| format!("error: {e}"))).into_response(),
        }
    }
}


// ── Cloud saves (per-user, per-game) ─────────────────────────────────────────

const SAVE_FILE_MAX_BYTES: usize = 50 * 1024 * 1024;

/// Forward-slash relative paths only — no traversal, no absolute paths.
fn valid_save_path(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 400
        && !p.starts_with('/')
        && !p.contains('\\')
        && !p.contains('\0')
        && !p.split('/').any(|seg| seg.is_empty() || seg == "." || seg == "..")
}

#[derive(Deserialize)]
struct SaveFileQuery {
    path: String,
    #[serde(default)]
    mtime: i64,
    // ROADMAP 2.7: optional optimistic-concurrency guard. When present, the PUT
    // succeeds only if the currently-stored mtime equals this value (i.e. the
    // client is overwriting the version it last saw). 0/absent disables the check.
    #[serde(rename = "baseMtime", default)]
    base_mtime: Option<i64>,
}

#[derive(Deserialize)]
struct SaveVersionQuery {
    path: String,
    #[serde(rename = "versionId", default)]
    version_id: u64,
}

// GET /api/saves/:game_id — list this user's stored save files for a game.
async fn api_saves_list(State(st): State<AppState>, AxumPath(game_id): AxumPath<String>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    match list_game_saves(&st.db, user.id, &game_id).await {
        Ok(files) => {
            let list: Vec<serde_json::Value> = files
                .into_iter()
                .map(|(path, mtime, size)| serde_json::json!({"path": path, "mtime": mtime, "size": size}))
                .collect();
            Json(serde_json::json!({"files": list})).into_response()
        }
        Err(e) => server_error(e),
    }
}

// GET /api/saves/:game_id/file?path=rel — raw bytes of one stored save file.
async fn api_saves_get(
    State(st): State<AppState>,
    AxumPath(game_id): AxumPath<String>,
    Query(q): Query<SaveFileQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if !valid_save_path(&q.path) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid path"}))).into_response();
    }
    match get_game_save(&st.db, user.id, &game_id, &q.path).await {
        Ok(Some(bytes)) => (
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such save file"}))).into_response(),
        Err(e) => server_error(e),
    }
}

// PUT /api/saves/:game_id/file?path=rel&mtime=N — upsert one save file.
async fn api_saves_put(
    State(st): State<AppState>,
    AxumPath(game_id): AxumPath<String>,
    Query(q): Query<SaveFileQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if !valid_save_path(&q.path) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid path"}))).into_response();
    }
    if body.is_empty() || body.len() > SAVE_FILE_MAX_BYTES {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "save file must be 1 byte..50 MB"}))).into_response();
    }
    // Optimistic-concurrency check (ROADMAP 2.7 conflict resolution).
    if let Some(base) = q.base_mtime {
        match get_game_save_mtime(&st.db, user.id, &game_id, &q.path).await {
            Ok(current) => {
                let cur = current.unwrap_or(0);
                if cur != base {
                    return (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "save conflict",
                            "currentMtime": cur,
                            "baseMtime": base,
                        })),
                    )
                        .into_response();
                }
            }
            Err(e) => return server_error(e),
        }
    }
    match put_game_save(&st.db, user.id, &game_id, &q.path, q.mtime, &body).await {
        Ok(()) => Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => server_error(e),
    }
}

// GET /api/metrics — Prometheus text-format gauges (ROADMAP 3.4). Auth-free so a
// scraper can poll it; exposes only aggregate counts, no per-user data.
async fn api_metrics(State(st): State<AppState>) -> Response {
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    async fn count(c: &mut mysql_async::Conn, sql: &str) -> i64 {
        c.query_first::<i64, _>(sql).await.ok().flatten().unwrap_or(0)
    }
    let users = count(&mut c, "SELECT COUNT(*) FROM admin_users").await;
    let games = count(&mut c, "SELECT COUNT(*) FROM games").await;
    let messages = count(&mut c, "SELECT COUNT(*) FROM social_messages").await;
    let friendships =
        count(&mut c, "SELECT COUNT(*) FROM social_friendships WHERE status='accepted'").await;
    let notifications = count(&mut c, "SELECT COUNT(*) FROM social_notifications").await;
    let body = format!(
        "# HELP arcade_users_total Registered accounts\n\
         # TYPE arcade_users_total gauge\narcade_users_total {users}\n\
         # HELP arcade_games_total Catalog games\n\
         # TYPE arcade_games_total gauge\narcade_games_total {games}\n\
         # HELP arcade_messages_total Direct messages stored\n\
         # TYPE arcade_messages_total gauge\narcade_messages_total {messages}\n\
         # HELP arcade_friendships_total Accepted friendships\n\
         # TYPE arcade_friendships_total gauge\narcade_friendships_total {friendships}\n\
         # HELP arcade_notifications_total Notifications stored\n\
         # TYPE arcade_notifications_total gauge\narcade_notifications_total {notifications}\n"
    );
    (
        [(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; version=0.0.4"))],
        body,
    )
        .into_response()
}

// GET /api/feature-flags — the server's feature-flag map (ROADMAP 3.5), stored as a
// JSON object under the `feature_flags` server setting. Public so clients can gate UI
// before sign-in; empty object when unset/malformed.
async fn api_feature_flags(State(st): State<AppState>) -> Response {
    let flags = match setting_value(&st.db, "feature_flags").await {
        Ok(Some(s)) => serde_json::from_str::<serde_json::Value>(&s)
            .unwrap_or(serde_json::Value::Object(Default::default())),
        _ => serde_json::Value::Object(Default::default()),
    };
    Json(serde_json::json!({ "flags": flags })).into_response()
}

#[derive(Deserialize)]
struct GameSearchQuery {
    q: String,
}

// GET /api/games/search?q= — full-text catalog search (ROADMAP 3.5). Uses MATCH…
// AGAINST in boolean mode, falling back to a LIKE scan when the FULLTEXT index is
// unavailable. Returns up to 50 lightweight hits.
async fn api_games_search(
    State(st): State<AppState>,
    Query(q): Query<GameSearchQuery>,
    headers: HeaderMap,
) -> Response {
    if launcher_user(&st, &headers).await.is_none() {
        return unauthorized();
    }
    let term = q.q.trim();
    if term.is_empty() {
        return Json(serde_json::json!({ "results": [] })).into_response();
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Boolean-mode query: each token becomes a required prefix match.
    let bool_query = term
        .split_whitespace()
        .take(8)
        .filter(|t| t.chars().all(|ch| ch.is_ascii_alphanumeric()))
        .map(|t| format!("+{t}*"))
        .collect::<Vec<_>>()
        .join(" ");
    let mut rows: Vec<(String, String, String)> = Vec::new();
    if !bool_query.is_empty() {
        if let Ok(r) = c
            .exec(
                r#"SELECT id, title, platform FROM games
                   WHERE MATCH(title, summary, genres) AGAINST (:q IN BOOLEAN MODE) LIMIT 50"#,
                params! {"q" => &bool_query},
            )
            .await
        {
            rows = r;
        }
    }
    if rows.is_empty() {
        let like = format!("%{}%", term.replace('%', "").replace('_', ""));
        rows = c
            .exec(
                "SELECT id, title, platform FROM games WHERE title LIKE :l ORDER BY title LIMIT 50",
                params! {"l" => like},
            )
            .await
            .unwrap_or_default();
    }
    let results: Vec<_> = rows
        .into_iter()
        .map(|(id, title, platform)| serde_json::json!({"gameId": id, "title": title, "platform": platform}))
        .collect();
    Json(serde_json::json!({ "results": results })).into_response()
}

// ROADMAP 2.8: a config namespace name must be a short, safe slug.
fn valid_config_namespace(ns: &str) -> bool {
    !ns.is_empty()
        && ns.len() <= 64
        && ns.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.')
}

const CONFIG_BLOB_MAX_BYTES: usize = 256 * 1024;

// GET /api/config — all of the caller's config namespaces as a { namespace: data } map.
async fn api_config_all(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    let mut c = match st.db.get_conn().await { Ok(c) => c, Err(e) => return server_error(e) };
    let rows: Vec<(String, String)> = match c
        .exec("SELECT namespace, data FROM user_config WHERE user_id=:u", params! {"u" => user.id})
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let mut map = serde_json::Map::new();
    for (ns, data) in rows {
        let val = serde_json::from_str::<serde_json::Value>(&data)
            .unwrap_or(serde_json::Value::String(data));
        map.insert(ns, val);
    }
    Json(serde_json::json!({ "config": map })).into_response()
}

// GET /api/config/:ns — one namespace's blob (404 if unset).
async fn api_config_get(
    State(st): State<AppState>,
    AxumPath(ns): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if !valid_config_namespace(&ns) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid namespace"}))).into_response();
    }
    let mut c = match st.db.get_conn().await { Ok(c) => c, Err(e) => return server_error(e) };
    let row: Option<(String, i64)> = match c
        .exec_first(
            "SELECT data, updated_at FROM user_config WHERE user_id=:u AND namespace=:n",
            params! {"u" => user.id, "n" => &ns},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    match row {
        Some((data, updated)) => {
            let val = serde_json::from_str::<serde_json::Value>(&data)
                .unwrap_or(serde_json::Value::String(data));
            Json(serde_json::json!({ "namespace": ns, "data": val, "updatedAt": updated })).into_response()
        }
        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "not set"}))).into_response(),
    }
}

// POST /api/config/:ns — upsert (body = the JSON blob) or, with a JSON `null` body,
// delete the caller's config for that namespace. Blob capped at 256 KiB.
async fn api_config_put(
    State(st): State<AppState>,
    AxumPath(ns): AxumPath<String>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if !valid_config_namespace(&ns) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid namespace"}))).into_response();
    }
    let mut c = match st.db.get_conn().await { Ok(c) => c, Err(e) => return server_error(e) };
    let now = now();
    if body.is_null() {
        let _ = c
            .exec_drop(
                "DELETE FROM user_config WHERE user_id=:u AND namespace=:n",
                params! {"u" => user.id, "n" => &ns},
            )
            .await;
        return Json(serde_json::json!({ "ok": true })).into_response();
    }
    let blob = body.to_string();
    if blob.len() > CONFIG_BLOB_MAX_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(serde_json::json!({"error": "config too large"}))).into_response();
    }
    if let Err(e) = c
        .exec_drop(
            r#"INSERT INTO user_config (user_id, namespace, data, updated_at)
               VALUES (:u, :n, :d, :t)
               ON DUPLICATE KEY UPDATE data=VALUES(data), updated_at=VALUES(updated_at)"#,
            params! {"u" => user.id, "n" => &ns, "d" => &blob, "t" => now},
        )
        .await
    {
        return server_error(e);
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

// GET /api/saves/:game_id/versions?path=rel — list archived versions of a save file.
async fn api_saves_versions(
    State(st): State<AppState>,
    AxumPath(game_id): AxumPath<String>,
    Query(q): Query<SaveFileQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    if !valid_save_path(&q.path) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": "invalid path"}))).into_response();
    }
    match list_save_versions(&st.db, user.id, &game_id, &q.path).await {
        Ok(rows) => {
            let list: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|(vid, mtime, size, created)| serde_json::json!({
                    "versionId": vid, "mtime": mtime, "size": size, "createdAt": created
                }))
                .collect();
            Json(serde_json::json!({"versions": list})).into_response()
        }
        Err(e) => server_error(e),
    }
}

// GET /api/saves/:game_id/version?versionId=N — raw bytes of one archived version.
async fn api_saves_version_get(
    State(st): State<AppState>,
    AxumPath(game_id): AxumPath<String>,
    Query(q): Query<SaveVersionQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    match get_save_version(&st.db, user.id, &game_id, q.version_id).await {
        Ok(Some((_, _, bytes))) => (
            [(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"))],
            bytes,
        )
            .into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such version"}))).into_response(),
        Err(e) => server_error(e),
    }
}

// POST /api/saves/:game_id/restore?versionId=N — make an archived version the
// current save (the live row's prior bytes are themselves archived first).
async fn api_saves_restore(
    State(st): State<AppState>,
    AxumPath(game_id): AxumPath<String>,
    Query(q): Query<SaveVersionQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else { return unauthorized(); };
    let (rel_path, mtime, bytes) = match get_save_version(&st.db, user.id, &game_id, q.version_id).await {
        Ok(Some(v)) => v,
        Ok(None) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "no such version"}))).into_response(),
        Err(e) => return server_error(e),
    };
    match put_game_save(&st.db, user.id, &game_id, &rel_path, mtime, &bytes).await {
        Ok(()) => Json(serde_json::json!({"ok": true, "path": rel_path, "mtime": mtime})).into_response(),
        Err(e) => server_error(e),
    }
}
