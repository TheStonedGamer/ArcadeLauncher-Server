// scan.rs - split out of main.rs and re-assembled via include! (crate-root scope).

async fn scan_catalog(library_root: &Path) -> Result<Vec<Game>> {
    let mut games = Vec::new();
    games.extend(scan_single_file_platforms(library_root).await?);
    games.extend(scan_xbox360_god(library_root).await?);
    games.extend(scan_xbox_original(library_root).await?);
    games.extend(scan_pc_games(library_root).await?);
    games.sort_by(|a, b| {
        (a.platform.as_str(), a.title.to_lowercase(), a.id.as_str())
            .cmp(&(b.platform.as_str(), b.title.to_lowercase(), b.id.as_str()))
    });
    Ok(games)
}

async fn scan_single_file_platforms(library_root: &Path) -> Result<Vec<Game>> {
    let specs: &[(&str, &str, &[&str])] = &[
        ("Nintendo/NES", "NES", &["nes", "fds", "unf", "unif"]),
        ("Nintendo/SNES", "SNES", &["sfc", "smc", "fig", "bs", "st", "zip", "7z"]),
        ("Nintendo/N64", "N64", &["z64", "n64", "v64", "rom"]),
        ("Nintendo/Switch", "Ryujinx", &["nsp", "xci", "nca", "nro"]),
        ("Nintendo/Gamecube", "GameCube", &["iso", "gcm", "rvz", "gcz"]),
        ("Nintendo/Wii", "Wii", &["iso", "rvz", "gcz", "wbfs", "dol", "elf"]),
    ];
    let skip: HashSet<&str> = ["sqlite", "db", "txt", "nfo", "jpg", "jpeg", "png", "webp"].into_iter().collect();
    let mut out = Vec::new();
    let games_root = library_root.join("games");
    for (relative_dir, platform, extensions) in specs {
        let Some(platform_root) = resolve_subdir_ci(&games_root, relative_dir).await else {
            continue;
        };
        let allowed: HashSet<&str> = extensions.iter().copied().collect();
        for path in walk_files(&platform_root).await? {
            let suffix = file_ext(&path);
            if suffix.is_empty() || skip.contains(suffix.as_str()) || !allowed.contains(suffix.as_str()) {
                continue;
            }
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
            out.push(game_entry(
                library_root,
                &path,
                platform,
                &clean_title(name),
                Path::new(name),
                "emulator_rom",
                "{rom}",
            ).await?);
        }
    }
    Ok(out)
}

async fn scan_xbox360_god(library_root: &Path) -> Result<Vec<Game>> {
    let xbox_root = library_root.join("games").join("Microsoft").join("Xbox 360");
    if fs::metadata(&xbox_root).await.is_err() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut seen_roots = HashSet::<PathBuf>::new();
    let god_dirs = ["00007000", "0007000"];
    for dir in walk_dirs(&xbox_root).await? {
        let Some(name) = dir.file_name().and_then(|s| s.to_str()) else { continue; };
        if !god_dirs.contains(&name) {
            continue;
        }
        let Some(package) = find_god_package(&dir).await? else { continue; };
        let relative_god_dir = dir.strip_prefix(&xbox_root).unwrap_or(&dir);
        let Some(first) = relative_god_dir.components().next() else { continue; };
        let game_root = xbox_root.join(first.as_os_str());
        if !seen_roots.insert(game_root.clone()) {
            continue;
        }
        let target = package.strip_prefix(&game_root).unwrap_or(&package).to_path_buf();
        let title = game_root.file_name().and_then(|s| s.to_str()).map(clean_title).unwrap_or_else(|| "Xbox 360 Game".into());
        out.push(game_entry(library_root, &game_root, "Xbox360", &title, &target, "emulator_rom", "{rom}").await?);
    }
    Ok(out)
}

