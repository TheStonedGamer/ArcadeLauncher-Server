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
            // Strictly validate the payload before committing: a usable match must
            // carry a real IGDB id and a non-empty name.
            Ok(Some(meta)) if meta.id > 0 && !meta.name.trim().is_empty() => {
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
            // Matched something but it failed validation, or no match at all:
            // fall back to local folder scraping so the queue never stalls.
            Ok(_) => {
                failed += 1;
                warn!("IGDB no valid match for id={} platform={} title={}", game.id, game.platform, game.title);
                if let Err(e) = apply_local_metadata_fallback(st, &game).await {
                    warn!("local metadata fallback failed for id={}: {e}", game.id);
                }
                tokio::time::sleep(std::time::Duration::from_millis(260)).await;
            }
            Err(e) => {
                failed += 1;
                error!("IGDB metadata failed for id={} platform={} title={}: {e}", game.id, game.platform, game.title);
                if let Err(fe) = apply_local_metadata_fallback(st, &game).await {
                    warn!("local metadata fallback failed for id={}: {fe}", game.id);
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Ok(format!("IGDB enrichment complete. matched: {matched}, skipped: {skipped}, unmatched/failed: {failed}"))
}

// Exponential backoff (1,2,4,8,16s, capped at 30) with ±1s jitter for IGDB/Twitch
// retries. Attempt is 1-based.
fn igdb_backoff_secs(attempt: u32) -> u64 {
    let base = 1u64 << attempt.saturating_sub(1).min(5);
    base.min(30) + rand::thread_rng().gen_range(0..=1)
}

// Whether a reqwest transport error is worth retrying (timeout / connect /
// transient request failure) rather than a permanent client error.
fn is_transient(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

const IGDB_MAX_ATTEMPTS: u32 = 5;

// POST to the IGDB games endpoint with exponential-backoff retries. Retries on
// HTTP 429 (honoring Retry-After when present) and 5xx / transient transport
// errors; surfaces the raw status + body on a permanent failure so callers can
// log the API error code.
async fn igdb_post_json(http: &Client, client_id: &str, token: &str, body: String) -> Result<serde_json::Value> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let sent = http
            .post("https://api.igdb.com/v4/games")
            .header("Client-ID", client_id)
            .bearer_auth(token)
            .body(body.clone())
            .send()
            .await;
        match sent {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp.json().await?);
                }
                let retryable = status.as_u16() == 429 || status.is_server_error();
                if retryable && attempt < IGDB_MAX_ATTEMPTS {
                    let retry_after = resp
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    let delay = retry_after.unwrap_or_else(|| igdb_backoff_secs(attempt));
                    warn!("IGDB API {status} (attempt {attempt}/{IGDB_MAX_ATTEMPTS}); retrying in {delay}s");
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    continue;
                }
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow!(
                    "IGDB API returned {status}: {}",
                    text.chars().take(200).collect::<String>()
                ));
            }
            Err(e) if attempt < IGDB_MAX_ATTEMPTS && is_transient(&e) => {
                let delay = igdb_backoff_secs(attempt);
                warn!("IGDB transport error (attempt {attempt}/{IGDB_MAX_ATTEMPTS}): {e}; retrying in {delay}s");
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

// Local-folder metadata fallback used when IGDB can't match, times out, or errors
// out — so the sync queue keeps moving instead of leaving a game blank. Scrapes a
// sibling `.nfo` sidecar (Genre/Description lines) and a local cover image, then
// commits whatever it found.
async fn apply_local_metadata_fallback(st: &AppState, game: &Game) -> Result<()> {
    let root = content_path_for(&st.cfg, game).await?;
    let dir = match fs::metadata(&root).await {
        Ok(m) if m.is_file() => root.parent().map(Path::to_path_buf),
        Ok(_) => Some(root.clone()),
        Err(_) => None,
    };
    let mut summary = String::new();
    let mut genres = String::new();
    if let Some(dir) = &dir {
        if let Ok(mut entries) = fs::read_dir(dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                let is_nfo = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("nfo"))
                    .unwrap_or(false);
                if !is_nfo {
                    continue;
                }
                if let Ok(text) = fs::read_to_string(&path).await {
                    for line in text.lines() {
                        let trimmed = line.trim();
                        let Some(colon) = trimmed.find(':') else { continue; };
                        let (label, value) = trimmed.split_at(colon);
                        let value = value[1..].trim();
                        if value.is_empty() {
                            continue;
                        }
                        match label.trim().to_ascii_lowercase().as_str() {
                            "genre" | "genres" if genres.is_empty() => genres = value.to_string(),
                            "summary" | "description" | "plot" if summary.is_empty() => summary = value.to_string(),
                            _ => {}
                        }
                    }
                }
                break;
            }
        }
    }
    let cover = if find_local_cover(&root).await.is_some() { "local".to_string() } else { String::new() };
    if summary.is_empty() && genres.is_empty() && cover.is_empty() {
        return Ok(());
    }
    let mut c = st.db.get_conn().await?;
    c.exec_drop(
        r#"UPDATE games
           SET summary=IF(:summary='',summary,:summary),
               genres=IF(:genres='',genres,:genres),
               cover_art_url=IF(:cover='',cover_art_url,:cover),
               updated_at=:t
           WHERE id=:id"#,
        params! {"summary" => &summary, "genres" => &genres, "cover" => &cover, "t" => now(), "id" => &game.id},
    )
    .await?;
    info!("local metadata fallback applied for id={} platform={}", game.id, game.platform);
    Ok(())
}

