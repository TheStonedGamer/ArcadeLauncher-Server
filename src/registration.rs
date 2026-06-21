// registration.rs — self-service account signup with admin email approval
// (ROADMAP TSd). A new user POSTs /api/auth/register; the account is held in
// `pending_registrations` (never in `admin_users`, so it cannot authenticate)
// and the admin is emailed single-use Approve/Deny links. Approve creates the
// real (non-admin) account; Deny discards the request. Gated OFF by default via
// `ARCADE_REGISTRATION_OPEN` so deploying the binary never silently opens signup.
//
// Split-file note: like the rest of the server this lives in crate-root scope
// (assembled via include! in main.rs), so it calls helpers from auth.rs/db.rs/
// crypto.rs (audit, client_ip, hash_password_argon2, derive_auth_key,
// random_token, now, public_base_url, esc) with no qualifier.

// random_token doubles its arg → 48 hex-ish chars, plenty of entropy for a
// single-use, server-side-validated approval token.
const REG_TOKEN_LEN: usize = 24;

#[derive(Deserialize)]
struct RegisterForm {
    username: String,
    email: String,
    password: String,
}

#[derive(Deserialize)]
struct RegTokenQuery {
    token: String,
}

// Pure validation of a signup request. Ok(()) or a human-readable reason.
// Username: 3..=32 chars, ASCII alphanumeric plus '_'/'-'/'.', must start with an
// alphanumeric. Email: a minimal shape check. Password: >=10 chars (matches the
// change-password rule in api_account_password).
fn validate_registration(username: &str, email: &str, password: &str) -> Result<(), &'static str> {
    let u = username.trim();
    if u.len() < 3 || u.len() > 32 {
        return Err("username must be 3–32 characters");
    }
    let first = u.chars().next().unwrap_or(' ');
    if !first.is_ascii_alphanumeric() {
        return Err("username must start with a letter or number");
    }
    if !u
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Err("username may only contain letters, numbers, '_', '-' and '.'");
    }
    if !is_plausible_email(email) {
        return Err("enter a valid email address");
    }
    if password.len() < 10 {
        return Err("password must be at least 10 characters");
    }
    Ok(())
}

// A deliberately conservative email shape check: exactly one '@', a non-empty
// local part, and a dotted domain with no leading/trailing dot and no spaces.
// Not RFC-5322 — just enough to reject obvious garbage before we email it.
fn is_plausible_email(email: &str) -> bool {
    let e = email.trim();
    if e.len() < 3 || e.len() > 254 || e.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let mut parts = e.split('@');
    let (local, domain) = match (parts.next(), parts.next(), parts.next()) {
        (Some(l), Some(d), None) => (l, d),
        _ => return false,
    };
    !local.is_empty()
        && domain.len() >= 3
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
}

fn normalize_username(s: &str) -> String {
    s.trim().to_lowercase()
}

