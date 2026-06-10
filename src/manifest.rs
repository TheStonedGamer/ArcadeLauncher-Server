// manifest.rs - split out of main.rs and re-assembled via include! (crate-root scope).

async fn download_art(
    State(st): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Response {
    let game = match find_game(&st.db, &id).await {
        Ok(Some(g)) => g,
        Ok(None) => return (StatusCode::NOT_FOUND, "game not found").into_response(),
        Err(e) => return server_error(e),
    };
    let root = match content_path_for(&st.cfg, &game).await {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    let cached = server_cover_path(&st.cfg.library_root, &game.id);
    let path = if fs::metadata(&cached).await.map(|m| m.is_file()).unwrap_or(false) {
        Some(cached)
    } else {
        find_local_cover(&root).await
    };
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "art not found").into_response();
    };
    match stream_file(path, None).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn download_emulator(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(rel): AxumPath<String>,
) -> Response {
    let path = match safe_join(&st.cfg.library_root.join("emulators"), &rel) {
        Ok(p) => p,
        Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    };
    match stream_file(path, headers.get(header::RANGE)).await {
        Ok(r) => r,
        Err(e) => server_error(e),
    }
}

async fn authorized_api(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(auth) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(token) = auth.strip_prefix("Bearer ").map(str::trim) else {
        return false;
    };
    (!st.cfg.auth_token.is_empty() && constant_eq(token.as_bytes(), st.cfg.auth_token.as_bytes()))
        || validate_launcher_token(&st.db, token).await
}

async fn manifest_for(st: &AppState, headers: &HeaderMap, game: &Game) -> Result<Manifest> {
    let mut game = game.clone();
    let base = public_base_url(st, headers).await;
    hydrate_server_art_url(st, &base, &mut game).await;

    // Use precomputed hashes from the DB when they match the current version.
    // Fall back to hashing on demand (and persist) only when missing/stale —
    // this avoids re-hashing tens of GB on every manifest request.
    let stored = match load_stored_manifest(&st.db, &game.id, &game.version).await {
        Ok(Some(files)) => files,
        _ => {
            let files = compute_stored_files(st, &game).await?;
            if let Err(e) = store_manifest(&st.db, &game.id, &game.version, &files).await {
                error!("failed to persist manifest for {}: {e}", game.id);
            }
            files
        }
    };

    // Split off the texture-pack sentinel (if any) so it never enters the
    // installable file list and `file_index` stays aligned with /chunks.
    let mut texture_pack = None;
    let real_files: Vec<&StoredFile> = stored
        .iter()
        .filter(|sf| {
            if sf.path == TEXTURE_PACK_SENTINEL {
                texture_pack = Some(TexturePack {
                    size: sf.size,
                    sha256: sf.sha256.clone(),
                    url: format!("{}/textures/{}", base, urlencoding::encode(&game.id)),
                });
                false
            } else {
                true
            }
        })
        .collect();

    let mut manifest_files = Vec::new();
    for (file_index, sf) in real_files.iter().enumerate() {
        let rel = sf.path.clone();
        let chunks = sf
            .chunks
            .iter()
            .map(|c| ManifestChunk {
                index: c.index,
                offset: c.offset,
                size: c.size,
                sha256: c.sha256.clone(),
                compression: "none".into(),
                url: format!(
                    "{}/chunks/{}/{}/{}/{}",
                    base,
                    urlencoding::encode(&game.id),
                    file_index,
                    c.index,
                    encode_path(&rel)
                ),
            })
            .collect();
        manifest_files.push(ManifestFile {
            path: rel.clone(),
            size: sf.size,
            sha256: sf.sha256.clone(),
            url: format!("{}/files/{}/{}", base, urlencoding::encode(&game.id), encode_path(&rel)),
            chunk_size: CHUNK_SIZE,
            chunks,
        });
    }
    Ok(Manifest {
        schema_version: 1,
        id: game.id.clone(),
        title: game.title.clone(),
        platform: game.platform.clone(),
        install_type: game.install_type.clone(),
        version: game.version.clone(),
        cover_art_url: game.cover_art_url.clone(),
        igdb_id: game.igdb_id,
        launch: game.launch.clone(),
        files: manifest_files,
        texture_pack,
    })
}

// Hash every file of a game once, producing the full-file sha256 and per-chunk
// hashes in a single read pass. This is the expensive work that must happen
// during scan/sync, never inside a manifest request.
async fn compute_stored_files(st: &AppState, game: &Game) -> Result<Vec<StoredFile>> {
    let cfg = &st.cfg;
    let root = content_path_for(cfg, game).await?;
    let (files, rel_root) = if fs::metadata(&root).await?.is_file() {
        (vec![root.clone()], root.parent().unwrap_or(&cfg.library_root).to_path_buf())
    } else {
        (walk_files(&root).await?, root.clone())
    };
    // Hash files in parallel across blocking threads. SHA-256 over many GB is
    // CPU-bound, so we fan out one spawn_blocking task per file instead of
    // hashing every file serially on one core. A process-wide semaphore
    // (`hash_semaphore`) caps the total in-flight hashes to the host's
    // parallelism, so this composes safely with the game-level fan-out in
    // `ensure_manifests` without oversubscribing the CPU.
    let concurrency = hash_concurrency();
    let mut out: Vec<StoredFile> = futures_util::stream::iter(files.into_iter().map(|path| {
        let rel_root = rel_root.clone();
        async move {
            let _permit = hash_semaphore().acquire().await.expect("hash semaphore closed");
            // Report the file about to be hashed. With concurrent files in flight
            // this reflects the most recently started one — a live "current file".
            let name = path
                .strip_prefix(&rel_root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            st.set_scan(|s| { s.current_file = name; });
            tokio::task::spawn_blocking(move || hash_file_blocking(&path, &rel_root))
                .await
                .map_err(|e| anyhow!("hash task panicked: {e}"))?
        }
    }))
    .buffer_unordered(concurrency)
    .try_collect()
    .await?;
    // buffer_unordered yields in completion order; sort by path so file_index is
    // deterministic across rescans.
    out.sort_by(|a, b| a.path.cmp(&b.path));

    // Append an optional Dolphin texture pack as a sentinel entry. It is hashed
    // here (during scan) so manifest requests stay instant, and promoted to the
    // manifest's `texture_pack` field rather than the installable file list.
    if let Some(tp) = texture_pack_path(cfg, game).await {
        let _permit = hash_semaphore().acquire().await.expect("hash semaphore closed");
        let rel_root = tp.parent().unwrap_or(&cfg.library_root).to_path_buf();
        let mut sf = tokio::task::spawn_blocking(move || hash_file_blocking(&tp, &rel_root))
            .await
            .map_err(|e| anyhow!("texture-pack hash task panicked: {e}"))??;
        sf.path = TEXTURE_PACK_SENTINEL.to_string();
        sf.chunks.clear(); // served as one ranged GET via /textures/<id>, not /chunks
        out.push(sf);
    }
    Ok(out)
}

// Number of files to hash concurrently. Capped so a giant single library scan
// can't oversubscribe the box; falls back to 4 if parallelism is unknown.
fn hash_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

// Process-wide cap on concurrent file hashes. Shared by the per-file fan-out in
// `compute_stored_files` and the per-game fan-out in `ensure_manifests` so the
// two nested layers never schedule more CPU-bound hashes than the box has cores.
fn hash_semaphore() -> &'static tokio::sync::Semaphore {
    static SEM: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SEM.get_or_init(|| tokio::sync::Semaphore::new(hash_concurrency()))
}

// Synchronous single-file hash: full-file sha256 plus per-chunk sha256 in one
// sequential read pass. Runs inside spawn_blocking so it never stalls the async
// runtime. Missing files surface as io::Error(NotFound), which callers map to a
// 404 "rescan" hint rather than a 500.
fn hash_file_blocking(path: &Path, rel_root: &Path) -> Result<StoredFile> {
    use std::io::Read;
    let rel = path
        .strip_prefix(rel_root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut file_hasher = Sha256::new();
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut offset = 0u64;
    let mut index = 0usize;
    while offset < size {
        let want = ((size - offset).min(CHUNK_SIZE as u64)) as usize;
        file.read_exact(&mut buf[..want])?;
        file_hasher.update(&buf[..want]);
        let mut ch = Sha256::new();
        ch.update(&buf[..want]);
        chunks.push(StoredChunk {
            index,
            offset,
            size: want as u64,
            sha256: hex::encode(ch.finalize()),
        });
        offset += want as u64;
        index += 1;
    }
    Ok(StoredFile {
        path: rel,
        size,
        sha256: hex::encode(file_hasher.finalize()),
        chunks,
    })
}

// Largest manifest segment written per row. Kept well under MariaDB's default
// `max_allowed_packet` (16 MB) so a single game — however large — never produces
// an oversized wire packet on insert or read.
const MANIFEST_SEGMENT_BYTES: usize = 4 * 1024 * 1024;

async fn load_stored_manifest(db: &Pool, game_id: &str, version: &str) -> Result<Option<Vec<StoredFile>>> {
    let mut c = db.get_conn().await?;
    let row: Option<(String, String)> = c
        .exec_first(
            "SELECT version, files_json FROM game_manifests WHERE game_id=:id",
            params! {"id" => game_id},
        )
        .await?;
    let Some((v, legacy_json)) = row else { return Ok(None); };
    if v != version {
        return Ok(None);
    }
    // Prefer segmented storage; fall back to the legacy inline column for rows
    // written before segmenting (they migrate to segments on next store).
    let segments: Vec<String> = c
        .exec(
            "SELECT body FROM game_manifest_segments WHERE game_id=:id ORDER BY seq",
            params! {"id" => game_id},
        )
        .await?;
    let json = if segments.is_empty() { legacy_json } else { segments.concat() };
    if json.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&json)?))
}

