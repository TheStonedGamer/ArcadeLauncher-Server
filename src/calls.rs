// Pure call-outcome core. A voice/video invite either gets answered or it does
// not, and the "does not" cases are what the clients had no story for: before
// this, an invite to an offline friend was relayed into nothing and the caller
// rang forever.
//
// This module owns the bookkeeping decisions — who is still ringing, what a
// terminating event means, and what the resulting DM line says — with no
// database, socket or clock of its own. `social_api` supplies the IO and the
// timestamps; everything here is deterministic and unit-tested below.

/// A ring that has been sent but not yet answered. Keyed by (caller, callee) in
/// the ring table, so simultaneous calls in opposite directions stay distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ringing {
    /// The invite asked for video. Only used to word the missed-call line.
    pub video: bool,
    /// When the invite was relayed (epoch seconds).
    pub started_at: i64,
}

/// What should happen to a ring, decided by the server rather than reported by
/// a client — a caller that is killed mid-ring never gets to send anything, and
/// a client that lies about the outcome must not be able to forge history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The callee picked up. Nothing is recorded; the call itself is the record.
    Answered,
    /// The ring ended without an answer. Worth a line in the conversation.
    Missed,
}

/// Ring table. Not a lock — `social_api` holds this behind the hub's mutex.
#[derive(Debug, Default)]
pub struct Rings {
    live: std::collections::HashMap<(u64, u64), Ringing>,
}

impl Rings {
    pub fn new() -> Self {
        Rings { live: std::collections::HashMap::new() }
    }

    /// Record that `caller` is ringing `callee`. A repeat invite for the same
    /// pair keeps the original start time, so a client that re-sends its invite
    /// (reconnect, retry) cannot extend the ring window indefinitely.
    pub fn start(&mut self, caller: u64, callee: u64, video: bool, now: i64) {
        self.live
            .entry((caller, callee))
            .or_insert(Ringing { video, started_at: now });
    }

    /// The callee answered. Returns false if there was no such ring — an accept
    /// with nothing ringing is stale signaling and must not clear anything.
    pub fn answered(&mut self, caller: u64, callee: u64) -> bool {
        self.live.remove(&(caller, callee)).is_some()
    }

    /// Either side ended the call. `Some(Missed)` only when the ring was still
    /// live — hanging up an *answered* call is not a missed call, and neither is
    /// an `end` frame that arrives twice.
    pub fn ended(&mut self, caller: u64, callee: u64) -> Option<(Outcome, Ringing)> {
        self.live.remove(&(caller, callee)).map(|r| (Outcome::Missed, r))
    }

    /// An `end` frame names only the peer, not the direction, so try both. This
    /// is how a declining callee and a cancelling caller both resolve the ring.
    pub fn ended_either_way(&mut self, a: u64, b: u64) -> Option<(u64, u64, Ringing)> {
        if let Some((_, r)) = self.ended(a, b) {
            return Some((a, b, r));
        }
        self.ended(b, a).map(|(_, r)| (b, a, r))
    }

    /// Every ring involving `user`, removed. Called when a socket drops: a
    /// caller whose client crashed mid-ring still owes the callee a record, and
    /// a callee who went offline while ringing missed the call.
    pub fn drop_for(&mut self, user: u64) -> Vec<(u64, u64, Ringing)> {
        let hit: Vec<(u64, u64)> = self
            .live
            .keys()
            .filter(|(caller, callee)| *caller == user || *callee == user)
            .copied()
            .collect();
        hit.into_iter()
            .filter_map(|k| self.live.remove(&k).map(|r| (k.0, k.1, r)))
            .collect()
    }

    /// Rings still addressed to `user`, left in place. A phone woken by a push
    /// connects with no idea a call is waiting; replaying these as ordinary
    /// invites is what turns the notification into an actual ringing call.
    pub fn incoming_for(&self, user: u64) -> Vec<(u64, Ringing)> {
        self.live
            .iter()
            .filter(|((_, callee), _)| *callee == user)
            .map(|((caller, _), r)| (*caller, *r))
            .collect()
    }

    /// Rings that have been outstanding for longer than `max_age` seconds. The
    /// sweeper resolves these as missed so a caller that vanishes without
    /// closing its socket cannot leave a ring live forever.
    pub fn expired(&mut self, now: i64, max_age: i64) -> Vec<(u64, u64, Ringing)> {
        let hit: Vec<(u64, u64)> = self
            .live
            .iter()
            .filter(|(_, r)| now - r.started_at >= max_age)
            .map(|(k, _)| *k)
            .collect();
        hit.into_iter()
            .filter_map(|k| self.live.remove(&k).map(|r| (k.0, k.1, r)))
            .collect()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.live.len()
    }
}

