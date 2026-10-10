//! Intents this bot has signed and sent, until the account shows what became of them.

use alloy::primitives::{B256, U256};

use super::decide::Side;
use crate::protocol::account::AccountState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Open,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inflight {
    pub side: Side,
    pub kind: Kind,
    pub intent_id: B256,
    /// Unix seconds after which the contract rejects the intent.
    pub deadline: u64,
    /// When it was signed, unix milliseconds.
    pub sent_at_ms: i64,
    /// Relayer queue revision at acceptance; `None` while we do not know whether it was accepted.
    pub queue_revision: Option<U256>,
    /// The account stream has shown the intent as queued.
    pub seen_queued: bool,
    /// The relayer reported that it ended without succeeding.
    pub failed: bool,
}

/// How much an in-flight intent holds the strategy back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Awaiting {
    /// Nothing in flight.
    Nothing,
    /// In flight but out of sight for too long. It may never execute, yet still
    /// could until its deadline: no new open, but a leg must not sit unhedged for it.
    Doubtful,
    /// Queued at the relayer, or only just sent: expected to execute shortly.
    Expected,
}

/// Checks an in-flight intent against the account's live state. Returns the
/// record to keep tracking, or `None` once the intent is settled.
///
/// Opens and closes settle differently, because being wrong costs differently:
///
/// - An open is settled only by evidence: the position it opened, a failure
///   reported by the relayer, or its deadline passing on the chain clock.
///   Assuming a vanished open has failed would make the other leg look unhedged.
/// - A close is also settled once it has left the relayer's queue. If the
///   position is still there, closing again is harmless.
pub fn track(inflight: Inflight, account: Option<&AccountState>) -> Option<Inflight> {
    let Some(account) = account else { return Some(inflight) };
    if inflight.failed {
        return None;
    }
    if is_queued(&inflight, account) {
        return Some(Inflight { seen_queued: true, ..inflight });
    }
    if account.positions.iter().any(|position| position.intent_id == Some(inflight.intent_id)) {
        return None;
    }
    if account.indexed_through_timestamp > inflight.deadline {
        return None;
    }
    // The view covers the relayer queue at acceptance, and the intent is not queued any more.
    let left_queue = inflight.seen_queued
        || inflight.queue_revision.is_some_and(|accepted_at| account.queue_revision >= accepted_at);
    match inflight.kind {
        Kind::Close if left_queue => None,
        _ => Some(inflight),
    }
}

/// How an intent that is still in flight should be treated right now.
pub fn awaiting(inflight: &Inflight, account: Option<&AccountState>, now_ms: i64, grace_ms: i64) -> Awaiting {
    let visible = account.is_some_and(|account| is_queued(inflight, account));
    if visible || now_ms - inflight.sent_at_ms <= grace_ms { Awaiting::Expected } else { Awaiting::Doubtful }
}

/// Outcomes the relayer reports for an intent that will never take effect.
pub fn is_failure(outcome: &str) -> bool {
    matches!(outcome, "failed" | "expired" | "cancelled")
}

