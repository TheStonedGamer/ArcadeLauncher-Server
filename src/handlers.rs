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
            "force_password_change" => match form.user_id {
                Some(id) => set_must_change_password(&st.db, id, true).await
                    .map(|_| "User must set a new password on next sign-in.".to_string())
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
            _ => "No action taken.".to_string(),
        };
        Html(admin_html(&st, Some(admin), &msg, &matcher_game_id, &matcher_query).await.unwrap_or_else(|e| format!("error: {e}"))).into_response()
    }
}

