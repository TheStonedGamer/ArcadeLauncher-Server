// admin_html.rs - split out of main.rs and re-assembled via include! (crate-root scope).

async fn restart_service(name: &str) -> Result<String> {
    let service = match name {
        "arcadelauncher-server" => "arcadelauncher-server.service",
        "mariadb" => "mariadb.service",
        _ => return Err(anyhow!("service is not restartable from this panel")),
    };
    if service == "arcadelauncher-server.service" {
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            std::process::exit(1);
        });
        return Ok("Restarting ArcadeLauncher Server. Refresh the admin panel in a few seconds.".into());
    }
    let out = Command::new("sudo")
        .arg("/bin/systemctl")
        .arg("restart")
        .arg(service)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(anyhow!(if err.is_empty() { "service restart failed".into() } else { err }));
    }
    Ok(format!("Restarted {service}."))
}

async fn admin_html(st: &AppState, admin: Option<User>, message: &str, matcher_game_id: &str, matcher_query: &str) -> Result<String> {
    let users = list_users(&st.db).await.unwrap_or_default();
    let tokens = list_launcher_tokens(&st.db).await.unwrap_or_default();
    let games = list_games(&st.db).await.unwrap_or_default();
    let settings = list_server_settings(&st.db).await.unwrap_or_default();
    let service_rows = service_status_rows(st, games.len(), users.len(), tokens.len()).await;
    let validation_summary = validation_summary_rows(st, &games).await;
    let mut by_platform = BTreeMap::<String, usize>::new();
    for g in &games {
        *by_platform.entry(g.platform.clone()).or_default() += 1;
    }
    let user_mgmt = user_management_html(&users);
    let token_rows = tokens
        .iter()
        .map(|(id, name, token, enabled)| {
            format!(
                "<tr><td>{}</td><td><code class='token'>{}</code></td><td>{}</td><td><form method='post' class='inline'><input type='hidden' name='user_id' value='{}'><button name='action' value='rotate_user'>Rotate</button><button name='action' value='delete_user' class='danger'>Delete</button></form></td></tr>",
                esc(name), esc(&masked_value(token)), if *enabled { "Enabled" } else { "Disabled" }, id
            )
        })
        .collect::<String>();
    let platform_rows = by_platform
        .iter()
        .map(|(p, c)| format!("<div class='platform-row'><span>{}</span><strong>{}</strong></div>", esc(p), c))
        .collect::<String>();
    let settings_rows = settings
        .iter()
        .map(|(k, v)| {
            let sensitive = is_sensitive_key(k);
            let value = if sensitive { String::new() } else { v.clone() };
            let masked = if sensitive { format!("<span class='muted'>{}</span>", esc(&masked_value(v))) } else { String::new() };
            format!(
                "<tr><td><code>{}</code></td><td><form method='post' class='inline'><input type='hidden' name='setting_key' value='{}'><input name='setting_value' value='{}'>{}<button name='action' value='save_setting'>Save</button></form></td></tr>",
                esc(k), esc(k), esc(&value), masked
            )
        })
        .collect::<String>();
    let igdb_client_id = settings.iter().find(|(k, _)| k == IGDB_CLIENT_ID_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let igdb_client_secret = settings.iter().find(|(k, _)| k == IGDB_CLIENT_SECRET_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let public_base_url = settings.iter().find(|(k, _)| k == PUBLIC_BASE_URL_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let discord_webhook = settings.iter().find(|(k, _)| k == DISCORD_WEBHOOK_KEY).map(|(_, v)| v.as_str()).unwrap_or("");
    let matcher_html = metadata_matcher_html(st, &games, matcher_game_id, matcher_query).await;

    // Changelog authoring panel: a game picker + note form, and a table of the
    // most recent entries with delete buttons.
    let cl_game_options = games
        .iter()
        .map(|g| format!("<option value='{}'>{}</option>", esc(&g.id), esc(&g.title)))
        .collect::<String>();
    let cl_admin_rows = list_changelogs_admin(&st.db).await.unwrap_or_default();
    let cl_rows = cl_admin_rows
        .iter()
        .map(|(id, game_title, version, title)| {
            format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td><td><form method='post' class='inline'><input type='hidden' name='changelog_id' value='{}'><button name='action' value='delete_changelog' class='danger'>Delete</button></form></td></tr>",
                esc(game_title),
                if version.is_empty() { "&mdash;".into() } else { esc(version) },
                if title.is_empty() { "&mdash;".into() } else { esc(title) },
                id
            )
        })
        .collect::<String>();
    let changelog_section = format!(
        r#"<section id="changelogs" class="section"><div class="section-heading"><h2>Changelogs</h2><span class="muted">Patch notes surfaced in the client game dashboard.</span></div><div class="two-col"><div><h3>Add Entry</h3><form method="post" class="stack"><select name="game_id">{}</select><input name="changelog_version" placeholder="Version (optional, e.g. 1.2.0)"><input name="changelog_title" placeholder="Title (optional)"><textarea name="changelog_body" rows="6" placeholder="Markdown / plain text release notes"></textarea><button name="action" value="add_changelog">Publish Entry</button></form></div><div><h3>Recent Entries</h3><table><thead><tr><th>Game</th><th>Version</th><th>Title</th><th>Action</th></tr></thead><tbody>{}</tbody></table></div></div></section>"#,
        if cl_game_options.is_empty() { "<option value=''>No games cataloged</option>".into() } else { cl_game_options },
        if cl_rows.is_empty() { "<tr><td colspan='4'>No changelog entries yet.</td></tr>".into() } else { cl_rows },
    );

    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          <aside class="sidebar"><div class="brand-block"><div class="brand-mark">AL</div><div><div class="brand-title">ArcadeLauncher</div><div class="brand-subtitle">Rust Server</div></div></div><nav><a href="#overview">Overview</a><a href="#services">Services</a><a href="#library">Library Setup</a><a href="#changelogs">Changelogs</a><a href="/admin/metadata">Metadata</a><a href="#auth">Auth</a><a href="#config">Configuration</a></nav></aside>
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Private library server</div><h1>Server Administration</h1></div><div class="account-box"><span>Signed in as <strong>{}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {}
            <section id="overview" class="section"><div class="section-heading"><h2>Overview</h2><span class="muted">Rust backend, MariaDB catalog, local file delivery</span></div><div class="metric-grid"><div class="metric"><span>Total Games</span><strong>{}</strong></div><div class="metric"><span>Platforms</span><strong>{}</strong></div><div class="metric"><span>Issued Tokens</span><strong>{}</strong></div><div class="metric"><span>Users</span><strong>{}</strong></div></div></section>
            <section id="services" class="section"><div class="section-heading"><h2>Backend Services</h2><span class="muted">Live checks from the running server process</span></div><table><thead><tr><th>Service</th><th>Status</th><th>Details</th><th>Action</th></tr></thead><tbody>{}</tbody></table></section>
            <section id="library" class="section split"><div><div class="section-heading"><h2>Library Setup</h2><span class="muted">Filesystem stores files; MariaDB stores lookup metadata and IGDB art.</span></div><dl class="kv"><dt>Library Root</dt><dd><code>{}</code></dd><dt>Backend</dt><dd><code>rust/axum</code></dd><dt>Art Cache</dt><dd><code>{}</code></dd></dl><form method="post" class="row"><button name="action" value="rescan">Rescan Filesystem and Sync DB</button><button name="action" value="igdb_enrich">Sync IGDB Metadata</button><button name="action" value="igdb_refresh">Force Refresh IGDB Metadata</button><button name="action" value="validate_games">Validate Games</button></form><div id="scan-status" class="scanbox"><div class="scanhead"><span id="sc-phase" class="scanphase idle"><span id="sc-spin" class="scanspin"></span><span id="sc-phtext">Idle</span></span><span class="scanmeta"><strong id="sc-count">0/0</strong> &middot; <span id="sc-pct">0%</span> &middot; elapsed <span id="sc-elapsed">0s</span></span></div><div id="sc-bar" class="scanbar"><i id="sc-fill"></i></div><div id="sc-cur" class="scancur"><span id="sc-cur-game"></span><span id="sc-curfile" class="scancurfile"></span></div><div id="sc-plat" class="scanplat"></div><div id="sc-msg" class="scanmsg"></div></div><script>(function(){{var box=document.getElementById('scan-status');if(!box)return;var phEl=document.getElementById('sc-phase'),phTx=document.getElementById('sc-phtext'),spin=document.getElementById('sc-spin'),cnt=document.getElementById('sc-count'),pctEl=document.getElementById('sc-pct'),elEl=document.getElementById('sc-elapsed'),bar=document.getElementById('sc-bar'),fill=document.getElementById('sc-fill'),curg=document.getElementById('sc-cur-game'),cf=document.getElementById('sc-curfile'),plat=document.getElementById('sc-plat'),msg=document.getElementById('sc-msg');var names={{scanning:'Scanning library',hashing:'Hashing files',igdb:'Enriching metadata',done:'Completed',error:'Failed',idle:'Idle'}};function fmt(sec){{if(sec<60)return sec+'s';var m=Math.floor(sec/60),s=sec%60;return m+'m '+(s<10?'0':'')+s+'s';}}function render(s){{var phase=s.phase||'idle';box.classList.add('active');phEl.className='scanphase '+(phase==='done'?'done':phase==='error'?'error':phase==='idle'?'idle':'');phTx.textContent=names[phase]||phase;spin.style.display=s.running?'inline-block':'none';var total=s.total||0,proc=s.processed||0;var pct=total?Math.floor(proc*100/total):0;cnt.textContent=proc+'/'+total;pctEl.textContent=pct+'%';var nowSec=Math.floor(Date.now()/1000);var srv=s.startedAt?((s.updatedAt||nowSec)-s.startedAt):0;var elapsed=s.running?Math.max(srv,nowSec-(s.startedAt||nowSec)):srv;elEl.textContent=fmt(Math.max(0,elapsed));if(s.running&&!total){{bar.className='scanbar indet';fill.style.width='40%';}}else{{bar.className='scanbar';fill.style.width=pct+'%';}}var act=s.active||0;curg.textContent=s.current?('Hashing: '+s.current+(act>1?(' (+'+(act-1)+' more in parallel)'):'')):(act>0?(act+' hashing in parallel'):'');cf.textContent=(s.currentFile&&phase==='hashing')?('Current File: '+s.currentFile):'';var pp=s.perPlatform||{{}};var keys=Object.keys(pp).sort();var ph='';for(var i=0;i<keys.length;i++){{var k=keys[i],v=pp[k]||{{}},t=v.total||0,p=v.processed||0,w=t?Math.floor(p*100/t):0,dn=(t&&p>=t);ph+='<div class="scanplat-row"><span class="scanplat-name">'+k+'</span><span class="scanplat-bar'+(dn?' done':'')+'"><i style="width:'+w+'%"></i></span><span class="scanplat-num">'+p+'/'+t+'</span></div>';}}plat.innerHTML=((phase==='hashing'||phase==='done')&&keys.length)?ph:'';msg.textContent=(phase==='done'||phase==='error')?(s.message||''):'';}}function poll(){{fetch('/admin/scan-status').then(function(r){{return r.json();}}).then(function(s){{if(!s)return;if(s.running||s.phase==='done'||s.phase==='error'){{render(s);}}else{{box.classList.remove('active');}}}}).catch(function(){{}});setTimeout(poll,1000);}}poll();}})();</script>{}<h3>Game Validation</h3><table><thead><tr><th>Check</th><th>Status</th><th>Details</th></tr></thead><tbody>{}</tbody></table></div><div class="platform-card"><h3>Platform Counts</h3>{}</div></section>
            {}
            <section id="auth" class="section"><div class="section-heading"><h2>Auth Management</h2><span class="muted">All users sign in with username/password; bearer tokens are issued behind the scenes.</span></div><div class="two-col"><div><h3>Create User</h3><form method="post" class="row"><input name="username" placeholder="Username"><input name="email" type="email" placeholder="Email"><input name="password" type="password" placeholder="Password"><label class="checkline"><input type="checkbox" name="is_admin" value="1"> Admin</label><button name="action" value="add_user">Create User</button></form><h3>Users</h3>{}</div><div><h3>Issued Tokens</h3><table><thead><tr><th>Name</th><th>Bearer Token</th><th>Status</th><th>Actions</th></tr></thead><tbody>{}</tbody></table></div></div></section>
            <section id="config" class="section"><div class="section-heading"><h2>Configuration</h2><span class="muted">Runtime env is read-only here; managed settings are stored in MariaDB.</span></div><div class="two-col"><div><h3>Runtime</h3><dl class="kv"><dt>API Listen</dt><dd><code>{}:{}</code></dd><dt>Admin Listen</dt><dd><code>{}:{}</code></dd><dt>Library</dt><dd><code>{}</code></dd><dt>Database</dt><dd><code>{}:{} / {}</code></dd><dt>Chunking</dt><dd><code>{} byte raw chunks; full-file fallback retained</code></dd></dl></div><div><h3>Backend URL</h3><form method="post" class="stack"><input type="hidden" name="setting_key" value="server.public_base_url"><input name="setting_value" type="url" placeholder="https://arcade.orlandoaio.net" value="{}"><button name="action" value="save_setting">Save Backend URL</button></form><h3>IGDB Credentials</h3><form method="post" class="stack"><input type="hidden" name="setting_key" value="igdb.client_id"><input name="setting_value" placeholder="IGDB/Twitch Client ID" value="{}"><button name="action" value="save_setting">Save Client ID</button></form><form method="post" class="stack credential-form"><input type="hidden" name="setting_key" value="igdb.client_secret"><input name="setting_value" type="password" placeholder="{}"><button name="action" value="save_setting">Save Client Secret</button></form><form method="post" class="row new-setting"><button name="action" value="igdb_enrich">Sync IGDB Metadata</button></form><h3>Discord Notifications</h3><form method="post" class="stack"><input type="hidden" name="setting_key" value="discord.webhook_url"><input name="setting_value" type="url" placeholder="https://discord.com/api/webhooks/..." value="{}"><div class="row"><button name="action" value="save_setting">Save Webhook</button><button name="action" value="test_webhook">Test Webhook</button></div></form><h3>Managed Settings</h3><table><thead><tr><th>Key</th><th>Value</th></tr></thead><tbody>{}</tbody></table><form method="post" class="row new-setting"><input name="setting_key" placeholder="setting.key"><input name="setting_value" placeholder="value"><button name="action" value="save_setting">Add / Save</button></form></div></div></section>
          </div>
        </div>
        "##,
        esc(&signed),
        notice(message),
        games.len(),
        by_platform.len(),
        tokens.len(),
        users.len(),
        service_rows,
        esc(&st.cfg.library_root.display().to_string()),
        esc(&st.cfg.library_root.join(".arcadelauncher").join("art").display().to_string()),
        matcher_html,
        validation_summary,
        if platform_rows.is_empty() { "<p class='muted'>No cataloged platforms yet.</p>".into() } else { platform_rows },
        changelog_section,
        user_mgmt,
        if token_rows.is_empty() { "<tr><td colspan='4'>No issued tokens yet.</td></tr>".into() } else { token_rows },
        esc(&st.cfg.host),
        st.cfg.port,
        esc(&st.cfg.admin_host),
        st.cfg.admin_port,
        esc(&st.cfg.library_root.display().to_string()),
        esc(&st.cfg.db_host),
        st.cfg.db_port,
        esc(&st.cfg.db_name),
        CHUNK_SIZE,
        esc(public_base_url),
        esc(igdb_client_id),
        if igdb_client_secret.is_empty() { "IGDB/Twitch Client Secret".into() } else { format!("Saved ({})", masked_value(igdb_client_secret)) },
        esc(discord_webhook),
        if settings_rows.is_empty() { "<tr><td colspan='2'>No managed settings saved yet.</td></tr>".into() } else { settings_rows },
    )))
}

