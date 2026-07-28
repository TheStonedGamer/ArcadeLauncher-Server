// The IO half of push: device-token storage, the OAuth exchange with Google,
// and the FCM send. Every decision worth arguing about is in push.rs and tested
// there; this file is the plumbing around it.
//
// Configuration is a single environment variable, ARCADE_FCM_SERVICE_ACCOUNT,
// holding either the path to the service-account JSON or the JSON itself. When
// it is unset the whole feature is dormant: registration still succeeds (so a
// phone does not have to know whether the server can push), sends are skipped,
// and calling an offline friend behaves exactly as it did before.

use jsonwebtoken::{Algorithm, EncodingKey, Header};

/// Everything needed to send, resolved once at startup.
struct PushService {
    account: ServiceAccount,
    key: EncodingKey,
    http: reqwest::Client,
    /// (access token, expiry epoch). Shared by every send; refreshed in place.
    token: tokio::sync::Mutex<(String, i64)>,
}

static PUSH: std::sync::OnceLock<Option<PushService>> = std::sync::OnceLock::new();

fn push_service() -> Option<&'static PushService> {
    PUSH.get_or_init(|| {
        let raw = std::env::var("ARCADE_FCM_SERVICE_ACCOUNT").ok()?;
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            return None;
        }
        // A path is the ordinary case (a mounted secret); inline JSON is
        // accepted so a container can be configured without a volume.
        let json = if raw.starts_with('{') {
            raw
        } else {
            match std::fs::read_to_string(&raw) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("push: cannot read ARCADE_FCM_SERVICE_ACCOUNT ({raw}): {e}");
                    return None;
                }
            }
        };
        let account = match parse_service_account(&json) {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("push: {e}");
                return None;
            }
        };
        let key = match EncodingKey::from_rsa_pem(account.private_key.as_bytes()) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!("push: service-account private key is unusable: {e}");
                return None;
            }
        };
        tracing::info!(
            "push: FCM enabled for project {} ({})",
            account.project_id,
            account.client_email
        );
        Some(PushService {
            account,
            key,
            http: reqwest::Client::new(),
            token: tokio::sync::Mutex::new((String::new(), 0)),
        })
    })
    .as_ref()
}

/// True when the server can actually wake a phone. Callers use this to decide
/// whether an offline callee is unreachable or merely asleep.
fn push_enabled() -> bool {
    push_service().is_some()
}

impl PushService {
    /// A valid access token, minted or reused. Held under a mutex so a burst of
    /// calls performs one OAuth round-trip rather than one each.
    async fn access_token(&self) -> Option<String> {
        let mut guard = self.token.lock().await;
        if access_token_is_fresh(guard.1, now()) && !guard.0.is_empty() {
            return Some(guard.0.clone());
        }
        let claims = token_claims(&self.account, now());
        let assertion =
            jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.key).ok()?;
        let resp = self
            .http
            .post(&self.account.token_uri)
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", &assertion),
            ])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!("push: token exchange failed ({})", resp.status());
            return None;
        }
        let body: serde_json::Value = resp.json().await.ok()?;
        let token = body.get("access_token")?.as_str()?.to_string();
        let ttl = body.get("expires_in").and_then(|v| v.as_i64()).unwrap_or(3600);
        *guard = (token.clone(), now() + ttl);
        Some(token)
    }
}

/// Send one call notification. Returns true if FCM accepted it — that is the
/// signal the caller uses to decide the callee is reachable after all.
async fn push_call(st: &AppState, callee: u64, caller_id: u64, video: bool) -> bool {
    let Some(svc) = push_service() else { return false };
    let tokens = load_push_tokens(st, callee).await;
    if tokens.is_empty() {
        return false;
    }
    let Some(access) = svc.access_token().await else { return false };
    let caller_name = display_name_of(st, caller_id).await;
    let endpoint = send_endpoint(&svc.account);
    let mut delivered = false;
    for token in tokens {
        let body = call_push_message(&token, caller_id, &caller_name, video);
        let resp = svc
            .http
            .post(&endpoint)
            .bearer_auth(&access)
            .json(&body)
            .send()
            .await;
        match resp {
            Ok(r) if r.status().is_success() => delivered = true,
            Ok(r) => {
                let status = r.status().as_u16();
                let text = r.text().await.unwrap_or_default();
                if token_is_dead(status, &text) {
                    // The phone was wiped, reinstalled or revoked. Keeping the
                    // row would mean retrying a dead token on every call.
                    forget_push_token(st, callee, token.as_str()).await;
                } else {
                    tracing::warn!("push: send failed ({status}): {text}");
                }
            }
            Err(e) => tracing::warn!("push: send error: {e}"),
        }
    }
    delivered
}

