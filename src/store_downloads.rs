// store_downloads.rs - self-hosted launcher installer listing.
// Assembled via include! at crate-root scope like the other store modules.
//
// The actual installer files live in `cfg.downloads_dir` (default
// <library_root>/.arcadelauncher/downloads, override ARCADE_DOWNLOADS_DIR) and
// are served statically via a ServeDir mounted at /downloads in main.rs. This
// module only builds the JSON manifest the Download SPA page consumes:
//   { version, files: [ { platform, arch, label, filename, url, size, primary } ] }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadFile {
    platform: String, // "windows" | "macos" | "linux"
    arch: String,     // "x64" | "arm64" | "universal" | ""
    label: String,    // human label, e.g. "Windows installer (.exe)"
    filename: String,
    url: String,
    size: u64,
    // The recommended file for this platform (one per platform); the SPA uses it
    // for the big OS-detected button.
    primary: bool,
}

// Classify one installer filename into (platform, arch, label, kind_rank).
// kind_rank orders candidates within a platform so the best one becomes primary
// (lower = preferred).
fn classify_download(name: &str) -> Option<(String, String, String, u32)> {
    let lower = name.to_ascii_lowercase();
    let arch = if lower.contains("arm64") || lower.contains("aarch64") {
        "arm64"
    } else if lower.contains("x64")
        || lower.contains("amd64")
        || lower.contains("x86_64")
    {
        "x64"
    } else {
        ""
    };
    let a = arch.to_string();
    if lower.ends_with("-setup.exe") || lower.ends_with("_setup.exe") {
        Some(("windows".into(), a, "Windows installer (.exe)".into(), 0))
    } else if lower.ends_with(".msi") {
        Some(("windows".into(), a, "Windows installer (.msi)".into(), 1))
    } else if lower.ends_with(".exe") {
        Some(("windows".into(), a, "Windows executable (.exe)".into(), 2))
    } else if lower.ends_with(".dmg") {
        let arch2 = if arch.is_empty() { "universal".into() } else { a };
        Some(("macos".into(), arch2, "macOS disk image (.dmg)".into(), 0))
    } else if lower.ends_with(".app.tar.gz") {
        let arch2 = if arch.is_empty() { "universal".into() } else { a };
        Some(("macos".into(), arch2, "macOS app bundle".into(), 1))
    } else if lower.ends_with(".appimage") {
        Some(("linux".into(), a, "Linux AppImage".into(), 0))
    } else if lower.ends_with(".deb") {
        Some(("linux".into(), a, "Linux package (.deb)".into(), 1))
    } else if lower.ends_with(".rpm") {
        Some(("linux".into(), a, "Linux package (.rpm)".into(), 2))
    } else {
        None
    }
}

// Pull a semver-ish version token out of a filename (e.g. "1.2.3" from
// "ArcadeLauncher_1.2.3_x64-setup.exe"). Returns None if none is present.
fn version_from_name(name: &str) -> Option<String> {
    let bytes = name.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut dots = 0;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit() || bytes[i] == b'.')
            {
                if bytes[i] == b'.' {
                    dots += 1;
                }
                i += 1;
            }
            let tok = &name[start..i];
            if dots >= 1 && !tok.ends_with('.') {
                return Some(tok.to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

// GET /api/downloads/latest — enumerate installer files and return the manifest.
async fn store_downloads_latest(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let dir = st.cfg.downloads_dir.clone();
    let base = public_base_url(&st, &headers).await;

    let mut read = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        // Missing directory is not an error — the release just hasn't been
        // populated yet. Return an empty manifest so the page renders cleanly.
        Err(_) => {
            return Json(serde_json::json!({
                "schemaVersion": 1,
                "version": "",
                "files": Vec::<DownloadFile>::new(),
            }))
            .into_response();
        }
    };

    let mut candidates: Vec<(DownloadFile, u32)> = Vec::new();
    let mut version: Option<String> = None;
    loop {
        let entry = match read.next_entry().await {
            Ok(Some(e)) => e,
            Ok(None) => break,
            Err(e) => return server_error(e),
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        if let Some((platform, arch, label, rank)) = classify_download(&name) {
            if version.is_none() {
                version = version_from_name(&name);
            }
            let url = format!("{}/downloads/{}", base, encode_path(&name));
            candidates.push((
                DownloadFile {
                    platform,
                    arch,
                    label,
                    filename: name,
                    url,
                    size: meta.len(),
                    primary: false,
                },
                rank,
            ));
        }
    }

    // Mark one primary per platform: the lowest kind_rank (ties broken by name).
    candidates.sort_by(|a, b| {
        a.0.platform
            .cmp(&b.0.platform)
            .then(a.1.cmp(&b.1))
            .then(a.0.filename.cmp(&b.0.filename))
    });
    let mut seen_platform: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (file, _rank) in &mut candidates {
        if seen_platform.insert(file.platform.clone()) {
            file.primary = true;
        }
    }

    let files: Vec<DownloadFile> = candidates.into_iter().map(|(f, _)| f).collect();
    Json(serde_json::json!({
        "schemaVersion": 1,
        "version": version.unwrap_or_default(),
        "files": files,
    }))
    .into_response()
}