// Enterprise-style user management: a filterable list of account cards, each
// with inline email/role/status/password editing plus lifecycle actions
// (2FA toggle, force password change, delete). The consolidated "Save Changes"
// button posts the whole card via the `update_user` action; the last-admin
// guard lives server-side in apply_user_update / delete_user_account.
fn user_management_html(users: &[User]) -> String {
    let total = users.len();
    let admins = users.iter().filter(|u| u.is_admin).count();
    let disabled = users.iter().filter(|u| !u.enabled).count();
    let cards = users
        .iter()
        .map(|u| {
            let initial = u
                .username
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_else(|| "?".into());
            let role_badge = if u.is_admin {
                "<span class='badge admin'>Admin</span>"
            } else {
                "<span class='badge client'>Client</span>"
            };
            let status_badge = if u.enabled {
                "<span class='badge on'>Enabled</span>"
            } else {
                "<span class='badge off'>Disabled</span>"
            };
            let twofa_badge = if u.totp_enabled {
                "<span class='badge on'>2FA on</span>"
            } else {
                "<span class='badge warn'>2FA off</span>"
            };
            let totp_action = if u.totp_enabled { "disable_totp" } else { "enable_totp" };
            let totp_label = if u.totp_enabled { "Disable 2FA" } else { "Enable 2FA" };
            let (admin_sel, client_sel) = if u.is_admin { (" selected", "") } else { ("", " selected") };
            let (enabled_sel, disabled_sel) = if u.enabled { (" selected", "") } else { ("", " selected") };
            format!(
                "<form method='post' class='user-card' data-search='{search}'>\
                 <input type='hidden' name='user_id' value='{id}'>\
                 <div class='user-card-head'><div class='user-avatar'>{initial}</div>\
                 <div class='user-card-id'><div class='user-name'>{name}</div><div class='user-sub'>{email}</div>\
                 <div class='badge-row'>{role}{status}{twofa}</div></div>\
                 <span class='user-id'>#{id}</span></div>\
                 <div class='user-grid'>\
                 <div class='field'><label>Email</label><input name='email' type='email' value='{email}'></div>\
                 <div class='field'><label>Role</label><select name='is_admin'><option value='1'{asel}>Admin</option><option value='0'{csel}>Client</option></select></div>\
                 <div class='field'><label>Status</label><select name='enabled'><option value='1'{esel}>Enabled</option><option value='0'{dsel}>Disabled</option></select></div>\
                 <div class='field'><label>Reset Password</label><input name='password' type='password' placeholder='Leave blank to keep' autocomplete='new-password'></div>\
                 </div>\
                 <div class='user-actions'>\
                 <button name='action' value='update_user'>Save Changes</button>\
                 <button name='action' value='{totp_action}' class='ghost'>{totp_label}</button>\
                 <button name='action' value='force_password_change' class='ghost'>Require Password Change</button>\
                 <button name='action' value='delete_account' class='danger'>Delete Account</button>\
                 </div></form>",
                search = esc(&format!("{} {}", u.username, u.email).to_lowercase()),
                id = u.id,
                initial = esc(&initial),
                name = esc(&u.username),
                email = esc(&u.email),
                role = role_badge,
                status = status_badge,
                twofa = twofa_badge,
                asel = admin_sel,
                csel = client_sel,
                esel = enabled_sel,
                dsel = disabled_sel,
                totp_action = totp_action,
                totp_label = totp_label,
            )
        })
        .collect::<String>();
    let cards = if cards.is_empty() {
        "<div class='empty-state'>No users yet. Create one above.</div>".to_string()
    } else {
        cards
    };
    format!(
        "<div class='user-toolbar'><input id='user-filter' placeholder='Filter by username or email…' autocomplete='off'>\
         <span class='muted'>{total} users &middot; {admins} admin &middot; {disabled} disabled</span></div>\
         <div id='user-list' class='user-list'>{cards}</div>\
         <script>(function(){{var f=document.getElementById('user-filter');if(!f)return;f.addEventListener('input',function(){{var q=f.value.trim().toLowerCase();var c=document.querySelectorAll('#user-list .user-card');for(var i=0;i<c.length;i++){{var s=c[i].getAttribute('data-search')||'';c[i].classList.toggle('userhidden',q!==''&&s.indexOf(q)<0);}}}});}})();</script>",
        total = total,
        admins = admins,
        disabled = disabled,
        cards = cards
    )
}

