// Sign-in approval ("Steam Guard"-style push) — the pure half.
//
// The rolling-code half of this feature needs no new server code at all: the
// deployed server already verifies 6-digit RFC 6238 TOTP at login
// (verify_user_totp, crypto.rs), and the phone's guard core generates exactly
// those codes. What is new is the *push* half: a sign-in on a PC raises a
// request that the phone approves or denies, showing the device and IP.
//
// Design rule that shapes everything below: an approval is an ALTERNATIVE to
// typing the code, never an extra requirement. A phone that is lost, flat or
// offline must never be able to lock the owner out of their own launcher, so a
// timed-out request degrades to "type your code" rather than to a failed login.
//
// Approvals live in memory, like the login challenge nonces do. They are valid
// for two minutes, so losing them on restart costs the user one retry and is
// cheaper than a table plus its migration.

/// How long a raised approval stays answerable, in seconds.
pub const APPROVAL_TTL_SECONDS: u64 = 120;

/// Where an approval request has got to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalState {
    Pending,
    Approved,
    Denied,
    Expired,
}

impl ApprovalState {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalState::Pending => "pending",
            ApprovalState::Approved => "approved",
            ApprovalState::Denied => "denied",
            ApprovalState::Expired => "expired",
        }
    }

    /// True once the answer can no longer change, so a poller can stop.
    pub fn is_final(self) -> bool {
        !matches!(self, ApprovalState::Pending)
    }
}

/// A sign-in waiting on the phone.
#[derive(Clone, Debug)]
pub struct Approval {
    pub id: String,
    pub user_id: u64,
    /// What the phone shows: the machine asking to sign in.
    pub device_name: String,
    /// What the phone shows: where from.
    pub ip: String,
    pub created_at: u64,
    pub state: ApprovalState,
}

/// Resolve the state a stored approval should report at `now`, folding an
/// unanswered-and-stale request into Expired. Decided requests keep their
/// answer forever (well, until eviction): a user who denied a sign-in should
/// keep seeing "denied", not have it drift to "expired".
pub fn effective_state(approval: &Approval, now: u64) -> ApprovalState {
    if approval.state == ApprovalState::Pending
        && now.saturating_sub(approval.created_at) >= APPROVAL_TTL_SECONDS
    {
        return ApprovalState::Expired;
    }
    approval.state
}

/// Why a decision was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecideError {
    /// Already approved, denied or timed out — decisions are single-use.
    AlreadyDecided,
    /// Not "approve" or "deny".
    BadAction,
}

/// Apply the phone's answer. Single-use: a second decision on the same request
/// is refused rather than overwriting the first, so a replayed or duplicated
/// frame cannot turn a deny into an approve.
pub fn decide(approval: &Approval, action: &str, now: u64) -> Result<ApprovalState, DecideError> {
    let next = match action.trim().to_ascii_lowercase().as_str() {
        "approve" | "allow" | "yes" => ApprovalState::Approved,
        "deny" | "reject" | "no" => ApprovalState::Denied,
        _ => return Err(DecideError::BadAction),
    };
    if effective_state(approval, now).is_final() {
        return Err(DecideError::AlreadyDecided);
    }
    Ok(next)
}

/// True when this approval may be spent to complete a login: approved, for the
/// right account, and still inside its window. Spending is the caller's job
/// (it removes the entry), which is what makes an approval single-use.
pub fn can_consume(approval: &Approval, user_id: u64, now: u64) -> bool {
    approval.user_id == user_id && effective_state(approval, now) == ApprovalState::Approved
}

/// Drop approvals that are past the point of being useful. Called opportunist-
/// ically on each new request so the map cannot grow without bound; decided
/// entries are kept for one extra TTL so a poller still sees its answer.
pub fn is_evictable(approval: &Approval, now: u64) -> bool {
    now.saturating_sub(approval.created_at) >= APPROVAL_TTL_SECONDS * 2
}

/// Reduce an IP to something safe to show on a phone screen. Full addresses
/// are shown for IPv4 (the owner recognises their own LAN), but IPv6 is
/// truncated to its routing prefix: the full address embeds interface
/// identifiers that are worth less than the screen space they cost.
pub fn display_ip(raw: &str) -> String {
    let ip = raw.trim();
    if ip.is_empty() {
        return "unknown location".to_string();
    }
    if ip.contains(':') {
        let parts: Vec<&str> = ip.split(':').take(4).collect();
        return format!("{}:…", parts.join(":"));
    }
    ip.chars().take(45).collect()
}

