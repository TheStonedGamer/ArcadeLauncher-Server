// db_setup.rs - split out of main.rs and re-assembled via include! (crate-root scope).

async fn ensure_database(cfg: &Config) -> Result<()> {
    validate_db_identifier(&cfg.db_name)?;
    let pool = Pool::new(cfg.database_url(false).as_str());
    let mut conn = pool.get_conn().await?;
    conn.query_drop(format!(
        "CREATE DATABASE IF NOT EXISTS `{}` CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
        cfg.db_name.replace('`', "``")
    ))
    .await?;
    drop(conn);
    pool.disconnect().await?;
    Ok(())
}

async fn ensure_schema(db: &Pool) -> Result<()> {
    let mut c = db.get_conn().await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS admin_users (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          username VARCHAR(80) NOT NULL UNIQUE,
          email VARCHAR(255) NOT NULL UNIQUE,
          password_hash VARCHAR(255) NOT NULL,
          is_admin BOOLEAN NOT NULL DEFAULT TRUE,
          enabled BOOLEAN NOT NULL DEFAULT TRUE,
          created_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT TRUE").await;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN totp_secret VARCHAR(64) NULL").await;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN totp_enabled BOOLEAN NOT NULL DEFAULT FALSE").await;
    // Password-derived shared key for client challenge-response auth (hex SHA-256).
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN auth_key CHAR(64) NULL").await;
    // Admin can force a user to set a new password on next client login.
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN must_change_password BOOLEAN NOT NULL DEFAULT FALSE").await;
    // Per-user profile picture (server-synced avatar). Bytes + mime stored
    // inline; avatar_updated is a version stamp the client polls to know when to
    // refetch. Capped client-side to a small re-encoded image.
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN avatar MEDIUMBLOB NULL").await;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN avatar_mime VARCHAR(64) NULL").await;
    let _ = c.query_drop("ALTER TABLE admin_users ADD COLUMN avatar_updated BIGINT NOT NULL DEFAULT 0").await;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS launcher_tokens (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          name VARCHAR(160) NOT NULL,
          user_id BIGINT UNSIGNED NULL,
          token_hash CHAR(64) NOT NULL UNIQUE,
          token_plain TEXT NULL,
          enabled BOOLEAN NOT NULL DEFAULT TRUE,
          created_at BIGINT NOT NULL,
          INDEX (user_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    let _ = c.query_drop("ALTER TABLE launcher_tokens ADD COLUMN user_id BIGINT UNSIGNED NULL").await;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS admin_sessions (
          token_hash CHAR(64) NOT NULL PRIMARY KEY,
          admin_id BIGINT UNSIGNED NOT NULL,
          expires_at BIGINT NOT NULL,
          created_at BIGINT NOT NULL,
          INDEX (admin_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS password_resets (
          token_hash CHAR(64) NOT NULL PRIMARY KEY,
          admin_id BIGINT UNSIGNED NOT NULL,
          expires_at BIGINT NOT NULL,
          created_at BIGINT NOT NULL,
          INDEX (admin_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS games (
          id VARCHAR(96) NOT NULL PRIMARY KEY,
          title VARCHAR(512) NOT NULL,
          platform VARCHAR(80) NOT NULL,
          install_type VARCHAR(80) NOT NULL,
          version VARCHAR(80) NOT NULL,
          content_path TEXT NOT NULL,
          launch_target TEXT NOT NULL,
          launch_arguments TEXT NOT NULL,
          cover_art_url TEXT NULL,
          igdb_id BIGINT NOT NULL DEFAULT 0,
          summary TEXT NULL,
          genres TEXT NULL,
          igdb_rating DOUBLE NOT NULL DEFAULT 0,
          release_date BIGINT NOT NULL DEFAULT 0,
          updated_at BIGINT NOT NULL,
          INDEX idx_games_platform_title (platform, title)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN summary TEXT NULL").await;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN genres TEXT NULL").await;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN igdb_rating DOUBLE NOT NULL DEFAULT 0").await;
    let _ = c.query_drop("ALTER TABLE games ADD COLUMN release_date BIGINT NOT NULL DEFAULT 0").await;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS server_settings (
          setting_key VARCHAR(120) NOT NULL PRIMARY KEY,
          setting_value TEXT NOT NULL,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS game_manifests (
          game_id VARCHAR(96) NOT NULL PRIMARY KEY,
          version VARCHAR(80) NOT NULL,
          files_json LONGTEXT NOT NULL,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Manifest payloads for large games (e.g. 145 GB repacks at 1 MiB chunks)
    // can exceed MariaDB's `max_allowed_packet`, so files_json is stored split
    // into ordered ≤4 MB segments here and reassembled on read. The `files_json`
    // column above is retained for legacy rows and read as a fallback when a
    // game has no segments yet (smooth migration — no forced re-hash).
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS game_manifest_segments (
          game_id VARCHAR(96) NOT NULL,
          seq INT NOT NULL,
          body LONGTEXT NOT NULL,
          PRIMARY KEY (game_id, seq)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Per-game changelog / patch-notes entries shown in the client's game
    // dashboard. Linked to games.id; rows are removed when the game row is
    // deleted (ON DELETE CASCADE). `version` is the optional version label the
    // note applies to; `body` is markdown/plain text authored in the admin UI.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS game_changelogs (
          id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
          game_id VARCHAR(96) NOT NULL,
          version VARCHAR(120) NOT NULL DEFAULT '',
          title VARCHAR(255) NOT NULL DEFAULT '',
          body MEDIUMTEXT NOT NULL,
          created_at BIGINT NOT NULL,
          INDEX idx_changelogs_game (game_id, created_at),
          CONSTRAINT fk_changelogs_game FOREIGN KEY (game_id)
            REFERENCES games(id) ON DELETE CASCADE
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    Ok(())
}

async fn ensure_bootstrap_admin(db: &Pool, cfg: &Config) -> Result<()> {
    if cfg.admin_username.is_empty() || cfg.admin_email.is_empty() || cfg.admin_password.is_empty() {
        return Ok(());
    }
    let mut c = db.get_conn().await?;
    let count: Option<u64> = c.query_first("SELECT COUNT(*) FROM admin_users").await?;
    if count.unwrap_or(0) == 0 {
        let hash = hash_password_argon2(&cfg.admin_password)?;
        let auth_key = derive_auth_key(&cfg.admin_username, &cfg.admin_password);
        c.exec_drop(
            "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at,auth_key) VALUES (:u,:e,:p,TRUE,TRUE,:t,:k)",
            params! {"u" => &cfg.admin_username, "e" => &cfg.admin_email, "p" => hash, "t" => now(), "k" => auth_key},
        )
        .await?;
    }
    Ok(())
}

