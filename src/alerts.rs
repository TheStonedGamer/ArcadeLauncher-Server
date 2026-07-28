// Admin-originated push: a test notification to one account, and a broadcast to
// everyone. Pure half — what counts as a sendable alert, what the FCM message
// looks like, and how a broadcast reports itself. `push_api.rs` does the sending.
//
// This is deliberately separate from the call push in push.rs. A call is urgent,
// short-lived and rings; an alert is an announcement that should survive a phone
// being off for a while and must never impersonate a ringing call.

/// An alert an admin has actually authorised for sending. Constructing one is
/// the validation step, so a handler cannot send an unchecked title/body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    title: String,
    body: String,
}

impl Alert {
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// Notification text is shown on a lock screen, so it is capped at lengths that
/// stay readable there rather than at whatever FCM would accept.
pub const ALERT_TITLE_MAX: usize = 80;
pub const ALERT_BODY_MAX: usize = 400;

/// Shown when an admin leaves the title blank — a broadcast with no title reads
/// as a system message rather than as nothing.
pub const ALERT_DEFAULT_TITLE: &str = "ArcadeLauncher";

/// Validate admin-entered alert text.
///
/// The body is required: a notification with a title and no body is a blank
/// banner, and sending one to every device is not undoable.
pub fn parse_alert(title: &str, body: &str) -> Result<Alert, String> {
    let title = title.trim();
    let body = body.trim();
    if body.is_empty() {
        return Err("A message body is required.".into());
    }
    if title.chars().count() > ALERT_TITLE_MAX {
        return Err(format!("Title must be {ALERT_TITLE_MAX} characters or fewer."));
    }
    if body.chars().count() > ALERT_BODY_MAX {
        return Err(format!("Message must be {ALERT_BODY_MAX} characters or fewer."));
    }
    // Control characters would render as boxes or silently truncate the banner.
    if title.chars().chain(body.chars()).any(|c| c.is_control() && c != '\n') {
        return Err("Message contains characters that cannot be displayed.".into());
    }
    Ok(Alert {
        title: if title.is_empty() { ALERT_DEFAULT_TITLE.to_string() } else { title.to_string() },
        body: body.to_string(),
    })
}

/// The fixed alert behind the admin "send test" button. Its wording says what it
/// is, so an admin who fires it at the wrong account has not alarmed anyone.
pub fn test_alert() -> Alert {
    Alert {
        title: "Test notification".to_string(),
        body: "Push notifications are working. This was sent from the admin dashboard.".to_string(),
    }
}

/// The FCM v1 message for an alert.
///
/// Note what is *not* here: an `android.notification.channel_id`. The app only
/// creates the `calls` channel, and Android silently drops a notification naming
/// a channel that does not exist — so an alert rides the app's default channel
/// instead. `type: "alert"` lets the app tell these from a call and not ring.
pub fn alert_push_message(token: &PushToken, alert: &Alert) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "token": token.as_str(),
            "notification": { "title": alert.title(), "body": alert.body() },
            "data": { "type": "alert" },
            "android": {
                // Normal priority: an announcement does not justify waking a
                // dozing device the way a call does. A day of TTL so a phone
                // that was off overnight still gets it.
                "priority": "NORMAL",
                "ttl": "86400s",
            }
        }
    })
}

/// What a send actually did, so the admin page can say so rather than claiming
/// success because the button was pressed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SendReport {
    /// Accounts that had at least one registered device.
    pub accounts: usize,
    /// Devices we attempted.
    pub devices: usize,
    /// Devices FCM accepted.
    pub delivered: usize,
    /// Devices dropped because their token was dead.
    pub dead: usize,
}

/// Human summary of a send. Distinguishes the two ways of reaching nobody —
/// nothing registered, versus registered devices that all failed — because they
/// need completely different fixes.
pub fn send_summary(r: &SendReport) -> String {
    if r.devices == 0 {
        return "No registered devices — nothing was sent. Sign in on the phone app first.".into();
    }
    let failed = r.devices.saturating_sub(r.delivered);
    let mut s = format!(
        "Delivered to {} of {} device(s) across {} account(s).",
        r.delivered, r.devices, r.accounts
    );
    if r.dead > 0 {
        s.push_str(&format!(" Removed {} stale device token(s).", r.dead));
    }
    if failed > r.dead {
        s.push_str(" Some sends failed — check the server log.");
    }
    s
}

