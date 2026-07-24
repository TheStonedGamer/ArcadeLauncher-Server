// store_api.rs - public, UNAUTHENTICATED storefront endpoints (Steam-style browse).
// Assembled via include! at crate-root scope like the other modules, so it shares
// AppState, Game, list_games/find_game, and the art helpers.
//
// SECURITY: every handler here is deliberately public (no authorized_api gate).
// It must expose ONLY non-sensitive catalog metadata + aggregate community stats.
// Never surface tokens, user identities, content_path, launch args, or per-user
// rows. The shaping structs below are the allow-list — do not serialize `Game`
// directly (it carries content_path / launch).

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreCard {
    id: String,
    title: String,
    platform: String,
    cover_art_url: String,
    genres: Vec<String>,
    igdb_rating: f64,
    release_date: i64,
    developer: String,
    publisher: String,
    // True when the requesting browser session already has this game in its
    // library. Always false for anonymous requests.
    owned: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreStats {
    player_count: u64,       // distinct users who have this game in their library
    total_playtime_hours: u64,
    play_count: u64,
    avg_user_rating: f64,    // 0..5, from game_stats.rating where set
    review_count: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreDetail {
    id: String,
    title: String,
    platform: String,
    install_type: String,
    cover_art_url: String,
    summary: String,
    genres: Vec<String>,
    igdb_rating: f64,
    release_date: i64,
    developer: String,
    publisher: String,
    franchise: String,
    screenshots: Vec<String>,
    stats: StoreStats,
    owned: bool,
}

fn split_genres(raw: &str) -> Vec<String> {
    raw.split([',', '|'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn card_from_game(g: Game) -> StoreCard {
    StoreCard {
        id: g.id,
        title: g.title,
        platform: g.platform,
        cover_art_url: g.cover_art_url,
        genres: split_genres(&g.genres),
        igdb_rating: g.igdb_rating,
        release_date: g.release_date,
        developer: g.developer,
        publisher: g.publisher,
        owned: false,
    }
}

// The caller's owned-game id set, empty when anonymous. Lets the storefront show
// "In Library ✓" without a second round-trip.
async fn owned_set_for_request(st: &AppState, headers: &HeaderMap) -> std::collections::HashSet<String> {
    match web_user(st, headers).await {
        Some(user) => owned_game_ids(&st.db, user.id)
            .await
            .unwrap_or_default()
            .into_iter()
            .collect(),
        None => std::collections::HashSet::new(),
    }
}

// GET /api/store/games — full public catalog as lightweight cards. No per-game
// stats here (kept fast + cacheable); the detail endpoint carries stats.
async fn store_games(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match list_games(&st.db).await {
        Ok(mut games) => {
            let base = public_base_url(&st, &headers).await;
            for game in &mut games {
                hydrate_server_art_url(&st, &base, game).await;
            }
            let owned = owned_set_for_request(&st, &headers).await;
            let cards: Vec<StoreCard> = games
                .into_iter()
                .map(|g| {
                    let mut card = card_from_game(g);
                    card.owned = owned.contains(&card.id);
                    card
                })
                .collect();
            Json(serde_json::json!({
                "schemaVersion": 1,
                "count": cards.len(),
                "games": cards,
            }))
            .into_response()
        }
        Err(e) => server_error(e),
    }
}

// Aggregate community stats for one game, computed from game_stats + game_reviews.
async fn store_stats_for(db: &Pool, game_id: &str) -> Result<StoreStats> {
    let mut c = db.get_conn().await?;
    let row: Option<(Option<i64>, Option<i64>, Option<f64>)> = c
        .exec_first(
            r#"SELECT
                 COALESCE(SUM(playtime_seconds),0)                   AS total_secs,
                 COALESCE(SUM(play_count),0)                         AS plays,
                 AVG(NULLIF(rating,0))                               AS avg_rating
               FROM game_stats WHERE game_id=:g"#,
            params! {"g" => game_id},
        )
        .await?;
    let (total_secs, plays, avg_rating) = row.unwrap_or((Some(0), Some(0), None));
    // player_count is now real ownership: distinct users who added the game to
    // their library (Steam-style), not merely anyone with a playtime row.
    let player_count: u64 = c
        .exec_first(
            "SELECT COUNT(*) FROM user_library WHERE game_id=:g",
            params! {"g" => game_id},
        )
        .await?
        .unwrap_or(0);
    let review_count: u64 = c
        .exec_first(
            "SELECT COUNT(*) FROM game_reviews WHERE game_id=:g",
            params! {"g" => game_id},
        )
        .await?
        .unwrap_or(0);
    Ok(StoreStats {
        player_count,
        total_playtime_hours: (total_secs.unwrap_or(0) as u64) / 3600,
        play_count: plays.unwrap_or(0) as u64,
        avg_user_rating: (avg_rating.unwrap_or(0.0) * 100.0).round() / 100.0,
        review_count,
    })
}

// GET /api/store/games/:id — one game's public detail + aggregate stats.
async fn store_game_detail(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let mut game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, Json(serde_json::json!({"error": "game not found"})))
                .into_response()
        }
        Err(e) => return server_error(e),
    };
    let base = public_base_url(&st, &headers).await;
    hydrate_server_art_url(&st, &base, &mut game).await;
    let stats = match store_stats_for(&st.db, &game.id).await {
        Ok(s) => s,
        Err(e) => return server_error(e),
    };
    let owned = owned_set_for_request(&st, &headers).await.contains(&game.id);
    let detail = StoreDetail {
        id: game.id,
        title: game.title,
        platform: game.platform,
        install_type: game.install_type,
        cover_art_url: game.cover_art_url,
        summary: game.summary,
        genres: split_genres(&game.genres),
        igdb_rating: game.igdb_rating,
        release_date: game.release_date,
        developer: game.developer,
        publisher: game.publisher,
        franchise: game.franchise,
        screenshots: game.screenshots,
        stats,
        owned,
    };
    Json(detail).into_response()
}

// GET /api/store/summary — landing-page totals across the whole catalog.
async fn store_summary(State(st): State<AppState>) -> Response {
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let total_games: u64 = c.query_first("SELECT COUNT(*) FROM games").await.ok().flatten().unwrap_or(0);
    let total_platforms: u64 = c
        .query_first("SELECT COUNT(DISTINCT platform) FROM games")
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    let total_playtime_hours: u64 = c
        .query_first("SELECT COALESCE(SUM(playtime_seconds),0) FROM game_stats")
        .await
        .ok()
        .flatten()
        .map(|s: i64| (s as u64) / 3600)
        .unwrap_or(0);
    Json(serde_json::json!({
        "schemaVersion": 1,
        "totalGames": total_games,
        "totalPlatforms": total_platforms,
        "totalPlaytimeHours": total_playtime_hours,
    }))
    .into_response()
}
