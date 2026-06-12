// discord.rs - outbound Discord webhook notifications (crate-root scope).
//
// Fired when a new game is registered in the catalog (see sync_catalog_db /
// rescan_catalog). All network work runs on a spawned task so it never blocks
// the scan or an axum request thread. No-ops silently when no webhook is set.

// Human-readable byte size, e.g. "42.3 GB". Binary units (1024-based) to match
// what the client reports for download sizes.
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0usize;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

// Total on-disk size of a game's content (a single file, or a folder summed
// recursively). Returns 0 if the path can't be resolved or read.
async fn content_size_bytes(cfg: &Config, game: &Game) -> u64 {
    let Ok(root) = content_path_for(cfg, game).await else { return 0; };
    match fs::metadata(&root).await {
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => {
            let mut total = 0u64;
            if let Ok(files) = walk_files(&root).await {
                for f in files {
                    if let Ok(m) = fs::metadata(&f).await {
                        total += m.len();
                    }
                }
            }
            total
        }
        Err(_) => 0,
    }
}

// The configured Discord webhook URL, validated to a real Discord webhook
// endpoint so we never POST catalog/game data to an arbitrary host.
async fn discord_webhook_url(db: &Pool) -> Option<String> {
    let value = setting_value(db, DISCORD_WEBHOOK_KEY).await.ok().flatten()?;
    let value = value.trim().to_string();
    if value.starts_with("https://discord.com/api/webhooks/")
        || value.starts_with("https://discordapp.com/api/webhooks/")
        || value.starts_with("https://ptb.discord.com/api/webhooks/")
        || value.starts_with("https://canary.discord.com/api/webhooks/")
    {
        Some(value)
    } else {
        None
    }
}

// Build the rich embed payload Discord renders for a newly added game.
fn new_game_embed(platform: &str, title: &str, size: &str) -> serde_json::Value {
    let field = |name: &str, value: &str| {
        serde_json::json!({
            "name": name,
            "value": if value.trim().is_empty() { "Unknown" } else { value },
            "inline": true,
        })
    };
    serde_json::json!({
        "username": "ArcadeLauncher",
        "embeds": [{
            "title": "🚀 New Game Added to ArcadeLauncher!",
            "color": 5_814_783,
            "fields": [
                field("Platform", platform),
                field("Game Name", title),
                field("Size", size),
            ],
            "timestamp": chrono::Utc::now().to_rfc3339(),
        }]
    })
}

async fn post_discord(url: &str, payload: &serde_json::Value) -> Result<()> {
    let http = Client::builder()
        .user_agent("ArcadeLauncher-Server/1.0")
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let resp = http.post(url).json(payload).send().await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Discord webhook returned {status}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    Ok(())
}

// Fire-and-forget Discord notification for a newly registered game. Spawned so it
// never blocks the scan/request path; silently no-ops when no webhook is set.
fn notify_new_game(st: &AppState, game: Game) {
    let st = st.clone();
    tokio::spawn(async move {
        let Some(url) = discord_webhook_url(&st.db).await else { return; };
        let size = human_size(content_size_bytes(&st.cfg, &game).await);
        let payload = new_game_embed(&game.platform, &game.title, &size);
        match post_discord(&url, &payload).await {
            Ok(()) => info!("Discord notified: new game {} [{}] ({size})", game.title, game.platform),
            Err(e) => warn!("Discord new-game notify failed for {} [{}]: {e}", game.title, game.platform),
        }
    });
}

// Admin "Test Webhook" button: validates the saved URL and sends a sample embed
// synchronously so the result can be surfaced back in the admin panel.
async fn test_discord_webhook(db: &Pool) -> Result<String> {
    let url = discord_webhook_url(db)
        .await
        .ok_or_else(|| anyhow!("save a valid https://discord.com/api/webhooks/... URL first"))?;
    let payload = new_game_embed("Test", "ArcadeLauncher Webhook Test", "42.3 GB");
    post_discord(&url, &payload).await?;
    Ok("Test embed sent to Discord successfully.".to_string())
}