// Build the admin notification email (subject, plain-text body, HTML body) for a
// pending signup. The HTML body renders Accept / Deny as styled buttons; the
// plain-text part keeps the raw URLs as a fallback for text-only clients. Both
// point at the same single-use Approve/Deny URLs.
fn registration_email(
    username: &str,
    email: &str,
    ip: &str,
    approve_url: &str,
    deny_url: &str,
) -> (String, String, String) {
    let subject = format!("ArcadeLauncher: new account request — {username}");
    let ip_line = if ip.is_empty() {
        String::new()
    } else {
        format!("From IP: {ip}\n")
    };
    let body = format!(
        "A new ArcadeLauncher account has been requested:\n\n\
         Username: {username}\n\
         Email:    {email}\n\
         {ip_line}\n\
         Approve and create the account:\n  {approve_url}\n\n\
         Deny and discard the request:\n  {deny_url}\n\n\
         These buttons are single-use. Until you approve, the account cannot sign in.\n"
    );
    let ip_row = if ip.is_empty() {
        String::new()
    } else {
        format!(
            "<tr><td style=\"color:#a9b0bd;padding:2px 12px 2px 0\">From IP</td>\
             <td style=\"color:#e6e6e6\">{}</td></tr>",
            esc(ip)
        )
    };
    let html = format!(
        "<!doctype html><html><body style=\"margin:0;background:#0f1116;\
         font-family:system-ui,-apple-system,sans-serif;color:#e6e6e6\">\
         <div style=\"max-width:34rem;margin:0 auto;padding:2rem\">\
         <h1 style=\"font-size:1.25rem;margin:0 0 1rem\">New ArcadeLauncher account request</h1>\
         <table style=\"border-collapse:collapse;margin:0 0 1.5rem;font-size:.95rem\">\
         <tr><td style=\"color:#a9b0bd;padding:2px 12px 2px 0\">Username</td>\
         <td style=\"color:#e6e6e6\">{username}</td></tr>\
         <tr><td style=\"color:#a9b0bd;padding:2px 12px 2px 0\">Email</td>\
         <td style=\"color:#e6e6e6\">{email}</td></tr>\
         {ip_row}</table>\
         <table style=\"border-collapse:separate\"><tr>\
         <td style=\"padding-right:12px\"><a href=\"{approve_url}\" \
         style=\"display:inline-block;background:#2ea043;color:#fff;text-decoration:none;\
         font-weight:600;padding:12px 28px;border-radius:8px\">Accept</a></td>\
         <td><a href=\"{deny_url}\" \
         style=\"display:inline-block;background:#da3633;color:#fff;text-decoration:none;\
         font-weight:600;padding:12px 28px;border-radius:8px\">Deny</a></td>\
         </tr></table>\
         <p style=\"color:#a9b0bd;font-size:.85rem;margin:1.5rem 0 0;line-height:1.5\">\
         These buttons are single-use. Until you accept, the account cannot sign in.</p>\
         </div></body></html>",
        username = esc(username),
        email = esc(email),
    );
    (subject, body, html)
}

// A small dark-themed HTML page shown after the admin clicks Approve/Deny.
fn reg_result_page(title: &str, message: &str) -> Response {
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <style>body{{font-family:system-ui,-apple-system,sans-serif;background:#0f1116;\
         color:#e6e6e6;display:flex;min-height:100vh;align-items:center;justify-content:center;\
         margin:0}}.card{{max-width:30rem;padding:2rem;background:#191c24;border-radius:12px;\
         box-shadow:0 8px 30px rgba(0,0,0,.4)}}h1{{margin:0 0 .5rem;font-size:1.3rem}}\
         p{{margin:0;color:#a9b0bd;line-height:1.5}}</style></head>\
         <body><div class=\"card\"><h1>{title}</h1><p>{message}</p></div></body></html>",
        title = esc(title),
        message = esc(message),
    );
    Html(html).into_response()
}

// POST /api/auth/register — create a pending signup and email the admin.
async fn api_auth_register(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RegisterForm>,
) -> Response {
    if !st.cfg.registration_open {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "account registration is currently closed"})),
        )
            .into_response();
    }
    let ip = client_ip(&headers);
    let username = form.username.trim().to_string();
    let email = form.email.trim().to_string();
    if let Err(msg) = validate_registration(&username, &email, &form.password) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": msg}))).into_response();
    }
    let uname_norm = normalize_username(&username);
    let email_norm = email.to_lowercase();

    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Collision against live accounts AND outstanding pending requests.
    let taken: Option<u64> = match c
        .exec_first(
            "SELECT 1 FROM admin_users WHERE LOWER(username)=:u OR LOWER(email)=:e LIMIT 1",
            params! {"u" => &uname_norm, "e" => &email_norm},
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    if taken.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "that username or email is already registered"})),
        )
            .into_response();
    }
    let pending: Option<u64> = match c
        .exec_first(
            "SELECT 1 FROM pending_registrations WHERE LOWER(username)=:u OR LOWER(email)=:e LIMIT 1",
            params! {"u" => &uname_norm, "e" => &email_norm},
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    if pending.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "a request for that username or email is already awaiting approval"})),
        )
            .into_response();
    }

    let hash = match hash_password_argon2(&form.password) {
        Ok(h) => h,
        Err(e) => return server_error(e),
    };
    let auth_key = derive_auth_key(&username, &form.password);
    let token = random_token(REG_TOKEN_LEN);
    if let Err(e) = c
        .exec_drop(
            "INSERT INTO pending_registrations (username,email,password_hash,auth_key,token,ip,created_at) \
             VALUES (:u,:e,:p,:k,:t,:i,:c)",
            params! {
                "u" => &username, "e" => &email, "p" => hash, "k" => auth_key,
                "t" => &token, "i" => ip.as_deref(), "c" => now(),
            },
        )
        .await
    {
        return server_error(e);
    }

    let base = public_base_url(&st, &headers).await;
    let approve_url = format!("{base}/api/auth/approve?token={token}");
    let deny_url = format!("{base}/api/auth/deny?token={token}");
    let (subject, body, html) =
        registration_email(&username, &email, ip.as_deref().unwrap_or(""), &approve_url, &deny_url);
    // Notify EVERY enabled admin at their configured email, plus the explicit
    // ARCADE_REGISTRATION_NOTIFY_EMAIL if set, deduped case-insensitively.
    let recipients = collect_admin_recipients(&mut c, &st.cfg.registration_notify_email).await;
    // Best-effort: a mail failure must never fail the signup. send_admin_email
    // logs the links when SMTP is unconfigured so the admin can still act.
    if recipients.is_empty() {
        warn!("no admin recipients (no enabled admin emails / ARCADE_REGISTRATION_NOTIFY_EMAIL); not emailing. Subject: {subject}");
    }
    for to in &recipients {
        send_admin_email(&st.cfg, to, &subject, &body, Some(&html)).await;
    }
    audit(&st.db, None, Some(&username), "register_requested", ip.as_deref(), Some(&email)).await;
    Json(serde_json::json!({
        "ok": true,
        "status": "pending",
        "message": "Request submitted — an administrator must approve your account before you can sign in.",
    }))
    .into_response()
}

