// Waking a phone that has no socket. Everything up to the HTTP request lives
// here and is pure: what a stored device token may look like, what an FCM v1
// message for an incoming call contains, and the OAuth assertion that
// authenticates us to Google. `push_api.rs` does the network and the caching.
//
// Why a *notification* message rather than a data-only one: a data push wakes
// the app only if the OS feels like it, and a killed app not at all. A high
// priority notification on the calls channel rings the phone the way a call is
// supposed to ring, and opening it connects the socket — at which point the
// still-live ring in `calls.rs` is replayed as an ordinary invite. So the push
// announces the call; it never *is* the call.

/// A device's FCM registration token, as stored. Kept as a newtype so a caller
/// cannot accidentally pass a username, a device id or an empty string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushToken(String);

impl PushToken {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// FCM tokens are long opaque strings. We do not attempt to validate their
/// structure — that is Google's business and it has changed before — only that
/// what we were handed is plausible enough to store and short enough not to be
/// an attempt to fill the table.
pub fn parse_push_token(raw: &str) -> Option<PushToken> {
    let t = raw.trim();
    if t.len() < 20 || t.len() > 512 {
        return None;
    }
    if t.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(PushToken(t.to_string()))
}

/// The notification channel the phone created for calls. Must match the id in
/// the app's `ensureCallChannel`, or Android rings it as an ordinary message.
pub const CALL_CHANNEL: &str = "calls";

/// The FCM v1 message body for an incoming call.
///
/// `data` carries the caller so the app can go straight to the right place, and
/// is deliberately all-strings: FCM rejects a data map containing anything else.
pub fn call_push_message(
    token: &PushToken,
    caller_id: u64,
    caller_name: &str,
    video: bool,
) -> serde_json::Value {
    let who = if caller_name.is_empty() { "Someone" } else { caller_name };
    serde_json::json!({
        "message": {
            "token": token.as_str(),
            "notification": {
                "title": if video { "Incoming video call" } else { "Incoming call" },
                "body": format!("{who} is calling"),
            },
            "data": {
                "type": "call",
                "callerId": caller_id.to_string(),
                "video": video.to_string(),
            },
            "android": {
                // A call is the one thing that genuinely justifies waking the
                // device: high priority, and a TTL short enough that a push
                // delivered late never rings for a call that is long over.
                "priority": "HIGH",
                "ttl": "45s",
                "notification": {
                    "channel_id": CALL_CHANNEL,
                    "default_sound": true,
                }
            }
        }
    })
}

/// The service-account credentials, as read from the JSON Google hands out.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct ServiceAccount {
    pub project_id: String,
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

/// Parse the service-account file. Rejects anything missing the three fields we
/// actually sign and send with, so a misconfigured server fails at startup with
/// a clear error rather than on the first call at 2am.
pub fn parse_service_account(json: &str) -> Result<ServiceAccount, String> {
    let sa: ServiceAccount =
        serde_json::from_str(json).map_err(|e| format!("service account is not valid JSON: {e}"))?;
    if sa.project_id.is_empty() || sa.client_email.is_empty() || sa.private_key.is_empty() {
        return Err("service account is missing project_id, client_email or private_key".into());
    }
    if !sa.private_key.contains("PRIVATE KEY") {
        return Err("service account private_key is not a PEM key".into());
    }
    Ok(sa)
}

/// The scope an FCM send needs, and nothing more.
pub const FCM_SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";

/// Claims for the JWT we exchange for an access token. One hour is the maximum
/// Google accepts; `push_api` refreshes well before that.
pub fn token_claims(sa: &ServiceAccount, now: i64) -> serde_json::Value {
    serde_json::json!({
        "iss": sa.client_email,
        "scope": FCM_SCOPE,
        "aud": sa.token_uri,
        "iat": now,
        "exp": now + 3600,
    })
}

/// Where a send goes for this project.
pub fn send_endpoint(sa: &ServiceAccount) -> String {
    format!(
        "https://fcm.googleapis.com/v1/projects/{}/messages:send",
        sa.project_id
    )
}

/// Whether an FCM error means the token is dead and should be forgotten, as
/// opposed to a transient failure worth keeping the token for. Deleting on the
/// wrong error silently un-registers a working phone, so this is narrow on
/// purpose: only "this token is not a thing" counts.
pub fn token_is_dead(status: u16, body: &str) -> bool {
    if status == 404 {
        return true;
    }
    status == 400 && (body.contains("INVALID_ARGUMENT") && body.contains("token"))
        || body.contains("UNREGISTERED")
}

/// Refresh the cached access token a little before it actually expires, so a
/// call never waits on an OAuth round-trip that a clock skew made necessary.
pub const TOKEN_REFRESH_MARGIN_SECS: i64 = 120;

pub fn access_token_is_fresh(expires_at: i64, now: i64) -> bool {
    expires_at - now > TOKEN_REFRESH_MARGIN_SECS
}

#[cfg(test)]
mod push_tests {
    use super::*;