// Scan original Xbox games under games/Microsoft/Xbox. Recognises three layouts:
//   * a top-level `.iso`/`.xiso` image (one game per file),
//   * a game folder containing `default.xbe` (an extracted game — the .xbe is the
//     canonical executable; the whole folder is the content),
//   * a game folder containing a single `.iso`/`.xiso` (a per-game ISO folder).
// All are tagged platform "Xbox" so they surface in the admin UI and catalog.
async fn scan_xbox_original(library_root: &Path) -> Result<Vec<Game>> {
    let games_root = library_root.join("games");
    let Some(xbox_root) = resolve_subdir_ci(&games_root, "Microsoft/Xbox").await else {
        return Ok(Vec::new());
    };
    let iso_exts: HashSet<&str> = ["iso", "xiso"].into_iter().collect();
    let mut out = Vec::new();
    let mut rd = fs::read_dir(&xbox_root).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_file() {
            if iso_exts.contains(file_ext(&path).as_str()) {
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
                out.push(game_entry(
                    library_root, &path, "Xbox", &clean_title(name),
                    Path::new(name), "emulator_rom", "-dvd_path {rom}",
                ).await?);
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let title = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(clean_title)
            .unwrap_or_else(|| "Xbox Game".into());
        let files = walk_files(&path).await?;
        // Prefer an extracted game keyed off default.xbe.
        if let Some(xbe) = files.iter().find(|f| {
            f.file_name().and_then(|s| s.to_str())
                .map(|n| n.eq_ignore_ascii_case("default.xbe")).unwrap_or(false)
        }) {
            let target = xbe.strip_prefix(&path).unwrap_or(xbe).to_path_buf();
            out.push(game_entry(library_root, &path, "Xbox", &title, &target, "emulator_rom", "{rom}").await?);
            continue;
        }
        // Otherwise fall back to a single ISO inside the folder.
        if let Some(iso) = files.iter().find(|f| iso_exts.contains(file_ext(f).as_str())) {
            let target = iso.strip_prefix(&path).unwrap_or(iso).to_path_buf();
            out.push(game_entry(library_root, &path, "Xbox", &title, &target, "emulator_rom", "-dvd_path {rom}").await?);
        }
    }
    Ok(out)
}

// True if `path` is the *primary* part of a repack archive that should become its
// own game. Recognises plain .zip/.7z/.rar and the first part of multi-part sets
// (.partN.rar where N==1, and split archives like name.7z.001 / name.rar.001).
// Crucially, a bare ".001" with no archive extension before it is NOT an archive —
// ".001" is a very common split-data extension inside installed game folders, and
// matching it blindly turned thousands of internal files into phantom games.
fn is_pc_primary_archive(path: &Path) -> bool {
    let name_l = match path.file_name().and_then(|s| s.to_str()) {
        Some(n) => n.to_ascii_lowercase(),
        None => return false,
    };
    let ext = file_ext(path);
    let is_archive_ext = matches!(ext.as_str(), "zip" | "7z" | "rar");
    // Split archive first part: name.<7z|zip|rar>.001
    let is_split_first = ext == "001"
        && (name_l.ends_with(".7z.001") || name_l.ends_with(".zip.001") || name_l.ends_with(".rar.001"));
    if !is_archive_ext && !is_split_first {
        return false;
    }
    // Skip continuation parts of multi-part RAR sets (.partN.rar, N>1).
    if let Some(idx) = name_l.find(".part") {
        let digits: String = name_l[idx + 5..].chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.parse::<u32>().map(|n| n > 1).unwrap_or(false) {
            return false;
        }
    }
    true
}

async fn scan_pc_games(library_root: &Path) -> Result<Vec<Game>> {
    let pc_root = library_root.join("games").join("PC");
    if fs::metadata(&pc_root).await.is_err() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut rd = fs::read_dir(&pc_root).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if meta.is_file() {
            // Loose repack archive sitting directly in games/PC.
            if is_pc_primary_archive(&path) {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    out.push(game_entry(library_root, &path, "PC", &clean_title(name), Path::new(""), "pc_archive", "{exe}").await?);
                }
            }
            continue;
        }
        if !meta.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
        if name.eq_ignore_ascii_case("steam") {
            let mut steam = fs::read_dir(&path).await?;
            while let Some(game_dir) = steam.next_entry().await? {
                let game_path = game_dir.path();
                if game_dir.metadata().await?.is_dir() {
                    if let Some(game) = pc_folder_entry(library_root, &game_path).await? {
                        out.push(game);
                    }
                }
            }
            continue;
        }
        // Recurse up to two levels so games nested under category folders aren't
        // missed — but stop the moment a folder is itself a game.
        collect_pc_folder_games(library_root, &path, 2, &mut out).await?;
    }
    Ok(out)
}