async fn igdb_authenticate(http: &Client, client_id: &str, client_secret: &str) -> Result<String> {
    let mut attempt = 0u32;
    let resp = loop {
        attempt += 1;
        let sent = http
            .post("https://id.twitch.tv/oauth2/token")
            .form(&[
                ("client_id", client_id),
                ("client_secret", client_secret),
                ("grant_type", "client_credentials"),
            ])
            .send()
            .await;
        match sent {
            Ok(r) => {
                let status = r.status();
                if (status.as_u16() == 429 || status.is_server_error()) && attempt < IGDB_MAX_ATTEMPTS {
                    let delay = igdb_backoff_secs(attempt);
                    warn!("Twitch auth {status} (attempt {attempt}/{IGDB_MAX_ATTEMPTS}); retrying in {delay}s");
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                    continue;
                }
                break r.error_for_status()?;
            }
            Err(e) if attempt < IGDB_MAX_ATTEMPTS && is_transient(&e) => {
                let delay = igdb_backoff_secs(attempt);
                warn!("Twitch auth transport error (attempt {attempt}/{IGDB_MAX_ATTEMPTS}): {e}; retrying in {delay}s");
                tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let json: serde_json::Value = resp.json().await?;
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
        "fields id,name,summary,rating,first_release_date,cover.image_id,genres.name,screenshots.image_id,artworks.image_id;where id = {igdb_id};limit 1;"
    );
    let value = igdb_post_json(http, client_id, token, body).await?;
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
        "search \"{escaped}\";fields id,name,summary,rating,first_release_date,cover.image_id,genres.name,screenshots.image_id,artworks.image_id;"
    );
    if !platforms.is_empty() {
        body.push_str("where release_dates.platform = (");
        body.push_str(&platforms.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(","));
        body.push_str(");");
    }
    body.push_str("limit 8;");
    let value = igdb_post_json(http, client_id, token, body).await?;
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
    // Gather gallery imagery: screenshots first (in-game shots), then artworks
    // (key art / wallpapers). Both come back as arrays of {image_id}; map each to
    // a full-resolution image URL.
    let collect = |key: &str, size: &str| -> Vec<String> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|s| s.get("image_id").and_then(|i| i.as_str()))
                    .map(|id| igdb_image_url(size, id))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let mut screenshots = collect("screenshots", "t_screenshot_big");
    screenshots.extend(collect("artworks", "t_1080p"));
    screenshots.truncate(12); // plenty for a gallery; keeps the row/JSON bounded

    Some(IgdbMatch {
        id,
        name,
        summary: v.get("summary").and_then(|s| s.as_str()).unwrap_or_default().to_string(),
        genres,
        rating: v.get("rating").and_then(|r| r.as_f64()).unwrap_or_default(),
        release_date: v.get("first_release_date").and_then(|d| d.as_i64()).unwrap_or_default(),
        cover_image_id: v.get("cover").and_then(|c| c.get("image_id")).and_then(|i| i.as_str()).unwrap_or_default().to_string(),
        screenshots,
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
               screenshots=IF(:screenshots='',screenshots,:screenshots),
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
            "screenshots" => meta.screenshots.join("\n"),
            "updated_at" => now(),
        },
    )
    .await?;
    Ok(())
}

