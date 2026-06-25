// admin_extra.rs — secondary admin pages split off the main dashboard:
//   • /admin/accounts — full account management (users, tokens, and pending
//     self-service signup approvals) moved off the crowded main page.
//   • /admin/requests — game-request triage (set status / delete), replacing the
//     inline admin controls that used to live on the client's Requests tab.
// Both render via the shared shell()/CSS and post back through admin_post, which
// dispatches on the `action` field and re-renders the page named by `return_to`.

// ── Pending self-service signups ─────────────────────────────────────────────

/// (id, username, email, ip, created_at) for every outstanding signup request,
/// oldest first.
async fn list_pending_registrations(db: &Pool) -> Result<Vec<(u64, String, String, String, i64)>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<(u64, String, String, Option<String>, i64)> = c
        .query(
            "SELECT id, username, email, ip, created_at FROM pending_registrations ORDER BY created_at ASC",
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|(id, u, e, ip, created)| (id, u, e, ip.unwrap_or_default(), created))
        .collect())
}

/// Approve a pending signup by row id: create the real (non-admin) account and
/// drop the pending row. Mirrors registration.rs's token-based approval.
async fn approve_pending_registration(db: &Pool, id: u64) -> Result<String> {
    let mut c = db.get_conn().await?;
    let row: Option<(u64, String, String, String, String)> = c
        .exec_first(
            "SELECT id, username, email, password_hash, auth_key FROM pending_registrations WHERE id=:id",
            params! {"id" => id},
        )
        .await?;
    let Some((id, username, email, password_hash, auth_key)) = row else {
        return Ok("Request not found (already handled?).".to_string());
    };
    let exists: Option<u64> = c
        .exec_first(
            "SELECT 1 FROM admin_users WHERE LOWER(username)=:u OR LOWER(email)=:e LIMIT 1",
            params! {"u" => username.to_lowercase(), "e" => email.to_lowercase()},
        )
        .await?;
    if exists.is_some() {
        let _ = c
            .exec_drop("DELETE FROM pending_registrations WHERE id=:id", params! {"id" => id})
            .await;
        return Ok(format!("{username} already exists; request discarded."));
    }
    c.exec_drop(
        "INSERT INTO admin_users (username,email,password_hash,is_admin,enabled,created_at,auth_key) \
         VALUES (:u,:e,:p,FALSE,TRUE,:t,:k)",
        params! {"u" => &username, "e" => &email, "p" => password_hash, "t" => now(), "k" => auth_key},
    )
    .await?;
    let _ = c
        .exec_drop("DELETE FROM pending_registrations WHERE id=:id", params! {"id" => id})
        .await;
    audit(db, None, Some(&username), "register_approved", None, Some(&email)).await;
    Ok(format!("Approved {username} — they can now sign in."))
}

/// Deny (discard) a pending signup by row id.
async fn deny_pending_registration(db: &Pool, id: u64) -> Result<String> {
    let mut c = db.get_conn().await?;
    let uname: Option<String> = c
        .exec_first("SELECT username FROM pending_registrations WHERE id=:id", params! {"id" => id})
        .await?;
    c.exec_drop("DELETE FROM pending_registrations WHERE id=:id", params! {"id" => id})
        .await?;
    match uname {
        Some(u) => {
            audit(db, None, Some(&u), "register_denied", None, None).await;
            Ok(format!("Denied and discarded {u}'s request."))
        }
        None => Ok("Request not found (already handled?).".to_string()),
    }
}

// ── Game-request triage ──────────────────────────────────────────────────────

/// The four request statuses, in display/sort order (mirrors the Requests service).
const REQUEST_STATUSES: [&str; 4] = ["pending", "approved", "fulfilled", "declined"];

/// (id, title, platform, requested_by_name, votes, status) for every game request,
/// ordered like the client board (status priority, then votes, then age).
async fn list_game_requests_admin(db: &Pool) -> Result<Vec<(u64, String, String, String, i64, String)>> {
    let mut c = db.get_conn().await?;
    let rows: Vec<(u64, String, String, String, i64, String)> = c
        .query(
            "SELECT id, title, platform, requested_by_name, votes, status FROM game_requests \
             ORDER BY FIELD(status,'pending','approved','fulfilled','declined'), votes DESC, created_at ASC",
        )
        .await?;
    Ok(rows)
}

async fn set_game_request_status(db: &Pool, id: u64, status: &str) -> Result<String> {
    if !REQUEST_STATUSES.contains(&status) {
        return Ok("Unknown status.".to_string());
    }
    let mut c = db.get_conn().await?;
    c.exec_drop(
        "UPDATE game_requests SET status=:s, updated_at=:t WHERE id=:id",
        params! {"s" => status, "t" => now(), "id" => id},
    )
    .await?;
    Ok(format!("Request #{id} set to {status}."))
}

