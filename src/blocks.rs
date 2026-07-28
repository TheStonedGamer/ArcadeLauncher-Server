// What a block actually means, decided in one place.
//
// The block *rows* have existed since the first social schema, but the rules
// around them were spread across the handlers that happened to check them: a
// friend request looked, a DM looked, a search looked, and calls only avoided
// blocked users by accident (blocking deletes the friendship, and calls require
// friendship). That is a rule holding by coincidence, which is how holes appear
// the next time one of those paths changes.
//
// So the decision lives here, pure and tested, and the handlers ask it. No
// database, no clock, no sockets.

/// The block relationship between two accounts, from the caller's side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BlockPair {
    /// The caller blocked the other account.
    pub i_blocked_them: bool,
    /// The other account blocked the caller.
    pub they_blocked_me: bool,
}

impl BlockPair {
    pub fn any(self) -> bool {
        self.i_blocked_them || self.they_blocked_me
    }
}

/// Things one account can try to do to another, that a block should stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interaction {
    FriendRequest,
    DirectMessage,
    Call,
    /// Appearing in the other person's search results or profile lookups.
    Discover,
}

/// Whether an interaction is allowed, and what to tell the caller if not.
///
/// The message deliberately does not distinguish "you blocked them" from "they
/// blocked you" for anything the *other* party initiated: telling someone they
/// have been blocked is information the blocker did not choose to share. The one
/// exception is an action the caller took against someone on their own block
/// list, where saying so is the only useful thing to say.
pub fn interaction_refusal(pair: BlockPair, what: Interaction) -> Option<&'static str> {
    if !pair.any() {
        return None;
    }
    if pair.i_blocked_them {
        return Some(match what {
            Interaction::FriendRequest => "You have blocked this person. Unblock them to send a request.",
            Interaction::DirectMessage => "You have blocked this person. Unblock them to send a message.",
            Interaction::Call => "You have blocked this person. Unblock them to call.",
            Interaction::Discover => "Blocked",
        });
    }
    // They blocked us. Same wording for every case, and nothing that confirms a
    // block exists rather than the account simply being unavailable.
    Some(match what {
        Interaction::FriendRequest => "This person is not accepting friend requests.",
        Interaction::DirectMessage => "This message could not be delivered.",
        Interaction::Call => "This person cannot be reached.",
        Interaction::Discover => "Unavailable",
    })
}

/// Blocking is not only a filter on future traffic: it ends what is already in
/// flight. These are the side effects a block must have, listed once so the
/// handler cannot quietly implement half of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockEffects {
    /// Remove any friendship or pending request between the two.
    pub drop_friendship: bool,
    /// End a call that is ringing or connected between them right now.
    pub end_live_call: bool,
    /// Stop sending presence updates in either direction.
    pub drop_presence: bool,
}

pub const ON_BLOCK: BlockEffects =
    BlockEffects { drop_friendship: true, end_live_call: true, drop_presence: true };

/// Unblocking restores nothing. A block deleted the friendship; getting it back
/// requires a fresh request that the other person accepts, exactly as if they
/// had never been friends. Anything else would let a block-unblock cycle
/// re-add someone who has since removed you.
pub const ON_UNBLOCK: BlockEffects =
    BlockEffects { drop_friendship: false, end_live_call: false, drop_presence: false };

/// A block target must be a real, other account. Guards the REST body before it
/// reaches SQL, so `userId: 0` or blocking yourself is a 400 rather than a row.
pub fn valid_block_target(me: u64, target: u64) -> bool {
    target != 0 && me != 0 && target != me
}

#[cfg(test)]
mod blocks_tests {
    use super::*;

    const CLEAR: BlockPair = BlockPair { i_blocked_them: false, they_blocked_me: false };
    const MINE: BlockPair = BlockPair { i_blocked_them: true, they_blocked_me: false };
    const THEIRS: BlockPair = BlockPair { i_blocked_them: false, they_blocked_me: true };

    #[test]
    fn no_block_permits_everything() {
        for what in [
            Interaction::FriendRequest,
            Interaction::DirectMessage,
            Interaction::Call,
            Interaction::Discover,
        ] {
            assert_eq!(interaction_refusal(CLEAR, what), None);
        }
    }

    #[test]
    fn every_interaction_is_refused_in_both_directions() {
        for pair in [MINE, THEIRS] {
            for what in [
                Interaction::FriendRequest,
                Interaction::DirectMessage,
                Interaction::Call,
                Interaction::Discover,
            ] {
                assert!(interaction_refusal(pair, what).is_some(), "{pair:?} {what:?}");
            }
        }
    }

    #[test]
    fn a_call_is_refused_and_not_merely_unfriended() {
        // The regression this module exists for: calls used to be stopped only
        // because blocking removes the friendship.
        assert!(interaction_refusal(THEIRS, Interaction::Call).is_some());
    }

    #[test]
    fn being_blocked_never_says_so() {
        // Wording on the receiving end must not confirm that a block exists.
        for what in [Interaction::FriendRequest, Interaction::DirectMessage, Interaction::Call] {
            let msg = interaction_refusal(THEIRS, what).unwrap();
            assert!(!msg.to_lowercase().contains("block"), "leaked: {msg}");
        }
    }

    #[test]
    fn blocking_someone_yourself_says_how_to_undo_it() {
        for what in [Interaction::FriendRequest, Interaction::DirectMessage, Interaction::Call] {
            let msg = interaction_refusal(MINE, what).unwrap();
            assert!(msg.contains("Unblock"), "unhelpful: {msg}");
        }
    }

    #[test]
    fn a_mutual_block_is_reported_as_our_own() {
        // If we blocked them too, saying "unblock them" is the actionable half.
        let both = BlockPair { i_blocked_them: true, they_blocked_me: true };
        assert_eq!(
            interaction_refusal(both, Interaction::Call),
            interaction_refusal(MINE, Interaction::Call)
        );
    }

    #[test]
    fn blocking_ends_what_is_already_running() {
        assert!(ON_BLOCK.drop_friendship);
        assert!(ON_BLOCK.end_live_call);
        assert!(ON_BLOCK.drop_presence);
    }

    #[test]
    fn unblocking_restores_nothing() {
        assert!(!ON_UNBLOCK.drop_friendship);
        assert_eq!(ON_UNBLOCK, BlockEffects {
            drop_friendship: false,
            end_live_call: false,
            drop_presence: false
        });
    }

    #[test]
    fn you_cannot_block_yourself_or_nobody() {
        assert!(valid_block_target(1, 2));
        assert!(!valid_block_target(1, 1));
        assert!(!valid_block_target(1, 0));
        assert!(!valid_block_target(0, 2));
    }

    #[test]
    fn any_is_true_for_either_direction() {
        assert!(!CLEAR.any());
        assert!(MINE.any());
        assert!(THEIRS.any());
    }
}