// ----------------------------------------------------------------------------
// Admin-originated alerts: a test send, and a broadcast to everyone
// ----------------------------------------------------------------------------

/// Send one alert to one account's devices.
async fn push_alert_to_user(st: &AppState, user_id: u64, alert: &Alert) -> SendReport {
    let tokens = load_push_tokens(st, user_id).await;
    push_alert_to_tokens(st, &[(user_id, tokens)], alert).await
}

/// Send one alert to every registered device on the server.
///
/// Sends are sequential on purpose. A broadcast is rare and never urgent, and a
/// burst of parallel requests is the reliable way to meet FCM's rate limits.
async fn push_alert_everyone(st: &AppState, alert: &Alert) -> SendReport {
    push_alert_to_tokens(st, &load_all_push_tokens(st).await, alert).await
}

/// The shared send loop. `groups` is per-account so the report can say how many
/// people were reached, not just how many handsets.
async fn push_alert_to_tokens(
    st: &AppState,
    groups: &[(u64, Vec<PushToken>)],
    alert: &Alert,
) -> SendReport {
    let mut report = SendReport::default();
    for (_, tokens) in groups {
        if !tokens.is_empty() {
            report.accounts += 1;
            report.devices += tokens.len();
        }
    }
    if report.devices == 0 {
        return report;
    }
    let Some(svc) = push_service() else { return report };
    let Some(access) = svc.access_token().await else { return report };
    let endpoint = send_endpoint(&svc.account);
    for (user_id, tokens) in groups {
        for token in tokens {
            let body = alert_push_message(token, alert);
            match svc.http.post(&endpoint).bearer_auth(&access).json(&body).send().await {
                Ok(r) if r.status().is_success() => report.delivered += 1,
                Ok(r) => {
                    let status = r.status().as_u16();
                    let text = r.text().await.unwrap_or_default();
                    if token_is_dead(status, &text) {
                        forget_push_token(st, *user_id, token.as_str()).await;
                        report.dead += 1;
                    } else {
                        tracing::warn!("push: alert send failed ({status}): {text}");
                    }
                }
                Err(e) => tracing::warn!("push: alert send error: {e}"),
            }
        }
    }
    report
}

/// Every registered device, grouped by account.
async fn load_all_push_tokens(st: &AppState) -> Vec<(u64, Vec<PushToken>)> {
    let Ok(mut c) = st.db.get_conn().await else { return Vec::new() };
    let rows: Vec<(u64, String)> = c
        .query("SELECT user_id, token FROM push_tokens ORDER BY user_id")
        .await
        .unwrap_or_default();
    let mut out: Vec<(u64, Vec<PushToken>)> = Vec::new();
    for (user_id, raw) in rows {
        let Some(token) = parse_push_token(&raw) else { continue };
        match out.last_mut() {
            Some((id, tokens)) if *id == user_id => tokens.push(token),
            _ => out.push((user_id, vec![token])),
        }
    }
    out
}

/// How many devices each account has registered, for the admin page. Accounts
/// with no device are omitted — the page is about who can actually be reached.
async fn push_device_counts(st: &AppState) -> Vec<(u64, String, u64)> {
    let Ok(mut c) = st.db.get_conn().await else { return Vec::new() };
    c.query(
        r#"SELECT p.user_id, COALESCE(u.username, CONCAT('#', p.user_id)), COUNT(*)
           FROM push_tokens p LEFT JOIN admin_users u ON u.id = p.user_id
           GROUP BY p.user_id, u.username ORDER BY u.username"#,
    )
    .await
    .unwrap_or_default()
}