// Resolve `dir` as either ONE game (it has a launchable exe → the whole folder is
// the game; never descend into it) or a category folder (recurse into subfolders
// and pick up any loose repack archives sitting directly inside it).
async fn collect_pc_folder_games(library_root: &Path, dir: &Path, max_depth: usize, out: &mut Vec<Game>) -> Result<()> {
    // A folder with a launchable exe is a single game. Its internal archives and
    // subfolders are part of that game and must NOT be scanned as separate games.
    if let Some(game) = pc_folder_entry(library_root, dir).await? {
        out.push(game);
        return Ok(());
    }
    // Not a game itself — treat as a category. Collect loose archives here, then
    // recurse into subfolders.
    let mut subdirs = Vec::<PathBuf>::new();
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if meta.is_file() {
            if is_pc_primary_archive(&path) {
                if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
                    out.push(game_entry(library_root, &path, "PC", &clean_title(name), Path::new(""), "pc_archive", "{exe}").await?);
                }
            }
        } else if meta.is_dir() {
            subdirs.push(path);
        }
    }
    if max_depth == 0 {
        if !subdirs.is_empty() {
            info!("PC scan: reached depth limit, not descending into {} subfolder(s) of {}", subdirs.len(), dir.display());
        }
        return Ok(());
    }
    for sub in subdirs {
        Box::pin(collect_pc_folder_games(library_root, &sub, max_depth - 1, out)).await?;
    }
    Ok(())
}

async fn pc_folder_entry(library_root: &Path, game_root: &Path) -> Result<Option<Game>> {
    let Some(title) = game_root.file_name().and_then(|s| s.to_str()).map(clean_title) else {
        return Ok(None);
    };
    let target = find_pc_launch_target(game_root).await?;
    let game = game_entry(library_root, game_root, "PC", &title, &target, "pc_folder", "{exe}").await?;
    if game.launch.target.is_empty() {
        return Ok(None);
    }
    Ok(Some(game))
}

async fn find_pc_launch_target(game_root: &Path) -> Result<PathBuf> {
    let mut candidates = Vec::<PathBuf>::new();
    let mut fallback = Vec::<PathBuf>::new();
    for path in walk_files(game_root).await? {
        if file_ext(&path) != "exe" {
            continue;
        }
        let rel = path.strip_prefix(game_root).unwrap_or(&path).to_path_buf();
        let lower = rel.to_string_lossy().to_ascii_lowercase();
        fallback.push(rel.clone());
        if lower.contains("unins") || lower.contains("setup") || lower.contains("redist") ||
           lower.contains("_commonredist") || lower.contains("crashreport") {
            continue;
        }
        candidates.push(rel);
    }
    // Prefer a "real" game exe, but fall back to any exe so a folder is never
    // silently dropped just because every exe matched an installer heuristic.
    let pick = if candidates.is_empty() { &mut fallback } else { &mut candidates };
    pick.sort_by_key(|p| {
        let depth = p.components().count();
        let len = p.to_string_lossy().len();
        (depth, len)
    });
    Ok(pick.first().cloned().unwrap_or_default())
}