/// The conversation line a missed call leaves behind. Deliberately plain text
/// in the message body: a client that predates the `kind` column renders it as
/// an ordinary message and still shows the user something true, rather than a
/// blank bubble or a raw JSON blob.
pub fn missed_call_body(video: bool) -> &'static str {
    if video {
        "Missed video call"
    } else {
        "Missed call"
    }
}

/// The message `kind` stored alongside that body. Clients that understand it
/// render the line as a call record instead of a chat bubble.
pub const KIND_MISSED_CALL: &str = "call_missed";
/// An ordinary text DM. The column default, spelled once.
pub const KIND_TEXT: &str = "text";

/// How long a ring may stay outstanding before the server calls it missed.
/// Comfortably longer than the clients' own ring timeout, so in normal
/// operation the client resolves the call and this is only the backstop.
pub const RING_TIMEOUT_SECS: i64 = 60;

#[cfg(test)]
mod calls_tests {
    use super::*;

    #[test]
    fn an_answered_ring_leaves_no_record() {
        let mut r = Rings::new();
        r.start(1, 2, false, 100);
        assert!(r.answered(1, 2));
        // The end of an answered call must not be logged as missed.
        assert_eq!(r.ended(1, 2), None);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn an_unanswered_ring_is_missed_when_it_ends() {
        let mut r = Rings::new();
        r.start(1, 2, true, 100);
        let (outcome, ring) = r.ended(1, 2).expect("ring was live");
        assert_eq!(outcome, Outcome::Missed);
        assert!(ring.video);
    }

    #[test]
    fn a_second_end_frame_records_nothing() {
        // Both ends send `end` when a call is declined; only one may count.
        let mut r = Rings::new();
        r.start(1, 2, false, 100);
        assert!(r.ended(1, 2).is_some());
        assert!(r.ended(1, 2).is_none());
    }

    #[test]
    fn either_side_can_end_the_ring() {
        let mut r = Rings::new();
        r.start(1, 2, false, 100);
        // The callee declines: it sends `end` naming the caller, so the pair
        // arrives reversed and still has to resolve.
        let (caller, callee, _) = r.ended_either_way(2, 1).expect("ring was live");
        assert_eq!((caller, callee), (1, 2));
    }

    #[test]
    fn accept_without_a_ring_changes_nothing() {
        let mut r = Rings::new();
        assert!(!r.answered(1, 2));
    }

    #[test]
    fn a_repeat_invite_does_not_extend_the_ring() {
        let mut r = Rings::new();
        r.start(1, 2, false, 100);
        r.start(1, 2, false, 140);
        assert_eq!(r.expired(160, 60).len(), 1, "should expire from the first invite");
    }

    #[test]
    fn opposite_direction_rings_are_separate() {
        let mut r = Rings::new();
        r.start(1, 2, false, 100);
        r.start(2, 1, false, 100);
        assert!(r.ended(1, 2).is_some());
        assert_eq!(r.len(), 1, "the other direction must survive");
    }

    #[test]
    fn a_dropped_socket_resolves_both_directions() {
        let mut r = Rings::new();
        r.start(1, 2, false, 100); // user 1 was calling out
        r.start(3, 1, true, 100); // user 3 was calling user 1
        r.start(4, 5, false, 100); // unrelated
        let dropped = r.drop_for(1);
        assert_eq!(dropped.len(), 2);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn only_old_rings_expire() {
        let mut r = Rings::new();
        r.start(1, 2, false, 100);
        r.start(3, 4, false, 155);
        let old = r.expired(160, 60);
        assert_eq!(old.len(), 1);
        assert_eq!((old[0].0, old[0].1), (1, 2));
        assert_eq!(r.len(), 1, "the fresh ring keeps ringing");
    }

    #[test]
    fn a_waking_phone_is_told_only_about_calls_to_it() {
        let mut r = Rings::new();
        r.start(1, 2, true, 100); // for user 2
        r.start(3, 2, false, 100); // also for user 2
        r.start(2, 4, false, 100); // user 2 is calling out; not an incoming ring
        let mut incoming = r.incoming_for(2);
        incoming.sort_by_key(|(caller, _)| *caller);
        assert_eq!(incoming.len(), 2);
        assert_eq!(incoming[0].0, 1);
        assert!(incoming[0].1.video);
        // Reading must not consume: the ring is still live until it resolves.
        assert_eq!(r.len(), 3);
    }

    #[test]
    fn the_missed_line_says_which_kind_of_call() {
        assert_eq!(missed_call_body(false), "Missed call");
        assert_eq!(missed_call_body(true), "Missed video call");
    }
}