async fn delete_game_request(db: &Pool, id: u64) -> Result<String> {
    let mut c = db.get_conn().await?;
    let _ = c
        .exec_drop("DELETE FROM request_ratings WHERE request_id=:id", params! {"id" => id})
        .await;
    let _ = c
        .exec_drop("DELETE FROM request_votes WHERE request_id=:id", params! {"id" => id})
        .await;
    c.exec_drop("DELETE FROM game_requests WHERE id=:id", params! {"id" => id})
        .await?;
    Ok(format!("Deleted request #{id}."))
}

// ── Page HTML ────────────────────────────────────────────────────────────────

/// Shared sidebar for the secondary admin pages, highlighting the active one.
fn admin_subnav(active: &str) -> String {
    let item = |href: &str, label: &str, key: &str| {
        let cls = if key == active { " class='active'" } else { "" };
        format!("<a href='{href}'{cls}>{label}</a>")
    };
    format!(
        "<aside class=\"sidebar\"><div class=\"brand-block\"><div class=\"brand-mark\">AL</div>\
         <div><div class=\"brand-title\">ArcadeLauncher</div><div class=\"brand-subtitle\">Rust Server</div></div></div>\
         <nav><a href=\"/admin\">Dashboard</a><a href=\"/admin/metadata\">Metadata</a>{}{}{}</nav></aside>",
        item("/admin/accounts", "Accounts", "accounts"),
        item("/admin/requests", "Game Requests", "requests"),
        item("/admin/social-test", "Social Test", "social-test"),
    )
}

async fn accounts_page_html(st: &AppState, admin: Option<User>, message: &str) -> Result<String> {
    let users = list_users(&st.db).await.unwrap_or_default();
    let tokens = list_launcher_tokens(&st.db).await.unwrap_or_default();
    let pending = list_pending_registrations(&st.db).await.unwrap_or_default();
    let user_mgmt = user_management_html(&users);
    let token_rows = tokens
        .iter()
        .map(|(id, name, token, enabled)| {
            format!(
                "<tr><td>{}</td><td><code class='token'>{}</code></td><td>{}</td><td><form method='post' class='inline'><input type='hidden' name='return_to' value='accounts'><input type='hidden' name='user_id' value='{}'><button name='action' value='rotate_user'>Rotate</button><button name='action' value='delete_user' class='danger'>Delete</button></form></td></tr>",
                esc(name), esc(&masked_value(token)), if *enabled { "Enabled" } else { "Disabled" }, id
            )
        })
        .collect::<String>();
    let pending_rows = pending
        .iter()
        .map(|(id, username, email, ip, created)| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>\
                 <form method='post' class='inline'><input type='hidden' name='return_to' value='accounts'><input type='hidden' name='pending_id' value='{}'>\
                 <button name='action' value='approve_pending'>Approve</button>\
                 <button name='action' value='deny_pending' class='danger'>Deny</button></form></td></tr>",
                esc(username),
                esc(email),
                if ip.is_empty() { "&mdash;".into() } else { esc(ip) },
                fmt_epoch(*created),
                id,
            )
        })
        .collect::<String>();
    let pending_section = format!(
        "<section class=\"section\"><div class=\"section-heading\"><h2>Pending Account Requests</h2>\
         <span class=\"muted\">Self-service signups awaiting approval. Approving creates a standard (non-admin) account.</span></div>\
         <table><thead><tr><th>Username</th><th>Email</th><th>From IP</th><th>Requested</th><th>Action</th></tr></thead><tbody>{}</tbody></table></section>",
        if pending_rows.is_empty() { "<tr><td colspan='5'>No pending requests.</td></tr>".into() } else { pending_rows },
    );

    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          {subnav}
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Account management</div><h1>Accounts</h1></div><div class="account-box"><span>Signed in as <strong>{signed}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {notice}
            {pending_section}
            <section class="section"><div class="section-heading"><h2>Create User</h2><span class="muted">Admins are created here directly; regular accounts can also self-register and be approved above.</span></div>
              <form method="post" class="row"><input type="hidden" name="return_to" value="accounts"><input name="username" placeholder="Username"><input name="email" type="email" placeholder="Email"><input name="password" type="password" placeholder="Password (6+ chars)"><label class="checkline"><input type="checkbox" name="is_admin" value="1"> Admin</label><button name="action" value="add_user">Create User</button></form>
            </section>
            <section class="section"><div class="section-heading"><h2>Users</h2><span class="muted">Edit email, role, status, or reset a password; manage 2FA and deletion.</span></div>{user_mgmt}</section>
            <section class="section"><div class="section-heading"><h2>Issued Tokens</h2><span class="muted">Bearer tokens issued to launcher clients.</span></div>
              <table><thead><tr><th>Name</th><th>Bearer Token</th><th>Status</th><th>Actions</th></tr></thead><tbody>{token_rows}</tbody></table></section>
          </div>
        </div>
        "##,
        subnav = admin_subnav("accounts"),
        signed = esc(&signed),
        notice = notice(message),
        pending_section = pending_section,
        user_mgmt = user_mgmt,
        token_rows = if token_rows.is_empty() { "<tr><td colspan='4'>No issued tokens yet.</td></tr>".into() } else { token_rows },
    )))
}