fn is_queued(inflight: &Inflight, account: &AccountState) -> bool {
    account.operations.iter().any(|operation| operation.intent_id == inflight.intent_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::wire::{Operation, Position};
    use crate::testing::{NOW, account, intent_id, position};

    const DEADLINE: u64 = NOW + 120;
    const SENT_AT_MS: i64 = 1_000_000;
    const GRACE_MS: i64 = 10_000;

    fn accepted(kind: Kind) -> Inflight {
        Inflight {
            side: Side::Long,
            kind,
            intent_id: intent_id(1),
            deadline: DEADLINE,
            sent_at_ms: SENT_AT_MS,
            queue_revision: Some(U256::from(5)),
            seen_queued: false,
            failed: false,
        }
    }

    fn unknown(kind: Kind) -> Inflight {
        Inflight { queue_revision: None, ..accepted(kind) }
    }

    fn at_revision(revision: u64) -> AccountState {
        AccountState { queue_revision: U256::from(revision), ..account() }
    }

    fn listing_it(revision: u64) -> AccountState {
        let operation =
            Operation { intent_id: intent_id(1), action: "openPosition".into(), deadline: DEADLINE };
        AccountState { operations: vec![operation], ..at_revision(revision) }
    }

    fn holding_its_position() -> AccountState {
        let opened = Position { intent_id: Some(intent_id(1)), ..position(42, true) };
        AccountState { positions: vec![opened], ..at_revision(9) }
    }

    #[test]
    fn stays_in_flight_until_the_view_has_caught_up_with_the_relayer_queue() {
        for kind in [Kind::Open, Kind::Close] {
            assert_eq!(track(accepted(kind), Some(&at_revision(4))), Some(accepted(kind)));
        }
    }

    #[test]
    fn stays_in_flight_while_the_relayer_still_holds_it() {
        for kind in [Kind::Open, Kind::Close] {
            let tracked = track(accepted(kind), Some(&listing_it(5)));
            assert_eq!(tracked, Some(Inflight { seen_queued: true, ..accepted(kind) }));
        }
    }

    #[test]
    fn a_close_is_settled_once_it_has_left_the_queue() {
        assert_eq!(track(accepted(Kind::Close), Some(&at_revision(5))), None);
        let seen = Inflight { seen_queued: true, ..unknown(Kind::Close) };
        assert_eq!(track(seen, Some(&at_revision(0))), None);
    }

    #[test]
    fn an_open_that_left_the_queue_is_not_settled_without_evidence() {
        // The position can show up after the operation is gone; treating the open as
        // failed in between would make the other leg look unhedged.
        let open = accepted(Kind::Open);
        assert_eq!(track(open.clone(), Some(&at_revision(5))), Some(open));
        let seen = Inflight { seen_queued: true, ..accepted(Kind::Open) };
        assert_eq!(track(seen.clone(), Some(&at_revision(9))), Some(seen));
    }

    #[test]
    fn an_open_is_settled_by_the_position_it_opened() {
        assert_eq!(track(accepted(Kind::Open), Some(&holding_its_position())), None);
        assert_eq!(track(unknown(Kind::Open), Some(&holding_its_position())), None);
    }

    #[test]
    fn a_reported_failure_settles_at_once() {
        for kind in [Kind::Open, Kind::Close] {
            let failed = Inflight { failed: true, ..accepted(kind) };
            assert_eq!(track(failed, Some(&listing_it(5))), None);
        }
    }

    #[test]
    fn an_unknown_outcome_is_not_assumed_lost_while_it_could_still_execute() {
        for kind in [Kind::Open, Kind::Close] {
            let on_deadline = AccountState { indexed_through_timestamp: DEADLINE, ..at_revision(99) };
            assert_eq!(track(unknown(kind), Some(&on_deadline)), Some(unknown(kind)));
        }
    }

    #[test]
    fn everything_is_settled_once_the_chain_clock_has_passed_the_deadline() {
        let later = AccountState { indexed_through_timestamp: DEADLINE + 1, ..account() };
        for inflight in [unknown(Kind::Open), unknown(Kind::Close), accepted(Kind::Open)] {
            assert_eq!(track(inflight, Some(&later)), None);
        }
    }

    #[test]
    fn an_unknown_outcome_turns_out_accepted_when_the_relayer_lists_it() {
        let tracked = track(unknown(Kind::Open), Some(&listing_it(0)));
        assert_eq!(tracked, Some(Inflight { seen_queued: true, ..unknown(Kind::Open) }));
    }

    #[test]
    fn nothing_is_learned_while_the_account_state_is_unavailable() {
        assert_eq!(track(accepted(Kind::Close), None), Some(accepted(Kind::Close)));
    }

    #[test]
    fn an_intent_is_expected_while_queued_or_only_just_sent() {
        let open = accepted(Kind::Open);
        let just_sent = SENT_AT_MS + GRACE_MS;
        assert_eq!(awaiting(&open, Some(&at_revision(0)), just_sent, GRACE_MS), Awaiting::Expected);
        assert_eq!(awaiting(&open, None, just_sent, GRACE_MS), Awaiting::Expected);
        // Queued at the relayer: expected however long it takes.
        assert_eq!(
            awaiting(&open, Some(&listing_it(5)), SENT_AT_MS + 3_600_000, GRACE_MS),
            Awaiting::Expected
        );
    }

    #[test]
    fn an_intent_out_of_sight_past_the_grace_period_is_doubtful() {
        let late = SENT_AT_MS + GRACE_MS + 1;
        for inflight in [unknown(Kind::Open), accepted(Kind::Open), unknown(Kind::Close)] {
            assert_eq!(awaiting(&inflight, Some(&at_revision(9)), late, GRACE_MS), Awaiting::Doubtful);
            assert_eq!(awaiting(&inflight, None, late, GRACE_MS), Awaiting::Doubtful);
        }
        assert!(Awaiting::Expected > Awaiting::Doubtful && Awaiting::Doubtful > Awaiting::Nothing);
    }
}