    #[test]
    fn plausible_tokens_are_kept_and_trimmed() {
        let t = parse_push_token("  fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV  ").expect("valid");
        assert_eq!(t.as_str(), "fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV");
    }

    #[test]
    fn junk_tokens_are_rejected() {
        assert!(parse_push_token("").is_none());
        assert!(parse_push_token("short").is_none());
        assert!(parse_push_token(&"x".repeat(600)).is_none());
        // Whitespace inside would break the JSON body and is never in a token.
        assert!(parse_push_token("aaaaaaaaaaaaaaaaaaaa bbbb").is_none());
    }

    #[test]
    fn a_call_push_names_the_caller_and_rings_on_the_call_channel() {
        let t = parse_push_token("fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV").unwrap();
        let m = call_push_message(&t, 42, "brian", true);
        let msg = &m["message"];
        assert_eq!(msg["notification"]["title"], "Incoming video call");
        assert_eq!(msg["notification"]["body"], "brian is calling");
        assert_eq!(msg["android"]["notification"]["channel_id"], CALL_CHANNEL);
        assert_eq!(msg["android"]["priority"], "HIGH");
        // Data values must all be strings or FCM rejects the whole message.
        for (_, v) in msg["data"].as_object().unwrap() {
            assert!(v.is_string());
        }
    }

    #[test]
    fn a_nameless_caller_still_reads_as_a_call() {
        let t = parse_push_token("fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV").unwrap();
        let m = call_push_message(&t, 42, "", false);
        assert_eq!(m["message"]["notification"]["body"], "Someone is calling");
        assert_eq!(m["message"]["notification"]["title"], "Incoming call");
    }

    #[test]
    fn a_short_ttl_stops_a_late_push_ringing_for_a_dead_call() {
        let t = parse_push_token("fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV").unwrap();
        let m = call_push_message(&t, 1, "x", false);
        assert_eq!(m["message"]["android"]["ttl"], "45s");
    }

    #[test]
    fn a_complete_service_account_parses() {
        let sa = parse_service_account(
            r#"{"project_id":"p","client_email":"a@b.iam.gserviceaccount.com",
                "private_key":"-----BEGIN PRIVATE KEY-----\nx\n-----END PRIVATE KEY-----\n"}"#,
        )
        .expect("valid");
        assert_eq!(sa.project_id, "p");
        assert_eq!(sa.token_uri, "https://oauth2.googleapis.com/token");
        assert!(send_endpoint(&sa).ends_with("/projects/p/messages:send"));
    }

    #[test]
    fn an_incomplete_service_account_is_an_error_not_a_default() {
        assert!(parse_service_account("{}").is_err());
        assert!(parse_service_account(r#"{"project_id":"p"}"#).is_err());
        assert!(parse_service_account(
            r#"{"project_id":"p","client_email":"a","private_key":"not a key"}"#
        )
        .is_err());
        assert!(parse_service_account("nonsense").is_err());
    }

    #[test]
    fn claims_ask_only_for_messaging_and_expire_in_an_hour() {
        let sa = parse_service_account(
            r#"{"project_id":"p","client_email":"a@b","private_key":"-----BEGIN PRIVATE KEY-----"}"#,
        )
        .unwrap();
        let c = token_claims(&sa, 1_000);
        assert_eq!(c["scope"], FCM_SCOPE);
        assert_eq!(c["iat"], 1_000);
        assert_eq!(c["exp"], 4_600);
    }

    #[test]
    fn only_a_genuinely_dead_token_is_forgotten() {
        assert!(token_is_dead(404, ""));
        assert!(token_is_dead(400, r#"{"error":{"status":"INVALID_ARGUMENT"},"token":"bad"}"#));
        assert!(token_is_dead(403, "UNREGISTERED"));
        // Transient failures must not un-register a working phone.
        assert!(!token_is_dead(500, "internal"));
        assert!(!token_is_dead(429, "QUOTA_EXCEEDED"));
        assert!(!token_is_dead(401, "invalid credentials"));
    }

    #[test]
    fn a_token_about_to_expire_is_not_fresh() {
        assert!(access_token_is_fresh(1_000, 500));
        assert!(!access_token_is_fresh(1_000, 900));
        assert!(!access_token_is_fresh(1_000, 1_500));
    }
}
