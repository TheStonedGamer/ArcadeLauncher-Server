// Mesh-VPN control endpoint (ROADMAP T12k-8 — play-from-anywhere). One route,
// `POST /api/social/mesh/preauth`: an authenticated launcher asks the server to
// mint a short-lived, single-use Headscale pre-auth key so its bundled
// `tailscaled` can join the overlay with no interactive Tailscale login. The
// server is the only holder of the Headscale API key (see [`HeadscaleConfig`]).
//
// Included at crate root like the other `*_api.rs` files, so it calls
// `launcher_user`/`unauthorized`/`AppState`/`st.cfg` directly. Pure request
// shaping lives in free helpers below and is covered by `mesh_api_tests`.

// Launcher → server: which device is joining. Body is optional; sensible
// defaults (ephemeral client, no hostname) apply when omitted.
#[derive(Deserialize, Default)]
struct MeshPreauthReq {
    // Per-device stable node name — informational here (the device passes it to
    // its own `tailscale up --hostname`); kept for symmetry/logging.
    #[serde(default)]
    hostname: String,
    // Ephemeral nodes (stream *clients*) are auto-reaped by Headscale shortly
    // after going offline; a *host* PC passes false so it stays listed/wakeable.
    #[serde(default = "mesh_default_true")]
    ephemeral: bool,
}

fn mesh_default_true() -> bool {
    true
}

// ---- pure helpers (KAT-tested) ---------------------------------------------

// Headscale's REST endpoint for minting pre-auth keys.
fn headscale_preauth_url(api_url: &str) -> String {
    format!("{}/api/v1/preauthkey", api_url.trim_end_matches('/'))
}

// Absolute expiry timestamp for a key minted `now`, in the RFC3339 form
// Headscale's gRPC-gateway accepts (UTC, `Z` suffix, second precision).
fn preauth_expiry_rfc3339(now: chrono::DateTime<chrono::Utc>, ttl_secs: i64) -> String {
    (now + chrono::Duration::seconds(ttl_secs))
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

// JSON body for `POST /api/v1/preauthkey`. Single-use (`reusable:false`) so a
// leaked key can't enroll extra nodes.
fn preauth_request_body(user: &str, ephemeral: bool, expiration: &str) -> serde_json::Value {
    serde_json::json!({
        "user": user,
        "reusable": false,
        "ephemeral": ephemeral,
        "expiration": expiration,
        "aclTags": [],
    })
}

// Pull `.preAuthKey.key` out of Headscale's response, tolerating unknown fields.
fn parse_preauth_key(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("preAuthKey")?
        .get("key")?
        .as_str()
        .map(|s| s.to_string())
}

// ---- handler ----------------------------------------------------------------

async fn api_social_mesh_preauth(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<MeshPreauthReq>>,
) -> Response {
    let Some(_me) = launcher_user(&st, &headers).await else {
        return unauthorized();
    };
    let Some(hs) = &st.cfg.headscale else {
        // Feature dormant until configured — same posture as TURN/S3/Redis.
        return (StatusCode::SERVICE_UNAVAILABLE, "mesh VPN not configured").into_response();
    };
    let req = body.map(|Json(b)| b).unwrap_or_default();
    tracing::info!(
        "mesh preauth: minting key for user {} device '{}' (ephemeral={})",
        _me.id,
        req.hostname,
        req.ephemeral
    );

    let expiration = preauth_expiry_rfc3339(chrono::Utc::now(), hs.key_ttl);
    let url = headscale_preauth_url(&hs.api_url);
    let payload = preauth_request_body(&hs.user, req.ephemeral, &expiration);

    let client = match Client::builder().build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("mesh preauth: http client build failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "client error").into_response();
        }
    };

    match client.post(&url).bearer_auth(&hs.api_key).json(&payload).send().await {
        Ok(r) if r.status().is_success() => {
            let text = r.text().await.unwrap_or_default();
            match parse_preauth_key(&text) {
                Some(key) => Json(serde_json::json!({
                    "key": key,
                    "loginServer": hs.api_url,
                    "user": hs.user,
                    "ephemeral": req.ephemeral,
                    "expiresAt": expiration,
                }))
                .into_response(),
                None => {
                    tracing::error!("mesh preauth: unexpected Headscale response shape");
                    (StatusCode::BAD_GATEWAY, "mesh control returned an unexpected response")
                        .into_response()
                }
            }
        }
        Ok(r) => {
            let code = r.status();
            let detail = r.text().await.unwrap_or_default();
            tracing::warn!("mesh preauth: Headscale returned {code}: {detail}");
            (StatusCode::BAD_GATEWAY, format!("mesh control error ({code})")).into_response()
        }
        Err(e) => {
            tracing::error!("mesh preauth: Headscale unreachable: {e}");
            (StatusCode::BAD_GATEWAY, "mesh control unreachable").into_response()
        }
    }
}

#[cfg(test)]
mod mesh_api_tests {
    use super::*;

    #[test]
    fn preauth_url_is_well_formed_and_trims_slash() {
        assert_eq!(
            headscale_preauth_url("https://headscale.orlandoaio.net"),
            "https://headscale.orlandoaio.net/api/v1/preauthkey"
        );
        assert_eq!(
            headscale_preauth_url("https://headscale.orlandoaio.net/"),
            "https://headscale.orlandoaio.net/api/v1/preauthkey"
        );
    }

    #[test]
    fn expiry_is_utc_rfc3339_seconds() {
        // 2023-11-14T22:13:20Z + 600s => 22:23:20Z
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        assert_eq!(preauth_expiry_rfc3339(now, 600), "2023-11-14T22:23:20Z");
    }

    #[test]
    fn request_body_is_single_use() {
        let b = preauth_request_body("arcade", true, "2023-11-14T22:23:20Z");
        assert_eq!(b["user"], "arcade");
        assert_eq!(b["reusable"], false);
        assert_eq!(b["ephemeral"], true);
        assert_eq!(b["expiration"], "2023-11-14T22:23:20Z");
        assert!(b["aclTags"].is_array());
    }

    #[test]
    fn parses_key_from_headscale_response() {
        let resp = r#"{"preAuthKey":{"user":"arcade","id":"7","key":"abc123def","reusable":false,"ephemeral":true,"used":false,"expiration":"2023-11-14T22:23:20Z"}}"#;
        assert_eq!(parse_preauth_key(resp).as_deref(), Some("abc123def"));
    }

    #[test]
    fn parse_key_rejects_malformed_or_missing() {
        assert_eq!(parse_preauth_key("not json"), None);
        assert_eq!(parse_preauth_key("{}"), None);
        assert_eq!(parse_preauth_key(r#"{"preAuthKey":{}}"#), None);
        assert_eq!(parse_preauth_key(r#"{"error":"user not found"}"#), None);
    }
}
