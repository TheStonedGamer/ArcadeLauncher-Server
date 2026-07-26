// qr_auth.rs - passwordless QR sign-in for launcher and storefront.
//
// The screen and phone receive different random capabilities:
// - scan_secret is embedded in the QR and may inspect/approve the request;
// - poll_token stays on the requesting screen and may consume the completed
//   login exactly once.
//
// Only an already-authenticated mobile bearer session can approve. Challenges
// live in MariaDB rather than process memory because production has multiple
// API replicas.

const QR_SIGNIN_TTL_SECONDS: i64 = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QrSigninTarget {
    Launcher,
    Store,
}

impl QrSigninTarget {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "launcher" | "desktop" => Some(Self::Launcher),
            "store" | "web" | "website" => Some(Self::Store),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Launcher => "launcher",
            Self::Store => "store",
        }
    }
}

#[derive(Deserialize)]
struct QrStartForm {
    target: String,
    #[serde(default, alias = "deviceName")]
    device_name: String,
}

#[derive(Deserialize)]
struct QrScanForm {
    #[serde(alias = "challengeId")]
    challenge_id: String,
    #[serde(alias = "scanSecret")]
    scan_secret: String,
}

#[derive(Deserialize)]
struct QrDecisionForm {
    #[serde(alias = "challengeId")]
    challenge_id: String,
    #[serde(alias = "scanSecret")]
    scan_secret: String,
    action: String,
}

#[derive(Deserialize)]
struct QrPollForm {
    #[serde(alias = "challengeId")]
    challenge_id: String,
    #[serde(alias = "pollToken")]
    poll_token: String,
}

fn qr_device_name(raw: &str, target: QrSigninTarget) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return match target {
            QrSigninTarget::Launcher => "Arcade Launcher desktop".into(),
            QrSigninTarget::Store => "Arcade Launcher website".into(),
        };
    }
    trimmed.chars().take(120).collect()
}

fn qr_bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": message})),
    )
        .into_response()
}

async fn qr_signin_start(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<QrStartForm>,
) -> Response {
    let Some(target) = QrSigninTarget::parse(&form.target) else {
        return qr_bad_request("target must be launcher or store");
    };
    let id = random_token(16);
    let scan_secret = random_token(32);
    let poll_token = random_token(32);
    let ts = now();
    let expires_at = ts + QR_SIGNIN_TTL_SECONDS;
    let device_name = qr_device_name(&form.device_name, target);
    let ip = client_ip(&headers).unwrap_or_default();

    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Opportunistic cleanup keeps abandoned rows bounded.
    let _ = c
        .exec_drop(
            "DELETE FROM qr_signin_challenges WHERE expires_at < :cutoff",
            params! {"cutoff" => ts - QR_SIGNIN_TTL_SECONDS},
        )
        .await;
    let active_from_ip: Option<u64> = c
        .exec_first(
            "SELECT COUNT(*) FROM qr_signin_challenges WHERE ip=:ip AND expires_at>:now",
            params! {"ip" => &ip, "now" => ts},
        )
        .await
        .unwrap_or(None);
    if active_from_ip.unwrap_or(0) >= 10 {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "too many active QR sign-in requests"})),
        )
            .into_response();
    }
    if let Err(e) = c
        .exec_drop(
            r#"INSERT INTO qr_signin_challenges
               (id,scan_hash,poll_hash,target,device_name,ip,state,expires_at,created_at)
               VALUES (:id,:scan,:poll,:target,:device,:ip,'pending',:expires,:created)"#,
            params! {
                "id" => &id,
                "scan" => sha256_hex(scan_secret.as_bytes()),
                "poll" => sha256_hex(poll_token.as_bytes()),
                "target" => target.as_str(),
                "device" => &device_name,
                "ip" => &ip,
                "expires" => expires_at,
                "created" => ts,
            },
        )
        .await
    {
        return server_error(e);
    }

    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "challengeId": id,
            "scanSecret": scan_secret,
            "pollToken": poll_token,
            "expiresIn": QR_SIGNIN_TTL_SECONDS,
        })),
    )
        .into_response()
}

async fn qr_signin_inspect(
    State(st): State<AppState>,
    Form(form): Form<QrScanForm>,
) -> Response {
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(String, String, String, String, i64)> = match c
        .exec_first(
            r#"SELECT target,device_name,COALESCE(ip,''),state,expires_at
               FROM qr_signin_challenges
               WHERE id=:id AND scan_hash=:scan LIMIT 1"#,
            params! {
                "id" => form.challenge_id.trim(),
                "scan" => sha256_hex(form.scan_secret.trim().as_bytes()),
            },
        )
        .await
    {
        Ok(row) => row,
        Err(e) => return server_error(e),
    };
    let Some((target, device_name, ip, state, expires_at)) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "QR sign-in request not found"})),
        )
            .into_response();
    };
    let remaining = expires_at - now();
    if remaining <= 0 || state != "pending" {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({"error": "QR sign-in request expired or was already used"})),
        )
            .into_response();
    }
    Json(serde_json::json!({
        "challengeId": form.challenge_id,
        "target": target,
        "deviceName": device_name,
        "ip": display_ip(&ip),
        "expiresIn": remaining,
    }))
    .into_response()
}