/// The one-line prompt the phone shows. Built here so the desktop, the phone
/// and any future notification body cannot word it three different ways.
pub fn approval_prompt(device_name: &str, ip: &str) -> String {
    let device = device_name.trim();
    let device = if device.is_empty() { "A PC" } else { device };
    format!("{} is trying to sign in from {}.", device, display_ip(ip))
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn req(state: ApprovalState, created_at: u64) -> Approval {
        Approval {
            id: "abc".into(),
            user_id: 7,
            device_name: "Living Room PC".into(),
            ip: "10.0.0.5".into(),
            created_at,
            state,
        }
    }

    #[test]
    fn pending_expires_once_the_window_closes() {
        let a = req(ApprovalState::Pending, 1000);
        assert_eq!(effective_state(&a, 1000), ApprovalState::Pending);
        assert_eq!(effective_state(&a, 1000 + APPROVAL_TTL_SECONDS - 1), ApprovalState::Pending);
        assert_eq!(effective_state(&a, 1000 + APPROVAL_TTL_SECONDS), ApprovalState::Expired);
    }

    #[test]
    fn a_decided_request_keeps_its_answer_forever() {
        let denied = req(ApprovalState::Denied, 1000);
        assert_eq!(effective_state(&denied, 9_000_000), ApprovalState::Denied);
        let approved = req(ApprovalState::Approved, 1000);
        assert_eq!(effective_state(&approved, 9_000_000), ApprovalState::Approved);
    }

    #[test]
    fn a_clock_that_went_backwards_does_not_expire_anything() {
        let a = req(ApprovalState::Pending, 1000);
        assert_eq!(effective_state(&a, 5), ApprovalState::Pending);
    }

    #[test]
    fn only_pending_is_non_final() {
        assert!(!ApprovalState::Pending.is_final());
        for s in [ApprovalState::Approved, ApprovalState::Denied, ApprovalState::Expired] {
            assert!(s.is_final(), "{s:?}");
        }
    }

    #[test]
    fn decide_accepts_the_wordings_a_client_might_send() {
        let a = req(ApprovalState::Pending, 1000);
        for yes in ["approve", "Allow", " YES "] {
            assert_eq!(decide(&a, yes, 1000), Ok(ApprovalState::Approved), "{yes}");
        }
        for no in ["deny", "Reject", " no "] {
            assert_eq!(decide(&a, no, 1000), Ok(ApprovalState::Denied), "{no}");
        }
    }

    #[test]
    fn decide_rejects_anything_it_does_not_understand() {
        let a = req(ApprovalState::Pending, 1000);
        for bad in ["", "maybe", "approved", "1"] {
            assert_eq!(decide(&a, bad, 1000), Err(DecideError::BadAction), "{bad}");
        }
    }

    #[test]
    fn a_decision_cannot_be_replayed_into_a_different_answer() {
        let denied = req(ApprovalState::Denied, 1000);
        assert_eq!(decide(&denied, "approve", 1000), Err(DecideError::AlreadyDecided));
        let approved = req(ApprovalState::Approved, 1000);
        assert_eq!(decide(&approved, "deny", 1000), Err(DecideError::AlreadyDecided));
    }

    #[test]
    fn an_expired_request_cannot_be_answered_late() {
        let a = req(ApprovalState::Pending, 1000);
        assert_eq!(
            decide(&a, "approve", 1000 + APPROVAL_TTL_SECONDS),
            Err(DecideError::AlreadyDecided)
        );
    }

    #[test]
    fn only_a_live_approval_for_the_right_account_can_be_spent() {
        let a = req(ApprovalState::Approved, 1000);
        assert!(can_consume(&a, 7, 1000));
        assert!(!can_consume(&a, 8, 1000), "another account must not spend it");
        assert!(!can_consume(&req(ApprovalState::Pending, 1000), 7, 1000));
        assert!(!can_consume(&req(ApprovalState::Denied, 1000), 7, 1000));
    }

    #[test]
    fn an_approval_stays_spendable_for_its_whole_window() {
        let a = req(ApprovalState::Approved, 1000);
        assert!(can_consume(&a, 7, 1000 + APPROVAL_TTL_SECONDS * 10));
    }

    #[test]
    fn eviction_waits_one_extra_window_so_a_poller_sees_its_answer() {
        let a = req(ApprovalState::Approved, 1000);
        assert!(!is_evictable(&a, 1000 + APPROVAL_TTL_SECONDS));
        assert!(is_evictable(&a, 1000 + APPROVAL_TTL_SECONDS * 2));
    }

    #[test]
    fn ipv4_is_shown_whole_and_ipv6_is_cut_to_its_prefix() {
        assert_eq!(display_ip("10.0.0.5"), "10.0.0.5");
        assert_eq!(display_ip("  192.168.1.20 "), "192.168.1.20");
        assert_eq!(display_ip("2001:db8:1:2:3:4:5:6"), "2001:db8:1:2:…");
    }

    #[test]
    fn a_missing_ip_reads_as_words_not_as_a_blank() {
        assert_eq!(display_ip(""), "unknown location");
        assert_eq!(display_ip("   "), "unknown location");
    }

    #[test]
    fn the_prompt_names_the_device_and_where_it_is() {
        assert_eq!(
            approval_prompt("Living Room PC", "10.0.0.5"),
            "Living Room PC is trying to sign in from 10.0.0.5."
        );
    }

    #[test]
    fn the_prompt_stays_readable_without_a_device_name() {
        assert_eq!(approval_prompt("  ", ""), "A PC is trying to sign in from unknown location.");
    }
}