async fn metadata_page_html(st: &AppState, admin: Option<User>, message: &str, selected_id: &str, query: &str) -> Result<String> {
    let games = list_games(&st.db).await?;
    let selected = games
        .iter()
        .find(|g| g.id == selected_id)
        .or_else(|| games.iter().find(|g| g.igdb_id == 0))
        .or_else(|| games.first());
    let selected_id = selected.map(|g| g.id.as_str()).unwrap_or("");
    let search_value = if query.trim().is_empty() {
        selected.map(|g| g.title.as_str()).unwrap_or("")
    } else {
        query
    };
    let options = games
        .iter()
        .map(|g| {
            let sel = if g.id == selected_id { " selected" } else { "" };
            let marker = if g.igdb_id == 0 { " *" } else { "" };
            format!("<option value=\"{}\"{}>{} - {}{}</option>", esc(&g.id), sel, esc(&g.platform), esc(&g.title), marker)
        })
        .collect::<String>();

    let mut result_cards = String::new();
    if let Some(game) = selected {
        if !query.trim().is_empty() {
            match igdb_search_for_game(st, game, query.trim()).await {
                Ok(results) if results.is_empty() => {
                    result_cards = format!("<div class='empty-state'>No IGDB matches found for {}.</div>", esc(&game.platform));
                }
                Ok(results) => {
                    result_cards = results
                        .into_iter()
                        .map(|m| {
                            let year = if m.release_date > 0 {
                                chrono::DateTime::from_timestamp(m.release_date, 0).map(|d| d.format("%Y").to_string()).unwrap_or_default()
                            } else {
                                String::new()
                            };
                            let cover = if m.cover_image_id.is_empty() {
                                "<div class='match-cover placeholder'>No Art</div>".to_string()
                            } else {
                                format!("<img class='match-cover' src='{}' alt=''>", esc(&igdb_cover_url(&m.cover_image_id)))
                            };
                            format!(
                                "<article class='match-card'>{}<div class='match-body'><div class='match-title'>{}</div><div class='match-meta'>{} · {:.0} · {}</div><p>{}</p><form method='post' action='/admin/metadata'><input type='hidden' name='game_id' value='{}'><input type='hidden' name='search_query' value='{}'><input type='hidden' name='igdb_id' value='{}'><button name='action' value='igdb_apply'>Apply Match</button></form></div></article>",
                                cover,
                                esc(&m.name),
                                esc(&year),
                                m.rating,
                                esc(&m.genres),
                                esc(&m.summary),
                                esc(&game.id),
                                esc(query),
                                m.id
                            )
                        })
                        .collect();
                }
                Err(e) => {
                    result_cards = format!("<div class='empty-state'>{}</div>", esc(&e.to_string()));
                }
            }
        }
    }
    if result_cards.is_empty() {
        result_cards = "<div class='empty-state'>Search IGDB to choose a metadata match. Platform filtering is applied automatically.</div>".into();
    }

    let current = selected
        .map(|g| {
            let cover_src = admin_cover_src(g);
            let cover = if cover_src.is_empty() {
                "<div class='current-cover placeholder'>No Art</div>".to_string()
            } else {
                format!("<img class='current-cover' src='{}' alt=''>", esc(&cover_src))
            };
            format!(
                "<div class='current-game'>{}<div><h2>{}</h2><dl class='kv'><dt>Platform</dt><dd>{}</dd><dt>IGDB ID</dt><dd>{}</dd><dt>Rating</dt><dd>{:.0}</dd><dt>Genres</dt><dd>{}</dd><dt>Summary</dt><dd>{}</dd></dl></div></div>",
                cover,
                esc(&g.title),
                esc(&g.platform),
                if g.igdb_id == 0 { "Unmatched".into() } else { g.igdb_id.to_string() },
                g.igdb_rating,
                esc(&g.genres),
                esc(&g.summary)
            )
        })
        .unwrap_or_else(|| "<div class='empty-state'>No games are currently cataloged.</div>".into());
    let signed = admin.map(|a| a.username).unwrap_or_default();
    Ok(shell(&format!(
        r##"
        <div class="admin-layout">
          <aside class="sidebar"><div class="brand-block"><div class="brand-mark">AL</div><div><div class="brand-title">ArcadeLauncher</div><div class="brand-subtitle">Rust Server</div></div></div><nav><a href="/admin">Dashboard</a><a href="/admin/metadata">Metadata</a><a href="/admin#library">Library</a><a href="/admin#auth">Auth</a><a href="/admin#config">Configuration</a></nav></aside>
          <div class="content">
            <section class="topbar"><div><div class="eyebrow">Metadata management</div><h1>IGDB Match Search</h1></div><div class="account-box"><span>Signed in as <strong>{}</strong></span><a class="buttonlink" href="/admin/logout">Sign Out</a></div></section>
            {}
            <section class="section metadata-shell">
              <div class="section-heading"><h2>Selected Game</h2><span class="muted">Games marked with * need a manual match</span></div>
              <form method="post" action="/admin/metadata" class="row matcher-form"><select name="game_id">{}</select><input name="search_query" value="{}" placeholder="Search title"><button name="action" value="igdb_search">Search IGDB</button></form>
              {}
            </section>
            <section class="section"><div class="section-heading"><h2>Search Results</h2><span class="muted">Filtered to the selected game's platform</span></div><div class="match-grid">{}</div></section>
          </div>
        </div>
        "##,
        esc(&signed),
        notice(message),
        options,
        esc(search_value),
        current,
        result_cards
    )))
}

