// igdb.rs - split out of main.rs and re-assembled via include! (crate-root scope).

async fn enrich_catalog_from_igdb(st: &AppState, force: bool) -> Result<String> {
    let client_id = setting_value(&st.db, IGDB_CLIENT_ID_KEY).await?.unwrap_or_default();
    let client_secret = setting_value(&st.db, IGDB_CLIENT_SECRET_KEY).await?.unwrap_or_default();
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(anyhow!("set {IGDB_CLIENT_ID_KEY} and {IGDB_CLIENT_SECRET_KEY} in Configuration first"));
    }

    let http = Client::builder().user_agent("ArcadeLauncher-Server/1.0").build()?;
    let token = igdb_authenticate(&http, &client_id, &client_secret).await?;
    let games = list_games(&st.db).await?;
    let mut matched = 0usize;
    let mut skipped = 0usize;
    let mut failed = 0usize;

    for game in games {
        if !force && game.igdb_id > 0 && !game.summary.is_empty() && !game.cover_art_url.is_empty() {
            skipped += 1;
            continue;
        }
        match igdb_best_match(&http, &client_id, &token, &game).await {
            Ok(Some(meta)) => {
                let cover_art_url = if !meta.cover_image_id.is_empty() {
                    match cache_igdb_cover(&http, st, &game.id, &meta.cover_image_id).await {
                        Ok(true) => "local".to_string(),
                        _ => igdb_cover_url(&meta.cover_image_id),
                    }
                } else {
                    game.cover_art_url.clone()
                };
                save_game_metadata(&st.db, &game.id, &meta, &cover_art_url).await?;
                matched += 1;
                tokio::time::sleep(std::time::Duration::from_millis(260)).await;
            }
            Ok(None) => {
                failed += 1;
                tokio::time::sleep(std::time::Duration::from_millis(260)).await;
            }
            Err(e) => {
                failed += 1;
                error!("IGDB metadata failed for {}: {e}", game.title);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Ok(format!("IGDB enrichment complete. matched: {matched}, skipped: {skipped}, unmatched/failed: {failed}"))
}

async fn igdb_authenticate(http: &Client, client_id: &str, client_secret: &str) -> Result<String> {
    let json: serde_json::Value = http
        .post("https://id.twitch.tv/oauth2/token")
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "client_credentials"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    json.get("access_token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Twitch auth response did not include access_token"))
}

async fn igdb_best_match(http: &Client, client_id: &str, token: &str, game: &Game) -> Result<Option<IgdbMatch>> {
    let title = clean_igdb_title(&game.title);
    let mut candidates = igdb_search(http, client_id, token, &title, igdb_platform_ids(&game.platform)).await?;
    if candidates.is_empty() && !igdb_platform_ids(&game.platform).is_empty() {
        candidates = igdb_search(http, client_id, token, &title, &[]).await?;
    }
    let norm_title = match_key(&title);
    let mut best: Option<(f64, IgdbMatch)> = None;
    for candidate in candidates {
        let score = title_similarity(&norm_title, &match_key(&candidate.name));
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, candidate));
        }
    }
    Ok(best.and_then(|(score, meta)| if score >= 0.60 { Some(meta) } else { None }))
}

async fn igdb_credentials(db: &Pool) -> Result<(String, String)> {
    let client_id = setting_value(db, IGDB_CLIENT_ID_KEY).await?.unwrap_or_default();
    let client_secret = setting_value(db, IGDB_CLIENT_SECRET_KEY).await?.unwrap_or_default();
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(anyhow!("set IGDB credentials in Configuration first"));
    }
    Ok((client_id, client_secret))
}

async fn igdb_search_for_game(st: &AppState, game: &Game, query: &str) -> Result<Vec<IgdbMatch>> {
    let query = sanitize_search_query(query)?;
    let platforms = igdb_platform_ids(&game.platform);
    if platforms.is_empty() {
        return Err(anyhow!("no IGDB platform mapping is configured for {}", game.platform));
    }
    let (client_id, client_secret) = igdb_credentials(&st.db).await?;
    let http = Client::builder().user_agent("ArcadeLauncher-Server/1.0").build()?;
    let token = igdb_authenticate(&http, &client_id, &client_secret).await?;
    igdb_search(&http, &client_id, &token, &query, platforms).await
}

