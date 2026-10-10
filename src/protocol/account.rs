//! One wallet as the state API reports it, folded from stream frames.

use std::collections::BTreeMap;

use alloy::primitives::{Address, U256};
use serde_json::Value;

use super::wire::{FundingTotals, IntentOutcome, Operation, Position, WalletPatch, WalletSnapshot};

/// Amounts are 18-decimal USD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub address: Address,
    pub account_revision: u64,
    /// Unix seconds of the last block the indexer has applied.
    pub indexed_through_timestamp: u64,
    pub queue_revision: U256,
    pub available_balance: U256,
    /// Winnings the LP still owes: waiting in the payout queue.
    pub queued: U256,
    /// In the same version as the balances: actual cashflows, without trading PnL.
    pub funding_totals: Option<FundingTotals>,
    /// Session key address → expiry in unix seconds.
    pub session_keys: BTreeMap<Address, u64>,
    pub positions: Vec<Position>,
    /// Intents the relayer has accepted and not yet executed.
    pub operations: Vec<Operation>,
}

/// A frame's effect: the new state plus the intent outcomes it announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUpdate {
    pub state: AccountState,
    pub outcomes: Vec<IntentOutcome>,
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// The patch does not continue the state we hold; a fresh snapshot is needed.
    #[error("out of sync: {0}")]
    OutOfSync(String),
    #[error("invalid frame: {0}")]
    Invalid(#[from] serde_json::Error),
}

/// Folds one `wallet` frame of the live stream into the account state.
///
/// A frame is either a full snapshot or a patch on top of a known revision.
/// Returns `None` for a snapshot that is not newer than what we already hold.
pub fn apply_frame(
    previous: Option<&AccountState>,
    frame: Value,
) -> Result<Option<AccountUpdate>, FrameError> {
    if frame.get("baseAccountRevision").is_some() {
        let patch: WalletPatch = serde_json::from_value(frame)?;
        let Some(previous) = previous else {
            return Err(FrameError::OutOfSync("patch received before any snapshot".into()));
        };
        if patch.base_account_revision != previous.account_revision {
            return Err(FrameError::OutOfSync(format!(
                "patch builds on revision {}, have {}",
                patch.base_account_revision, previous.account_revision
            )));
        }
        if patch.account_revision <= patch.base_account_revision {
            return Err(FrameError::OutOfSync("patch does not advance the account revision".into()));
        }
        if patch.address != previous.address {
            return Err(FrameError::OutOfSync(format!(
                "patch is for {}, stream is for {}",
                patch.address, previous.address
            )));
        }
        let outcomes = patch.intent_outcomes.clone().unwrap_or_default();
        return Ok(Some(AccountUpdate { state: patched(previous, patch), outcomes }));
    }
    let snapshot: WalletSnapshot = serde_json::from_value(frame)?;
    if previous.is_some_and(|held| snapshot.account_revision <= held.account_revision) {
        return Ok(None);
    }
    Ok(Some(AccountUpdate { state: from_snapshot(snapshot), outcomes: Vec::new() }))
}

fn session_key_map(keys: Vec<(Address, super::wire::Dec)>) -> BTreeMap<Address, u64> {
    keys.into_iter().map(|(key, expiry)| (key, u64::try_from(expiry.0).unwrap_or(u64::MAX))).collect()
}

fn from_snapshot(snapshot: WalletSnapshot) -> AccountState {
    AccountState {
        address: snapshot.address,
        account_revision: snapshot.account_revision,
        indexed_through_timestamp: snapshot.indexed_through_timestamp,
        queue_revision: snapshot.queue_revision.0,
        available_balance: snapshot.funds.available_balance.0,
        queued: snapshot.queued.0,
        funding_totals: snapshot.funding_totals,
        session_keys: session_key_map(snapshot.session_keys),
        positions: snapshot.open_positions,
        operations: snapshot.operations,
    }
}