/// The name to show on the phone's lock screen. Falls back to empty, which
/// push.rs renders as "Someone is calling" rather than a blank.
async fn display_name_of(st: &AppState, user_id: u64) -> String {
    find_user_by_id(&st.db, user_id)
        .await
        .ok()
        .flatten()
        .map(|u| u.username)
        .unwrap_or_default()
}

async fn load_push_tokens(st: &AppState, user_id: u64) -> Vec<PushToken> {
    let Ok(mut c) = st.db.get_conn().await else { return Vec::new() };
    let rows: Vec<String> = c
        .exec(
            "SELECT token FROM push_tokens WHERE user_id=:u ORDER BY updated_at DESC LIMIT 10",
            params! {"u" => user_id},
        )
        .await
        .unwrap_or_default();
    rows.iter().filter_map(|t| parse_push_token(t)).collect()
}

async fn forget_push_token(st: &AppState, user_id: u64, token: &str) {
    if let Ok(mut c) = st.db.get_conn().await {
        let _ = c
            .exec_drop(
                "DELETE FROM push_tokens WHERE user_id=:u AND token=:t",
                params! {"u" => user_id, "t" => token},
            )
            .await;
    }
}

// ----------------------------------------------------------------------------
// REST: a phone tells us where to reach it
// ----------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct PushRegisterBody {
    token: String,
    #[serde(default)]
    platform: String,
}

/// POST /api/push/register — store (or refresh) this device's FCM token.
///
/// A token belongs to a device, and a device can change hands between accounts:
/// the same token is therefore claimed by whoever registers it last, and any
/// other account's row for it is removed. Without that, signing out and in as
/// someone else would ring the previous user's calls on this phone.
async fn api_push_register(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PushRegisterBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let Some(token) = parse_push_token(&body.token) else {
        return (StatusCode::BAD_REQUEST, "Invalid push token").into_response();
    };
    let platform = if body.platform.len() > 16 { "" } else { body.platform.as_str() };
    let Ok(mut c) = st.db.get_conn().await else {
        return (StatusCode::SERVICE_UNAVAILABLE, "Database unavailable").into_response();
    };
    let _ = c
        .exec_drop(
            "DELETE FROM push_tokens WHERE token=:t AND user_id<>:u",
            params! {"t" => token.as_str(), "u" => me.id},
        )
        .await;
    if c.exec_drop(
        r#"INSERT INTO push_tokens (user_id, token, platform, updated_at)
           VALUES (:u, :t, :p, :now)
           ON DUPLICATE KEY UPDATE user_id=:u, platform=:p, updated_at=:now"#,
        params! {"u" => me.id, "t" => token.as_str(), "p" => platform, "now" => now()},
    )
    .await
    .is_err()
    {
        return (StatusCode::SERVICE_UNAVAILABLE, "Could not store token").into_response();
    }
    // `enabled` tells the phone whether background ringing will actually work,
    // so it can say so in Settings instead of promising something it won't do.
    Json(serde_json::json!({ "ok": true, "enabled": push_enabled() })).into_response()
}

#[derive(serde::Deserialize)]
struct PushUnregisterBody {
    token: String,
}

/// POST /api/push/unregister — stop ringing this device. Sent on sign-out, so a
/// signed-out phone does not keep receiving someone's calls.
async fn api_push_unregister(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PushUnregisterBody>,
) -> Response {
    let Some(me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    forget_push_token(&st, me.id, body.token.trim()).await;
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// Table for the above. Called from the social migrations, which already run on
/// every start.
async fn ensure_push_tables(c: &mut mysql_async::Conn) -> anyhow::Result<()> {
    c.query_drop(
        r#"CREATE TABLE IF NOT EXISTS push_tokens (
          token VARCHAR(512) NOT NULL PRIMARY KEY,
          user_id BIGINT UNSIGNED NOT NULL,
          platform VARCHAR(16) NOT NULL DEFAULT '',
          updated_at BIGINT NOT NULL,
          INDEX idx_push_user (user_id)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"#,
    )
    .await?;
    Ok(())
}
