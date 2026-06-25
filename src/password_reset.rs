// password_reset.rs — self-service "forgot password" via emailed single-use link.
//
// Flow (mirrors registration.rs):
//   1. POST /api/auth/forgot {identifier}  — user gives their username OR email.
//      We look up an enabled account; if found we store sha256(token) in
//      `password_resets` (raw token never hits the DB) and email the *user* a
//      reset link. The response is ALWAYS a generic success so the endpoint can't
//      be used to enumerate which usernames/emails exist.
//   2. GET  /api/auth/reset?token=  — renders a small dark-themed page with a
//      new-password form (POSTing back to the same path). Invalid/expired tokens
//      get an error page.
//   3. POST /api/auth/reset {token,password} — validates the token + password,
//      updates admin_users.password_hash + auth_key (the challenge-response key,
//      exactly like api_account_password), then burns the token.
//
// Crate-root scope (assembled via include! in main.rs): calls helpers from
// crypto.rs/auth.rs/registration.rs (random_token, sha256_hex, now, audit,
// client_ip, public_base_url, esc, server_error, hash_password_argon2,
// derive_auth_key, build_and_send_email) with no qualifier.

// random_token doubles its arg → 48 hex chars; sha256(token) is what we persist.
const RESET_TOKEN_LEN: usize = 24;
// Reset links are valid for one hour.
const RESET_TTL_SECS: i64 = 3600;

#[derive(Deserialize)]
struct ForgotForm {
    // Username or email — we accept either.
    identifier: String,
}

#[derive(Deserialize)]
struct ResetPageQuery {
    token: String,
}

#[derive(Deserialize)]
struct ResetSubmitForm {
    token: String,
    password: String,
}

// Build the user-facing reset email.
fn reset_email(username: &str, reset_url: &str) -> (String, String) {
    let subject = "ArcadeLauncher: reset your password".to_string();
    let body = format!(
        "Hi {username},\n\n\
         We received a request to reset your ArcadeLauncher password. Open this \
         link to choose a new one:\n\n  {reset_url}\n\n\
         This link is single-use and expires in 1 hour. If you didn't request a \
         password reset you can ignore this email — your password stays unchanged.\n"
    );
    (subject, body)
}

