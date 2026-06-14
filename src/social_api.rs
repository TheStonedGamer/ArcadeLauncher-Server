// social_api.rs - Steam-like social subsystem (crate-root scope, included from main.rs).
//
// Provides:
//   * ensure_social_schema  - friendships / blocks / messages / presence tables
//   * REST control plane     - friends, requests, message history, presence snapshot
//   * /ws/social gateway     - persistent WebSocket: presence diffs, chat delivery,
//                              typing indicators, voice-signaling relay, heartbeats
//
// Real-time fan-out uses a process-global connection hub (OnceLock) so we don't
// have to thread new fields through AppState's constructor. Each authenticated
// socket registers an unbounded mpsc sender keyed by user_id; server push is a
// non-blocking try_send to every live connection for the target user. Durable
// state (relationships, messages, last-known presence) lives in MariaDB so the
// REST plane and reconnects stay correct even with zero sockets connected.

// ----------------------------------------------------------------------------
// Schema
// ----------------------------------------------------------------------------

async fn ensure_social_schema(c: &mut mysql_async::Conn) -> Result<()> {
    // Friendship is stored once per pair with a canonical ordering (user_lo <
    // user_hi) so there is exactly one row regardless of who initiated. status
    // is 'pending' | 'accepted'; requested_by records the direction so we can
    // render "incoming" vs "outgoing" requests. Blocks are directional and
    // tracked separately so a block can coexist independent of friendship.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_friendships (
          user_lo BIGINT UNSIGNED NOT NULL,
          user_hi BIGINT UNSIGNED NOT NULL,
          status VARCHAR(16) NOT NULL DEFAULT 'pending',
          requested_by BIGINT UNSIGNED NOT NULL,
          created_at BIGINT NOT NULL,
          updated_at BIGINT NOT NULL,
          PRIMARY KEY (user_lo, user_hi),
          INDEX idx_friend_hi (user_hi)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_blocks (
          blocker_id BIGINT UNSIGNED NOT NULL,
          blocked_id BIGINT UNSIGNED NOT NULL,
          created_at BIGINT NOT NULL,
          PRIMARY KEY (blocker_id, blocked_id),
          INDEX idx_block_blocked (blocked_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_messages (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          sender_id BIGINT UNSIGNED NOT NULL,
          receiver_id BIGINT UNSIGNED NOT NULL,
          body MEDIUMTEXT NOT NULL,
          created_at BIGINT NOT NULL,
          read_at BIGINT NULL,
          INDEX idx_msg_pair (sender_id, receiver_id, id),
          INDEX idx_msg_inbox (receiver_id, id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Last-known presence, one row per user. state: offline|online|away|busy|
    // invisible|ingame. game_* describe rich presence while InGame. updated_at
    // doubles as a heartbeat stamp so a stale row can be treated as offline.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_presence (
          user_id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
          state VARCHAR(16) NOT NULL DEFAULT 'offline',
          game_id VARCHAR(128) NULL,
          game_title VARCHAR(512) NULL,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Durable notification feed, one row per recipient per event. Unlike the
    // ephemeral hub push, these survive a restart and are redelivered to the
    // client when it (re)connects, so events that happen while a user is offline
    // are not lost. kind: friend_request|friend_accepted|friend_removed|message|
    // voice_invite|system. actor_* identify who caused it; payload is an optional
    // serialized-JSON blob for deep-linking. seen_at/read_at track badge + read
    // state (seen = surfaced as a toast; read = explicitly acknowledged).
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_notifications (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          user_id BIGINT UNSIGNED NOT NULL,
          kind VARCHAR(32) NOT NULL,
          actor_id BIGINT UNSIGNED NULL,
          actor_name VARCHAR(80) NULL,
          body TEXT NULL,
          payload TEXT NULL,
          seen_at BIGINT NULL,
          read_at BIGINT NULL,
          created_at BIGINT NOT NULL,
          INDEX idx_notif_user (user_id, id),
          INDEX idx_notif_unread (user_id, read_at)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Server-synced social preferences (favorites, nicknames, notif toggles) so
    // personalization follows the user across devices instead of living only in
    // the client's social_prefs.json. One opaque JSON blob per user (the client
    // owns the shape); last write wins.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_user_prefs (
          user_id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
          prefs MEDIUMTEXT NOT NULL,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Per-user friend-request privacy (ROADMAP 1.1). friend_policy controls who
    // may send an incoming friend request: 'everyone' (default) | 'mutual' (only
    // people who share at least one accepted friend) | 'nobody'. One row per user;
    // absence means the default 'everyone'.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_friend_settings (
          user_id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
          friend_policy VARCHAR(16) NOT NULL DEFAULT 'everyone',
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Pending friend requests carry an expiry stamp so stale invites self-clean
    // (ROADMAP 1.1). Best-effort ADD COLUMN for pre-existing installs; NULL means
    // "never expires" (legacy rows / accepted friendships).
    let _ = c
        .query_drop("ALTER TABLE social_friendships ADD COLUMN expires_at BIGINT NULL")
        .await;
    // DM edit/delete history (ROADMAP 1.2). Best-effort ADD COLUMN for existing
    // installs; NULL means "never edited" / "not deleted".
    let _ = c
        .query_drop("ALTER TABLE social_messages ADD COLUMN edited_at BIGINT NULL")
        .await;
    let _ = c
        .query_drop("ALTER TABLE social_messages ADD COLUMN deleted_at BIGINT NULL")
        .await;
    // DM privacy (ROADMAP 1.1b). dm_policy controls who may DM this user:
    // 'everyone' (default) | 'friends' (accepted friends only) | 'nobody'.
    // Best-effort ADD COLUMN for pre-existing installs.
    let _ = c
        .query_drop("ALTER TABLE social_friend_settings ADD COLUMN dm_policy VARCHAR(16) NOT NULL DEFAULT 'everyone'")
        .await;
    // Persistent per-sender ignore (ROADMAP 1.1b). An ignore survives re-requests:
    // a row (ignorer_id, ignored_id) means ignorer never receives requests/DMs from
    // ignored. Distinct from blocks (which are mutual-visible and reject with 403).
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_ignores (
          ignorer_id BIGINT UNSIGNED NOT NULL,
          ignored_id BIGINT UNSIGNED NOT NULL,
          created_at BIGINT NOT NULL,
          PRIMARY KEY (ignorer_id, ignored_id),
          INDEX idx_ign_ed (ignored_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Message reactions (ROADMAP 1.2b). One row per (message, user, emoji).
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_message_reactions (
          message_id BIGINT UNSIGNED NOT NULL,
          user_id BIGINT UNSIGNED NOT NULL,
          emoji VARCHAR(32) NOT NULL,
          created_at BIGINT NOT NULL,
          PRIMARY KEY (message_id, user_id, emoji),
          INDEX idx_react_msg (message_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Replies + offline-send idempotency (ROADMAP 1.2b). Best-effort ADD COLUMN.
    let _ = c
        .query_drop("ALTER TABLE social_messages ADD COLUMN reply_to BIGINT NULL")
        .await;
    let _ = c
        .query_drop("ALTER TABLE social_messages ADD COLUMN client_nonce VARCHAR(40) NULL")
        .await;
    // User profiles (ROADMAP 1.4).
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_profiles (
          user_id BIGINT UNSIGNED NOT NULL PRIMARY KEY,
          banner VARCHAR(512) NULL,
          bio VARCHAR(1024) NULL,
          xp BIGINT NOT NULL DEFAULT 0,
          updated_at BIGINT NOT NULL
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Friend organization metadata (ROADMAP 1.5): per-owner-per-friend note,
    // comma-separated group labels, and a pin flag.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_friend_meta (
          owner_id BIGINT UNSIGNED NOT NULL,
          friend_id BIGINT UNSIGNED NOT NULL,
          note VARCHAR(512) NULL,
          groups VARCHAR(255) NULL,
          pinned TINYINT NOT NULL DEFAULT 0,
          PRIMARY KEY (owner_id, friend_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Presence depth (ROADMAP 1.6): a free-form custom status string.
    let _ = c
        .query_drop("ALTER TABLE social_presence ADD COLUMN status_text VARCHAR(128) NULL")
        .await;
    // DM attachments (ROADMAP 1.3): one row per uploaded object. object_key is the
    // MinIO key; message_id links it to a DM once sent (NULL while pending upload).
    // Bytes live in MinIO, not here — we only track metadata + access control.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS social_attachments (
          id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
          owner_id BIGINT UNSIGNED NOT NULL,
          message_id BIGINT UNSIGNED NULL,
          object_key VARCHAR(512) NOT NULL,
          filename VARCHAR(255) NOT NULL,
          content_type VARCHAR(128) NULL,
          size BIGINT NOT NULL DEFAULT 0,
          created_at BIGINT NOT NULL,
          INDEX idx_att_msg (message_id),
          INDEX idx_att_owner (owner_id, id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    // Library tracking (ROADMAP 2.4): per-account, per-game playtime / last-played /
    // completion / rating. game_id is the client's stable catalog id (string). This
    // is account-scoped state synced across devices, so it lives with the other
    // per-account social tables.
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS game_stats (
          user_id BIGINT UNSIGNED NOT NULL,
          game_id VARCHAR(80) NOT NULL,
          playtime_seconds BIGINT NOT NULL DEFAULT 0,
          last_played BIGINT NOT NULL DEFAULT 0,
          play_count BIGINT NOT NULL DEFAULT 0,
          completion TINYINT NOT NULL DEFAULT 0,
          rating TINYINT NOT NULL DEFAULT 0,
          updated_at BIGINT NOT NULL,
          PRIMARY KEY (user_id, game_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    Ok(())
}

// ROADMAP 1.3: max attachment size accepted for a presigned upload (25 MiB) and
// how long the presigned PUT/GET URLs stay valid.
const ATTACHMENT_MAX_BYTES: i64 = 25 * 1024 * 1024;
const ATTACHMENT_URL_TTL_SECS: u64 = 600;

// True if `ignorer` has persistently ignored `ignored` (ROADMAP 1.1b).
async fn has_ignored(db: &Pool, ignorer: u64, ignored: u64) -> bool {
    let Ok(mut c) = db.get_conn().await else { return false; };
    let n: Option<u64> = c
        .exec_first(
            "SELECT COUNT(*) FROM social_ignores WHERE ignorer_id=:a AND ignored_id=:b",
            params! {"a" => ignorer, "b" => ignored},
        )
        .await
        .ok()
        .flatten();
    n.unwrap_or(0) > 0
}

// Keep only the basename and a safe character set; used for the object key and
// the stored display name so a crafted filename can't escape the key prefix.
fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .take(120)
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    cleaned.trim_matches('.').to_string()
}

#[derive(Deserialize)]
struct PresignBody {
    filename: String,
    #[serde(default, rename = "contentType")]
    content_type: String,
    #[serde(default)]
    size: i64,
}

// POST /api/social/attachments/presign — register a pending attachment and return
// a short-lived presigned PUT URL the client uploads the bytes to directly. 503
// when no object store is configured (feature dormant).
async fn api_social_attachment_presign(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PresignBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let Some(s3) = st.cfg.s3.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Attachments not configured").into_response();
    };
    if body.size <= 0 || body.size > ATTACHMENT_MAX_BYTES {
        return (StatusCode::BAD_REQUEST, "Invalid or too-large attachment size").into_response();
    }
    let safe = sanitize_filename(&body.filename);
    if safe.is_empty() {
        return (StatusCode::BAD_REQUEST, "Invalid filename").into_response();
    }
    let ts = now();
    let rnd: u64 = rand::random();
    let object_key = format!("dm/{}/{:x}-{:x}/{}", me.id, ts, rnd, safe);
    let ct: Option<String> = if body.content_type.is_empty() {
        None
    } else {
        Some(body.content_type.chars().take(128).collect())
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let ins = c
        .exec_iter(
            "INSERT INTO social_attachments (owner_id, object_key, filename, content_type, size, created_at) VALUES (:o,:k,:f,:ct,:s,:t)",
            params! {"o" => me.id, "k" => &object_key, "f" => &safe, "ct" => &ct, "s" => body.size, "t" => ts},
        )
        .await;
    let id = match ins {
        Ok(r) => r.last_insert_id().unwrap_or(0),
        Err(e) => return server_error(e),
    };
    let url = s3_presign(s3, "PUT", &object_key, ATTACHMENT_URL_TTL_SECS);
    Json(serde_json::json!({
        "attachmentId": id,
        "objectKey": object_key,
        "uploadUrl": url,
        "expiresIn": ATTACHMENT_URL_TTL_SECS,
    }))
    .into_response()
}

// GET /api/social/attachments/:id — presigned download URL, gated to the owner or
// (once the attachment is linked to a DM) the message's two participants.
async fn api_social_attachment_get(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let Some(s3) = st.cfg.s3.as_ref() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Attachments not configured").into_response();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(u64, Option<u64>, String, String, Option<String>, i64)> = c
        .exec_first(
            "SELECT owner_id, message_id, object_key, filename, content_type, size FROM social_attachments WHERE id=:id",
            params! {"id" => id},
        )
        .await
        .ok()
        .flatten();
    let Some((owner, msg_id, key, filename, ct, size)) = row else {
        return (StatusCode::NOT_FOUND, "No such attachment").into_response();
    };
    let allowed = owner == me.id
        || match msg_id {
            Some(mid) => {
                let p: Option<(u64, u64)> = c
                    .exec_first(
                        "SELECT sender_id, receiver_id FROM social_messages WHERE id=:m",
                        params! {"m" => mid},
                    )
                    .await
                    .ok()
                    .flatten();
                matches!(p, Some((s, r)) if s == me.id || r == me.id)
            }
            None => false,
        };
    if !allowed {
        return (StatusCode::FORBIDDEN, "Not your attachment").into_response();
    }
    let url = s3_presign(s3, "GET", &key, ATTACHMENT_URL_TTL_SECS);
    Json(serde_json::json!({
        "attachmentId": id,
        "downloadUrl": url,
        "filename": filename,
        "contentType": ct,
        "size": size,
        "expiresIn": ATTACHMENT_URL_TTL_SECS,
    }))
    .into_response()
}

// Link a freshly-uploaded attachment to the DM that carries it. Best-effort:
// only the owner's own still-unlinked attachment is claimed. Returns true on link.
async fn link_attachment(st: &AppState, owner: u64, attachment_id: u64, message_id: u64) -> bool {
    if attachment_id == 0 || message_id == 0 {
        return false;
    }
    let Ok(mut c) = st.db.get_conn().await else { return false; };
    let r = c
        .exec_iter(
            "UPDATE social_attachments SET message_id=:m WHERE id=:a AND owner_id=:o AND message_id IS NULL",
            params! {"m" => message_id, "a" => attachment_id, "o" => owner},
        )
        .await;
    matches!(r, Ok(res) if res.affected_rows() > 0)
}

// The caller's DM privacy policy (defaults to 'everyone').
async fn dm_policy_of(db: &Pool, user_id: u64) -> String {
    let Ok(mut c) = db.get_conn().await else { return "everyone".into(); };
    c.exec_first(
        "SELECT dm_policy FROM social_friend_settings WHERE user_id=:id",
        params! {"id" => user_id},
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "everyone".to_string())
}

// ROADMAP 1.1 tunables. A pending friend request lives 30 days before it is
// swept. A single user may have at most FRIEND_REQUEST_MAX_OUTGOING requests
// they initiated outstanding (pending) at once, which doubles as spam control.
const FRIEND_REQUEST_TTL_SECS: i64 = 30 * 24 * 60 * 60;
const FRIEND_REQUEST_MAX_OUTGOING: u64 = 50;

// Delete any pending friendship rows whose expiry has passed. Best-effort; called
// opportunistically from the request/list paths so expired invites disappear
// without a dedicated sweeper task.
async fn sweep_expired_requests(db: &Pool) {
    let Ok(mut c) = db.get_conn().await else { return; };
    let _ = c
        .exec_drop(
            "DELETE FROM social_friendships WHERE status='pending' AND expires_at IS NOT NULL AND expires_at < :t",
            params! {"t" => now()},
        )
        .await;
}

// The caller's friend-request privacy policy (defaults to 'everyone').
async fn friend_policy_of(db: &Pool, user_id: u64) -> String {
    let Ok(mut c) = db.get_conn().await else { return "everyone".into(); };
    c.exec_first(
        "SELECT friend_policy FROM social_friend_settings WHERE user_id=:id",
        params! {"id" => user_id},
    )
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| "everyone".to_string())
}

// True if a and b share at least one accepted friend (for the 'mutual' policy).
async fn shares_mutual_friend(db: &Pool, a: u64, b: u64) -> bool {
    let af = friend_ids(db, a).await;
    if af.is_empty() {
        return false;
    }
    let bf = friend_ids(db, b).await;
    bf.iter().any(|id| af.contains(id))
}

// ----------------------------------------------------------------------------
// Connection hub (real-time fan-out)
// ----------------------------------------------------------------------------

// A frame queued for delivery to a socket. Text carries JSON control/chat
// events; Binary carries voice audio (a 8-byte LE sender-id header followed by
// the raw codec/PCM payload) relayed on the low-latency path.
enum OutMsg {
    Text(String),
    Binary(Vec<u8>),
}

struct SocialConn {
    conn_id: u64,
    tx: tokio::sync::mpsc::UnboundedSender<OutMsg>,
}

struct SocialHub {
    // user_id -> live sockets for that user (a user may be online on >1 device).
    conns: std::sync::Mutex<std::collections::HashMap<u64, Vec<SocialConn>>>,
    next_id: std::sync::atomic::AtomicU64,
    // Canonical (lo,hi) pairs with an active, friendship-verified voice call.
    // Audio frames are relayed only between pairs in this set so the hot path
    // never touches the database (verification happens once at invite/accept).
    voice_pairs: std::sync::Mutex<std::collections::HashSet<(u64, u64)>>,
}

static SOCIAL_HUB: std::sync::OnceLock<SocialHub> = std::sync::OnceLock::new();

fn social_hub() -> &'static SocialHub {
    SOCIAL_HUB.get_or_init(|| SocialHub {
        conns: std::sync::Mutex::new(std::collections::HashMap::new()),
        next_id: std::sync::atomic::AtomicU64::new(1),
        voice_pairs: std::sync::Mutex::new(std::collections::HashSet::new()),
    })
}

impl SocialHub {
    fn register(&self, user_id: u64, tx: tokio::sync::mpsc::UnboundedSender<OutMsg>) -> u64 {
        let conn_id = self.next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut g = self.conns.lock().unwrap();
        g.entry(user_id).or_default().push(SocialConn { conn_id, tx });
        conn_id
    }

    fn unregister(&self, user_id: u64, conn_id: u64) -> bool {
        let mut g = self.conns.lock().unwrap();
        if let Some(v) = g.get_mut(&user_id) {
            v.retain(|c| c.conn_id != conn_id);
            if v.is_empty() {
                g.remove(&user_id);
                return true; // user has no more live sockets
            }
        }
        false
    }

    fn is_online(&self, user_id: u64) -> bool {
        self.conns.lock().unwrap().get(&user_id).map_or(false, |v| !v.is_empty())
    }

    // Deliver to every live socket for `user_id` *on this instance only*. Used by
    // both the public `push` (after local delivery it also fans out) and the Redis
    // subscriber (which must not re-publish). Non-blocking; dead senders are
    // dropped lazily by the receive loop's unregister.
    fn deliver_local(&self, user_id: u64, msg: &str) {
        let g = self.conns.lock().unwrap();
        if let Some(v) = g.get(&user_id) {
            for c in v {
                let _ = c.tx.send(OutMsg::Text(msg.to_string()));
            }
        }
    }

    // Non-blocking push to every live socket for `user_id`: local delivery now,
    // plus a Redis fan-out to peer instances (no-op when Redis is disabled).
    fn push(&self, user_id: u64, msg: &str) {
        self.deliver_local(user_id, msg);
        fanout_publish(user_id, msg);
    }

    // Non-blocking binary push (voice audio) to every live socket for `user_id`.
    fn push_binary(&self, user_id: u64, data: &[u8]) {
        let g = self.conns.lock().unwrap();
        if let Some(v) = g.get(&user_id) {
            for c in v {
                let _ = c.tx.send(OutMsg::Binary(data.to_vec()));
            }
        }
    }

    fn allow_voice(&self, a: u64, b: u64) {
        self.voice_pairs.lock().unwrap().insert(pair(a, b));
    }
    fn disallow_voice(&self, a: u64, b: u64) {
        self.voice_pairs.lock().unwrap().remove(&pair(a, b));
    }
    fn voice_allowed(&self, a: u64, b: u64) -> bool {
        self.voice_pairs.lock().unwrap().contains(&pair(a, b))
    }
    // Drop every open call pair involving `user_id` (called when they go offline).
    fn drop_voice_for(&self, user_id: u64) {
        self.voice_pairs
            .lock()
            .unwrap()
            .retain(|&(lo, hi)| lo != user_id && hi != user_id);
    }
}

// ----------------------------------------------------------------------------
// Auth helpers
// ----------------------------------------------------------------------------

// Resolve a launcher account from a raw bearer token string (WS query param or
// first-frame auth). Mirrors launcher_user() which only reads from HeaderMap.
async fn user_from_token(st: &AppState, token: &str) -> Option<User> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let hash = sha256_hex(token.as_bytes());
    let mut c = st.db.get_conn().await.ok()?;
    let uid: Option<u64> = c
        .exec_first(
            "SELECT user_id FROM launcher_tokens WHERE token_hash=:h AND enabled=TRUE LIMIT 1",
            params! {"h" => hash},
        )
        .await
        .ok()
        .flatten();
    find_user_by_id(&st.db, uid?).await.ok().flatten()
}

#[inline]
fn pair(a: u64, b: u64) -> (u64, u64) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

// ----------------------------------------------------------------------------
// Relationship queries
// ----------------------------------------------------------------------------

async fn is_blocked_either(db: &Pool, a: u64, b: u64) -> bool {
    let Ok(mut c) = db.get_conn().await else { return false; };
    let n: Option<u64> = c
        .exec_first(
            "SELECT COUNT(*) FROM social_blocks WHERE (blocker_id=:a AND blocked_id=:b) OR (blocker_id=:b AND blocked_id=:a)",
            params! {"a" => a, "b" => b},
        )
        .await
        .ok()
        .flatten();
    n.unwrap_or(0) > 0
}

async fn are_friends(db: &Pool, a: u64, b: u64) -> bool {
    let (lo, hi) = pair(a, b);
    let Ok(mut c) = db.get_conn().await else { return false; };
    let st: Option<String> = c
        .exec_first(
            "SELECT status FROM social_friendships WHERE user_lo=:lo AND user_hi=:hi",
            params! {"lo" => lo, "hi" => hi},
        )
        .await
        .ok()
        .flatten();
    st.as_deref() == Some("accepted")
}

// Snapshot a user's presence; treats a row older than PRESENCE_STALE_SECS as
// offline so a crashed client doesn't appear perpetually online.
const PRESENCE_STALE_SECS: i64 = 70;

// Returns (state, game_id, game_title, status_text). status_text rides along
// (ROADMAP 1.6) and is cleared when the user is treated as offline/invisible.
async fn presence_of(
    db: &Pool,
    user_id: u64,
    online_hint: bool,
) -> (String, Option<String>, Option<String>, Option<String>) {
    let Ok(mut c) = db.get_conn().await else {
        return ("offline".into(), None, None, None);
    };
    let row: Option<(String, Option<String>, Option<String>, i64, Option<String>)> = c
        .exec_first(
            "SELECT state, game_id, game_title, updated_at, status_text FROM social_presence WHERE user_id=:id",
            params! {"id" => user_id},
        )
        .await
        .ok()
        .flatten();
    match row {
        Some((state, gid, gtitle, upd, status)) => {
            let fresh = online_hint || (now() - upd) < PRESENCE_STALE_SECS;
            if !fresh || state == "invisible" {
                ("offline".into(), None, None, None)
            } else {
                (state, gid, gtitle, status)
            }
        }
        None => ("offline".into(), None, None, None),
    }
}

async fn set_presence(
    db: &Pool,
    user_id: u64,
    state: &str,
    game_id: Option<&str>,
    game_title: Option<&str>,
    status_text: Option<&str>,
) {
    let Ok(mut c) = db.get_conn().await else { return; };
    let _ = c
        .exec_drop(
            r#"INSERT INTO social_presence (user_id, state, game_id, game_title, status_text, updated_at)
               VALUES (:id, :s, :gi, :gt, :st, :t)
               ON DUPLICATE KEY UPDATE state=:s, game_id=:gi, game_title=:gt, status_text=:st, updated_at=:t"#,
            params! {"id" => user_id, "s" => state, "gi" => game_id, "gt" => game_title, "st" => status_text, "t" => now()},
        )
        .await;
}

// All accepted-friend account IDs for a user.
async fn friend_ids(db: &Pool, user_id: u64) -> Vec<u64> {
    let Ok(mut c) = db.get_conn().await else { return Vec::new(); };
    let rows: Vec<(u64, u64)> = c
        .exec(
            "SELECT user_lo, user_hi FROM social_friendships WHERE status='accepted' AND (user_lo=:id OR user_hi=:id)",
            params! {"id" => user_id},
        )
        .await
        .unwrap_or_default();
    rows.into_iter()
        .map(|(lo, hi)| if lo == user_id { hi } else { lo })
        .collect()
}

// Broadcast a JSON event to all of a user's accepted friends who are currently
// connected. Used for presence diffs and status changes.
async fn broadcast_to_friends(db: &Pool, user_id: u64, json: &str) {
    for fid in friend_ids(db, user_id).await {
        social_hub().push(fid, json);
    }
}

async fn push_presence_diff(st: &AppState, user_id: u64) {
    let online = presence_online(user_id).await;
    let (state, gid, gtitle, status) = presence_of(&st.db, user_id, online).await;
    let evt = serde_json::json!({
        "type": "presence",
        "userId": user_id,
        "state": state,
        "gameId": gid,
        "gameTitle": gtitle,
        "statusText": status,
    })
    .to_string();
    broadcast_to_friends(&st.db, user_id, &evt).await;
}

// ----------------------------------------------------------------------------
// REST: friends list / requests
// ----------------------------------------------------------------------------

async fn api_social_friends(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    sweep_expired_requests(&st.db).await;
    let mut conn = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Every relationship touching me, plus the other party's username.
    let rows: Vec<(u64, u64, String, u64)> = match conn
        .exec(
            "SELECT user_lo, user_hi, status, requested_by FROM social_friendships WHERE user_lo=:id OR user_hi=:id",
            params! {"id" => me.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };

    let mut out = Vec::new();
    for (lo, hi, status, requested_by) in rows {
        let other = if lo == me.id { hi } else { lo };
        let uname = match find_user_by_id(&st.db, other).await {
            Ok(Some(u)) => u.username,
            _ => continue, // account deleted; skip dangling relationship
        };
        let relation = match status.as_str() {
            "accepted" => "accepted",
            "pending" if requested_by == me.id => "request_sent",
            "pending" => "request_received",
            _ => "none",
        };
        let online = presence_online(other).await;
        let (pstate, gid, gtitle, status) = presence_of(&st.db, other, online).await;
        // Friend organization metadata (ROADMAP 1.5): per-row note/groups/pinned.
        let meta: Option<(Option<String>, Option<String>, i8)> = conn
            .exec_first(
                "SELECT note, groups, pinned FROM social_friend_meta WHERE owner_id=:o AND friend_id=:f",
                params! {"o" => me.id, "f" => other},
            )
            .await
            .ok()
            .flatten();
        let (note, groups, pinned) = match meta {
            Some((n, g, p)) => (n, g, p != 0),
            None => (None, None, false),
        };
        out.push(serde_json::json!({
            "accountId": other,
            "username": uname,
            "relation": relation,
            "presence": pstate,
            "currentGameId": gid,
            "currentGameTitle": gtitle,
            "statusText": status,
            "note": note,
            "groups": groups,
            "pinned": pinned,
        }));
    }
    Json(serde_json::json!({ "friends": out })).into_response()
}

#[derive(Deserialize)]
struct FriendRequestBody {
    username: String,
}

async fn api_social_request(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FriendRequestBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let target = match find_user(&st.db, body.username.trim()).await {
        Ok(Some(u)) => u,
        Ok(None) => return (StatusCode::NOT_FOUND, "No such user").into_response(),
        Err(e) => return server_error(e),
    };
    if target.id == me.id {
        return (StatusCode::BAD_REQUEST, "Cannot friend yourself").into_response();
    }
    if is_blocked_either(&st.db, me.id, target.id).await {
        return (StatusCode::FORBIDDEN, "Blocked").into_response();
    }
    // Persistent ignore (ROADMAP 1.1b): if the target has ignored me, or I have
    // ignored the target, pretend the request was sent but create/notify nothing.
    if has_ignored(&st.db, target.id, me.id).await || has_ignored(&st.db, me.id, target.id).await {
        return Json(serde_json::json!({ "status": "request_sent" })).into_response();
    }
    // Clear any expired pending invites first so they neither block a fresh
    // request nor count against the outstanding-request cap.
    sweep_expired_requests(&st.db).await;
    // Privacy: honour the target's friend-request policy. 'mutual' requires a
    // shared accepted friend; 'nobody' rejects outright. A request that would be
    // an instant accept (they already invited me) bypasses the policy below.
    let policy = friend_policy_of(&st.db, target.id).await;
    let (lo, hi) = pair(me.id, target.id);
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let existing: Option<(String, u64)> = c
        .exec_first(
            "SELECT status, requested_by FROM social_friendships WHERE user_lo=:lo AND user_hi=:hi",
            params! {"lo" => lo, "hi" => hi},
        )
        .await
        .ok()
        .flatten();
    match existing {
        Some((s, _)) if s == "accepted" => {
            return (StatusCode::CONFLICT, "Already friends").into_response();
        }
        Some((s, req_by)) if s == "pending" && req_by == target.id => {
            // They already requested me -> accept immediately.
            let _ = c
                .exec_drop(
                    "UPDATE social_friendships SET status='accepted', updated_at=:t WHERE user_lo=:lo AND user_hi=:hi",
                    params! {"t" => now(), "lo" => lo, "hi" => hi},
                )
                .await;
            notify_relationship(&st, me.id, target.id, "friend_accepted").await;
            // ROADMAP 1.4: small XP reward for a new mutual friendship (both sides).
            award_xp(&st.db, me.id, 10).await;
            award_xp(&st.db, target.id, 10).await;
            store_notification(
                &st,
                target.id,
                "friend_accepted",
                Some(me.id),
                Some(&me.username),
                Some(&format!("{} accepted your friend request", me.username)),
                Some(serde_json::json!({ "userId": me.id, "username": me.username })),
            )
            .await;
            return Json(serde_json::json!({ "status": "accepted" })).into_response();
        }
        Some(_) => {
            return Json(serde_json::json!({ "status": "request_sent" })).into_response();
        }
        None => {}
    }
    // No existing relationship → this is a genuinely new outgoing request, so the
    // target's privacy policy applies.
    match policy.as_str() {
        "nobody" => {
            return (StatusCode::FORBIDDEN, "This user is not accepting friend requests").into_response();
        }
        "mutual" => {
            if !shares_mutual_friend(&st.db, me.id, target.id).await {
                return (StatusCode::FORBIDDEN, "This user only accepts requests from people they share a friend with").into_response();
            }
        }
        _ => {}
    }
    // Spam control: cap how many outgoing pending requests one account may have.
    let outstanding: u64 = c
        .exec_first(
            "SELECT COUNT(*) FROM social_friendships WHERE status='pending' AND requested_by=:me",
            params! {"me" => me.id},
        )
        .await
        .ok()
        .flatten()
        .unwrap_or(0);
    if outstanding >= FRIEND_REQUEST_MAX_OUTGOING {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pending friend requests; wait for some to be answered",
        )
            .into_response();
    }
    let expires = now() + FRIEND_REQUEST_TTL_SECS;
    if let Err(e) = c
        .exec_drop(
            r#"INSERT INTO social_friendships (user_lo, user_hi, status, requested_by, created_at, updated_at, expires_at)
               VALUES (:lo, :hi, 'pending', :rb, :t, :t, :exp)"#,
            params! {"lo" => lo, "hi" => hi, "rb" => me.id, "t" => now(), "exp" => expires},
        )
        .await
    {
        return server_error(e);
    }
    // Push an incoming-request event to the target if connected.
    social_hub().push(
        target.id,
        &serde_json::json!({
            "type": "friend_request",
            "fromId": me.id,
            "fromUsername": me.username,
        })
        .to_string(),
    );
    // Durable notification so the target still sees it if currently offline.
    store_notification(
        &st,
        target.id,
        "friend_request",
        Some(me.id),
        Some(&me.username),
        Some(&format!("{} sent you a friend request", me.username)),
        Some(serde_json::json!({ "fromId": me.id, "fromUsername": me.username })),
    )
    .await;
    Json(serde_json::json!({ "status": "request_sent" })).into_response()
}

#[derive(Deserialize)]
struct FriendActionBody {
    #[serde(rename = "userId")]
    user_id: u64,
    action: String, // accept | decline | cancel | remove | ignore
}

async fn api_social_respond(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FriendActionBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let (lo, hi) = pair(me.id, body.user_id);
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    match body.action.as_str() {
        "accept" => {
            let n = c
                .exec_iter(
                    "UPDATE social_friendships SET status='accepted', updated_at=:t WHERE user_lo=:lo AND user_hi=:hi AND status='pending' AND requested_by=:other",
                    params! {"t" => now(), "lo" => lo, "hi" => hi, "other" => body.user_id},
                )
                .await;
            match n {
                Ok(r) if r.affected_rows() > 0 => {
                    notify_relationship(&st, me.id, body.user_id, "friend_accepted").await;
                    award_xp(&st.db, me.id, 10).await;
                    award_xp(&st.db, body.user_id, 10).await;
                    store_notification(
                        &st,
                        body.user_id,
                        "friend_accepted",
                        Some(me.id),
                        Some(&me.username),
                        Some(&format!("{} accepted your friend request", me.username)),
                        Some(serde_json::json!({ "userId": me.id, "username": me.username })),
                    )
                    .await;
                    Json(serde_json::json!({ "status": "accepted" })).into_response()
                }
                Ok(_) => (StatusCode::NOT_FOUND, "No pending request").into_response(),
                Err(e) => server_error(e),
            }
        }
        "decline" | "cancel" | "remove" => {
            if let Err(e) = c
                .exec_drop(
                    "DELETE FROM social_friendships WHERE user_lo=:lo AND user_hi=:hi",
                    params! {"lo" => lo, "hi" => hi},
                )
                .await
            {
                return server_error(e);
            }
            notify_relationship(&st, me.id, body.user_id, "friend_removed").await;
            Json(serde_json::json!({ "status": "removed" })).into_response()
        }
        // Silently drop an incoming request: delete the pending row but send no
        // friend_removed event, so the requester is not told they were rebuffed.
        // Only affects a request the other party sent to me.
        "ignore" => {
            if let Err(e) = c
                .exec_drop(
                    "DELETE FROM social_friendships WHERE user_lo=:lo AND user_hi=:hi AND status='pending' AND requested_by=:other",
                    params! {"lo" => lo, "hi" => hi, "other" => body.user_id},
                )
                .await
            {
                return server_error(e);
            }
            // Persist the ignore so it survives re-requests (ROADMAP 1.1b): future
            // requests/DMs from this user are silently dropped.
            let _ = c
                .exec_drop(
                    "INSERT IGNORE INTO social_ignores (ignorer_id, ignored_id, created_at) VALUES (:me, :other, :t)",
                    params! {"me" => me.id, "other" => body.user_id, "t" => now()},
                )
                .await;
            Json(serde_json::json!({ "status": "ignored" })).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, "Unknown action").into_response(),
    }
}

#[derive(Deserialize)]
struct BlockBody {
    #[serde(rename = "userId")]
    user_id: u64,
    block: bool,
}

async fn api_social_block(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<BlockBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if body.block {
        let (lo, hi) = pair(me.id, body.user_id);
        let _ = c
            .exec_drop(
                "DELETE FROM social_friendships WHERE user_lo=:lo AND user_hi=:hi",
                params! {"lo" => lo, "hi" => hi},
            )
            .await;
        if let Err(e) = c
            .exec_drop(
                "INSERT IGNORE INTO social_blocks (blocker_id, blocked_id, created_at) VALUES (:a, :b, :t)",
                params! {"a" => me.id, "b" => body.user_id, "t" => now()},
            )
            .await
        {
            return server_error(e);
        }
        notify_relationship(&st, me.id, body.user_id, "friend_removed").await;
        Json(serde_json::json!({ "status": "blocked" })).into_response()
    } else {
        if let Err(e) = c
            .exec_drop(
                "DELETE FROM social_blocks WHERE blocker_id=:a AND blocked_id=:b",
                params! {"a" => me.id, "b" => body.user_id},
            )
            .await
        {
            return server_error(e);
        }
        Json(serde_json::json!({ "status": "unblocked" })).into_response()
    }
}

#[derive(Deserialize)]
struct FriendPolicyBody {
    // Both fields optional so a PUT may set either or both (ROADMAP 1.1b).
    #[serde(default)]
    #[serde(rename = "friendPolicy")]
    friend_policy: Option<String>,
    #[serde(default)]
    #[serde(rename = "dmPolicy")]
    dm_policy: Option<String>,
}

// GET /api/social/privacy — the caller's friend-request + DM privacy policies.
async fn api_social_privacy_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let policy = friend_policy_of(&st.db, me.id).await;
    let dm = dm_policy_of(&st.db, me.id).await;
    Json(serde_json::json!({ "friendPolicy": policy, "dmPolicy": dm })).into_response()
}

// PUT /api/social/privacy — set friend-request and/or DM policy. Each field is
// optional; a missing field is left untouched. Unknown enum values are rejected.
async fn api_social_privacy_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FriendPolicyBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let friend_policy = body.friend_policy.as_deref().map(str::trim);
    let dm_policy = body.dm_policy.as_deref().map(str::trim);
    if let Some(p) = friend_policy {
        if !matches!(p, "everyone" | "mutual" | "nobody") {
            return (StatusCode::BAD_REQUEST, "friendPolicy must be everyone, mutual, or nobody").into_response();
        }
    }
    if let Some(p) = dm_policy {
        if !matches!(p, "everyone" | "friends" | "nobody") {
            return (StatusCode::BAD_REQUEST, "dmPolicy must be everyone, friends, or nobody").into_response();
        }
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Ensure a row exists, then update only the supplied columns.
    let _ = c
        .exec_drop(
            r#"INSERT IGNORE INTO social_friend_settings (user_id, friend_policy, updated_at)
               VALUES (:id, 'everyone', :t)"#,
            params! {"id" => me.id, "t" => now()},
        )
        .await;
    if let Some(p) = friend_policy {
        if let Err(e) = c
            .exec_drop(
                "UPDATE social_friend_settings SET friend_policy=:p, updated_at=:t WHERE user_id=:id",
                params! {"p" => p, "t" => now(), "id" => me.id},
            )
            .await
        {
            return server_error(e);
        }
    }
    if let Some(p) = dm_policy {
        if let Err(e) = c
            .exec_drop(
                "UPDATE social_friend_settings SET dm_policy=:p, updated_at=:t WHERE user_id=:id",
                params! {"p" => p, "t" => now(), "id" => me.id},
            )
            .await
        {
            return server_error(e);
        }
    }
    let fp = friend_policy_of(&st.db, me.id).await;
    let dm = dm_policy_of(&st.db, me.id).await;
    Json(serde_json::json!({ "friendPolicy": fp, "dmPolicy": dm })).into_response()
}

// GET /api/social/ignores — the list of account ids the caller is ignoring.
async fn api_social_ignores_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let ids: Vec<u64> = match c
        .exec(
            "SELECT ignored_id FROM social_ignores WHERE ignorer_id=:id ORDER BY created_at DESC",
            params! {"id" => me.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    Json(serde_json::json!({ "ignored": ids })).into_response()
}

#[derive(Deserialize)]
struct IgnoreBody {
    #[serde(rename = "userId")]
    user_id: u64,
    ignore: bool,
}

// POST /api/social/ignores — add/remove a persistent ignore on another account.
async fn api_social_ignores_post(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<IgnoreBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    if body.user_id == 0 || body.user_id == me.id {
        return (StatusCode::BAD_REQUEST, "invalid userId").into_response();
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let res = if body.ignore {
        c.exec_drop(
            "INSERT IGNORE INTO social_ignores (ignorer_id, ignored_id, created_at) VALUES (:me, :o, :t)",
            params! {"me" => me.id, "o" => body.user_id, "t" => now()},
        )
        .await
    } else {
        c.exec_drop(
            "DELETE FROM social_ignores WHERE ignorer_id=:me AND ignored_id=:o",
            params! {"me" => me.id, "o" => body.user_id},
        )
        .await
    };
    if let Err(e) = res {
        return server_error(e);
    }
    Json(serde_json::json!({ "status": if body.ignore { "ignored" } else { "unignored" } })).into_response()
}

// ----------------------------------------------------------------------------
// REST: user profiles (ROADMAP 1.4)
// ----------------------------------------------------------------------------

// Level curve: level = floor(sqrt(xp / 100)). So 100 xp = L1, 400 = L2,
// 900 = L3, 1600 = L4 … a gentle quadratic that keeps early levels quick.
fn level_for_xp(xp: i64) -> i64 {
    if xp <= 0 {
        return 0;
    }
    ((xp as f64) / 100.0).sqrt().floor() as i64
}

// GET /api/social/profile/:id — public profile view for any account.
async fn api_social_profile_get(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<u64>,
) -> Response {
    if launcher_user(&st, &headers).await.is_none() {
        return unauthorized();
    }
    let user = match find_user_by_id(&st.db, id).await {
        Ok(Some(u)) => u,
        Ok(None) => return (StatusCode::NOT_FOUND, "No such user").into_response(),
        Err(e) => return server_error(e),
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(Option<String>, Option<String>, i64)> = c
        .exec_first(
            "SELECT banner, bio, xp FROM social_profiles WHERE user_id=:id",
            params! {"id" => id},
        )
        .await
        .ok()
        .flatten();
    let (banner, bio, xp) = row.unwrap_or((None, None, 0));
    let avatar_version = get_user_avatar_version(&st.db, id).await;
    Json(serde_json::json!({
        "userId": id,
        "username": user.username,
        "avatarVersion": avatar_version,
        "banner": banner,
        "bio": bio,
        "level": level_for_xp(xp),
        "xp": xp,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ProfilePutBody {
    #[serde(default)]
    banner: Option<String>,
    #[serde(default)]
    bio: Option<String>,
}

// PUT /api/social/profile — update the caller's own banner/bio (self only).
async fn api_social_profile_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ProfilePutBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let banner = body.banner.map(|s| s.chars().take(512).collect::<String>());
    let bio = body.bio.map(|s| s.chars().take(1024).collect::<String>());
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Ensure a row, then update only supplied columns so each can change alone.
    let _ = c
        .exec_drop(
            "INSERT IGNORE INTO social_profiles (user_id, xp, updated_at) VALUES (:id, 0, :t)",
            params! {"id" => me.id, "t" => now()},
        )
        .await;
    if banner.is_some() {
        let _ = c
            .exec_drop(
                "UPDATE social_profiles SET banner=:b, updated_at=:t WHERE user_id=:id",
                params! {"b" => &banner, "t" => now(), "id" => me.id},
            )
            .await;
    }
    if bio.is_some() {
        let _ = c
            .exec_drop(
                "UPDATE social_profiles SET bio=:b, updated_at=:t WHERE user_id=:id",
                params! {"b" => &bio, "t" => now(), "id" => me.id},
            )
            .await;
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

// Award XP to a user (ROADMAP 1.4, low-risk). Best-effort; creates the row if
// absent. No-op on non-positive amounts.
async fn award_xp(db: &Pool, user_id: u64, amount: i64) {
    if amount <= 0 {
        return;
    }
    let Ok(mut c) = db.get_conn().await else { return; };
    let _ = c
        .exec_drop(
            r#"INSERT INTO social_profiles (user_id, xp, updated_at) VALUES (:id, :amt, :t)
               ON DUPLICATE KEY UPDATE xp = xp + :amt, updated_at=:t"#,
            params! {"id" => user_id, "amt" => amount, "t" => now()},
        )
        .await;
}

// ----------------------------------------------------------------------------
// REST: friend organization (ROADMAP 1.5)
// ----------------------------------------------------------------------------

// GET /api/social/friendmeta — all the caller's friend-meta rows.
async fn api_social_friendmeta_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let rows: Vec<(u64, Option<String>, Option<String>, i8)> = match c
        .exec(
            "SELECT friend_id, note, groups, pinned FROM social_friend_meta WHERE owner_id=:o",
            params! {"o" => me.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let items: Vec<_> = rows
        .into_iter()
        .map(|(fid, note, groups, pinned)| {
            serde_json::json!({
                "userId": fid,
                "note": note,
                "groups": groups,
                "pinned": pinned != 0,
            })
        })
        .collect();
    Json(serde_json::json!({ "meta": items })).into_response()
}

#[derive(Deserialize)]
struct FriendMetaBody {
    #[serde(rename = "userId")]
    user_id: u64,
    #[serde(default)]
    note: Option<String>,
    #[serde(default)]
    groups: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
}

// PUT /api/social/friendmeta — upsert note/groups/pinned for one friend. Each
// field is optional and only supplied fields change.
async fn api_social_friendmeta_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<FriendMetaBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    if body.user_id == 0 || body.user_id == me.id {
        return (StatusCode::BAD_REQUEST, "invalid userId").into_response();
    }
    let note = body.note.map(|s| s.chars().take(512).collect::<String>());
    let groups = body.groups.map(|s| s.chars().take(255).collect::<String>());
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let _ = c
        .exec_drop(
            "INSERT IGNORE INTO social_friend_meta (owner_id, friend_id, pinned) VALUES (:o, :f, 0)",
            params! {"o" => me.id, "f" => body.user_id},
        )
        .await;
    if note.is_some() {
        let _ = c
            .exec_drop(
                "UPDATE social_friend_meta SET note=:n WHERE owner_id=:o AND friend_id=:f",
                params! {"n" => &note, "o" => me.id, "f" => body.user_id},
            )
            .await;
    }
    if groups.is_some() {
        let _ = c
            .exec_drop(
                "UPDATE social_friend_meta SET groups=:g WHERE owner_id=:o AND friend_id=:f",
                params! {"g" => &groups, "o" => me.id, "f" => body.user_id},
            )
            .await;
    }
    if let Some(p) = body.pinned {
        let _ = c
            .exec_drop(
                "UPDATE social_friend_meta SET pinned=:p WHERE owner_id=:o AND friend_id=:f",
                params! {"p" => if p { 1 } else { 0 }, "o" => me.id, "f" => body.user_id},
            )
            .await;
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

// GET /api/social/search?q= — username search (LIKE, ≤20), excluding self and
// anyone in a block relationship with the caller.
async fn api_social_search(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let term = query.q.trim();
    if term.is_empty() {
        return Json(serde_json::json!({ "users": [] })).into_response();
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Escape LIKE wildcards in the user-supplied term, then wrap with %…%.
    let escaped = term.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
    let pattern = format!("%{escaped}%");
    let rows: Vec<(u64, String)> = match c
        .exec(
            r#"SELECT id, username FROM admin_users
               WHERE enabled = TRUE AND id != :me AND username LIKE :pat
               ORDER BY username LIMIT 20"#,
            params! {"me" => me.id, "pat" => pattern},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let mut users = Vec::new();
    for (id, username) in rows {
        if is_blocked_either(&st.db, me.id, id).await {
            continue;
        }
        users.push(serde_json::json!({ "userId": id, "username": username }));
    }
    Json(serde_json::json!({ "users": users })).into_response()
}

// Notify both parties of a relationship change so their friend lists refresh.
async fn notify_relationship(st: &AppState, a: u64, b: u64, kind: &str) {
    let evt = serde_json::json!({ "type": kind, "userId": a }).to_string();
    social_hub().push(b, &evt);
    let evt2 = serde_json::json!({ "type": kind, "userId": b }).to_string();
    social_hub().push(a, &evt2);
}

// ----------------------------------------------------------------------------
// Durable notifications
// ----------------------------------------------------------------------------

// Persist a notification for `user_id` and push it live if they're connected.
// Returns the new row id (0 on failure). The DB row is the source of truth and
// is redelivered by deliver_pending_notifications on the user's next connect, so
// an offline recipient still sees the event. The live `notification` frame is a
// best-effort optimization for already-connected clients.
async fn store_notification(
    st: &AppState,
    user_id: u64,
    kind: &str,
    actor_id: Option<u64>,
    actor_name: Option<&str>,
    body: Option<&str>,
    payload: Option<serde_json::Value>,
) -> u64 {
    let payload_str = payload.as_ref().map(|p| p.to_string());
    let ts = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let ins = c
        .exec_iter(
            r#"INSERT INTO social_notifications
               (user_id, kind, actor_id, actor_name, body, payload, created_at)
               VALUES (:u, :k, :ai, :an, :b, :p, :t)"#,
            params! {
                "u" => user_id, "k" => kind, "ai" => actor_id,
                "an" => actor_name, "b" => body, "p" => payload_str, "t" => ts,
            },
        )
        .await;
    let id = match ins {
        Ok(r) => r.last_insert_id().unwrap_or(0),
        Err(_) => return 0,
    };
    let frame = serde_json::json!({
        "type": "notification",
        "id": id,
        "kind": kind,
        "actorId": actor_id,
        "actorName": actor_name,
        "body": body,
        "payload": payload,
        "timestamp": ts,
        "read": false,
    })
    .to_string();
    social_hub().push(user_id, &frame);
    id
}

// Map one DB row to the wire shape shared by the live frame, the connect batch,
// and the REST list.
fn notification_row_json(
    id: u64,
    kind: String,
    actor_id: Option<u64>,
    actor_name: Option<String>,
    body: Option<String>,
    payload: Option<String>,
    ts: i64,
    read_at: Option<i64>,
) -> serde_json::Value {
    serde_json::json!({
        "id": id,
        "kind": kind,
        "actorId": actor_id,
        "actorName": actor_name,
        "body": body,
        "payload": payload.and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok()),
        "timestamp": ts,
        "read": read_at.is_some(),
    })
}

type NotifRow = (
    u64,
    String,
    Option<u64>,
    Option<String>,
    Option<String>,
    Option<String>,
    i64,
    Option<i64>,
);

// On (re)connect, push every still-unread notification as one batch frame so the
// client can repopulate its notification center for anything missed offline.
async fn deliver_pending_notifications(
    st: &AppState,
    uid: u64,
    tx: &tokio::sync::mpsc::UnboundedSender<OutMsg>,
) {
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(_) => return,
    };
    let rows: Vec<NotifRow> = match c
        .exec(
            r#"SELECT id, kind, actor_id, actor_name, body, payload, created_at, read_at
               FROM social_notifications
               WHERE user_id=:u AND read_at IS NULL
               ORDER BY id DESC LIMIT 100"#,
            params! {"u" => uid},
        )
        .await
    {
        Ok(r) => r,
        Err(_) => return,
    };
    if rows.is_empty() {
        return;
    }
    let items: Vec<_> = rows
        .into_iter()
        .rev()
        .map(|(id, kind, aid, aname, body, payload, ts, read)| {
            notification_row_json(id, kind, aid, aname, body, payload, ts, read)
        })
        .collect();
    let _ = tx.send(OutMsg::Text(
        serde_json::json!({ "type": "notifications", "items": items }).to_string(),
    ));
}

// REST: list recent notifications (read + unread) plus the unread count.
async fn api_social_notifications(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let rows: Vec<NotifRow> = match c
        .exec(
            r#"SELECT id, kind, actor_id, actor_name, body, payload, created_at, read_at
               FROM social_notifications
               WHERE user_id=:u ORDER BY id DESC LIMIT 200"#,
            params! {"u" => me.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let mut unread: u64 = 0;
    let items: Vec<_> = rows
        .into_iter()
        .map(|(id, kind, aid, aname, body, payload, ts, read)| {
            if read.is_none() {
                unread += 1;
            }
            notification_row_json(id, kind, aid, aname, body, payload, ts, read)
        })
        .collect();
    Json(serde_json::json!({ "notifications": items, "unread": unread })).into_response()
}

#[derive(Deserialize)]
struct NotifReadBody {
    #[serde(default)]
    #[serde(rename = "upToId")]
    up_to_id: u64,
}

// REST: mark notifications read. With upToId>0, marks only rows up to that id
// (inclusive); otherwise marks all currently-unread rows.
async fn api_social_notifications_read(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<NotifReadBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let ts = now();
    let res = if body.up_to_id > 0 {
        c.exec_drop(
            "UPDATE social_notifications SET read_at=:t WHERE user_id=:u AND read_at IS NULL AND id<=:id",
            params! {"t" => ts, "u" => me.id, "id" => body.up_to_id},
        )
        .await
    } else {
        c.exec_drop(
            "UPDATE social_notifications SET read_at=:t WHERE user_id=:u AND read_at IS NULL",
            params! {"t" => ts, "u" => me.id},
        )
        .await
    };
    if let Err(e) = res {
        return server_error(e);
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

// ----------------------------------------------------------------------------
// REST: server-synced social preferences (ROADMAP 0.5)
// ----------------------------------------------------------------------------

// GET the stored prefs blob for the caller (an empty object if none yet).
async fn api_social_prefs_get(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(String, i64)> = match c
        .exec_first(
            "SELECT prefs, updated_at FROM social_user_prefs WHERE user_id=:u",
            params! {"u" => me.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let (prefs, updated_at) = match row {
        Some((p, t)) => (
            serde_json::from_str::<serde_json::Value>(&p)
                .unwrap_or_else(|_| serde_json::json!({})),
            t,
        ),
        None => (serde_json::json!({}), 0),
    };
    Json(serde_json::json!({ "prefs": prefs, "updatedAt": updated_at })).into_response()
}

// PUT/POST the prefs blob (opaque JSON; client owns the shape). Last write wins.
async fn api_social_prefs_put(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    // Accept either the bare prefs object or an envelope { "prefs": {...} }.
    let prefs = match body.get("prefs") {
        Some(p) => p.clone(),
        None => body,
    };
    let serialized = prefs.to_string();
    // Guard against an absurd payload (prefs are small key/value maps).
    if serialized.len() > 256 * 1024 {
        return (StatusCode::PAYLOAD_TOO_LARGE, "prefs too large").into_response();
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let res = c
        .exec_drop(
            r#"INSERT INTO social_user_prefs (user_id, prefs, updated_at)
               VALUES (:u, :p, :t)
               ON DUPLICATE KEY UPDATE prefs=:p, updated_at=:t"#,
            params! {"u" => me.id, "p" => &serialized, "t" => now()},
        )
        .await;
    if let Err(e) = res {
        return server_error(e);
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

// ----------------------------------------------------------------------------
// REST: message history
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    since: u64,
    #[serde(default = "default_limit")]
    limit: u64,
    // Pagination cursor for infinite history (1.2): when >0, return the page of
    // messages with id < before (older than the cursor) instead of id > since.
    #[serde(default)]
    before: u64,
}
fn default_limit() -> u64 {
    100
}

async fn api_social_history(
    State(st): State<AppState>,
    headers: HeaderMap,
    AxumPath(other): AxumPath<u64>,
    Query(q): Query<HistoryQuery>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let limit = q.limit.clamp(1, 500);
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // `before>0` pages backwards (older than the cursor) for infinite scroll;
    // otherwise the default forward window (newer than `since`) is returned.
    type HistRow = (u64, u64, u64, String, i64, Option<i64>, Option<i64>, Option<i64>, Option<u64>);
    let rows: Vec<HistRow> = match if q.before > 0 {
        c.exec(
            r#"SELECT id, sender_id, receiver_id, body, created_at, read_at, edited_at, deleted_at, reply_to
               FROM social_messages
               WHERE ((sender_id=:me AND receiver_id=:other) OR (sender_id=:other AND receiver_id=:me))
                 AND id < :before
               ORDER BY id DESC LIMIT :lim"#,
            params! {"me" => me.id, "other" => other, "before" => q.before, "lim" => limit},
        )
        .await
    } else {
        c.exec(
            r#"SELECT id, sender_id, receiver_id, body, created_at, read_at, edited_at, deleted_at, reply_to
               FROM social_messages
               WHERE ((sender_id=:me AND receiver_id=:other) OR (sender_id=:other AND receiver_id=:me))
                 AND id > :since
               ORDER BY id DESC LIMIT :lim"#,
            params! {"me" => me.id, "other" => other, "since" => q.since, "lim" => limit},
        )
        .await
    } {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    // Per-message reactions for this page (ROADMAP 1.2b).
    let ids: Vec<u64> = rows.iter().map(|r| r.0).collect();
    let react_map = reactions_for_messages(&st.db, &ids).await;
    // Per-message attachment id for this page (ROADMAP 1.3).
    let att_map = attachments_for_messages(&st.db, &ids).await;
    // Mark inbound messages read up to now.
    let _ = c
        .exec_drop(
            "UPDATE social_messages SET read_at=:t WHERE receiver_id=:me AND sender_id=:other AND read_at IS NULL",
            params! {"t" => now(), "me" => me.id, "other" => other},
        )
        .await;
    let msgs: Vec<_> = rows
        .into_iter()
        .rev()
        .map(|(id, sndr, rcvr, body, ts, read_at, edited_at, deleted_at, reply_to)| {
            serde_json::json!({
                "messageId": id,
                "senderId": sndr,
                "receiverId": rcvr,
                "text": body,
                "timestamp": ts,
                "isRead": read_at.is_some(),
                "editedAt": edited_at,
                "deleted": deleted_at.is_some(),
                "replyTo": reply_to,
                "reactions": react_map.get(&id).cloned().unwrap_or_default(),
                "attachmentId": att_map.get(&id).copied().unwrap_or(0),
            })
        })
        .collect();
    Json(serde_json::json!({ "messages": msgs })).into_response()
}

// Build message_id -> attachment_id for a page of messages (ROADMAP 1.3). Only
// the first attachment per message is surfaced (DMs carry at most one today).
async fn attachments_for_messages(
    db: &Pool,
    ids: &[u64],
) -> std::collections::HashMap<u64, u64> {
    let mut map = std::collections::HashMap::new();
    if ids.is_empty() {
        return map;
    }
    let Ok(mut c) = db.get_conn().await else { return map; };
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT message_id, id FROM social_attachments WHERE message_id IN ({list}) ORDER BY id ASC"
    );
    let rows: Vec<(u64, u64)> = c.query(sql).await.unwrap_or_default();
    for (mid, aid) in rows {
        map.entry(mid).or_insert(aid);
    }
    map
}

// ----------------------------------------------------------------------------
// WebSocket gateway: /ws/social
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct WsAuthQuery {
    #[serde(default)]
    token: String,
}

async fn ws_social(
    State(st): State<AppState>,
    Query(q): Query<WsAuthQuery>,
    headers: HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> Response {
    // Token may arrive as ?token= (browser-style) or Authorization: Bearer.
    let token = if !q.token.is_empty() {
        q.token.clone()
    } else {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let Some(user) = user_from_token(&st, &token).await else {
        return unauthorized();
    };
    let uid = user.id;
    ws.on_upgrade(move |socket| social_socket(st, uid, socket))
}

async fn social_socket(st: AppState, uid: u64, socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;
    use futures_util::SinkExt;

    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<OutMsg>();
    let conn_id = social_hub().register(uid, tx.clone());
    fanout_set_online(uid); // cross-instance online registry (no-op without Redis)

    // First frame tells the client its own account id so it can align sent vs
    // received messages and ignore self-presence echoes.
    let _ = tx.send(OutMsg::Text(
        serde_json::json!({ "type": "hello", "selfId": uid }).to_string(),
    ));

    // Redeliver anything that happened while this user was offline.
    deliver_pending_notifications(&st, uid, &tx).await;

    // Mark online (unless invisible already chosen) and announce to friends.
    let was_online = social_hub()
        .conns
        .lock()
        .unwrap()
        .get(&uid)
        .map_or(0, |v| v.len())
        > 1;
    if !was_online {
        set_presence(&st.db, uid, "online", None, None, None).await;
        push_presence_diff(&st, uid).await;
    }

    // Outbound pump: drains the hub channel to the socket. Also emits a server
    // heartbeat every 25s so idle connections (and proxies) stay alive.
    let mut send_task = {
        tokio::spawn(async move {
            let mut hb = tokio::time::interval(std::time::Duration::from_secs(25));
            loop {
                tokio::select! {
                    msg = rx.recv() => {
                        match msg {
                            Some(OutMsg::Text(text)) => {
                                if sink.send(Message::Text(text)).await.is_err() {
                                    break;
                                }
                            }
                            Some(OutMsg::Binary(data)) => {
                                if sink.send(Message::Binary(data)).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = hb.tick() => {
                        if sink.send(Message::Ping(Vec::new())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        })
    };

    // Inbound loop: handle client frames until close/error.
    let st_in = st.clone();
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(frame)) = stream.next().await {
            match frame {
                Message::Text(txt) => {
                    handle_ws_message(&st_in, uid, &txt).await;
                }
                Message::Binary(data) => {
                    handle_ws_audio(uid, &data);
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => {}
            }
        }
    });

    // Whichever task ends first tears down the other.
    tokio::select! {
        _ = &mut send_task => { recv_task.abort(); }
        _ = &mut recv_task => { send_task.abort(); }
    }

    // Teardown: drop this connection; if it was the last, go offline + notify.
    let last = social_hub().unregister(uid, conn_id);
    if last {
        social_hub().drop_voice_for(uid);
        fanout_clear_online(uid); // remove from cross-instance registry
        set_presence(&st.db, uid, "offline", None, None, None).await;
        push_presence_diff(&st, uid).await;
    }
}

#[derive(Deserialize)]
struct WsEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    to: u64,
    #[serde(default)]
    text: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    #[serde(rename = "gameId")]
    game_id: String,
    #[serde(default)]
    #[serde(rename = "gameTitle")]
    game_title: String,
    #[serde(default)]
    payload: serde_json::Value,
    // Resume: highest social_messages.id the client already has. The server
    // backfills only messages newer than this, instead of the client re-pulling
    // every conversation's full history on reconnect.
    #[serde(default)]
    #[serde(rename = "afterMsgId")]
    after_msg_id: u64,
    // DM edit/delete (1.2): the target message id the sender wants to mutate.
    #[serde(default)]
    #[serde(rename = "msgId")]
    msg_id: u64,
    // Reactions (1.2b): emoji + on/off toggle for a "react" frame.
    #[serde(default)]
    emoji: String,
    #[serde(default)]
    on: bool,
    // Replies (1.2b): optional parent message id for a "chat" frame (0 = none).
    #[serde(default)]
    #[serde(rename = "replyTo")]
    reply_to: u64,
    // Offline-send idempotency (1.2b): client-generated dedup key for a "chat" frame.
    #[serde(default)]
    #[serde(rename = "clientNonce")]
    client_nonce: String,
    // DM attachment (1.3): id from /attachments/presign to link onto a "chat" frame.
    #[serde(default)]
    #[serde(rename = "attachmentId")]
    attachment_id: u64,
    // Presence depth (1.6): optional custom status text + DND flag for "presence".
    #[serde(default)]
    #[serde(rename = "statusText")]
    status_text: String,
    #[serde(default)]
    dnd: bool,
}

async fn handle_ws_message(st: &AppState, uid: u64, txt: &str) {
    let Ok(env) = serde_json::from_str::<WsEnvelope>(txt) else {
        return;
    };
    match env.kind.as_str() {
        "ping" => {
            // Pong only needs to reach this instance's socket — deliver locally
            // (no fan-out) and refresh the cross-instance online TTL.
            social_hub().deliver_local(uid, &serde_json::json!({ "type": "pong" }).to_string());
            fanout_refresh_online(uid);
        }
        "presence" => {
            // dnd is an alias for busy (ROADMAP 1.6). 'offline' is accepted so a
            // client can explicitly go dark without disconnecting.
            let mut state = match env.state.as_str() {
                "online" | "away" | "busy" | "invisible" | "ingame" | "offline" => env.state.as_str(),
                _ => "online",
            };
            if env.dnd {
                state = "busy";
            }
            let (gid, gtitle) = if state == "ingame" {
                (
                    (!env.game_id.is_empty()).then(|| env.game_id.as_str()),
                    (!env.game_title.is_empty()).then(|| env.game_title.as_str()),
                )
            } else {
                (None, None)
            };
            let status = (!env.status_text.is_empty())
                .then(|| env.status_text.chars().take(128).collect::<String>());
            set_presence(&st.db, uid, state, gid, gtitle, status.as_deref()).await;
            push_presence_diff(st, uid).await;
        }
        "typing" => {
            if env.to != 0 && are_friends(&st.db, uid, env.to).await {
                social_hub().push(
                    env.to,
                    &serde_json::json!({ "type": "typing", "fromId": uid }).to_string(),
                );
            }
        }
        "chat" => {
            handle_ws_chat(st, uid, env.to, &env.text, env.reply_to, &env.client_nonce, env.attachment_id).await;
        }
        "react" => {
            handle_ws_react(st, uid, env.msg_id, &env.emoji, env.on).await;
        }
        "resume" => {
            backfill_messages(st, uid, env.after_msg_id).await;
        }
        "read" => {
            // Mark every inbound message from env.to as read and tell that peer
            // (the original sender) so their UI can show a read receipt.
            mark_conversation_read(st, uid, env.to).await;
        }
        "edit" => {
            handle_ws_edit(st, uid, env.msg_id, &env.text).await;
        }
        "delete" => {
            handle_ws_delete(st, uid, env.msg_id).await;
        }
        "voice_signal" => {
            // Opaque relay for call invite/accept/end signaling frames. We also
            // gate the binary audio relay here: a friendship-verified invite or
            // accept opens the (uid,to) pair for audio; an end closes it. This
            // keeps the per-frame audio path off the database entirely.
            if env.to != 0 && are_friends(&st.db, uid, env.to).await {
                let kind = env
                    .payload
                    .get("kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                match kind {
                    "invite" | "accept" => social_hub().allow_voice(uid, env.to),
                    "end" => social_hub().disallow_voice(uid, env.to),
                    _ => {}
                }
                social_hub().push(
                    env.to,
                    &serde_json::json!({
                        "type": "voice_signal",
                        "fromId": uid,
                        "payload": env.payload,
                    })
                    .to_string(),
                );
            }
        }
        _ => {}
    }
}

async fn handle_ws_chat(
    st: &AppState,
    uid: u64,
    to: u64,
    text: &str,
    reply_to: u64,
    client_nonce: &str,
    attachment_id: u64,
) {
    // An attachment-only message (no text) is allowed when an attachment rides along.
    if to == 0 || (text.is_empty() && attachment_id == 0) {
        return;
    }
    let trimmed: String = text.chars().take(4000).collect();
    if is_blocked_either(&st.db, uid, to).await {
        return;
    }
    // DM privacy (ROADMAP 1.1b): honour the recipient's dm_policy, and drop if the
    // recipient has persistently ignored the sender.
    match dm_policy_of(&st.db, to).await.as_str() {
        "nobody" => return,
        "friends" => {
            if !are_friends(&st.db, uid, to).await {
                return;
            }
        }
        _ => {}
    }
    if has_ignored(&st.db, to, uid).await {
        return;
    }
    let nonce_opt = (!client_nonce.is_empty()).then(|| client_nonce.chars().take(40).collect::<String>());
    let reply_opt: Option<u64> = (reply_to > 0).then_some(reply_to);
    let ts = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(_) => return,
    };
    // Offline-send idempotency (ROADMAP 1.2b): if this sender already stored a row
    // with this client_nonce, skip the insert and re-broadcast the existing row so
    // the client can reconcile its optimistic message instead of duplicating it.
    let (msg_id, ts, reply_final): (u64, i64, Option<u64>) = if let Some(ref n) = nonce_opt {
        let existing: Option<(u64, i64, Option<u64>)> = c
            .exec_first(
                "SELECT id, created_at, reply_to FROM social_messages WHERE sender_id=:s AND client_nonce=:n LIMIT 1",
                params! {"s" => uid, "n" => n},
            )
            .await
            .ok()
            .flatten();
        if let Some((id, ets, rt)) = existing {
            (id, ets, rt)
        } else {
            let ins = c
                .exec_iter(
                    "INSERT INTO social_messages (sender_id, receiver_id, body, created_at, reply_to, client_nonce) VALUES (:s, :r, :b, :t, :rt, :n)",
                    params! {"s" => uid, "r" => to, "b" => &trimmed, "t" => ts, "rt" => reply_opt, "n" => n},
                )
                .await;
            match ins {
                Ok(r) => (r.last_insert_id().unwrap_or(0), ts, reply_opt),
                Err(_) => return,
            }
        }
    } else {
        let ins = c
            .exec_iter(
                "INSERT INTO social_messages (sender_id, receiver_id, body, created_at, reply_to) VALUES (:s, :r, :b, :t, :rt)",
                params! {"s" => uid, "r" => to, "b" => &trimmed, "t" => ts, "rt" => reply_opt},
            )
            .await;
        match ins {
            Ok(r) => (r.last_insert_id().unwrap_or(0), ts, reply_opt),
            Err(_) => return,
        }
    };
    // Link a presigned attachment to this message (owner-checked) so the GET
    // endpoint will authorize both DM participants for it.
    let att_id = if attachment_id > 0 && link_attachment(st, uid, attachment_id, msg_id).await {
        attachment_id
    } else {
        0
    };
    let mut evt = serde_json::json!({
        "type": "chat",
        "messageId": msg_id,
        "senderId": uid,
        "receiverId": to,
        "text": trimmed,
        "timestamp": ts,
        "replyTo": reply_final,
    });
    if let Some(ref n) = nonce_opt {
        evt["clientNonce"] = serde_json::Value::String(n.clone());
    }
    if att_id > 0 {
        evt["attachmentId"] = serde_json::Value::from(att_id);
    }
    let evt = evt.to_string();
    // Deliver to recipient (if online) and echo back to sender for ack + multi-device sync.
    social_hub().push(to, &evt);
    social_hub().push(uid, &evt);
}

// Toggle a reaction on a message (ROADMAP 1.2b). Only the sender or receiver of
// the message may react; the reaction event is broadcast to both parties.
async fn handle_ws_react(st: &AppState, uid: u64, msg_id: u64, emoji: &str, on: bool) {
    if msg_id == 0 || emoji.is_empty() {
        return;
    }
    let emoji: String = emoji.chars().take(32).collect();
    let Ok(mut c) = st.db.get_conn().await else { return; };
    let parties: Option<(u64, u64)> = c
        .exec_first(
            "SELECT sender_id, receiver_id FROM social_messages WHERE id=:id",
            params! {"id" => msg_id},
        )
        .await
        .ok()
        .flatten();
    let Some((sender, receiver)) = parties else { return; };
    if uid != sender && uid != receiver {
        return;
    }
    if on {
        let _ = c
            .exec_drop(
                "INSERT IGNORE INTO social_message_reactions (message_id, user_id, emoji, created_at) VALUES (:m, :u, :e, :t)",
                params! {"m" => msg_id, "u" => uid, "e" => &emoji, "t" => now()},
            )
            .await;
    } else {
        let _ = c
            .exec_drop(
                "DELETE FROM social_message_reactions WHERE message_id=:m AND user_id=:u AND emoji=:e",
                params! {"m" => msg_id, "u" => uid, "e" => &emoji},
            )
            .await;
    }
    let evt = serde_json::json!({
        "type": "reaction",
        "messageId": msg_id,
        "userId": uid,
        "emoji": emoji,
        "on": on,
    })
    .to_string();
    social_hub().push(sender, &evt);
    social_hub().push(receiver, &evt);
}

// Build message_id -> [{emoji,userId}] for a page of messages (ROADMAP 1.2b).
async fn reactions_for_messages(
    db: &Pool,
    ids: &[u64],
) -> std::collections::HashMap<u64, Vec<serde_json::Value>> {
    let mut map: std::collections::HashMap<u64, Vec<serde_json::Value>> = std::collections::HashMap::new();
    if ids.is_empty() {
        return map;
    }
    let Ok(mut c) = db.get_conn().await else { return map; };
    let list = ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT message_id, user_id, emoji FROM social_message_reactions WHERE message_id IN ({list})"
    );
    let rows: Vec<(u64, u64, String)> = c.query(sql).await.unwrap_or_default();
    for (mid, uid, emoji) in rows {
        map.entry(mid)
            .or_default()
            .push(serde_json::json!({ "emoji": emoji, "userId": uid }));
    }
    map
}

// Mark all messages `peer`→`me` as read and notify the peer with a read receipt
// carrying the highest message id now read, so their client can flag the thread.
async fn mark_conversation_read(st: &AppState, me: u64, peer: u64) {
    if peer == 0 {
        return;
    }
    let Ok(mut c) = st.db.get_conn().await else { return; };
    let up_to: Option<u64> = c
        .exec_first(
            "SELECT MAX(id) FROM social_messages WHERE receiver_id=:me AND sender_id=:peer",
            params! {"me" => me, "peer" => peer},
        )
        .await
        .ok()
        .flatten();
    let _ = c
        .exec_drop(
            "UPDATE social_messages SET read_at=:t WHERE receiver_id=:me AND sender_id=:peer AND read_at IS NULL",
            params! {"t" => now(), "me" => me, "peer" => peer},
        )
        .await;
    social_hub().push(
        peer,
        &serde_json::json!({
            "type": "read",
            "readerId": me,
            "upToId": up_to.unwrap_or(0),
        })
        .to_string(),
    );
}

// Edit a message: only the original sender may edit, and only if it isn't already
// deleted. Broadcasts a chat_edit to both parties so every client converges.
async fn handle_ws_edit(st: &AppState, uid: u64, msg_id: u64, text: &str) {
    if msg_id == 0 || text.is_empty() {
        return;
    }
    let trimmed: String = text.chars().take(4000).collect();
    let ts = now();
    let Ok(mut c) = st.db.get_conn().await else { return; };
    let row: Option<(u64, u64)> = c
        .exec_first(
            "SELECT sender_id, receiver_id FROM social_messages WHERE id=:id AND sender_id=:uid AND deleted_at IS NULL",
            params! {"id" => msg_id, "uid" => uid},
        )
        .await
        .ok()
        .flatten();
    let Some((sender, receiver)) = row else { return; };
    let _ = c
        .exec_drop(
            "UPDATE social_messages SET body=:b, edited_at=:t WHERE id=:id",
            params! {"b" => &trimmed, "t" => ts, "id" => msg_id},
        )
        .await;
    let evt = serde_json::json!({
        "type": "chat_edit",
        "messageId": msg_id,
        "text": trimmed,
        "editedAt": ts,
    })
    .to_string();
    social_hub().push(receiver, &evt);
    social_hub().push(sender, &evt);
}

// Delete a message: only the sender may delete. The row is tombstoned
// (deleted_at set, body blanked) and a chat_delete is broadcast to both parties.
async fn handle_ws_delete(st: &AppState, uid: u64, msg_id: u64) {
    if msg_id == 0 {
        return;
    }
    let ts = now();
    let Ok(mut c) = st.db.get_conn().await else { return; };
    let row: Option<(u64, u64)> = c
        .exec_first(
            "SELECT sender_id, receiver_id FROM social_messages WHERE id=:id AND sender_id=:uid AND deleted_at IS NULL",
            params! {"id" => msg_id, "uid" => uid},
        )
        .await
        .ok()
        .flatten();
    let Some((sender, receiver)) = row else { return; };
    let _ = c
        .exec_drop(
            "UPDATE social_messages SET body='', deleted_at=:t WHERE id=:id",
            params! {"t" => ts, "id" => msg_id},
        )
        .await;
    let evt = serde_json::json!({
        "type": "chat_delete",
        "messageId": msg_id,
    })
    .to_string();
    social_hub().push(receiver, &evt);
    social_hub().push(sender, &evt);
}

// Resume backfill: send every message involving `uid` newer than `after_id` in a
// single `chat_backfill` batch. Replaces the reconnecting client's per-conversation
// full re-pull — only the genuinely missed tail crosses the wire. Capped so a very
// stale client falls back gracefully (it can still REST-pull a conversation).
async fn backfill_messages(st: &AppState, uid: u64, after_id: u64) {
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(_) => return,
    };
    let rows: Vec<(u64, u64, u64, String, i64, Option<i64>)> = match c
        .exec(
            r#"SELECT id, sender_id, receiver_id, body, created_at, read_at
               FROM social_messages
               WHERE (sender_id=:u OR receiver_id=:u) AND id > :since
               ORDER BY id ASC LIMIT 500"#,
            params! {"u" => uid, "since" => after_id},
        )
        .await
    {
        Ok(r) => r,
        Err(_) => return,
    };
    let msgs: Vec<_> = rows
        .into_iter()
        .map(|(id, sndr, rcvr, body, ts, read_at)| {
            serde_json::json!({
                "messageId": id,
                "senderId": sndr,
                "receiverId": rcvr,
                "text": body,
                "timestamp": ts,
                "isRead": read_at.is_some(),
            })
        })
        .collect::<Vec<_>>();
    social_hub().push(
        uid,
        &serde_json::json!({ "type": "chat_backfill", "messages": msgs }).to_string(),
    );
}

// Relay a binary voice frame from `uid` to its call peer. Wire format from the
// client is [u64 LE target_id][payload]; we replace the header with the sender
// id and forward to the target only if the pair has an open, verified call.
// O(1) hot path: a HashSet membership check, no DB and no JSON.
fn handle_ws_audio(uid: u64, data: &[u8]) {
    if data.len() < 8 {
        return;
    }
    let target = u64::from_le_bytes(data[0..8].try_into().unwrap());
    if target == 0 || !social_hub().voice_allowed(uid, target) {
        return;
    }
    let mut frame = Vec::with_capacity(data.len());
    frame.extend_from_slice(&uid.to_le_bytes());
    frame.extend_from_slice(&data[8..]);
    social_hub().push_binary(target, &frame);
}

// ----------------------------------------------------------------------------
// REST: library tracking (ROADMAP 2.4) — per-account playtime / last-played /
// completion / rating, synced across devices. game_id is the client's stable
// catalog id. All endpoints are self-scoped (the caller's own account only).
// ----------------------------------------------------------------------------

// GET /api/library/stats — every stats row for the caller.
async fn api_library_stats(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let rows: Vec<(String, i64, i64, i64, i8, i8)> = match c
        .exec(
            r#"SELECT game_id, playtime_seconds, last_played, play_count, completion, rating
               FROM game_stats WHERE user_id=:id"#,
            params! {"id" => me.id},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
    let out: Vec<_> = rows
        .into_iter()
        .map(|(game_id, secs, last, count, comp, rating)| {
            serde_json::json!({
                "gameId": game_id,
                "playtimeSeconds": secs,
                "lastPlayed": last,
                "playCount": count,
                "completion": comp,
                "rating": rating,
            })
        })
        .collect();
    Json(serde_json::json!({ "stats": out })).into_response()
}

#[derive(Deserialize)]
struct PlaytimeBody {
    #[serde(rename = "gameId")]
    game_id: String,
    seconds: i64,
}

// POST /api/library/playtime — add a completed session's seconds to a game,
// bump last_played + play_count. Upserts the row. Ignores non-positive seconds
// and absurd values (cap one session at 24h to blunt a stuck-timer client).
async fn api_library_playtime(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PlaytimeBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let gid: String = body.game_id.chars().take(80).collect();
    if gid.is_empty() {
        return (StatusCode::BAD_REQUEST, "gameId required").into_response();
    }
    let secs = body.seconds.clamp(0, 24 * 3600);
    if secs <= 0 {
        return Json(serde_json::json!({ "status": "ok", "ignored": true })).into_response();
    }
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let now = now();
    if let Err(e) = c
        .exec_drop(
            r#"INSERT INTO game_stats
                 (user_id, game_id, playtime_seconds, last_played, play_count, updated_at)
               VALUES (:id, :gid, :secs, :now, 1, :now)
               ON DUPLICATE KEY UPDATE
                 playtime_seconds = playtime_seconds + :secs,
                 last_played = :now,
                 play_count = play_count + 1,
                 updated_at = :now"#,
            params! {"id" => me.id, "gid" => &gid, "secs" => secs, "now" => now},
        )
        .await
    {
        return server_error(e);
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

#[derive(Deserialize)]
struct RatingBody {
    #[serde(rename = "gameId")]
    game_id: String,
    #[serde(default)]
    rating: Option<i8>,
    #[serde(default)]
    completion: Option<i8>,
}

// POST /api/library/rating — set the caller's rating (0-5) and/or completion
// flag (0/1) for a game. Either field may be supplied alone. Upserts the row.
async fn api_library_rating(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RatingBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let gid: String = body.game_id.chars().take(80).collect();
    if gid.is_empty() {
        return (StatusCode::BAD_REQUEST, "gameId required").into_response();
    }
    let rating = body.rating.map(|r| r.clamp(0, 5));
    let completion = body.completion.map(|c| if c != 0 { 1 } else { 0 });
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let now = now();
    // Ensure a row exists first, then update only supplied columns.
    let _ = c
        .exec_drop(
            "INSERT IGNORE INTO game_stats (user_id, game_id, updated_at) VALUES (:id, :gid, :now)",
            params! {"id" => me.id, "gid" => &gid, "now" => now},
        )
        .await;
    if let Some(r) = rating {
        let _ = c
            .exec_drop(
                "UPDATE game_stats SET rating=:r, updated_at=:now WHERE user_id=:id AND game_id=:gid",
                params! {"r" => r, "now" => now, "id" => me.id, "gid" => &gid},
            )
            .await;
    }
    if let Some(comp) = completion {
        let _ = c
            .exec_drop(
                "UPDATE game_stats SET completion=:c, updated_at=:now WHERE user_id=:id AND game_id=:gid",
                params! {"c" => comp, "now" => now, "id" => me.id, "gid" => &gid},
            )
            .await;
    }
    Json(serde_json::json!({ "status": "ok" })).into_response()
}