async fn requests_page_html(st: &AppState, admin: Option<User>, message: &str) -> Result<String> {
    let requests = list_game_requests_admin(&st.db).await.unwrap_or_default();
    let rows = requests
        .iter()
        .map(|(id, title, platform, by, votes, status)| {
            let options = REQUEST_STATUSES
                .iter()
                .map(|s| {
                    let sel = if s == status { " selected" } else { "" };
                    format!("<option value='{s}'{sel}>{}</option>", cap_first(s))
                })
                .collect::<String>();
            format!(
                "<tr><td>{title}</td><td>{platform}</td><td>{by}</td><td>{votes}</td>\
                 <td><form method='post' class='inline'><input type='hidden' name='return_to' value='requests'><input type='hidden' name='request_id' value='{id}'>\
                 <select name='request_status'>{options}</select>\
                 <button name='action' value='set_request_status'>Save</button>\
                 <button name='action' value='delete_request' class='danger'>Delete</button></form></td></tr>",
                title = esc(title),
                platform = if platform.is_empty() { "&mdash;".into() } else { esc(platform) },
                by = if by.is_empty() { "&mdash;".into() } else { esc(by) },
                votes = votes,
                id = id,
                options = options,
            )
        })
        .collect::<String>();
    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          {subnav}
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Community</div><h1>Game Requests</h1></div><div class="account-box"><span>Signed in as <strong>{signed}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {notice}
            <section class="section"><div class="section-heading"><h2>Triage</h2><span class="muted">Approve, fulfil, or decline community game requests. Status is reflected on the client Requests board.</span></div>
              <table><thead><tr><th>Title</th><th>Platform</th><th>Requested By</th><th>Votes</th><th>Status / Action</th></tr></thead><tbody>{rows}</tbody></table></section>
          </div>
        </div>
        "##,
        subnav = admin_subnav("requests"),
        signed = esc(&signed),
        notice = notice(message),
        rows = if rows.is_empty() { "<tr><td colspan='5'>No game requests yet.</td></tr>".into() } else { rows },
    )))
}

/// Capitalise the first character (status labels: "pending" → "Pending").
fn cap_first(s: &str) -> String {
    let mut ch = s.chars();
    match ch.next() {
        Some(c) => c.to_uppercase().collect::<String>() + ch.as_str(),
        None => String::new(),
    }
}

/// Format a unix-seconds timestamp as a compact UTC date-time for admin tables.
fn fmt_epoch(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm), UTC.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        y, m, d, tod / 3600, (tod % 3600) / 60
    )
}

// ── GET handlers ─────────────────────────────────────────────────────────────

async fn admin_accounts_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(admin)) => Html(
            accounts_page_html(&st, Some(admin), "").await.unwrap_or_else(|e| format!("error: {e}")),
        )
        .into_response(),
        _ => Html(login_html("Please sign in first.")).into_response(),
    }
}

async fn admin_requests_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(admin)) => Html(
            requests_page_html(&st, Some(admin), "").await.unwrap_or_else(|e| format!("error: {e}")),
        )
        .into_response(),
        _ => Html(login_html("Please sign in first.")).into_response(),
    }
}

// ── Social test harness page ─────────────────────────────────────────────────

/// Build <option> tags for an account/bot dropdown; `selected_first` pre-selects
/// the first row so a form always submits a valid id.
fn st_options(rows: &[(u64, String)]) -> String {
    rows.iter()
        .map(|(id, name)| format!("<option value='{id}'>{} (#{id})</option>", esc(name)))
        .collect()
}