async fn find_god_package(god_dir: &Path) -> Result<Option<PathBuf>> {
    let mut rd = fs::read_dir(god_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        let meta = entry.metadata().await?;
        if !meta.is_file() || path.extension().is_some() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue; };
        if fs::metadata(god_dir.join(format!("{name}.data"))).await.map(|m| m.is_dir()).unwrap_or(false) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

async fn game_entry(
    library_root: &Path,
    content_path: &Path,
    platform: &str,
    title: &str,
    target: &Path,
    install_type: &str,
    arguments: &str,
) -> Result<Game> {
    let relative_content = content_path.strip_prefix(library_root).unwrap_or(content_path);
    let mut game = Game {
        id: stable_id(platform, relative_content),
        title: title.to_string(),
        platform: platform.to_string(),
        install_type: install_type.to_string(),
        version: version_for(content_path).await?,
        content_path: relative_content.to_string_lossy().replace('\\', "/"),
        cover_art_url: String::new(),
        igdb_id: 0,
        summary: String::new(),
        genres: String::new(),
        igdb_rating: 0.0,
        release_date: 0,
        launch: Launch {
            target: target.to_string_lossy().replace('\\', "/"),
            arguments: arguments.to_string(),
        },
    };
    apply_sidecar_metadata(content_path, &mut game).await;
    Ok(game)
}

async fn sync_catalog_db(db: &Pool, games: &[Game]) -> Result<()> {
    let mut c = db.get_conn().await?;
    let ts = now();
    for game in games {
        c.exec_drop(
            r#"INSERT INTO games
              (id,title,platform,install_type,version,content_path,launch_target,launch_arguments,cover_art_url,igdb_id,summary,genres,igdb_rating,release_date,updated_at)
              VALUES (:id,:title,:platform,:install_type,:version,:content_path,:launch_target,:launch_arguments,:cover_art_url,:igdb_id,:summary,:genres,:igdb_rating,:release_date,:updated_at)
              ON DUPLICATE KEY UPDATE
                title=VALUES(title),
                platform=VALUES(platform),
                install_type=VALUES(install_type),
                version=VALUES(version),
                content_path=VALUES(content_path),
                launch_target=VALUES(launch_target),
                launch_arguments=VALUES(launch_arguments),
                cover_art_url=IF(VALUES(cover_art_url)='',cover_art_url,VALUES(cover_art_url)),
                igdb_id=IF(VALUES(igdb_id)=0,igdb_id,VALUES(igdb_id)),
                summary=IF(VALUES(summary)='',summary,VALUES(summary)),
                genres=IF(VALUES(genres)='',genres,VALUES(genres)),
                igdb_rating=IF(VALUES(igdb_rating)=0,igdb_rating,VALUES(igdb_rating)),
                release_date=IF(VALUES(release_date)=0,release_date,VALUES(release_date)),
                updated_at=VALUES(updated_at)"#,
            params! {
                "id" => &game.id,
                "title" => &game.title,
                "platform" => &game.platform,
                "install_type" => &game.install_type,
                "version" => &game.version,
                "content_path" => &game.content_path,
                "launch_target" => &game.launch.target,
                "launch_arguments" => &game.launch.arguments,
                "cover_art_url" => &game.cover_art_url,
                "igdb_id" => game.igdb_id,
                "summary" => &game.summary,
                "genres" => &game.genres,
                "igdb_rating" => game.igdb_rating,
                "release_date" => game.release_date,
                "updated_at" => ts,
            },
        )
        .await?;
    }
    let ids: HashSet<&str> = games.iter().map(|g| g.id.as_str()).collect();
    let existing: Vec<String> = c.query("SELECT id FROM games").await?;
    for id in existing {
        if !ids.contains(id.as_str()) {
            c.exec_drop("DELETE FROM games WHERE id=:id", params! {"id" => id}).await?;
        }
    }
    Ok(())
}

async fn version_for(path: &Path) -> Result<String> {
    let meta = fs::metadata(path).await?;
    if meta.is_file() {
        return Ok(sha1_short(&format!(
            "{}:{}",
            path.file_name().and_then(|s| s.to_str()).unwrap_or(""),
            stable_sig(&meta)
        )));
    }
    let mut h = Sha1::new();
    for file_path in walk_files(path).await? {
        let meta = fs::metadata(&file_path).await?;
        let rel = file_path.strip_prefix(path).unwrap_or(&file_path).to_string_lossy().replace('\\', "/");
        h.update(format!("{}:{}\n", rel, stable_sig(&meta)).as_bytes());
    }
    Ok(hex::encode(h.finalize())[..12].to_string())
}

// Stable per-file fingerprint component for change detection. Uses size plus a
// coarse (whole-second) mtime, but — crucially — omits mtime entirely when the
// platform can't report it instead of substituting 0. The old code used
// `mtime.unwrap_or(0)`, so a filesystem that intermittently failed to return
// mtime (common on NAS/NFS mounts) would oscillate the fingerprint between a
// real value and 0 on alternating scans, flagging the game as perpetually
// modified and forcing an endless re-hash loop.
fn stable_sig(meta: &std::fs::Metadata) -> String {
    match meta.modified().ok().and_then(|m| m.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => format!("{}:{}", meta.len(), d.as_secs()),
        None => format!("{}:-", meta.len()),
    }
}

fn stable_id(platform: &str, relative: &Path) -> String {
    format!("{}-{}", platform.to_lowercase(), sha1_short(&relative.to_string_lossy().replace('\\', "/")))
}

fn sha1_short(text: &str) -> String {
    let mut h = Sha1::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())[..12].to_string()
}

