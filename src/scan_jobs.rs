// scan_jobs.rs - split out of main.rs and re-assembled via include! (crate-root scope).

fn start_library_watcher(st: AppState) -> bool {
    use notify::{EventKind, RecursiveMode, Watcher};
    let watch_dir = {
        let games = st.cfg.library_root.join("games");
        if games.is_dir() { games } else { st.cfg.library_root.clone() }
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(64);
    let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            // Content/structure changes only; ignore Access (atime) noise.
            let relevant = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );
            if relevant {
                // Non-blocking: a full channel already has a pending wake-up.
                let _ = tx.try_send(());
            }
        }
    });
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            warn!("filesystem watcher init failed: {e}");
            return false;
        }
    };
    if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::Recursive) {
        warn!("filesystem watch on {} failed: {e}", watch_dir.display());
        return false;
    }
    // Keep the watcher alive for the lifetime of the process without holding it
    // across an await (RecommendedWatcher owns an OS-level handle/thread).
    Box::leak(Box::new(watcher));
    info!("filesystem watcher active on {}", watch_dir.display());

    let debounce = std::time::Duration::from_secs(LIBRARY_WATCH_DEBOUNCE_SECS);
    tokio::spawn(async move {
        while rx.recv().await.is_some() {
            // Drain until the library goes quiet for `debounce`, so a big copy
            // doesn't trigger a rescan per file.
            loop {
                match tokio::time::timeout(debounce, rx.recv()).await {
                    Ok(Some(())) => continue,
                    Ok(None) => return,
                    Err(_) => break,
                }
            }
            let msg = spawn_rescan(&st);
            info!("fs-watch: library change detected -> {msg}");
        }
    });
    true
}

fn spawn_rescan(st: &AppState) -> String {
    {
        let mut guard = match st.scan.lock() {
            Ok(g) => g,
            Err(_) => return "Scan status unavailable.".to_string(),
        };
        if guard.running {
            return "A rescan is already in progress.".to_string();
        }
        *guard = ScanStatus {
            running: true,
            phase: "scanning".to_string(),
            message: "Starting catalog scan…".to_string(),
            started_at: now(),
            updated_at: now(),
            ..Default::default()
        };
    }
    let st = st.clone();
    tokio::spawn(async move {
        match rescan_catalog(&st).await {
            Ok(msg) => st.set_scan(|s| {
                s.running = false;
                s.phase = "done".to_string();
                s.current = String::new();
                s.message = msg;
            }),
            Err(e) => st.set_scan(|s| {
                s.running = false;
                s.phase = "error".to_string();
                s.current = String::new();
                s.message = format!("Rescan failed: {e}");
            }),
        }
    });
    "Catalog rescan started in the background.".to_string()
}

async fn rescan_catalog(st: &AppState) -> Result<String> {
    st.set_scan(|s| { s.phase = "scanning".to_string(); s.message = "Scanning library…".to_string(); });
    let games = scan_catalog(&st.cfg.library_root).await?;
    let new_games = sync_catalog_db(&st.db, &games).await?;
    // Announce genuinely new catalog entries to Discord (no-op without a webhook).
    for game in &new_games {
        notify_new_game(st, game.clone());
    }
    if let Err(e) = ensure_manifests(st, &games).await {
        error!("manifest precompute failed: {e}");
    }
    st.set_scan(|s| { s.phase = "igdb".to_string(); s.current = String::new(); s.message = "Enriching metadata…".to_string(); });
    let enrichment = enrich_catalog_from_igdb(st, false).await.unwrap_or_else(|e| format!("IGDB enrichment skipped: {e}"));

    let mut by_platform = BTreeMap::<String, usize>::new();
    for game in &games {
        *by_platform.entry(game.platform.clone()).or_default() += 1;
    }
    let mut msg = format!(
        "Synced {} games to MariaDB.",
        games.len()
    );
    for (platform, count) in by_platform {
        msg.push_str(&format!("\n{platform}: {count}"));
    }
    msg.push_str(&format!("\n{enrichment}"));
    Ok(msg)
}

async fn admin_scan_status(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(_)) => Json(st.scan_snapshot()).into_response(),
        _ => (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "not signed in"}))).into_response(),
    }
}

