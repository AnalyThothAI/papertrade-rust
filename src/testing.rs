//! Builders shared by the unit tests.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, U256, address};

use crate::protocol::account::AccountState;
use crate::protocol::deployment::usd;
use crate::protocol::wire::{Position, TradingState};

pub const TRADING_STATE: &str = include_str!("../tests/fixtures/trading-state.json");
pub const WALLET_SNAPSHOT: &str = include_str!("../tests/fixtures/wallet-snapshot.json");
pub const PROTOCOL_SUMMARY: &str = include_str!("../tests/fixtures/protocol-summary.json");

/// The session key the test accounts have registered.
pub const KEY: Address = address!("0x00000000000000000000000000000000000000aa");
pub const NOW: u64 = 1_800_000_000;

/// Trading state as captured from production, with the given actions refused by the relayer.
pub fn trading_state(paused: &[&str]) -> TradingState {
    let mut state: TradingState = serde_json::from_str(TRADING_STATE).unwrap();
    state.paused_actions = paused.iter().map(|action| (*action).to_owned()).collect();
    state
}

/// A funded account with a long-lived session key and nothing open.
pub fn account() -> AccountState {
    AccountState {
        address: address!("0x000000000000000000000000000000000000dead"),
        account_revision: 1,
        indexed_through_timestamp: NOW,
        queue_revision: U256::ZERO,
        available_balance: usd(1_500),
        queued: U256::ZERO,
        funding_totals: Some(crate::protocol::wire::FundingTotals {
            deposited: usd(1_500),
            withdrawn: U256::ZERO,
        }),
        session_keys: BTreeMap::from([(KEY, NOW + 20 * 86_400)]),
        positions: Vec::new(),
        operations: Vec::new(),
    }
}

/// A 500 USD × 20 BTC position entered at 80,000.0.
pub fn position(id: u64, is_long: bool) -> Position {
    Position {
        position_id: U256::from(id),
        instrument_id: 0,
        is_long,
        entry_price: 800_000,
        margin: usd(500),
        leverage: 20,
        bust_price: 760_400,
        opened_at_ms: None,
        intent_id: None,
    }
}

pub fn intent_id(n: u8) -> B256 {
    B256::with_last_byte(n)
}