async fn qr_signin_decide(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<QrDecisionForm>,
) -> Response {
    let Some(user) = launcher_user(&st, &headers).await else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "mobile sign-in required"})),
        )
            .into_response();
    };
    let state = match form.action.trim().to_ascii_lowercase().as_str() {
        "approve" | "allow" => "approved",
        "deny" | "reject" => "denied",
        _ => return qr_bad_request("action must be approve or deny"),
    };
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    if let Err(e) = c
        .exec_drop(
            r#"UPDATE qr_signin_challenges
               SET state=:state,user_id=:uid
               WHERE id=:id AND scan_hash=:scan AND state='pending' AND expires_at>:now"#,
            params! {
                "state" => state,
                "uid" => user.id,
                "id" => form.challenge_id.trim(),
                "scan" => sha256_hex(form.scan_secret.trim().as_bytes()),
                "now" => now(),
            },
        )
        .await
    {
        return server_error(e);
    }
    if c.affected_rows() != 1 {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({"error": "QR sign-in request expired or was already answered"})),
        )
            .into_response();
    }
    audit(
        &st.db,
        Some(user.id),
        Some(&user.username),
        if state == "approved" { "qr_approved" } else { "qr_denied" },
        client_ip(&headers).as_deref(),
        Some("mobile"),
    )
    .await;
    Json(serde_json::json!({"status": state})).into_response()
}

async fn qr_signin_poll(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<QrPollForm>,
) -> Response {
    let poll_hash = sha256_hex(form.poll_token.trim().as_bytes());
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(String, String, Option<u64>, i64)> = match c
        .exec_first(
            r#"SELECT target,state,user_id,expires_at FROM qr_signin_challenges
               WHERE id=:id AND poll_hash=:poll LIMIT 1"#,
            params! {"id" => form.challenge_id.trim(), "poll" => &poll_hash},
        )
        .await
    {
        Ok(row) => row,
        Err(e) => return server_error(e),
    };
    let Some((target_raw, state, user_id, expires_at)) = row else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "QR sign-in request not found"})),
        )
            .into_response();
    };
    if expires_at <= now() {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({"status": "expired", "error": "QR sign-in request expired"})),
        )
            .into_response();
    }
    if state == "pending" {
        return (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"status": "pending", "expiresIn": expires_at - now()})),
        )
            .into_response();
    }
    if state == "denied" {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"status": "denied", "error": "QR sign-in denied on your phone"})),
        )
            .into_response();
    }
    let (Some(target), Some(user_id)) = (QrSigninTarget::parse(&target_raw), user_id) else {
        return server_error(anyhow!("invalid QR sign-in challenge state"));
    };
    let user = match find_user_by_id(&st.db, user_id).await {
        Ok(Some(user)) if user.enabled => user,
        Ok(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "account is unavailable"})),
            )
                .into_response()
        }
        Err(e) => return server_error(e),
    };

    // Compare-and-swap makes consumption single-use even when two poll requests
    // land on different API replicas simultaneously.
    if let Err(e) = c
        .exec_drop(
            r#"UPDATE qr_signin_challenges SET state='consumed',consumed_at=:now
               WHERE id=:id AND poll_hash=:poll AND state='approved' AND expires_at>:now"#,
            params! {
                "now" => now(),
                "id" => form.challenge_id.trim(),
                "poll" => &poll_hash,
            },
        )
        .await
    {
        return server_error(e);
    }
    if c.affected_rows() != 1 {
        return (
            StatusCode::GONE,
            Json(serde_json::json!({"error": "QR sign-in request was already used"})),
        )
            .into_response();
    }

    audit(
        &st.db,
        Some(user.id),
        Some(&user.username),
        "login",
        client_ip(&headers).as_deref(),
        Some(if target == QrSigninTarget::Store { "qr store" } else { "qr launcher" }),
    )
    .await;

    match target {
        QrSigninTarget::Launcher => {
            let token = match issue_user_token(&st.db, user.id, &user.username).await {
                Ok(token) => token,
                Err(e) => return server_error(e),
            };
            let must_change = user_must_change_password(&st.db, user.id).await;
            Json(serde_json::json!({
                "status": "complete",
                "token": token,
                "username": user.username,
                "isAdmin": user.is_admin,
                "mustChangePassword": must_change,
            }))
            .into_response()
        }
        QrSigninTarget::Store => {
            let token = random_token(36);
            let hash = sha256_hex(token.as_bytes());
            let ts = now();
            if let Err(e) = c
                .exec_drop(
                    "INSERT INTO user_sessions (token_hash,user_id,expires_at,created_at) VALUES (:h,:u,:e,:t)",
                    params! {
                        "h" => hash,
                        "u" => user.id,
                        "e" => ts + STORE_SESSION_TTL_SECONDS,
                        "t" => ts,
                    },
                )
                .await
            {
                return server_error(e);
            }
            let cookie = store_cookie(&token, request_is_secure(&headers), STORE_SESSION_TTL_SECONDS);
            (
                [(header::SET_COOKIE, cookie)],
                Json(serde_json::json!({
                    "status": "complete",
                    "username": user.username,
                    "email": user.email,
                    "isAdmin": user.is_admin,
                })),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod qr_auth_tests {
    use super::*;

    #[test]
    fn target_aliases_are_narrow_and_canonical() {
        assert_eq!(QrSigninTarget::parse("launcher"), Some(QrSigninTarget::Launcher));
        assert_eq!(QrSigninTarget::parse(" Desktop "), Some(QrSigninTarget::Launcher));
        assert_eq!(QrSigninTarget::parse("web"), Some(QrSigninTarget::Store));
        assert_eq!(QrSigninTarget::parse("admin"), None);
        assert_eq!(QrSigninTarget::parse(""), None);
    }

    #[test]
    fn device_names_have_safe_defaults_and_bounds() {
        assert_eq!(
            qr_device_name("", QrSigninTarget::Launcher),
            "Arcade Launcher desktop"
        );
        assert_eq!(
            qr_device_name(" ", QrSigninTarget::Store),
            "Arcade Launcher website"
        );
        assert_eq!(qr_device_name(&"x".repeat(200), QrSigninTarget::Store).len(), 120);
    }
}
