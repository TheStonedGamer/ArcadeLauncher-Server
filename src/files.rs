// files.rs - split out of main.rs and re-assembled via include! (crate-root scope).

async fn content_path_for(cfg: &Config, game: &Game) -> Result<PathBuf> {
    safe_join(&cfg.library_root, &game.content_path)
}

// Locate an optional Dolphin custom-texture pack for a GameCube/Wii game. The
// convention is a zip sitting next to the ROM, named `<rom-stem>.textures.zip`
// (e.g. `Metroid Prime.iso` -> `Metroid Prime.textures.zip`). Returns the path
// only when the file exists on disk.
async fn texture_pack_path(cfg: &Config, game: &Game) -> Option<PathBuf> {
    if game.platform != "GameCube" && game.platform != "Wii" {
        return None;
    }
    let content = content_path_for(cfg, game).await.ok()?;
    let dir = content.parent()?;
    let stem = content.file_stem()?.to_str()?;
    let candidate = dir.join(format!("{stem}.textures.zip"));
    if fs::metadata(&candidate).await.ok()?.is_file() {
        Some(candidate)
    } else {
        None
    }
}

async fn file_path_for(cfg: &Config, game: &Game, rel: &str) -> Result<PathBuf> {
    let root = content_path_for(cfg, game).await?;
    if fs::metadata(&root).await?.is_file() {
        let requested = Path::new(rel).file_name().and_then(|s| s.to_str()).unwrap_or("");
        let actual = root.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if requested != actual {
            return Err(anyhow!("invalid file path"));
        }
        Ok(root)
    } else {
        safe_join(&root, rel)
    }
}

async fn stream_file(path: PathBuf, range: Option<&HeaderValue>) -> Result<Response> {
    let meta = fs::metadata(&path).await?;
    if !meta.is_file() {
        return Err(anyhow!("file not found"));
    }
    let size = meta.len();
    let parsed = parse_range(range.and_then(|h| h.to_str().ok()), size)?;
    let (start, end, status) = if let Some((s, e)) = parsed {
        (s, e, StatusCode::PARTIAL_CONTENT)
    } else {
        (0, size.saturating_sub(1), StatusCode::OK)
    };
    // Empty files: `end` saturates to 0, so the naive `end - start + 1` would
    // claim a 1-byte body while `take(len)` streams 0 bytes. nginx then reports
    // "upstream prematurely closed connection" (502) and clients fail to decode
    // the truncated body. Serve a genuine 0-length body for empty files.
    let len = if size == 0 {
        0
    } else {
        end.saturating_sub(start).saturating_add(1)
    };
    let mut file = File::open(&path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let stream = ReaderStream::new(file.take(len)).map_ok(Bytes::from);
    let mut resp = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, mime_guess::from_path(&path).first_or_octet_stream().as_ref())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string());
    if status == StatusCode::PARTIAL_CONTENT {
        resp = resp.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    }
    Ok(resp.body(Body::from_stream(stream))?)
}

async fn stream_chunk(path: PathBuf, _file_index: usize, chunk_index: usize) -> Result<Response> {
    let meta = fs::metadata(&path).await?;
    if !meta.is_file() {
        return Err(anyhow!("file not found"));
    }
    let size = meta.len();
    let start = (chunk_index as u64).saturating_mul(CHUNK_SIZE as u64);
    if start >= size {
        return Err(anyhow!("chunk out of range"));
    }
    let len = ((CHUNK_SIZE as u64).min(size - start)) as u64;
    let mut file = File::open(&path).await?;
    file.seek(SeekFrom::Start(start)).await?;
    let stream = ReaderStream::new(file.take(len)).map_ok(Bytes::from);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, len.to_string())
        .body(Body::from_stream(stream))?)
}