async fn igdb_fetch_by_id(http: &Client, client_id: &str, token: &str, igdb_id: u64) -> Result<IgdbMatch> {
    let body = format!(
        "fields id,name,summary,rating,first_release_date,cover.image_id,genres.name;where id = {igdb_id};limit 1;"
    );
    let value: serde_json::Value = http
        .post("https://api.igdb.com/v4/games")
        .header("Client-ID", client_id)
        .bearer_auth(token)
        .body(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    value
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(parse_igdb_match)
        .ok_or_else(|| anyhow!("IGDB game {igdb_id} was not found"))
}

async fn apply_manual_igdb_match(st: &AppState, game_id: &str, igdb_id: u64) -> Result<String> {
    let game = find_game(&st.db, game_id).await?.ok_or_else(|| anyhow!("game not found"))?;
    let (client_id, client_secret) = igdb_credentials(&st.db).await?;
    let http = Client::builder().user_agent("ArcadeLauncher-Server/1.0").build()?;
    let token = igdb_authenticate(&http, &client_id, &client_secret).await?;
    let meta = igdb_fetch_by_id(&http, &client_id, &token, igdb_id).await?;
    let cover_art_url = if !meta.cover_image_id.is_empty() {
        match cache_igdb_cover(&http, st, &game.id, &meta.cover_image_id).await {
            Ok(true) => "local".to_string(),
            _ => igdb_cover_url(&meta.cover_image_id),
        }
    } else {
        game.cover_art_url.clone()
    };
    save_game_metadata(&st.db, &game.id, &meta, &cover_art_url).await?;
    Ok(game.title)
}

async fn igdb_search(http: &Client, client_id: &str, token: &str, title: &str, platforms: &[i32]) -> Result<Vec<IgdbMatch>> {
    let escaped = title.replace('\\', "\\\\").replace('"', "\\\"");
    let mut body = format!(
        "search \"{escaped}\";fields id,name,summary,rating,first_release_date,cover.image_id,genres.name;"
    );
    if !platforms.is_empty() {
        body.push_str("where release_dates.platform = (");
        body.push_str(&platforms.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(","));
        body.push_str(");");
    }
    body.push_str("limit 8;");
    let value: serde_json::Value = http
        .post("https://api.igdb.com/v4/games")
        .header("Client-ID", client_id)
        .bearer_auth(token)
        .body(body)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let Some(items) = value.as_array() else { return Ok(Vec::new()); };
    Ok(items.iter().filter_map(parse_igdb_match).collect())
}

fn parse_igdb_match(v: &serde_json::Value) -> Option<IgdbMatch> {
    let id = v.get("id")?.as_u64()?;
    let name = v.get("name")?.as_str()?.to_string();
    let genres = v
        .get("genres")
        .and_then(|g| g.as_array())
        .map(|arr| arr.iter().filter_map(|g| g.get("name").and_then(|n| n.as_str())).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    Some(IgdbMatch {
        id,
        name,
        summary: v.get("summary").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        genres,
        rating: v.get("rating").and_then(|r| r.as_f64()).unwrap_or_default(),
        release_date: v.get("first_release_date").and_then(|d| d.as_i64()).unwrap_or_default(),
        cover_image_id: v.get("cover").and_then(|c| c.get("image_id")).and_then(|i| i.as_str()).unwrap_or_default().to_string(),
    })
}

async fn cache_igdb_cover(http: &Client, st: &AppState, game_id: &str, image_id: &str) -> Result<bool> {
    let bytes = http.get(igdb_cover_url(image_id)).send().await?.error_for_status()?.bytes().await?;
    let path = server_cover_path(&st.cfg.library_root, game_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, bytes).await?;
    Ok(true)
}

async fn save_game_metadata(db: &Pool, game_id: &str, meta: &IgdbMatch, cover_art_url: &str) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop(
        r#"UPDATE games
           SET igdb_id=:igdb_id,
               summary=:summary,
               genres=:genres,
               igdb_rating=:igdb_rating,
               release_date=:release_date,
               cover_art_url=IF(:cover_art_url='',cover_art_url,:cover_art_url),
               updated_at=:updated_at
           WHERE id=:id"#,
        params! {
            "id" => game_id,
            "igdb_id" => meta.id,
            "summary" => &meta.summary,
            "genres" => &meta.genres,
            "igdb_rating" => meta.rating,
            "release_date" => meta.release_date,
            "cover_art_url" => cover_art_url,
            "updated_at" => now(),
        },
    )
    .await?;
    Ok(())
}