// Split a string into ordered ≤`max` byte pieces on UTF-8 char boundaries.
// Concatenating the result reproduces the input exactly.
fn split_segments(s: &str, max: usize) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0usize;
    let max = max.max(1);
    while start < s.len() {
        let mut end = (start + max).min(s.len());
        while end < s.len() && !s.is_char_boundary(end) {
            end -= 1;
        }
        // If `max` is smaller than the next char, backoff reaches `start`; take
        // the whole next char so we always make progress (can't happen at the
        // 4 MB production size, but keeps the helper total).
        if end == start {
            end = start + 1;
            while end < s.len() && !s.is_char_boundary(end) {
                end += 1;
            }
        }
        segments.push(&s[start..end]);
        start = end;
    }
    segments
}

async fn store_manifest(db: &Pool, game_id: &str, version: &str, files: &[StoredFile]) -> Result<()> {
    let json = serde_json::to_string(files)?;
    let segments = split_segments(&json, MANIFEST_SEGMENT_BYTES);

    let mut c = db.get_conn().await?;
    let mut tx = c.start_transaction(mysql_async::TxOpts::default()).await?;
    // Metadata row: keep version/updated_at; clear the legacy inline column so
    // segments are the single source of truth going forward.
    tx.exec_drop(
        r#"INSERT INTO game_manifests (game_id, version, files_json, updated_at)
           VALUES (:id, :version, '', :ts)
           ON DUPLICATE KEY UPDATE version=VALUES(version), files_json='', updated_at=VALUES(updated_at)"#,
        params! {"id" => game_id, "version" => version, "ts" => now()},
    )
    .await?;
    tx.exec_drop(
        "DELETE FROM game_manifest_segments WHERE game_id=:id",
        params! {"id" => game_id},
    )
    .await?;
    for (seq, body) in segments.iter().enumerate() {
        tx.exec_drop(
            "INSERT INTO game_manifest_segments (game_id, seq, body) VALUES (:id, :seq, :body)",
            params! {"id" => game_id, "seq" => seq as i32, "body" => *body},
        )
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

// ── Game changelogs ─────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Changelog {
    id: i64,
    version: String,
    title: String,
    body: String,
    created_at: i64,
}

async fn load_changelogs(db: &Pool, game_id: &str) -> Result<Vec<Changelog>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<(i64, String, String, String, i64)> = c
        .exec(
            "SELECT id, version, title, body, created_at FROM game_changelogs \
             WHERE game_id=:g ORDER BY created_at DESC, id DESC",
            params! {"g" => game_id},
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, version, title, body, created_at)| Changelog { id, version, title, body, created_at })
        .collect())
}