async fn metadata_matcher_html(st: &AppState, games: &[Game], selected_id: &str, query: &str) -> String {
    let selected = games
        .iter()
        .find(|g| g.id == selected_id)
        .or_else(|| games.first());
    let selected_id = selected.map(|g| g.id.as_str()).unwrap_or("");
    let default_query = if query.trim().is_empty() {
        selected.map(|g| g.title.as_str()).unwrap_or("")
    } else {
        query
    };
    let options = games
        .iter()
        .map(|g| {
            let sel = if g.id == selected_id { " selected" } else { "" };
            format!("<option value=\"{}\"{}>{} - {}</option>", esc(&g.id), sel, esc(&g.platform), esc(&g.title))
        })
        .collect::<String>();
    let mut result_rows = String::new();
    if let Some(game) = selected {
        if !query.trim().is_empty() {
            match igdb_search_for_game(st, game, query.trim()).await {
                Ok(results) if results.is_empty() => {
                    result_rows = format!("<tr><td colspan='6'>No IGDB matches found for {}.</td></tr>", esc(&game.platform));
                }
                Ok(results) => {
                    result_rows = results
                        .into_iter()
                        .map(|m| {
                            let year = if m.release_date > 0 {
                                chrono::DateTime::from_timestamp(m.release_date, 0).map(|d| d.format("%Y").to_string()).unwrap_or_default()
                            } else {
                                String::new()
                            };
                            let summary = if m.summary.chars().count() > 180 {
                                format!("{}...", m.summary.chars().take(180).collect::<String>())
                            } else {
                                m.summary.clone()
                            };
                            format!(
                                "<tr><td>{}</td><td>{}</td><td>{:.0}</td><td>{}</td><td>{}</td><td><form method='post'><input type='hidden' name='game_id' value='{}'><input type='hidden' name='search_query' value='{}'><input type='hidden' name='igdb_id' value='{}'><button name='action' value='igdb_apply'>Apply</button></form></td></tr>",
                                esc(&m.name), esc(&year), m.rating, esc(&m.genres), esc(&summary), esc(&game.id), esc(query), m.id
                            )
                        })
                        .collect();
                }
                Err(e) => {
                    result_rows = format!("<tr><td colspan='6'>{}</td></tr>", esc(&e.to_string()));
                }
            }
        }
    }
    if result_rows.is_empty() {
        result_rows = "<tr><td colspan='6'>Search IGDB to choose a metadata match.</td></tr>".into();
    }
    format!(
        r#"<h3>Metadata Matcher <span class="muted">({})</span></h3><form method="post" action="/admin/metadata" class="row matcher-form"><select name="game_id">{}</select><input name="search_query" value="{}" placeholder="Search title"><button name="action" value="igdb_search">Open Full Search</button></form><table class="matcher-results"><thead><tr><th>IGDB Title</th><th>Year</th><th>Rating</th><th>Genres</th><th>Summary</th><th>Action</th></tr></thead><tbody>{}</tbody></table>"#,
        esc(selected.map(|g| g.platform.as_str()).unwrap_or("No platform")),
        options,
        esc(default_query),
        result_rows
    )
}

