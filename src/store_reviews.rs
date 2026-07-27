// store_reviews.rs — player reviews on the storefront game pages.
//
// The launcher already has reviews (`/api/social/review*` in social_api.rs), but
// those handlers authenticate with a **bearer token** via `launcher_user`, which
// a browser session does not have. These are the same feature over the same
// `game_reviews` table, gated by the storefront cookie via `web_user` — so a
// review written on the website is the same row the desktop client reads, and
// vice versa. One review per user per game (the table's primary key).
//
// Assembled via include! at crate-root scope.
//
// SECURITY: reading requires a signed-in storefront session, like the rest of
// the catalog. Writing only ever touches the caller's own row — the user id
// comes from the session, never from the request body. The one exception is
// moderation: `store_review_moderate_delete` can remove anyone's row, and is
// gated on `user.is_admin` from the session, never on a client-supplied flag.

/// Longest review body we store. Matches the launcher's limit so a review
/// written on either surface round-trips unchanged.
const REVIEW_BODY_MAX: usize = 4000;
/// Newest-first cap on a game page. Far above any real review count here, but it
/// keeps one popular game from returning an unbounded row set.
const REVIEW_PAGE_MAX: usize = 200;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreReview {
    user_id: u64,
    username: String,
    rating: i8,
    body: String,
    updated_at: i64,
    /// True for the caller's own review, so the page can offer edit/delete.
    mine: bool,
}

#[derive(Debug, Deserialize)]
struct StoreReviewForm {
    rating: i8,
    #[serde(default)]
    body: String,
}

/// Rating distribution 1..5 → count, for the histogram on the game page.
fn rating_histogram(reviews: &[StoreReview]) -> [u64; 5] {
    let mut out = [0u64; 5];
    for r in reviews {
        if (1..=5).contains(&r.rating) {
            out[(r.rating - 1) as usize] += 1;
        }
    }
    out
}

/// Mean rating rounded to two decimals; 0.0 when there are no reviews.
fn rating_average(reviews: &[StoreReview]) -> f64 {
    if reviews.is_empty() {
        return 0.0;
    }
    let sum: i64 = reviews.iter().map(|r| r.rating as i64).sum();
    ((sum as f64 / reviews.len() as f64) * 100.0).round() / 100.0
}

// GET /api/store/games/:id/reviews — every review for a game, newest first.
async fn store_reviews_get(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return store_signin_required();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let rows: Vec<(u64, String, i8, Option<String>, i64)> = match c
        .exec(
            r#"SELECT r.user_id, COALESCE(u.username,''), r.rating, r.body, r.updated_at
               FROM game_reviews r LEFT JOIN admin_users u ON u.id = r.user_id
               WHERE r.game_id=:g ORDER BY r.updated_at DESC LIMIT :lim"#,
            params! {"g" => &id, "lim" => REVIEW_PAGE_MAX},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let reviews: Vec<StoreReview> = rows
        .into_iter()
        .map(|(uid, username, rating, body, updated_at)| StoreReview {
            mine: uid == user.id,
            user_id: uid,
            username,
            rating,
            body: body.unwrap_or_default(),
            updated_at,
        })
        .collect();
    Json(serde_json::json!({
        "schemaVersion": 1,
        "gameId": id,
        // Lets the page render a remove control on other people's reviews
        // without a second request. The server re-checks on delete regardless.
        "canModerate": user.is_admin,
        "count": reviews.len(),
        "average": rating_average(&reviews),
        "histogram": rating_histogram(&reviews),
        "reviews": reviews,
    }))
    .into_response()
}

// PUT /api/store/games/:id/review — create or replace the caller's own review.
async fn store_review_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Form(form): Form<StoreReviewForm>,
) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return store_signin_required();
    };
    // Only real catalog games can be reviewed, so a typo'd id can't seed a row
    // that no page will ever show.
    match find_game(&st.db, &id).await {
        Ok(Some(_)) => {}
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "game not found"})))
                .into_response()
        }
        Err(e) => return server_error(e),
    }
    if !(1..=5).contains(&form.rating) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "rating must be 1-5"})),
        )
            .into_response();
    }
    // Truncate by characters, not bytes — a byte slice could split a multi-byte
    // character and produce invalid UTF-8.
    let body: String = form.body.trim().chars().take(REVIEW_BODY_MAX).collect();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            r#"INSERT INTO game_reviews (user_id, game_id, rating, body, updated_at)
               VALUES (:u, :g, :r, :b, :t)
               ON DUPLICATE KEY UPDATE rating=VALUES(rating), body=VALUES(body), updated_at=VALUES(updated_at)"#,
            params! {"u" => user.id, "g" => &id, "r" => form.rating, "b" => &body, "t" => now()},
        )
        .await
    {
        return server_error(e);
    }
    // Same activity-feed event the launcher posts, so a review written on the
    // website still shows up in friends' feeds.
    record_activity(
        &st,
        user.id,
        "review",
        Some(&id),
        Some(serde_json::json!({"rating": form.rating})),
    )
    .await;
    Json(serde_json::json!({"ok": true, "gameId": id, "rating": form.rating})).into_response()
}

// DELETE /api/store/games/:id/review — remove the caller's own review.
async fn store_review_delete(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return store_signin_required();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            "DELETE FROM game_reviews WHERE user_id=:u AND game_id=:g",
            params! {"u" => user.id, "g" => &id},
        )
        .await
    {
        return server_error(e);
    }
    Json(serde_json::json!({"ok": true, "gameId": id})).into_response()
}

// DELETE /api/store/games/:id/reviews/:user_id — admin moderation: remove any
// user's review. Non-admins get 403 rather than 404, so a mistyped id can't be
// used to probe which reviews exist.
async fn store_review_moderate_delete(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath((id, target_id)): AxumPath<(String, u64)>,
) -> Response {
    let Some(user) = web_user(&st, &headers).await else {
        return store_signin_required();
    };
    if !user.is_admin {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "admin required"})),
        )
            .into_response();
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            "DELETE FROM game_reviews WHERE user_id=:u AND game_id=:g",
            params! {"u" => target_id, "g" => &id},
        )
        .await
    {
        return server_error(e);
    }
    tracing::info!(
        "admin {} removed review by user {} on game {}",
        user.username,
        target_id,
        id
    );
    Json(serde_json::json!({"ok": true, "gameId": id, "userId": target_id})).into_response()
}