fn file_ext(path: &Path) -> String {
    path.extension().and_then(|s| s.to_str()).unwrap_or("").to_ascii_lowercase()
}

fn clean_title(name: &str) -> String {
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let mut out = String::new();
    let mut skip_square = 0u32;
    let mut skip_paren = 0u32;
    for ch in stem.chars() {
        match ch {
            '[' => skip_square += 1,
            ']' if skip_square > 0 => skip_square -= 1,
            '(' => skip_paren += 1,
            ')' if skip_paren > 0 => skip_paren -= 1,
            '_' if skip_square == 0 && skip_paren == 0 => out.push(' '),
            _ if skip_square == 0 && skip_paren == 0 => out.push(ch),
            _ => {}
        }
    }
    let collapsed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim_matches(|c: char| c == ' ' || c == '.' || c == '-' || c == '_').to_string();
    if trimmed.is_empty() { stem.to_string() } else { trimmed }
}

struct GameValidationReport {
    total: usize,
    ok: usize,
    missing: Vec<String>,
    empty: Vec<String>,
    errors: Vec<String>,
    bytes: u64,
}

impl GameValidationReport {
    fn to_message(&self) -> String {
        let mut msg = format!(
            "Game validation complete.\n{} checked, {} OK, {} missing, {} empty, {} errors, {} total bytes.",
            self.total,
            self.ok,
            self.missing.len(),
            self.empty.len(),
            self.errors.len(),
            self.bytes
        );
        for (label, rows) in [("Missing", &self.missing), ("Empty", &self.empty), ("Errors", &self.errors)] {
            if !rows.is_empty() {
                msg.push_str(&format!("\n\n{label}:"));
                for row in rows.iter().take(50) {
                    msg.push_str("\n- ");
                    msg.push_str(row);
                }
                if rows.len() > 50 {
                    msg.push_str(&format!("\n- ... {} more", rows.len() - 50));
                }
            }
        }
        msg
    }
}

async fn validate_games(st: &AppState) -> Result<GameValidationReport> {
    let games = list_games(&st.db).await?;
    let mut report = GameValidationReport {
        total: games.len(),
        ok: 0,
        missing: Vec::new(),
        empty: Vec::new(),
        errors: Vec::new(),
        bytes: 0,
    };
    for game in games {
        let path = match content_path_for(&st.cfg, &game).await {
            Ok(p) => p,
            Err(e) => {
                report.errors.push(format!("{}: {}", game.title, e));
                continue;
            }
        };
        let meta = match fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                report.missing.push(format!("{} ({}) -> {}", game.title, game.platform, path.display()));
                continue;
            }
        };
        if meta.is_file() {
            if meta.len() == 0 {
                report.empty.push(format!("{} ({}) -> {}", game.title, game.platform, path.display()));
            } else {
                report.ok += 1;
                report.bytes += meta.len();
            }
            continue;
        }
        if meta.is_dir() {
            match dir_file_stats(&path).await {
                Ok((count, bytes)) if count > 0 && bytes > 0 => {
                    report.ok += 1;
                    report.bytes += bytes;
                }
                Ok(_) => report.empty.push(format!("{} ({}) -> {}", game.title, game.platform, path.display())),
                Err(e) => report.errors.push(format!("{}: {}", game.title, e)),
            }
        }
    }
    Ok(report)
}

async fn dir_file_stats(root: &Path) -> Result<(usize, u64)> {
    let mut count = 0usize;
    let mut bytes = 0u64;
    for path in walk_files(root).await? {
        let meta = fs::metadata(path).await?;
        count += 1;
        bytes += meta.len();
    }
    Ok((count, bytes))
}