async fn service_status_rows(st: &AppState, game_count: usize, user_count: usize, token_count: usize) -> String {
    let mut rows = Vec::new();
    rows.push(status_row(
        "ArcadeLauncher Server",
        true,
        &format!("Rust process listening on {}:{}", st.cfg.host, st.cfg.port),
        Some("arcadelauncher-server"),
    ));

    let db_ok = db_ping(&st.db).await;
    rows.push(status_row(
        "MariaDB",
        db_ok,
        &format!(
            "{}:{} / {} as {}",
            st.cfg.db_host, st.cfg.db_port, st.cfg.db_name, st.cfg.db_user
        ),
        Some("mariadb"),
    ));

    rows.push(status_row(
        "Catalog Database",
        game_count > 0,
        &format!("{game_count} games, {user_count} users, {token_count} issued tokens"),
        None,
    ));

    let library_meta = fs::metadata(&st.cfg.library_root).await;
    rows.push(status_row(
        "Library Root",
        library_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        &st.cfg.library_root.display().to_string(),
        None,
    ));

    let games_path = st.cfg.library_root.join("games");
    let games_meta = fs::metadata(&games_path).await;
    rows.push(status_row(
        "Game Storage",
        games_meta.as_ref().map(|m| m.is_dir()).unwrap_or(false),
        &games_path.display().to_string(),
        None,
    ));

    rows.push(status_row(
        "Catalog Generator",
        true,
        "Native Rust scanner/upserter",
        None,
    ));

    let mount_detail = command_output("findmnt", &["-T", st.cfg.library_root.to_str().unwrap_or("")]).await;
    rows.push(status_row(
        "Library Mount",
        mount_detail.is_ok(),
        mount_detail.as_deref().unwrap_or("mount lookup unavailable"),
        None,
    ));

    let disk_detail = command_output("df", &["-h", st.cfg.library_root.to_str().unwrap_or("")]).await;
    rows.push(status_row(
        "Disk Space",
        disk_detail.is_ok(),
        disk_detail.as_deref().unwrap_or("disk usage unavailable"),
        None,
    ));

    rows.join("")
}