/// Message shown when push is configured off. Says which knob is missing, since
/// the usual cause is a container started without the secret mounted.
pub const PUSH_DISABLED_MESSAGE: &str =
    "Push is not configured on this server (ARCADE_FCM_SERVICE_ACCOUNT is unset or unreadable).";

#[cfg(test)]
mod alert_tests {
    use super::*;

    #[test]
    fn a_normal_alert_is_trimmed_and_kept() {
        let a = parse_alert("  Maintenance  ", "  Back in ten minutes.  ").expect("valid");
        assert_eq!(a.title(), "Maintenance");
        assert_eq!(a.body(), "Back in ten minutes.");
    }

    #[test]
    fn a_blank_title_becomes_the_app_name_not_a_blank_banner() {
        let a = parse_alert("", "Something happened").unwrap();
        assert_eq!(a.title(), ALERT_DEFAULT_TITLE);
    }

    #[test]
    fn an_empty_body_is_refused() {
        assert!(parse_alert("Title", "").is_err());
        assert!(parse_alert("Title", "   \n ").is_err());
    }

    #[test]
    fn over_long_text_is_refused_rather_than_truncated() {
        assert!(parse_alert(&"x".repeat(ALERT_TITLE_MAX + 1), "body").is_err());
        assert!(parse_alert("t", &"x".repeat(ALERT_BODY_MAX + 1)).is_err());
        // The limits themselves are allowed.
        assert!(parse_alert(&"x".repeat(ALERT_TITLE_MAX), &"y".repeat(ALERT_BODY_MAX)).is_ok());
    }

    #[test]
    fn undisplayable_control_characters_are_refused_but_newlines_are_not() {
        assert!(parse_alert("t", "line\u{0}break").is_err());
        assert!(parse_alert("t", "line\nbreak").is_ok());
    }

    #[test]
    fn an_alert_does_not_ring_like_a_call() {
        let t = parse_push_token("fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV").unwrap();
        let m = alert_push_message(&t, &parse_alert("Hi", "There").unwrap());
        let msg = &m["message"];
        assert_eq!(msg["data"]["type"], "alert");
        assert_eq!(msg["android"]["priority"], "NORMAL");
        // Naming a channel the app never created would drop it entirely.
        assert!(msg["android"]["notification"].is_null());
        assert_ne!(msg["android"]["ttl"], "45s");
        // FCM rejects a data map holding anything but strings.
        for (_, v) in msg["data"].as_object().unwrap() {
            assert!(v.is_string());
        }
    }

    #[test]
    fn the_test_alert_says_that_it_is_a_test() {
        let t = parse_push_token("fMEP0vJqS0aBcDeFgHiJkLmNoPqRsTuV").unwrap();
        let m = alert_push_message(&t, &test_alert());
        assert_eq!(m["message"]["notification"]["title"], "Test notification");
    }

    #[test]
    fn nothing_registered_reads_differently_from_everything_failing() {
        let none = send_summary(&SendReport::default());
        assert!(none.contains("No registered devices"));
        let all_failed = send_summary(&SendReport {
            accounts: 1,
            devices: 2,
            delivered: 0,
            dead: 0,
        });
        assert!(!all_failed.contains("No registered devices"));
        assert!(all_failed.contains("Some sends failed"));
    }

    #[test]
    fn a_clean_send_does_not_warn_about_failures() {
        let s = send_summary(&SendReport {
            accounts: 3,
            devices: 4,
            delivered: 4,
            dead: 0,
        });
        assert!(s.contains("4 of 4"));
        assert!(!s.contains("failed"));
        assert!(!s.contains("stale"));
    }

    #[test]
    fn a_dead_token_is_reported_as_cleanup_not_as_a_failure() {
        let s = send_summary(&SendReport {
            accounts: 2,
            devices: 3,
            delivered: 2,
            dead: 1,
        });
        assert!(s.contains("Removed 1 stale"));
        assert!(!s.contains("Some sends failed"));
    }
}
