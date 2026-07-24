// store_lib.rs - per-user library ("ownership") endpoints for the storefront.
// Assembled via include! at crate-root scope. Ownership is free but explicit:
// a user clicks "Add to Library" on the web store, and only then does the
// launcher install/show the game (see manifest gating). All handlers here are
// gated by the storefront session cookie via web_user() (store_auth.rs).

// True iff `user_id` has `game_id` in their library. Shared with manifest gating.
async fn user_owns(db: &Pool, user_id: u64, game_id: &str) -> bool {
    let Ok(mut c) = db.get_conn().await else { return false; };
    let row: Option<u8> = c
        .exec_first(
            "SELECT 1 FROM user_library WHERE user_id=:u AND game_id=:g LIMIT 1",
            params! {"u" => user_id, "g" => game_id},
        )
        .await
        .ok()
        .flatten();
    row.is_some()
}

// The set of game ids a user owns — used to annotate storefront cards with
// `owned` and by the launcher's library filter.
async fn owned_game_ids(db: &Pool, user_id: u64) -> Result<Vec<String>> {
    let mut c = db.get_conn().await?;
    let ids: Vec<String> = c
        .exec(
            "SELECT game_id FROM user_library WHERE user_id=:u ORDER BY added_at DESC",
            params! {"u" => user_id},
        )
        .await?;
    Ok(ids)
}

// POST /api/store/library/:id — add a game to the caller's library (idempotent).
async fn store_library_add(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not signed in"}))).into_response();
    };
    // Only real catalog games can be added.
    match find_game(&st.db, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "game not found"}))).into_response(),
        Err(e) => return server_error(e),
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            "INSERT INTO user_library (user_id,game_id,added_at) VALUES (:u,:g,:t) \
             ON DUPLICATE KEY UPDATE added_at = added_at",
            params! {"u" => user.id, "g" => &id, "t" => now()},
        )
        .await
    {
        return server_error(e);
    }
    Json(serde_json::json!({"ok": true, "id": id, "owned": true})).into_response()
}

// DELETE /api/store/library/:id — remove a game from the caller's library.
async fn store_library_remove(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not signed in"}))).into_response();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            "DELETE FROM user_library WHERE user_id=:u AND game_id=:g",
            params! {"u" => user.id, "g" => &id},
        )
        .await
    {
        return server_error(e);
    }
    Json(serde_json::json!({"ok": true, "id": id, "owned": false})).into_response()
}

// GET /api/store/library — the caller's owned games as storefront cards.
async fn store_library_list(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not signed in"}))).into_response();
    };
    let ids = match owned_game_ids(&st.db, user.id).await {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let base = public_base_url(&st, &headers).await;
    let mut cards: Vec<StoreCard> = Vec::with_capacity(ids.len());
    for id in ids {
        if let Ok(Some(mut game)) = find_game(&st.db, &id).await {
            hydrate_server_art_url(&st, &base, &mut game).await;
            cards.push(card_from_game(game));
        }
    }
    Json(serde_json::json!({
        "schemaVersion": 1,
        "count": cards.len(),
        "games": cards,
    }))
    .into_response()
}