async fn db_ping(db: &Pool) -> bool {
    let Ok(mut conn) = db.get_conn().await else { return false; };
    conn.query_drop("SELECT 1").await.is_ok()
}

async fn command_output(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if !out.status.success() {
        return Err(anyhow!(text));
    }
    Ok(text)
}

async fn validation_summary_rows(st: &AppState, games: &[Game]) -> String {
    let mut missing = 0usize;
    let mut present = 0usize;
    for game in games {
        if let Ok(path) = content_path_for(&st.cfg, game).await {
            if fs::metadata(&path).await.is_ok() {
                present += 1;
            } else {
                missing += 1;
            }
        } else {
            missing += 1;
        }
    }
    let ok = missing == 0 && !games.is_empty();
    format!(
        "<tr><td>Catalog Paths</td><td><span class='status {}'>{}</span></td><td><code>{} present, {} missing. Click Validate Games for file/byte details.</code></td></tr>",
        if ok { "ok" } else { "bad" },
        if ok { "OK" } else { "Needs Attention" },
        present,
        missing
    )
}

fn status_row(name: &str, ok: bool, details: &str, restart: Option<&str>) -> String {
    let action = restart
        .map(|svc| {
            format!(
                "<form method='post' class='inline'><input type='hidden' name='service_name' value='{}'><button name='action' value='restart_service'>Restart</button></form>",
                esc(svc)
            )
        })
        .unwrap_or_else(|| "<span class='muted'>Not restartable</span>".into());
    format!(
        "<tr><td>{}</td><td><span class='status {}'>{}</span></td><td><code>{}</code></td><td>{}</td></tr>",
        esc(name),
        if ok { "ok" } else { "bad" },
        if ok { "Online" } else { "Needs Attention" },
        esc(details),
        action
    )
}

