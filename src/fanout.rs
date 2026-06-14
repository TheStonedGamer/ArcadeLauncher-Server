// Cross-instance fan-out for the social gateway (ROADMAP 0.4).
//
// The gateway's live socket map (`social_hub`) is in-process: a frame pushed on
// instance A only reaches sockets connected to A. To let the server scale beyond
// one instance we add an *optional* Redis pub/sub bus and a cross-instance online
// registry. When `ARCADE_REDIS_URL` is unset everything degrades to the previous
// single-instance behavior (no Redis dependency at runtime).
//
// Design:
//   * Every text `push` still delivers to local sockets immediately (lowest
//     latency for the common same-instance case) and *additionally* publishes the
//     frame on the `social:fanout` channel. Each instance runs a subscriber that
//     delivers frames originating from *other* instances to its own local sockets.
//     Frames are tagged with a per-process instance id so we never double-deliver.
//   * Online presence across instances is tracked with a per-user Redis key
//     (`social:online:<uid>`) carrying a TTL that the gateway heartbeat refreshes;
//     `presence_online` checks the local hub first, then Redis.
//   * Binary voice audio stays local-only (1:1, hot path); cross-instance voice
//     is deferred to the voice-v2 work (ROADMAP 2.1–2.3).

use redis::AsyncCommands;

// A user is considered online in Redis for this long after the last heartbeat
// refresh; must exceed the client's ~20s app-level ping cadence with margin.
const ONLINE_TTL_SECS: u64 = 75;
const FANOUT_CHANNEL: &str = "social:fanout";

struct Fanout {
    instance: u64,
    // Connection manager for publishes + online-key ops (cheap to clone).
    conn: redis::aio::ConnectionManager,
    // Non-blocking publish queue: the sync `push` path enqueues here so it never
    // awaits Redis; a background task drains and PUBLISHes.
    tx: tokio::sync::mpsc::UnboundedSender<String>,
}

static FANOUT: std::sync::OnceLock<Option<Fanout>> = std::sync::OnceLock::new();

fn fanout() -> Option<&'static Fanout> {
    FANOUT.get().and_then(|o| o.as_ref())
}

// Initialize the fan-out bus. Called once from main when a Redis URL is set; a
// connection failure is non-fatal (we log and stay single-instance).
async fn init_fanout(url: &str) {
    let client = match redis::Client::open(url) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("redis: invalid ARCADE_REDIS_URL ({e}); staying single-instance");
            let _ = FANOUT.set(None);
            return;
        }
    };
    let conn = match redis::aio::ConnectionManager::new(client.clone()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("redis: connect failed ({e}); staying single-instance");
            let _ = FANOUT.set(None);
            return;
        }
    };
    let instance: u64 = rand::random();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Publisher drain task.
    {
        let mut pub_conn = conn.clone();
        tokio::spawn(async move {
            while let Some(payload) = rx.recv().await {
                let _: redis::RedisResult<i64> = pub_conn.publish(FANOUT_CHANNEL, payload).await;
            }
        });
    }

    // Subscriber task: deliver frames from *other* instances to local sockets.
    {
        tokio::spawn(async move {
            loop {
                match client.get_async_pubsub().await {
                    Ok(mut pubsub) => {
                        if pubsub.subscribe(FANOUT_CHANNEL).await.is_err() {
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            continue;
                        }
                        tracing::info!("redis: social fan-out subscriber connected");
                        use futures_util::StreamExt;
                        let mut stream = pubsub.on_message();
                        while let Some(msg) = stream.next().await {
                            let Ok(payload) = msg.get_payload::<String>() else { continue };
                            let Ok(env) = serde_json::from_str::<FanoutEnvelope>(&payload) else {
                                continue;
                            };
                            if env.origin == instance {
                                continue; // we already delivered locally
                            }
                            social_hub().deliver_local(env.user_id, &env.frame);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("redis: pubsub connect failed ({e}); retrying");
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
    }

    let _ = FANOUT.set(Some(Fanout { instance, conn, tx }));
    tracing::info!("redis: social fan-out enabled (instance {instance:#x})");
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FanoutEnvelope {
    origin: u64,
    #[serde(rename = "userId")]
    user_id: u64,
    frame: String,
}

// Publish a text frame to peer instances (no-op when Redis is disabled). Never
// blocks: serializes and enqueues onto the publisher task.
fn fanout_publish(user_id: u64, frame: &str) {
    if let Some(f) = fanout() {
        let env = FanoutEnvelope {
            origin: f.instance,
            user_id,
            frame: frame.to_string(),
        };
        if let Ok(payload) = serde_json::to_string(&env) {
            let _ = f.tx.send(payload);
        }
    }
}

// Mark / refresh / clear a user's cross-instance online key. Fire-and-forget;
// failures are tolerated (presence also has a DB staleness fallback).
fn fanout_set_online(user_id: u64) {
    if let Some(f) = fanout() {
        let mut conn = f.conn.clone();
        tokio::spawn(async move {
            let key = format!("social:online:{user_id}");
            let _: redis::RedisResult<()> = conn.set_ex(key, 1u8, ONLINE_TTL_SECS).await;
        });
    }
}

fn fanout_refresh_online(user_id: u64) {
    // SET with EX again simply pushes the TTL forward.
    fanout_set_online(user_id);
}

fn fanout_clear_online(user_id: u64) {
    if let Some(f) = fanout() {
        let mut conn = f.conn.clone();
        tokio::spawn(async move {
            let key = format!("social:online:{user_id}");
            let _: redis::RedisResult<i64> = conn.del(key).await;
        });
    }
}

// True if the user has a live socket anywhere in the cluster. Checks the local
// hub first (fast, authoritative for this instance), then the Redis registry.
async fn presence_online(user_id: u64) -> bool {
    if social_hub().is_online(user_id) {
        return true;
    }
    if let Some(f) = fanout() {
        let mut conn = f.conn.clone();
        let key = format!("social:online:{user_id}");
        return conn.exists(key).await.unwrap_or(false);
    }
    false
}
