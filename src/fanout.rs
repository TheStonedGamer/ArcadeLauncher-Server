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

// The device registry is a hash per user, field = publishing instance id. Redis
// cannot expire individual hash fields, so freshness is decided on read from the
// timestamp inside each entry (see `merge_device_registry`); this TTL only stops
// the key itself outliving an account that never signs in again.
const DEVICES_KEY_TTL_SECS: u64 = 3600;

fn devices_key(user_id: u64) -> String {
    format!("social:devices:{user_id}")
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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

/// True when frames can reach other instances at all.
fn fanout_enabled() -> bool {
    fanout().is_some()
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
                            match env.device.as_deref() {
                                // Addressed to one machine (remote install): only
                                // the instance actually holding that socket
                                // delivers, the rest no-op.
                                Some(device_id) => {
                                    social_hub().send_to_device(env.user_id, device_id, &env.frame);
                                }
                                None => social_hub().deliver_local(env.user_id, &env.frame),
                            }
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
    // Some(device_id) narrows delivery to a single machine; None means every
    // socket the account has on the receiving instance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device: Option<String>,
}

// Publish a text frame to peer instances (no-op when Redis is disabled). Never
// blocks: serializes and enqueues onto the publisher task.
fn fanout_publish(user_id: u64, frame: &str) {
    fanout_publish_inner(user_id, frame, None);
}

// Publish a frame meant for one machine of the account. Used by remote install
// when the target PC's socket lives on another instance.
fn fanout_publish_device(user_id: u64, device_id: &str, frame: &str) {
    fanout_publish_inner(user_id, frame, Some(device_id.to_string()));
}

fn fanout_publish_inner(user_id: u64, frame: &str, device: Option<String>) {
    if let Some(f) = fanout() {
        let env = FanoutEnvelope {
            origin: f.instance,
            user_id,
            frame: frame.to_string(),
            device,
        };
        if let Ok(payload) = serde_json::to_string(&env) {
            let _ = f.tx.send(payload);
        }
    }
}

// Publish (or, when the list is empty, withdraw) this instance's device list for
// one user. Fire-and-forget: a missed write costs at most one stale picker until
// the next heartbeat rewrites it.
fn fanout_set_devices(user_id: u64, devices: &[Device]) {
    let Some(f) = fanout() else { return };
    let mut conn = f.conn.clone();
    let field = f.instance.to_string();
    let key = devices_key(user_id);
    if devices.is_empty() {
        tokio::spawn(async move {
            let _: redis::RedisResult<i64> = conn.hdel(key, field).await;
        });
        return;
    }
    let entry = DeviceRegistryEntry {
        at: unix_now(),
        devices: devices.to_vec(),
    };
    let Ok(payload) = serde_json::to_string(&entry) else { return };
    tokio::spawn(async move {
        let _: redis::RedisResult<i64> = conn.hset(&key, field, payload).await;
        let _: redis::RedisResult<bool> = conn.expire(&key, DEVICES_KEY_TTL_SECS as i64).await;
    });
}

// Every machine of this account the cluster can currently reach: `local` (this
// instance's live sockets, always trusted) unioned with what peers published.
// Falls back to `local` alone whenever Redis is off or unreachable, which is
// exactly the old single-instance behavior.
async fn fanout_devices(user_id: u64, local: Vec<Device>) -> Vec<Device> {
    let Some(f) = fanout() else {
        return collapse_devices(local);
    };
    let mut conn = f.conn.clone();
    let raw: std::collections::HashMap<String, String> = match conn.hgetall(devices_key(user_id)).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("redis: device registry read failed ({e}); using local devices only");
            return collapse_devices(local);
        }
    };
    let mut entries: Vec<DeviceRegistryEntry> = raw
        .iter()
        // Our own field is ignored: `local` is the live truth for this instance
        // and is never stale, whereas our published copy can lag a disconnect.
        .filter(|(field, _)| *field != &f.instance.to_string())
        .filter_map(|(_, v)| serde_json::from_str(v).ok())
        .collect();
    entries.push(DeviceRegistryEntry {
        at: unix_now(),
        devices: local,
    });
    merge_device_registry(entries, unix_now(), ONLINE_TTL_SECS)
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
