//! Intents this bot has signed and sent, until the account shows what became of them.

use alloy::primitives::{B256, U256};

use super::decide::Side;
use crate::protocol::account::AccountState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    pub side: Side,
    pub intent_id: B256,
    /// Unix seconds after which the contract rejects the intent.
    pub deadline: u64,
    /// Relayer queue revision at acceptance; `None` while we do not know whether it was accepted.
    pub queue_revision: Option<U256>,
    /// The account stream has shown the intent as queued.
    pub seen_queued: bool,
}

/// Checks an in-flight intent against the account's live state. Returns the
/// record to keep tracking, or `None` once the intent can no longer change the
/// account: it executed, was dropped, or its deadline passed on-chain.
///
/// A submission with an unknown outcome is never assumed lost. It stays in
/// flight until the account proves it settled or the chain clock passes its
/// deadline.
pub fn track(inflight: Inflight, account: Option<&AccountState>) -> Option<Inflight> {
    let Some(account) = account else { return Some(inflight) };
    if account.operations.iter().any(|operation| operation.intent_id == inflight.intent_id) {
        return Some(Inflight { seen_queued: true, ..inflight });
    }
    if inflight.seen_queued {
        return None;
    }
    if account.positions.iter().any(|position| position.intent_id == Some(inflight.intent_id)) {
        return None;
    }
    // The view already covers the relayer queue at acceptance, and the intent is not queued.
    if inflight.queue_revision.is_some_and(|accepted_at| account.queue_revision >= accepted_at) {
        return None;
    }
    (account.indexed_through_timestamp <= inflight.deadline).then_some(inflight)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire::{Operation, Position};
    use crate::testing::{NOW, account, intent_id, position};

    const DEADLINE: u64 = NOW + 120;

    fn accepted() -> Inflight {
        Inflight {
            side: Side::Long,
            intent_id: intent_id(1),
            deadline: DEADLINE,
            queue_revision: Some(U256::from(5)),
            seen_queued: false,
        }
    }

    fn unknown() -> Inflight {
        Inflight { queue_revision: None, ..accepted() }
    }

    fn at_revision(revision: u64) -> AccountState {
        AccountState { queue_revision: U256::from(revision), ..account() }
    }

    fn listing_it(revision: u64) -> AccountState {
        let operation =
            Operation { intent_id: intent_id(1), action: "openPosition".into(), deadline: DEADLINE };
        AccountState { operations: vec![operation], ..at_revision(revision) }
    }

    #[test]
    fn accepted_stays_in_flight_until_the_view_has_caught_up_with_the_relayer_queue() {
        assert_eq!(track(accepted(), Some(&at_revision(4))), Some(accepted()));
    }

    #[test]
    fn accepted_stays_in_flight_while_the_relayer_still_holds_it() {
        let tracked = track(accepted(), Some(&listing_it(5)));
        assert_eq!(tracked, Some(Inflight { seen_queued: true, ..accepted() }));
    }

    #[test]
    fn accepted_is_settled_once_the_caught_up_view_no_longer_lists_it() {
        assert_eq!(track(accepted(), Some(&at_revision(5))), None);
    }

    #[test]
    fn settled_once_it_was_seen_queued_and_then_left_the_queue() {
        let seen = Inflight { seen_queued: true, ..accepted() };
        assert_eq!(track(seen, Some(&at_revision(4))), None);
    }

    #[test]
    fn unknown_outcome_is_not_assumed_lost_while_it_could_still_execute() {
        let caught_up = AccountState { indexed_through_timestamp: DEADLINE, ..at_revision(99) };
        assert_eq!(track(unknown(), Some(&caught_up)), Some(unknown()));
    }

    #[test]
    fn unknown_outcome_is_settled_once_the_chain_clock_has_passed_its_deadline() {
        let later = AccountState { indexed_through_timestamp: DEADLINE + 1, ..account() };
        assert_eq!(track(unknown(), Some(&later)), None);
    }

    #[test]
    fn unknown_outcome_is_settled_as_soon_as_the_position_it_opened_shows_up() {
        let opened = Position { intent_id: Some(intent_id(1)), ..position(42, true) };
        let account = AccountState { positions: vec![opened], ..account() };
        assert_eq!(track(unknown(), Some(&account)), None);
    }

    #[test]
    fn unknown_outcome_turns_out_accepted_when_the_relayer_lists_it() {
        assert_eq!(track(unknown(), Some(&listing_it(0))), Some(Inflight { seen_queued: true, ..unknown() }));
    }

    #[test]
    fn nothing_is_learned_while_the_account_state_is_unavailable() {
        assert_eq!(track(accepted(), None), Some(accepted()));
    }
}