fn login_html(message: &str) -> String {
    shell(&format!(
        r#"<section><h2>Sign In</h2>{}<form method="post" action="/admin/login" class="stack"><input name="username" placeholder="Username or email" autofocus required><input name="password" type="password" placeholder="Password" required><input name="totp_code" inputmode="numeric" autocomplete="one-time-code" placeholder="2FA code, if enabled"><button name="action" value="login">Sign In</button></form></section>"#,
        notice(message)
    ))
}

fn notice(message: &str) -> String {
    if message.is_empty() {
        String::new()
    } else {
        format!("<pre class='notice'>{}</pre>", esc(message))
    }
}

fn shell(body: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>ArcadeLauncher Server</title><link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><rect width='100' height='100' rx='20' fill='%237c5cff'/><text x='50' y='72' font-size='64' text-anchor='middle' fill='white'>%F0%9F%8E%AE</text></svg>"><style>{}</style></head><body><main>{}</main></body></html>"#,
        CSS, body
    )
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

static CSS: &str = r#"
:root{color-scheme:dark;--bg:#0f1115;--panel:#171b21;--panel2:#1d232b;--line:#2c3540;--text:#e8edf2;--muted:#9aa7b5;--accent:#4cc2ff;--bad:#ff6b6b}
*{box-sizing:border-box}body{margin:0;font:14px/1.45 "Segoe UI",sans-serif;background:var(--bg);color:var(--text)}main{width:100%;min-height:100vh}h1,h2,h3{margin:0;letter-spacing:0}h1{font-size:28px}h2{font-size:19px}h3{font-size:15px;margin:18px 0 10px}.admin-layout{display:grid;grid-template-columns:250px 1fr;min-height:100vh}.sidebar{background:#11161d;border-right:1px solid var(--line);padding:24px 18px}.brand-block{display:flex;gap:12px;align-items:center;margin-bottom:28px}.brand-mark{width:42px;height:42px;display:grid;place-items:center;background:var(--accent);color:#041019;font-weight:800;border-radius:8px}.brand-title{font-weight:700}.brand-subtitle,.muted,.eyebrow{color:var(--muted)}nav{display:flex;flex-direction:column;gap:6px}nav a,.buttonlink{color:var(--text);text-decoration:none;padding:9px 10px;border-radius:6px;border:1px solid transparent}nav a:hover,.buttonlink:hover{border-color:var(--line);background:var(--panel)}.content{padding:24px;min-width:0}.topbar{display:flex;justify-content:space-between;gap:16px;align-items:center;margin-bottom:20px}.account-box{display:flex;gap:12px;align-items:center;flex-wrap:wrap}.section{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:18px;margin-bottom:16px}.section-heading{display:flex;justify-content:space-between;gap:14px;align-items:end;margin-bottom:14px}.metric-grid{display:grid;grid-template-columns:repeat(4,minmax(120px,1fr));gap:12px}.metric{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px}.metric span{display:block;color:var(--muted)}.metric strong{font-size:26px}.split,.two-col{display:grid;grid-template-columns:minmax(0,1fr) 320px;gap:20px}.two-col{grid-template-columns:repeat(2,minmax(0,1fr))}.platform-card{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px}.platform-row{display:flex;justify-content:space-between;border-bottom:1px solid var(--line);padding:7px 0}.kv{display:grid;grid-template-columns:120px minmax(0,1fr);gap:8px 12px}.kv dt{color:var(--muted)}.kv dd{margin:0;min-width:0}.row{display:flex;gap:10px;flex-wrap:wrap;align-items:center}.stack{display:flex;gap:10px;flex-direction:column;align-items:flex-start}.inline{display:flex;gap:8px;flex-wrap:wrap}.new-setting{margin-top:12px}.matcher-form{margin:10px 0}.matcher-form select{max-width:420px}.matcher-results{margin-bottom:14px}.checkline{display:inline-flex;align-items:center;gap:6px;color:var(--muted)}input,select{background:#0c1015;color:var(--text);border:1px solid var(--line);border-radius:6px;padding:9px 10px;min-width:180px}button{background:var(--accent);color:#041019;border:0;border-radius:6px;padding:9px 12px;font-weight:700;cursor:pointer}.danger{background:var(--bad);color:#180406}table{width:100%;border-collapse:collapse;background:var(--panel2);border-radius:8px;overflow:hidden}th,td{text-align:left;border-bottom:1px solid var(--line);padding:9px;vertical-align:top}th{color:var(--muted);font-weight:600}code,.token{overflow-wrap:anywhere;white-space:pre-wrap}.status{display:inline-flex;align-items:center;border-radius:999px;padding:4px 9px;font-weight:700;font-size:12px}.status.ok{background:#10351f;color:#74e19a}.status.bad{background:#3d1518;color:#ff8b8b}.notice{white-space:pre-wrap;background:#102033;border:1px solid #285b86;padding:12px;border-radius:8px}@media(max-width:900px){.admin-layout{grid-template-columns:1fr}.sidebar{position:static}.metric-grid,.split,.two-col{grid-template-columns:1fr}.topbar{align-items:flex-start;flex-direction:column}}
.metadata-shell{overflow:hidden}.current-game{display:grid;grid-template-columns:180px minmax(0,1fr);gap:18px;align-items:start}.current-cover,.match-cover{width:100%;aspect-ratio:3/4;object-fit:cover;border-radius:8px;background:#0c1015;border:1px solid var(--line)}.current-cover.placeholder,.match-cover.placeholder{display:grid;place-items:center;color:var(--muted);font-weight:700}.match-grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(360px,1fr));gap:14px}.match-card{display:grid;grid-template-columns:104px minmax(0,1fr);gap:14px;background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:12px}.match-title{font-weight:800;font-size:16px}.match-meta{color:var(--muted);margin:3px 0 8px}.match-body p{margin:0 0 12px;color:#c9d3dd}.empty-state{background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:16px;color:var(--muted)}@media(max-width:900px){.current-game,.match-card{grid-template-columns:1fr}.match-grid{grid-template-columns:1fr}}
.scanbox{margin:12px 0;background:var(--panel2);border:1px solid var(--line);border-radius:8px;padding:14px;display:none}.scanbox.active{display:block}.scanhead{display:flex;align-items:center;gap:10px;flex-wrap:wrap;margin-bottom:8px}.scanphase{display:inline-flex;align-items:center;gap:7px;border-radius:999px;padding:4px 11px;font-weight:700;font-size:12px;background:#11283a;color:var(--accent)}.scanphase.done{background:#10351f;color:#74e19a}.scanphase.error{background:#3d1518;color:#ff8b8b}.scanphase.idle{background:#22282f;color:var(--muted)}.scanspin{width:11px;height:11px;border-radius:50%;border:2px solid rgba(76,194,255,.35);border-top-color:var(--accent);animation:scanspin .8s linear infinite}@keyframes scanspin{to{transform:rotate(360deg)}}.scanmeta{color:var(--muted);font-size:13px}.scanmeta strong{color:var(--text)}.scanbar{height:10px;border-radius:999px;background:#0c1015;border:1px solid var(--line);overflow:hidden;margin:8px 0}.scanbar>i{display:block;height:100%;width:0;background:linear-gradient(90deg,#4cc2ff,#7c5cff);transition:width .4s ease}.scanbar.indet>i{width:40%;animation:scanindet 1.2s ease-in-out infinite}@keyframes scanindet{0%{margin-left:-40%}100%{margin-left:100%}}.scancur{font-size:13px;color:#c9d3dd;overflow-wrap:anywhere;min-height:18px}.scancurfile{margin-left:40px;color:var(--muted)}.scancurfile:empty{margin-left:0}.scanmsg{margin-top:8px;color:var(--muted);white-space:pre-wrap;font-size:13px}.scanplat{margin-top:10px;display:flex;flex-direction:column;gap:6px}.scanplat-row{display:grid;grid-template-columns:120px minmax(0,1fr) 74px;gap:10px;align-items:center;font-size:12px}.scanplat-name{color:var(--text);font-weight:600;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.scanplat-bar{height:8px;border-radius:999px;background:#0c1015;border:1px solid var(--line);overflow:hidden}.scanplat-bar>i{display:block;height:100%;width:0;background:linear-gradient(90deg,#4cc2ff,#7c5cff);transition:width .4s ease}.scanplat-bar.done>i{background:linear-gradient(90deg,#39d98a,#74e19a)}.scanplat-num{color:var(--muted);text-align:right;font-variant-numeric:tabular-nums}
.user-toolbar{display:flex;gap:12px;align-items:center;margin-bottom:14px;flex-wrap:wrap}.user-toolbar input{flex:1;min-width:220px}.user-list{display:flex;flex-direction:column;gap:12px}.user-card{background:var(--panel2);border:1px solid var(--line);border-radius:10px;padding:14px;margin:0}.user-card-head{display:flex;align-items:center;gap:12px;margin-bottom:12px}.user-avatar{width:40px;height:40px;border-radius:50%;display:grid;place-items:center;background:linear-gradient(135deg,#4cc2ff,#7c5cff);color:#041019;font-weight:800;font-size:16px;flex:0 0 auto}.user-card-id{min-width:0}.user-name{font-weight:700;font-size:15px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.user-sub{color:var(--muted);font-size:12px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}.user-id{margin-left:auto;color:var(--muted);font-size:12px;font-variant-numeric:tabular-nums;flex:0 0 auto}.badge{display:inline-flex;align-items:center;border-radius:999px;padding:3px 9px;font-weight:700;font-size:11px;letter-spacing:.02em}.badge.admin{background:#2a1d3d;color:#c9a6ff}.badge.client{background:#11283a;color:#7cc7ff}.badge.on{background:#10351f;color:#74e19a}.badge.off{background:#3d1518;color:#ff8b8b}.badge.warn{background:#3a2f12;color:#f4cf6b}.badge-row{display:flex;gap:6px;flex-wrap:wrap;margin-top:4px}.user-grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:10px;margin-bottom:12px}.field{display:flex;flex-direction:column;gap:4px;min-width:0}.field label{color:var(--muted);font-size:12px}.field input,.field select{min-width:0;width:100%}.user-actions{display:flex;gap:8px;flex-wrap:wrap}.ghost{background:transparent;color:var(--text);border:1px solid var(--line)}.ghost:hover{border-color:var(--accent)}.userhidden{display:none!important}
"#;

