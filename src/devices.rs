// Device identity for the social gateway.
//
// `/ws/social` has always keyed connections by *account*: SocialHub holds a
// Vec<SocialConn> per user id, because one person may be signed in on a
// desktop and a phone at once. Nothing distinguished those sockets from each
// other, which is fine for chat (everything a user sends goes to all of their
// devices) but useless for the mobile companion's remote-install feature: "put
// this game on the living-room PC" has to reach exactly one socket.
//
// This module is the pure half of that: naming, validation and selection rules
// with no IO and no lock handling, so `cargo test` covers them on both CI legs.
// The glue that attaches a Device to a live connection lives in social_api.rs.

/// Longest device id or display name we keep. Ids come from the client, so the
/// cap is a storage/DoS bound rather than a formatting preference.
const DEVICE_FIELD_MAX: usize = 64;

/// A signed-in client on one machine.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Device {
    /// Stable per-install identifier chosen by the client and persisted there.
    pub id: String,
    /// Human label shown in the phone's device picker.
    pub name: String,
    /// "desktop", "mobile" or "unknown" — see [`normalize_kind`].
    pub kind: String,
    /// Client version string, for showing "too old to accept installs".
    pub version: String,
}

/// Accept a client-supplied device id, or reject it.
///
/// Ids are echoed back to other devices and used as routing keys, so the
/// alphabet is restricted to characters that cannot be confused with JSON,
/// path or query syntax. An id that does not survive sanitisation is dropped
/// rather than rewritten: a silently-altered id would route commands to the
/// wrong machine, which is worse than the connection simply having no device
/// identity and being excluded from the picker.
pub fn sanitize_device_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > DEVICE_FIELD_MAX {
        return None;
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// Clean a display name for the device picker. Unlike the id this one *is*
/// rewritten, because it is presentation only: control characters are dropped,
/// runs of whitespace collapse, and an empty result falls back to the kind so
/// the picker never renders a blank row.
pub fn sanitize_device_name(raw: &str, kind: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped: String = collapsed.chars().take(DEVICE_FIELD_MAX).collect();
    let capped = capped.trim().to_string();
    if !capped.is_empty() {
        return capped;
    }
    match normalize_kind(kind) {
        "desktop" => "PC".to_string(),
        "mobile" => "Phone".to_string(),
        _ => "Device".to_string(),
    }
}

/// Fold an arbitrary client string into the three kinds the server reasons
/// about. Anything unrecognised becomes "unknown", which is deliberately not
/// install-capable — a client we do not recognise must opt in by naming itself.
pub fn normalize_kind(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "desktop" | "pc" | "windows" | "linux" | "mac" | "macos" => "desktop",
        "mobile" | "android" | "ios" | "phone" | "tablet" => "mobile",
        _ => "unknown",
    }
}

/// Only desktops run games, so only desktops can be told to install one.
/// Keeping this a named rule (rather than an inline `== "desktop"`) means the
/// phone's picker and the relay's guard cannot drift apart.
pub fn can_receive_install(kind: &str) -> bool {
    normalize_kind(kind) == "desktop"
}

/// Build a Device from the raw query/frame fields, or None when the id is
/// unusable. Kind and name are always normalised, never rejected.
pub fn parse_device(id: &str, name: &str, kind: &str, version: &str) -> Option<Device> {
    let id = sanitize_device_id(id)?;
    let kind = normalize_kind(kind);
    Some(Device {
        id,
        name: sanitize_device_name(name, kind),
        kind: kind.to_string(),
        version: version.trim().chars().take(DEVICE_FIELD_MAX).collect(),
    })
}

/// Collapse the devices behind a user's live sockets into the list the picker
/// shows. One machine may hold several sockets (a reconnect that has not timed
/// out yet, or the launcher plus its updater), so entries are deduplicated by
/// id with the most recently registered one winning — that is the socket a
/// command should be routed to.
///
/// Ordering is desktops first (they are the actionable targets), then by name,
/// then by id, so the phone's list does not reshuffle between refreshes.
pub fn collapse_devices(mut devices: Vec<Device>) -> Vec<Device> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    devices.reverse(); // most recent registration first
    let mut out: Vec<Device> = Vec::new();
    for d in devices {
        if seen.insert(d.id.clone()) {
            out.push(d);
        }
    }
    out.sort_by(|a, b| {
        let rank = |d: &Device| if d.kind == "desktop" { 0 } else { 1 };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Why a remote-install command was refused. Returned to the phone verbatim so
/// the user sees the actual reason instead of a generic failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallReject {
    NoTarget,
    NoGame,
    UnknownDevice,
    NotInstallable,
}

impl InstallReject {
    pub fn code(self) -> &'static str {
        match self {
            InstallReject::NoTarget => "no_target",
            InstallReject::NoGame => "no_game",
            InstallReject::UnknownDevice => "unknown_device",
            InstallReject::NotInstallable => "not_installable",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            InstallReject::NoTarget => "Pick a PC to install to.",
            InstallReject::NoGame => "No game was named.",
            InstallReject::UnknownDevice => "That PC is not signed in right now.",
            InstallReject::NotInstallable => "That device can't install games.",
        }
    }
}

