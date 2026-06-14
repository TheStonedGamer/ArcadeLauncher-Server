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
    Ok(())
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

    // Non-blocking push to every live socket for `user_id`. Dead senders are
    // dropped lazily by the receive loop's unregister; try_send never blocks.
    fn push(&self, user_id: u64, msg: &str) {
        let g = self.conns.lock().unwrap();
        if let Some(v) = g.get(&user_id) {
            for c in v {
                let _ = c.tx.send(OutMsg::Text(msg.to_string()));
            }
        }
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

async fn presence_of(db: &Pool, user_id: u64, online_hint: bool) -> (String, Option<String>, Option<String>) {
    let Ok(mut c) = db.get_conn().await else {
        return ("offline".into(), None, None);
    };
    let row: Option<(String, Option<String>, Option<String>, i64)> = c
        .exec_first(
            "SELECT state, game_id, game_title, updated_at FROM social_presence WHERE user_id=:id",
            params! {"id" => user_id},
        )
        .await
        .ok()
        .flatten();
    match row {
        Some((state, gid, gtitle, upd)) => {
            let fresh = online_hint || (now() - upd) < PRESENCE_STALE_SECS;
            if !fresh || state == "invisible" {
                ("offline".into(), None, None)
            } else {
                (state, gid, gtitle)
            }
        }
        None => ("offline".into(), None, None),
    }
}

async fn set_presence(db: &Pool, user_id: u64, state: &str, game_id: Option<&str>, game_title: Option<&str>) {
    let Ok(mut c) = db.get_conn().await else { return; };
    let _ = c
        .exec_drop(
            r#"INSERT INTO social_presence (user_id, state, game_id, game_title, updated_at)
               VALUES (:id, :s, :gi, :gt, :t)
               ON DUPLICATE KEY UPDATE state=:s, game_id=:gi, game_title=:gt, updated_at=:t"#,
            params! {"id" => user_id, "s" => state, "gi" => game_id, "gt" => game_title, "t" => now()},
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
    let online = social_hub().is_online(user_id);
    let (state, gid, gtitle) = presence_of(&st.db, user_id, online).await;
    let evt = serde_json::json!({
        "type": "presence",
        "userId": user_id,
        "state": state,
        "gameId": gid,
        "gameTitle": gtitle,
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
        let online = social_hub().is_online(other);
        let (pstate, gid, gtitle) = presence_of(&st.db, other, online).await;
        out.push(serde_json::json!({
            "accountId": other,
            "username": uname,
            "relation": relation,
            "presence": pstate,
            "currentGameId": gid,
            "currentGameTitle": gtitle,
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
    if let Err(e) = c
        .exec_drop(
            r#"INSERT INTO social_friendships (user_lo, user_hi, status, requested_by, created_at, updated_at)
               VALUES (:lo, :hi, 'pending', :rb, :t, :t)"#,
            params! {"lo" => lo, "hi" => hi, "rb" => me.id, "t" => now()},
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
    action: String, // accept | decline | cancel | remove
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
// REST: message history
// ----------------------------------------------------------------------------

#[derive(Deserialize)]
struct HistoryQuery {
    #[serde(default)]
    since: u64,
    #[serde(default = "default_limit")]
    limit: u64,
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
    let rows: Vec<(u64, u64, u64, String, i64, Option<i64>)> = match c
        .exec(
            r#"SELECT id, sender_id, receiver_id, body, created_at, read_at
               FROM social_messages
               WHERE ((sender_id=:me AND receiver_id=:other) OR (sender_id=:other AND receiver_id=:me))
                 AND id > :since
               ORDER BY id DESC LIMIT :lim"#,
            params! {"me" => me.id, "other" => other, "since" => q.since, "lim" => limit},
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return server_error(e),
    };
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
        .collect();
    Json(serde_json::json!({ "messages": msgs })).into_response()
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
        set_presence(&st.db, uid, "online", None, None).await;
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
        set_presence(&st.db, uid, "offline", None, None).await;
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
}

async fn handle_ws_message(st: &AppState, uid: u64, txt: &str) {
    let Ok(env) = serde_json::from_str::<WsEnvelope>(txt) else {
        return;
    };
    match env.kind.as_str() {
        "ping" => {
            social_hub().push(uid, &serde_json::json!({ "type": "pong" }).to_string());
        }
        "presence" => {
            let state = match env.state.as_str() {
                "online" | "away" | "busy" | "invisible" | "ingame" => env.state.as_str(),
                _ => "online",
            };
            let (gid, gtitle) = if state == "ingame" {
                (
                    (!env.game_id.is_empty()).then(|| env.game_id.as_str()),
                    (!env.game_title.is_empty()).then(|| env.game_title.as_str()),
                )
            } else {
                (None, None)
            };
            set_presence(&st.db, uid, state, gid, gtitle).await;
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
            handle_ws_chat(st, uid, env.to, &env.text).await;
        }
        "resume" => {
            backfill_messages(st, uid, env.after_msg_id).await;
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

async fn handle_ws_chat(st: &AppState, uid: u64, to: u64, text: &str) {
    if to == 0 || text.is_empty() {
        return;
    }
    let trimmed: String = text.chars().take(4000).collect();
    if is_blocked_either(&st.db, uid, to).await {
        return;
    }
    let ts = now();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(_) => return,
    };
    let ins = c
        .exec_iter(
            "INSERT INTO social_messages (sender_id, receiver_id, body, created_at) VALUES (:s, :r, :b, :t)",
            params! {"s" => uid, "r" => to, "b" => &trimmed, "t" => ts},
        )
        .await;
    let msg_id = match ins {
        Ok(r) => r.last_insert_id().unwrap_or(0),
        Err(_) => return,
    };
    let evt = serde_json::json!({
        "type": "chat",
        "messageId": msg_id,
        "senderId": uid,
        "receiverId": to,
        "text": trimmed,
        "timestamp": ts,
    })
    .to_string();
    // Deliver to recipient (if online) and echo back to sender for ack + multi-device sync.
    social_hub().push(to, &evt);
    social_hub().push(uid, &evt);
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