// GET /api/auth/approve?token= — admin clicks Approve: create the account.
async fn api_auth_approve(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RegTokenQuery>,
) -> Response {
    let token = q.token.trim();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(u64, String, String, String, String)> = match c
        .exec_first(
            "SELECT id,username,email,password_hash,auth_key FROM pending_registrations WHERE token=:t LIMIT 1",
            params! {"t" => token},
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let Some((id, username, email, password_hash, auth_key)) = row else {
        return reg_result_page(
            "Request not found",
            "This approval link is invalid or has already been used.",
        );
    };
    // Re-check collisions at approval time (the name may have been taken since).
    let exists: Option<u64> = match c
        .exec_first(
            "SELECT 1 FROM admin_users WHERE LOWER(username)=:u OR LOWER(email)=:e LIMIT 1",
            params! {"u" => username.to_lowercase(), "e" => email.to_lowercase()},
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    if exists.is_some() {
        let _ = c
            .exec_drop("DELETE FROM pending_registrations WHERE id=:id", params! {"id" => id})
            .await;
        return reg_result_page(
            "Already registered",
            "An account with that username or email already exists; the request was discarded.",
        );
    }
    // New accounts are non-admin (is_admin=FALSE) and immediately enabled.
    if let Err(e) = c
        .exec_drop(
            "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at,auth_key) \
             VALUES (:u,:e,:p,FALSE,TRUE,:t,:k)",
            params! {"u" => &username, "e" => &email, "p" => password_hash, "t" => now(), "k" => auth_key},
        )
        .await
    {
        return server_error(e);
    }
    let _ = c
        .exec_drop("DELETE FROM pending_registrations WHERE id=:id", params! {"id" => id})
        .await;
    audit(&st.db, None, Some(&username), "register_approved", client_ip(&headers).as_deref(), Some(&email)).await;
    reg_result_page("Account approved", &format!("{username} can now sign in to ArcadeLauncher."))
}

// GET /api/auth/deny?token= — admin clicks Deny: discard the request.
async fn api_auth_deny(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RegTokenQuery>,
) -> Response {
    let token = q.token.trim();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let row: Option<(u64, String)> = match c
        .exec_first(
            "SELECT id,username FROM pending_registrations WHERE token=:t LIMIT 1",
            params! {"t" => token},
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let Some((id, username)) = row else {
        return reg_result_page("Request not found", "This link is invalid or has already been used.");
    };
    let _ = c
        .exec_drop("DELETE FROM pending_registrations WHERE id=:id", params! {"id" => id})
        .await;
    audit(&st.db, None, Some(&username), "register_denied", client_ip(&headers).as_deref(), None).await;
    reg_result_page("Request denied", &format!("The account request for {username} was discarded."))
}

// Gather the set of admin notification recipients: every enabled admin's
// non-empty email, plus the explicit ARCADE_REGISTRATION_NOTIFY_EMAIL if set.
// Deduped case-insensitively (preserving the first-seen casing). Best-effort —
// a DB error just falls back to the configured notify email so signups still
// alert someone.
async fn collect_admin_recipients(c: &mut mysql_async::Conn, notify: &str) -> Vec<String> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    let mut push = |addr: &str, seen: &mut std::collections::HashSet<String>, out: &mut Vec<String>| {
        let a = addr.trim();
        if a.is_empty() {
            return;
        }
        if seen.insert(a.to_lowercase()) {
            out.push(a.to_string());
        }
    };
    let rows: Vec<String> = c
        .query("SELECT email FROM admin_users WHERE is_admin=TRUE AND enabled=TRUE AND email IS NOT NULL AND email<>''")
        .await
        .unwrap_or_default();
    for addr in &rows {
        push(addr, &mut seen, &mut out);
    }
    push(notify, &mut seen, &mut out);
    out
}

// Best-effort admin email. Logs (and prints the links via the caller's body)
// instead of erroring when SMTP is unconfigured or the recipient is unset.
async fn send_admin_email(cfg: &Config, to: &str, subject: &str, body: &str, html: Option<&str>) {
    let Some(smtp) = cfg.smtp.as_ref() else {
        warn!("SMTP not configured; not emailing admin. Subject: {subject}\n{body}");
        return;
    };
    if to.trim().is_empty() {
        warn!("no registration notify email set (ARCADE_REGISTRATION_NOTIFY_EMAIL / ARCADE_ADMIN_EMAIL); not emailing. Subject: {subject}");
        return;
    }
    match build_and_send_email_parts(smtp, to, subject, body, html).await {
        Ok(()) => info!("admin notification email sent to {to}"),
        Err(e) => warn!("admin email send failed ({e}); links:\n{body}"),
    }
}

async fn build_and_send_email(smtp: &SmtpConfig, to: &str, subject: &str, body: &str) -> Result<()> {
    build_and_send_email_parts(smtp, to, subject, body, None).await
}

// Sends a plain-text email, or a multipart/alternative (text + HTML) when an
// `html` part is supplied so clients render buttons while text-only clients
// still get the raw links.
async fn build_and_send_email_parts(
    smtp: &SmtpConfig,
    to: &str,
    subject: &str,
    body: &str,
    html: Option<&str>,
) -> Result<()> {
    let builder = Message::builder()
        .from(smtp.from.parse().context("invalid SMTP From address")?)
        .to(to.parse().context("invalid recipient address")?)
        .subject(subject);
    let email = match html {
        Some(h) => builder
            .multipart(
                MultiPart::alternative()
                    .singlepart(SinglePart::plain(body.to_string()))
                    .singlepart(SinglePart::html(h.to_string())),
            )
            .context("build email message")?,
        None => builder
            .header(ContentType::TEXT_PLAIN)
            .body(body.to_string())
            .context("build email message")?,
    };
    let mut builder = if smtp.starttls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&smtp.host).context("smtp starttls relay")?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&smtp.host).context("smtp relay")?
    };
    builder = builder.port(smtp.port);
    if !smtp.username.is_empty() {
        builder = builder.credentials(Credentials::new(smtp.username.clone(), smtp.password.clone()));
    }
    builder.build().send(email).await.context("send email")?;
    Ok(())
}