/// Validate a remote-install request against the user's *own* live devices.
///
/// The candidate list is always the requesting account's devices, which is what
/// keeps this safe: there is no cross-account addressing to authorise, because
/// a user can only ever name a machine they are themselves signed in on.
pub fn check_install_target(
    target_id: &str,
    game_id: &str,
    devices: &[Device],
) -> Result<String, InstallReject> {
    let Some(target) = sanitize_device_id(target_id) else {
        return Err(InstallReject::NoTarget);
    };
    if game_id.trim().is_empty() {
        return Err(InstallReject::NoGame);
    }
    let Some(device) = devices.iter().find(|d| d.id == target) else {
        return Err(InstallReject::UnknownDevice);
    };
    if !can_receive_install(&device.kind) {
        return Err(InstallReject::NotInstallable);
    }
    Ok(target)
}

#[cfg(test)]
mod devices_tests {
    use super::*;

    fn dev(id: &str, name: &str, kind: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            version: "0.14.0".into(),
        }
    }

    #[test]
    fn accepts_ordinary_ids() {
        assert_eq!(sanitize_device_id("pc-living-room").as_deref(), Some("pc-living-room"));
        assert_eq!(sanitize_device_id("  A1_b.2 ").as_deref(), Some("A1_b.2"));
    }

    #[test]
    fn rejects_ids_it_cannot_route_verbatim() {
        for bad in ["", "   ", "has space", "quote\"", "slash/es", "new\nline", "emoji😀"] {
            assert_eq!(sanitize_device_id(bad), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn rejects_over_long_ids_instead_of_truncating() {
        let long = "a".repeat(DEVICE_FIELD_MAX + 1);
        assert_eq!(sanitize_device_id(&long), None);
        assert!(sanitize_device_id(&"a".repeat(DEVICE_FIELD_MAX)).is_some());
    }

    #[test]
    fn names_collapse_whitespace_and_drop_control_chars() {
        assert_eq!(sanitize_device_name("  Living\tRoom   PC \n", "desktop"), "Living Room PC");
    }

    #[test]
    fn blank_names_fall_back_to_the_kind() {
        assert_eq!(sanitize_device_name("", "desktop"), "PC");
        assert_eq!(sanitize_device_name("   ", "android"), "Phone");
        assert_eq!(sanitize_device_name("\u{0}", "toaster"), "Device");
    }

    #[test]
    fn names_are_capped() {
        assert_eq!(sanitize_device_name(&"x".repeat(200), "desktop").chars().count(), DEVICE_FIELD_MAX);
    }

    #[test]
    fn kinds_fold_onto_three_values() {
        for pc in ["desktop", "PC", "Windows", "linux", "macOS"] {
            assert_eq!(normalize_kind(pc), "desktop", "{pc}");
        }
        for phone in ["mobile", "Android", "ios", "Tablet"] {
            assert_eq!(normalize_kind(phone), "mobile", "{phone}");
        }
        for other in ["", "server", "tv", "???"] {
            assert_eq!(normalize_kind(other), "unknown", "{other}");
        }
    }

    #[test]
    fn only_desktops_can_install() {
        assert!(can_receive_install("windows"));
        assert!(!can_receive_install("android"));
        assert!(!can_receive_install(""));
    }

    #[test]
    fn parse_device_normalizes_everything_but_the_id() {
        let d = parse_device(" desk-1 ", "  My   PC ", "Windows", " 0.14.0 ").unwrap();
        assert_eq!(d, dev("desk-1", "My PC", "desktop"));
    }

    #[test]
    fn parse_device_fails_only_on_a_bad_id() {
        assert!(parse_device("bad id", "x", "desktop", "").is_none());
        assert!(parse_device("ok", "", "", "").is_some());
    }

    #[test]
    fn collapse_keeps_the_most_recent_socket_per_device() {
        let out = collapse_devices(vec![
            dev("a", "Old Name", "desktop"),
            dev("a", "New Name", "desktop"),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "New Name");
    }

    #[test]
    fn collapse_puts_desktops_first_then_sorts_by_name() {
        let out = collapse_devices(vec![
            dev("m1", "Pixel", "mobile"),
            dev("d2", "Zeta PC", "desktop"),
            dev("d1", "alpha pc", "desktop"),
        ]);
        let ids: Vec<&str> = out.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, vec!["d1", "d2", "m1"]);
    }

    #[test]
    fn collapse_of_nothing_is_nothing() {
        assert!(collapse_devices(Vec::new()).is_empty());
    }

    #[test]
    fn install_target_accepts_a_live_desktop() {
        let devices = vec![dev("d1", "PC", "desktop")];
        assert_eq!(check_install_target("d1", "game-7", &devices).unwrap(), "d1");
    }

    #[test]
    fn install_target_reports_why_it_refused() {
        let devices = vec![dev("d1", "PC", "desktop"), dev("m1", "Pixel", "mobile")];
        assert_eq!(check_install_target("", "g", &devices), Err(InstallReject::NoTarget));
        assert_eq!(check_install_target("bad id", "g", &devices), Err(InstallReject::NoTarget));
        assert_eq!(check_install_target("d1", "  ", &devices), Err(InstallReject::NoGame));
        assert_eq!(check_install_target("nope", "g", &devices), Err(InstallReject::UnknownDevice));
        assert_eq!(check_install_target("m1", "g", &devices), Err(InstallReject::NotInstallable));
    }

    #[test]
    fn reject_codes_and_messages_are_distinct() {
        let all = [
            InstallReject::NoTarget,
            InstallReject::NoGame,
            InstallReject::UnknownDevice,
            InstallReject::NotInstallable,
        ];
        let codes: std::collections::HashSet<&str> = all.iter().map(|r| r.code()).collect();
        assert_eq!(codes.len(), all.len());
        assert!(all.iter().all(|r| !r.message().is_empty()));
    }
}