// Recent changelog entries joined with their game title, for the admin table.
async fn list_changelogs_admin(db: &Pool) -> Result<Vec<(i64, String, String, String)>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<(i64, String, String, String)> = c
        .query(
            "SELECT c.id, g.title, c.version, c.title \
             FROM game_changelogs c JOIN games g ON g.id = c.game_id \
             ORDER BY c.created_at DESC, c.id DESC LIMIT 100",
        )
        .await?;
    Ok(rows)
}

async fn add_changelog(db: &Pool, game_id: &str, version: &str, title: &str, body: &str) -> Result<()> {
    let mut c = db.get_conn().await?;
    // Guard the FK: reject notes for games that don't exist.
    let exists: Option<String> =
        c.exec_first("SELECT id FROM games WHERE id=:id", params! {"id" => game_id}).await?;
    if exists.is_none() {
        return Err(anyhow!("unknown game id"));
    }
    c.exec_drop(
        r#"INSERT INTO game_changelogs (game_id, version, title, body, created_at)
           VALUES (:g, :v, :t, :b, :ts)"#,
        params! {
            "g" => game_id,
            "v" => version,
            "t" => title,
            "b" => body,
            "ts" => now(),
        },
    )
    .await?;
    Ok(())
}

async fn delete_changelog(db: &Pool, id: u64) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.exec_drop("DELETE FROM game_changelogs WHERE id=:id", params! {"id" => id}).await?;
    Ok(())
}