async fn social_test_page_html(st: &AppState, admin: Option<User>, message: &str) -> Result<String> {
    let accounts = st_list_accounts(&st.db).await;
    let bots = st_list_bots(&st.db).await;
    let real: Vec<(u64, String)> = accounts
        .iter()
        .filter(|(id, _)| !bots.iter().any(|(b, _)| b == id))
        .cloned()
        .collect();
    let target_opts = st_options(&real);
    let all_target_opts = st_options(&accounts);
    let bot_opts = st_options(&bots);
    let signed = admin.map(|a| a.username).unwrap_or_default();

    let state_opts = ["online", "away", "busy", "ingame", "invisible", "offline"]
        .iter()
        .map(|s| format!("<option value='{s}'>{}</option>", cap_first(s)))
        .collect::<String>();
    let kind_opts = ["played", "review", "screenshot"]
        .iter()
        .map(|k| format!("<option value='{k}'>{}</option>", cap_first(k)))
        .collect::<String>();

    let bot_count = bots.len();
    let no_bots_note = if bots.is_empty() {
        "<p class=\"muted\">No test bots yet — spawn one above to enable the status / activity / DM controls.</p>"
    } else {
        ""
    };

    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          {subnav}
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Development</div><h1>Social Test Harness</h1></div><div class="account-box"><span>Signed in as <strong>{signed}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {notice}

            <section class="section"><div class="section-heading"><h2>Spawn Fake Friend</h2><span class="muted">Creates a puppet account ([bot]) and instantly friends it to the target. Puppets are tagged by email so cleanup is safe.</span></div>
              <form method="post" class="row"><input type="hidden" name="return_to" value="social-test"><input type="hidden" name="action" value="bot_spawn">
                <label>Target<select name="target_id">{target_opts}</select></label>
                <input name="bot_name" placeholder="Bot username (e.g. TestPal)">
                <button type="submit">Spawn &amp; Friend</button>
              </form>
            </section>

            <section class="section"><div class="section-heading"><h2>Set Status / Presence</h2><span class="muted">Pushes a live presence diff to the bot's friends. Presence goes stale after ~70s; re-apply to refresh.</span></div>
              {no_bots_note}
              <form method="post" class="row"><input type="hidden" name="return_to" value="social-test"><input type="hidden" name="action" value="bot_set_status">
                <label>Bot<select name="bot_id">{bot_opts}</select></label>
                <label>State<select name="presence_state">{state_opts}</select></label>
                <input name="status_text" placeholder="Custom status (optional)">
                <input name="game_id" placeholder="Game id (for in-game)">
                <input name="game_title" placeholder="Game title (for in-game)">
                <button type="submit">Apply Status</button>
              </form>
            </section>

            <section class="section"><div class="section-heading"><h2>Post Activity</h2><span class="muted">Injects a feed entry authored by the bot. Appears in the target's Activity tab on refresh. Value = seconds played / 0–5 rating.</span></div>
              <form method="post" class="row"><input type="hidden" name="return_to" value="social-test"><input type="hidden" name="action" value="bot_post_activity">
                <label>Bot<select name="bot_id">{bot_opts}</select></label>
                <label>Kind<select name="activity_kind">{kind_opts}</select></label>
                <input name="game_id" placeholder="Game id (optional)">
                <input name="activity_value" type="number" value="0">
                <button type="submit">Post Activity</button>
              </form>
            </section>

            <section class="section"><div class="section-heading"><h2>Send DM</h2><span class="muted">Delivers a chat message from the bot to the target, live over the gateway.</span></div>
              <form method="post" class="row"><input type="hidden" name="return_to" value="social-test"><input type="hidden" name="action" value="bot_send_dm">
                <label>From bot<select name="bot_id">{bot_opts}</select></label>
                <label>To<select name="target_id">{all_target_opts}</select></label>
                <input name="message_body" placeholder="Message text">
                <button type="submit">Send DM</button>
              </form>
            </section>

            <section class="section"><div class="section-heading"><h2>Send Friend Request</h2><span class="muted">Creates a pending incoming request from the bot (tests the Requests tab + badge).</span></div>
              <form method="post" class="row"><input type="hidden" name="return_to" value="social-test"><input type="hidden" name="action" value="bot_send_request">
                <label>From bot<select name="bot_id">{bot_opts}</select></label>
                <label>To<select name="target_id">{all_target_opts}</select></label>
                <button type="submit">Send Request</button>
              </form>
            </section>

            <section class="section"><div class="section-heading"><h2>Cleanup</h2><span class="muted">Deletes all {bot_count} test bot(s) and their friendships, messages, presence, and activity.</span></div>
              <form method="post" class="inline" onsubmit="return confirm('Delete all test bots and their social data?');"><input type="hidden" name="return_to" value="social-test"><input type="hidden" name="action" value="bot_cleanup">
                <button type="submit" class="danger">Remove All Test Bots</button>
              </form>
            </section>
          </div>
        </div>
        "##,
        subnav = admin_subnav("social-test"),
        signed = esc(&signed),
        notice = notice(message),
    )))
}

async fn admin_social_test_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    match current_admin(&st.db, &headers).await {
        Ok(Some(admin)) => Html(
            social_test_page_html(&st, Some(admin), "").await.unwrap_or_else(|e| format!("error: {e}")),
        )
        .into_response(),
        _ => Html(login_html("Please sign in first.")).into_response(),
    }
}