#[allow(dead_code)]
async fn chunks_for_file(
    path: &Path,
    base: &str,
    game_id: &str,
    file_index: usize,
    rel: &str,
    size: u64,
) -> Result<Vec<ManifestChunk>> {
    let mut out = Vec::new();
    let mut file = File::open(path).await?;
    let mut offset = 0u64;
    let mut index = 0usize;
    let mut buf = vec![0u8; CHUNK_SIZE];
    while offset < size {
        let want = ((size - offset).min(CHUNK_SIZE as u64)) as usize;
        file.read_exact(&mut buf[..want]).await?;
        let mut hasher = Sha256::new();
        hasher.update(&buf[..want]);
        out.push(ManifestChunk {
            index,
            offset,
            size: want as u64,
            sha256: hex::encode(hasher.finalize()),
            compression: "none".into(),
            url: format!(
                "{}/chunks/{}/{}/{}/{}",
                base,
                urlencoding::encode(game_id),
                file_index,
                index,
                encode_path(rel)
            ),
        });
        offset += want as u64;
        index += 1;
    }
    Ok(out)
}

fn parse_range(header: Option<&str>, size: u64) -> Result<Option<(u64, u64)>> {
    let Some(header) = header else { return Ok(None); };
    let Some(spec) = header.strip_prefix("bytes=") else { return Err(anyhow!("unsupported range unit")); };
    let spec = spec.split(',').next().unwrap_or("").trim();
    let Some((start_s, end_s)) = spec.split_once('-') else { return Err(anyhow!("invalid range")); };
    if start_s.is_empty() {
        let suffix: u64 = end_s.parse()?;
        if suffix == 0 {
            return Err(anyhow!("invalid suffix range"));
        }
        return Ok(Some((size.saturating_sub(suffix), size.saturating_sub(1))));
    }
    let start: u64 = start_s.parse()?;
    let end = if end_s.is_empty() { size.saturating_sub(1) } else { end_s.parse()? };
    if start >= size || end < start {
        return Err(anyhow!("range not satisfiable"));
    }
    Ok(Some((start, end.min(size.saturating_sub(1)))))
}

async fn walk_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

// Resolve a relative path (e.g. "Nintendo/Gamecube") under `root` matching each
// component case-insensitively. The production server runs on a case-sensitive
// Linux filesystem, so a folder named "GameCube" on disk would not match a
// hard-coded "Gamecube" spec. Returns the real on-disk path if every component
// resolves, else None.
async fn resolve_subdir_ci(root: &Path, relative: &str) -> Option<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.split('/').filter(|c| !c.is_empty()) {
        // Fast path: exact match exists.
        let exact = current.join(component);
        if fs::metadata(&exact).await.is_ok() {
            current = exact;
            continue;
        }
        // Slow path: scan the directory for a case-insensitive match.
        let mut rd = match fs::read_dir(&current).await {
            Ok(rd) => rd,
            Err(_) => return None,
        };
        let mut matched: Option<PathBuf> = None;
        while let Ok(Some(entry)) = rd.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                if name.eq_ignore_ascii_case(component) {
                    matched = Some(entry.path());
                    break;
                }
            }
        }
        current = matched?;
    }
    Some(current)
}

async fn walk_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut rd = fs::read_dir(dir).await?;
        while let Some(entry) = rd.next_entry().await? {
            let path = entry.path();
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                out.push(path.clone());
                stack.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

#[allow(dead_code)]
async fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).await?;
    // File-lock / stability safety: snapshot size+mtime before hashing so we can
    // confirm the file was not being actively rewritten while we read it. The
    // open handle `f` is held for the whole hash (the "lock-safety closure"), so
    // the file can't be deleted out from under us; bracketing with metadata then
    // rejects a hash computed over a file that changed mid-read (e.g. an
    // in-progress copy onto the NAS). A rejected file simply has no stored
    // manifest this round and is retried on the next scan once it settles.
    let before = f.metadata().await?;
    let (before_len, before_mtime) = (before.len(), before.modified().ok());

    let mut h = Sha256::new();
    let mut buf = vec![0u8; CHUNK_SIZE];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }

    let after = f.metadata().await?;
    if after.len() != before_len || after.modified().ok() != before_mtime {
        return Err(anyhow!(
            "file changed while hashing (still being written?): {}",
            path.display()
        ));
    }
    Ok(hex::encode(h.finalize()))
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    let rel = urlencoding::decode(relative)?.replace('\\', "/");
    let rel_path = Path::new(&rel);
    if rel_path.is_absolute() {
        return Err(anyhow!("invalid path"));
    }
    let mut out = root.to_path_buf();
    for c in rel_path.components() {
        match c {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            _ => return Err(anyhow!("invalid path")),
        }
    }
    Ok(out)
}