async fn api_changelogs(State(st): State<AppState>, headers: HeaderMap, AxumPath(id): AxumPath<String>) -> Response {
    if !authorized_api(&st, &headers).await {
        return unauthorized();
    }
    match load_changelogs(&st.db, &id).await {
        Ok(entries) => Json(serde_json::json!({
            "gameId": id,
            "changelogs": entries,
        }))
        .into_response(),
        Err(e) => server_error(e),
    }
}

// Precompute (or refresh) stored manifests for every game whose version changed.
// Called from the scan/sync path so manifest requests are instant.
async fn ensure_manifests(st: &AppState, games: &[Game]) -> Result<()> {
    let mut platform_totals = BTreeMap::<String, PlatformProgress>::new();
    for game in games {
        platform_totals.entry(game.platform.clone()).or_default().total += 1;
    }
    st.set_scan(|s| {
        s.phase = "hashing".to_string();
        s.total = games.len();
        s.processed = 0;
        s.active = 0;
        s.current = String::new();
        s.current_file = String::new();
        s.message = "Generating file hashes…".to_string();
        s.per_platform = platform_totals.clone();
    });
    // Hash games concurrently. The shared `hash_semaphore` bounds total CPU work,
    // so single-file ISOs (one file each) still saturate every core because
    // multiple games hash at once, while multi-file repacks fan out internally.
    let game_concurrency = hash_concurrency();
    futures_util::stream::iter(games.iter())
        .for_each_concurrent(game_concurrency, |game| async move {
            // Only games whose stored manifest is missing/stale need hashing. We
            // report `current`/`active` for those alone, so the status reflects
            // real hashing work instead of flickering through instant cache hits.
            let needs_hash = !matches!(
                load_stored_manifest(&st.db, &game.id, &game.version).await,
                Ok(Some(_))
            );
            if needs_hash {
                st.set_scan(|s| {
                    s.active += 1;
                    s.current = game.title.clone();
                });
                match compute_stored_files(st, game).await {
                    Ok(files) => {
                        if let Err(e) = store_manifest(&st.db, &game.id, &game.version, &files).await {
                            error!("failed to persist manifest for {}: {e}", game.id);
                        }
                    }
                    Err(e) => error!("failed to hash files for {}: {e}", game.id),
                }
                st.set_scan(|s| { s.active = s.active.saturating_sub(1); });
            }
            let platform = game.platform.clone();
            st.set_scan(|s| {
                s.processed += 1;
                s.per_platform.entry(platform).or_default().processed += 1;
                if s.active == 0 {
                    s.current = String::new();
                    s.current_file = String::new();
                }
            });
        })
        .await;
    // Drop stale manifests for games no longer present.
    let ids: HashSet<&str> = games.iter().map(|g| g.id.as_str()).collect();
    let mut c = st.db.get_conn().await?;
    let existing: Vec<String> = c.query("SELECT game_id FROM game_manifests").await?;
    for id in existing {
        if !ids.contains(id.as_str()) {
            c.exec_drop("DELETE FROM game_manifests WHERE game_id=:id", params! {"id" => &id}).await?;
            c.exec_drop("DELETE FROM game_manifest_segments WHERE game_id=:id", params! {"id" => id}).await?;
        }
    }
    Ok(())
}