// Dark-themed result/notice page (matches reg_result_page in registration.rs).
fn reset_notice_page(title: &str, message: &str) -> Response {
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

// The new-password form, shown for a valid token.
fn reset_form_page(token: &str, error: Option<&str>) -> Response {
    let err_html = match error {
        Some(e) => format!("<p class=\"err\">{}</p>", esc(e)),
        None => String::new(),
    };
    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Reset password</title>\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <style>body{{font-family:system-ui,-apple-system,sans-serif;background:#0f1116;\
         color:#e6e6e6;display:flex;min-height:100vh;align-items:center;justify-content:center;\
         margin:0}}.card{{max-width:24rem;width:90%;padding:2rem;background:#191c24;\
         border-radius:12px;box-shadow:0 8px 30px rgba(0,0,0,.4)}}h1{{margin:0 0 1rem;\
         font-size:1.3rem}}label{{display:block;font-size:.85rem;color:#a9b0bd;margin:.75rem 0 .25rem}}\
         input{{width:100%;box-sizing:border-box;padding:.6rem .7rem;border-radius:8px;\
         border:1px solid #2a2f3a;background:#0f1116;color:#e6e6e6;font-size:1rem}}\
         button{{margin-top:1.25rem;width:100%;padding:.7rem;border:0;border-radius:8px;\
         background:#5865f2;color:#fff;font-size:1rem;cursor:pointer}}\
         button:disabled{{opacity:.5;cursor:not-allowed}}.err{{color:#ff6b6b;font-size:.85rem;\
         margin:.5rem 0 0}}.hint{{color:#6b7280;font-size:.78rem;margin:.35rem 0 0}}</style></head>\
         <body><div class=\"card\"><h1>Choose a new password</h1>{err_html}\
         <form method=\"post\" action=\"/api/auth/reset\" onsubmit=\"return chk()\">\
         <input type=\"hidden\" name=\"token\" value=\"{token}\">\
         <label for=\"p\">New password</label>\
         <input id=\"p\" name=\"password\" type=\"password\" minlength=\"6\" required autocomplete=\"new-password\">\
         <label for=\"p2\">Confirm new password</label>\
         <input id=\"p2\" type=\"password\" minlength=\"6\" required autocomplete=\"new-password\">\
         <p class=\"hint\">At least 6 characters.</p>\
         <button type=\"submit\">Reset password</button></form>\
         <script>function chk(){{var a=document.getElementById('p').value,\
         b=document.getElementById('p2').value;if(a.length<6){{alert('Password must be at \
         least 6 characters.');return false}}if(a!==b){{alert('Passwords do not match.');\
         return false}}return true}}</script></div></body></html>",
        token = esc(token),
    );
    Html(html).into_response()
}

// POST /api/auth/forgot — request a reset link. Anti-enumeration: always 200 with
// a generic message regardless of whether the account exists.
async fn api_auth_forgot(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ForgotForm>,
) -> Response {
    let generic = || {
        Json(serde_json::json!({
            "ok": true,
            "message": "If an account matches, a password reset link has been emailed.",
        }))
        .into_response()
    };
    let ident = form.identifier.trim().to_lowercase();
    if ident.is_empty() {
        return generic();
    }
    let ip = client_ip(&headers);
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    // Only enabled accounts that actually have an email on file can be reset.
    let row: Option<(u64, String, String)> = match c
        .exec_first(
            "SELECT id,username,email FROM admin_users \
             WHERE (LOWER(username)=:i OR LOWER(email)=:i) AND enabled=TRUE \
             AND email IS NOT NULL AND email<>'' LIMIT 1",
            params! {"i" => &ident},
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return server_error(e),
    };
    let Some((admin_id, username, email)) = row else {
        // Unknown / disabled / emailless account: pretend success.
        audit(&st.db, None, Some(&ident), "password_reset_requested_unknown", ip.as_deref(), None).await;
        return generic();
    };

    let token = random_token(RESET_TOKEN_LEN);
    let token_hash = sha256_hex(token.as_bytes());
    // One active token per account: clear any prior outstanding resets first.
    let _ = c
        .exec_drop("DELETE FROM password_resets WHERE admin_id=:a", params! {"a" => admin_id})
        .await;
    if let Err(e) = c
        .exec_drop(
            "INSERT INTO password_resets (token_hash,admin_id,expires_at,created_at) \
             VALUES (:h,:a,:x,:c)",
            params! {
                "h" => &token_hash, "a" => admin_id,
                "x" => now() + RESET_TTL_SECS, "c" => now(),
            },
        )
        .await
    {
        return server_error(e);
    }

    let base = public_base_url(&st, &headers).await;
    let reset_url = format!("{base}/api/auth/reset?token={token}");
    let (subject, body) = reset_email(&username, &reset_url);
    send_reset_email(&st.cfg, &email, &subject, &body).await;
    audit(&st.db, Some(admin_id), Some(&username), "password_reset_requested", ip.as_deref(), None).await;
    generic()
}

// Resolve a raw token to (admin_id, username) if present and unexpired.
async fn lookup_reset_token(
    c: &mut mysql_async::Conn,
    token: &str,
) -> Result<Option<(u64, String)>> {
    let token_hash = sha256_hex(token.trim().as_bytes());
    let row: Option<(u64, i64)> = c
        .exec_first(
            "SELECT admin_id,expires_at FROM password_resets WHERE token_hash=:h LIMIT 1",
            params! {"h" => &token_hash},
        )
        .await?;
    let Some((admin_id, expires_at)) = row else {
        return Ok(None);
    };
    if expires_at < now() {
        // Expired: clean it up and treat as not found.
        let _ = c
            .exec_drop("DELETE FROM password_resets WHERE token_hash=:h", params! {"h" => &token_hash})
            .await;
        return Ok(None);
    }
    let username: Option<String> = c
        .exec_first("SELECT username FROM admin_users WHERE id=:id", params! {"id" => admin_id})
        .await?;
    Ok(username.map(|u| (admin_id, u)))
}

// GET /api/auth/reset?token= — show the new-password form for a valid token.
async fn api_auth_reset_page(
    State(st): State<AppState>,
    Query(q): Query<ResetPageQuery>,
) -> Response {
    let token = q.token.trim().to_string();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    match lookup_reset_token(&mut c, &token).await {
        Ok(Some(_)) => reset_form_page(&token, None),
        Ok(None) => reset_notice_page(
            "Link expired",
            "This password reset link is invalid or has expired. Request a new one from the sign-in screen.",
        ),
        Err(e) => server_error(e),
    }
}

// POST /api/auth/reset {token,password} — set the new password and burn the token.
async fn api_auth_reset_submit(
    State(st): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ResetSubmitForm>,
) -> Response {
    let token = form.token.trim().to_string();
    let mut c = match st.db.get_conn().await {
        Ok(c) => c,
        Err(e) => return server_error(e),
    };
    let (admin_id, username) = match lookup_reset_token(&mut c, &token).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            return reset_notice_page(
                "Link expired",
                "This password reset link is invalid or has expired. Request a new one from the sign-in screen.",
            )
        }
        Err(e) => return server_error(e),
    };
    if form.password.len() < 6 {
        return reset_form_page(&token, Some("Password must be at least 6 characters."));
    }
    let hash = match hash_password_argon2(&form.password) {
        Ok(h) => h,
        Err(e) => return server_error(e),
    };
    // auth_key is the challenge-response key derived from username+password, kept
    // in lockstep with the Argon2 hash exactly as api_account_password does.
    let auth_key = derive_auth_key(&username, &form.password);
    if let Err(e) = c
        .exec_drop(
            "UPDATE admin_users SET password_hash=:p, auth_key=:k, must_change_password=FALSE WHERE id=:id",
            params! {"p" => hash, "k" => auth_key, "id" => admin_id},
        )
        .await
    {
        return server_error(e);
    }
    // Burn every outstanding reset for this account.
    let _ = c
        .exec_drop("DELETE FROM password_resets WHERE admin_id=:a", params! {"a" => admin_id})
        .await;
    audit(&st.db, Some(admin_id), Some(&username), "password_reset", client_ip(&headers).as_deref(), None).await;
    reset_notice_page(
        "Password updated",
        "Your password has been reset. You can now sign in to ArcadeLauncher with your new password.",
    )
}

// Best-effort reset email to the user. Like send_admin_email: never fails the
// request; logs the link if SMTP is unconfigured so the link isn't lost.
async fn send_reset_email(cfg: &Config, to: &str, subject: &str, body: &str) {
    let Some(smtp) = cfg.smtp.as_ref() else {
        warn!("SMTP not configured; not emailing password reset. Subject: {subject}\n{body}");
        return;
    };
    if to.trim().is_empty() {
        warn!("password reset target has no email; not sending. Subject: {subject}");
        return;
    }
    match build_and_send_email(smtp, to, subject, body).await {
        Ok(()) => info!("password reset email sent to {to}"),
        Err(e) => warn!("password reset email send failed ({e}); link:\n{body}"),
    }
}