fn patched(previous: &AccountState, patch: WalletPatch) -> AccountState {
    let WalletPatch { state, .. } = patch;
    let mut positions: Vec<Position> = previous
        .positions
        .iter()
        .filter(|held| !patch.removed_position_ids.iter().any(|id| id.0 == held.position_id))
        .cloned()
        .collect();
    positions.extend(patch.added_positions);
    let mut operations: Vec<Operation> = previous
        .operations
        .iter()
        .filter(|held| !patch.removed_operation_ids.contains(&held.intent_id))
        .cloned()
        .collect();
    operations.extend(patch.added_operations);
    AccountState {
        address: previous.address,
        account_revision: patch.account_revision,
        indexed_through_timestamp: state
            .indexed_through_timestamp
            .unwrap_or(previous.indexed_through_timestamp),
        queue_revision: state.queue_revision.map_or(previous.queue_revision, |value| value.0),
        available_balance: state.funds.map_or(previous.available_balance, |funds| funds.available_balance.0),
        queued: state.queued.map_or(previous.queued, |value| value.0),
        funding_totals: patch.funding_totals.or(previous.funding_totals),
        session_keys: state.session_keys.map_or_else(|| previous.session_keys.clone(), session_key_map),
        positions,
        operations,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::address;
    use serde_json::json;

    use super::*;
    use crate::testing::{WALLET_SNAPSHOT, intent_id};

    const BALANCE: u128 = 673_377_893_000_000_000_000;

    fn snapshot() -> Value {
        serde_json::from_str(WALLET_SNAPSHOT).unwrap()
    }

    fn start() -> AccountState {
        apply_frame(None, snapshot()).unwrap().unwrap().state
    }

    fn patch(base: u64, body: Value) -> Value {
        let mut frame = json!({
            "address": "0x000000000000000000000000000000000000dEaD",
            "accountRevision": base + 1,
            "baseAccountRevision": base,
            "state": {},
            "addedPositions": [],
            "removedPositionIds": [],
            "addedOperations": [],
            "removedOperationIds": [],
        });
        frame.as_object_mut().unwrap().extend(body.as_object().unwrap().clone());
        frame
    }

    fn position_tuple(id: &str, opener: Value) -> Value {
        json!([
            id,
            0,
            true,
            false,
            "827935",
            "500000000000000000000",
            20,
            "786932",
            1_791_620_900_000u64,
            opener
        ])
    }

    #[test]
    fn reads_a_production_wallet_frame() {
        let state = start();
        assert_eq!(state.address, address!("0x000000000000000000000000000000000000dead"));
        assert_eq!(state.available_balance, U256::from(BALANCE));
        assert_eq!(state.queued, U256::ZERO);
        assert_eq!(state.queue_revision, U256::from(1));
        assert_eq!(
            state.session_keys.get(&address!("0xd3d94506d082e545de3ef5d5341726514319b7bc")),
            Some(&1_794_212_392)
        );
        assert_eq!(state.operations.len(), 1);
        assert_eq!(state.operations[0].action, "registerSessionKey");
        assert_eq!(state.operations[0].deadline, 1_791_623_993);
    }

    #[test]
    fn a_snapshot_that_is_not_newer_is_ignored() {
        assert_eq!(apply_frame(Some(&start()), snapshot()).unwrap(), None);
    }

    #[test]
    fn a_frame_missing_required_state_is_invalid() {
        let mut incomplete = snapshot();
        incomplete.as_object_mut().unwrap().remove("funds");
        assert!(matches!(apply_frame(None, incomplete), Err(FrameError::Invalid(_))));
    }

    #[test]
    fn an_executed_open_removes_the_operation_and_adds_the_position() {
        let opener = intent_id(5);
        let queued = apply_frame(
            Some(&start()),
            patch(1, json!({
                "state": { "queueRevision": "2" },
                "addedOperations": [[opener, "openPosition", 0, true, false, "500000000000000000000", 20, 1_791_620_890_000u64, "1791621000", null]],
            })),
        )
        .unwrap()
        .unwrap()
        .state;
        assert_eq!(
            queued.operations.iter().map(|o| o.action.as_str()).collect::<Vec<_>>(),
            ["registerSessionKey", "openPosition"]
        );

        let opened = apply_frame(
            Some(&queued),
            patch(
                2,
                json!({
                    "state": {
                        "indexedThroughTimestamp": 1_791_620_901u64,
                        "funds": { "availableBalance": "173377893000000000000", "availableQueued": "0" },
                    },
                    "addedPositions": [position_tuple("42", json!(opener))],
                    "removedOperationIds": [opener],
                    "intentOutcomes": [[opener, "succeeded"]],
                }),
            ),
        )
        .unwrap()
        .unwrap();
        let state = &opened.state;
        assert_eq!((state.account_revision, state.indexed_through_timestamp), (3, 1_791_620_901));
        assert_eq!(state.queue_revision, U256::from(2));
        assert_eq!(state.available_balance, U256::from(BALANCE - 500_000_000_000_000_000_000));
        assert_eq!(
            state.operations.iter().map(|o| o.action.as_str()).collect::<Vec<_>>(),
            ["registerSessionKey"]
        );
        let position = &state.positions[0];
        assert_eq!((position.position_id, position.is_long), (U256::from(42), true));
        assert_eq!((position.entry_price, position.bust_price, position.leverage), (827_935, 786_932, 20));
        assert_eq!(position.intent_id, Some(opener));
        assert_eq!(opened.outcomes, [IntentOutcome { intent_id: opener, outcome: "succeeded".into() }]);
    }

    #[test]
    fn a_close_removes_the_position() {
        let held = apply_frame(
            Some(&start()),
            patch(1, json!({ "addedPositions": [position_tuple("42", Value::Null)] })),
        )
        .unwrap()
        .unwrap()
        .state;
        assert_eq!(held.positions[0].intent_id, None);
        let closed =
            apply_frame(Some(&held), patch(2, json!({ "removedPositionIds": ["42"] }))).unwrap().unwrap();
        assert!(closed.state.positions.is_empty());
    }

    #[test]
    fn a_patch_keeps_what_it_does_not_mention() {
        let next = apply_frame(Some(&start()), patch(1, json!({ "state": { "queued": "5" } })))
            .unwrap()
            .unwrap()
            .state;
        assert_eq!(next.queued, U256::from(5));
        assert_eq!(next.queue_revision, U256::from(1));
        assert_eq!(next.available_balance, U256::from(BALANCE));
        assert_eq!(next.session_keys.len(), 1);
    }

    #[test]
    fn a_patch_that_does_not_continue_the_held_state_is_out_of_sync() {
        let state = start();
        assert!(matches!(apply_frame(Some(&state), patch(4, json!({}))), Err(FrameError::OutOfSync(_))));
        assert!(matches!(apply_frame(None, patch(1, json!({}))), Err(FrameError::OutOfSync(_))));
        let mut foreign = patch(1, json!({}));
        foreign["address"] = json!("0x00000000000000000000000000000000000000bb");
        assert!(matches!(apply_frame(Some(&state), foreign), Err(FrameError::OutOfSync(_))));
    }

    #[test]
    fn cashflow_totals_are_folded_with_the_same_funds_revision_and_inherited_when_absent() {
        let original = start();
        let cash = original.funding_totals.unwrap();
        assert_eq!(cash.withdrawn, U256::ZERO);
        let next = apply_frame(Some(&original), patch(1, json!({
            "lifetime": { "depositedRaw": (cash.deposited + U256::from(10)).to_string(), "withdrawnRaw": "2" },
            "state": { "funds": { "availableBalance": "7" } }
        }))).unwrap().unwrap().state;
        assert_eq!(next.account_revision, 2);
        assert_eq!(next.available_balance, U256::from(7));
        assert_eq!(
            next.funding_totals,
            Some(FundingTotals { deposited: cash.deposited + U256::from(10), withdrawn: U256::from(2) })
        );
        let inherited = apply_frame(Some(&next), patch(2, json!({}))).unwrap().unwrap().state;
        assert_eq!(inherited.funding_totals, next.funding_totals);
    }

    #[test]
    fn missing_cashflow_evidence_is_unknown_and_null_or_partial_evidence_is_invalid() {
        let mut missing = snapshot();
        missing.as_object_mut().unwrap().remove("lifetime");
        let state = apply_frame(None, missing).unwrap().unwrap().state;
        assert_eq!(state.funding_totals, None);
        assert_eq!(state.available_balance, U256::from(BALANCE));
        for lifetime in [Value::Null, json!({ "depositedRaw": "0" })] {
            assert!(matches!(
                apply_frame(Some(&start()), patch(1, json!({"lifetime": lifetime}))),
                Err(FrameError::Invalid(_))
            ));
            let mut full = snapshot();
            full["lifetime"] = lifetime;
            assert!(matches!(apply_frame(None, full), Err(FrameError::Invalid(_))));
        }
    }

    #[test]
    fn a_patch_cannot_replace_cash_totals_without_advancing_its_revision() {
        let mut frame =
            patch(1, json!({ "lifetime": { "depositedRaw": "999999999999999999999", "withdrawnRaw": "0" } }));
        frame["accountRevision"] = json!(1);
        assert!(matches!(apply_frame(Some(&start()), frame), Err(FrameError::OutOfSync(_))));
    }
}
