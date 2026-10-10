//! What the bot currently sees and intends, as written to the database for
//! every viewer (TUI, `status`, any SQLite tool) to read.

use alloy::primitives::{I256, U256};
use serde::{Deserialize, Serialize};

use crate::protocol::deployment::USD_DECIMALS;
use crate::strategy::decide::{Mode, Phase, Side};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub at_ms: i64,
    pub dry_run: bool,
    pub mode: Mode,
    pub coin: String,
    pub close_bps: u32,
    /// Where the two legs stand.
    pub phase: Phase,
    /// `wait`, `open` or `close`.
    pub decision: String,
    /// The decision in words: what the bot is doing or why it is waiting.
    pub summary: String,
    pub mid: Option<String>,
    /// Signed distance of the mid from the mean entry in basis points, while hedged.
    pub move_bps: Option<f64>,
    /// `None` when the trading state could not be fetched recently.
    pub trading: Option<TradingSnapshot>,
    pub queue_total_usd: Option<String>,
    pub legs: Vec<LegSnapshot>,
    /// What a round is expected to cost and mint. `None` until the inputs are known.
    #[serde(default)]
    pub estimate: Option<EstimateSnapshot>,
    /// How the two accounts are funded. `None` while an account's state is unavailable.
    #[serde(default)]
    pub funds: Option<FundsSnapshot>,
    /// The transfer between the two accounts that is under way, if any.
    #[serde(default)]
    pub transfer: Option<TransferSnapshot>,
}

/// What a round is expected to cost and mint, by the protocol's published rules.
/// An estimate: the contract has the last word at settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EstimateSnapshot {
    /// PAPER minted per USD of loss basis right now.
    pub mint_rate: String,
    /// If both legs are closed once price is the close distance from the entry.
    pub at_target: RoundEstimate,
    /// If both legs were closed at the current mid. Only while hedged.
    pub now: Option<RoundEstimate>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundEstimate {
    /// What the losing leg loses, USD.
    pub loser_usd: String,
    /// What the winning leg is paid after the impact haircut and the win fee, USD.
    pub winner_usd: String,
    /// The pair's net result, USD. Negative: a cost.
    pub net_usd: String,
    pub paper: String,
    /// USD spent per PAPER minted. `None` when no PAPER would be minted.
    pub usd_per_paper: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundsSnapshot {
    pub total_usd: String,
    /// How much more the richer account holds.
    pub difference_usd: String,
    /// Rounds both accounts can still fund at the current margin, before any transfer.
    pub rounds_affordable: u32,
    /// `enough`, `rebalance` or `underfunded`.
    pub verdict: String,
    /// Both wallet keys are present, so a transfer can be signed without the operator.
    pub can_rebalance: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferSnapshot {
    pub from: Side,
    pub to: Side,
    pub amount_usd: String,
    /// `withdraw`, `send`, `credit`, `done` or `stuck`.
    pub stage: String,
    pub started_at_ms: i64,
    /// What it is waiting for, or why it is stuck.
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TradingSnapshot {
    pub trading_paused: bool,
    pub paused_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegSnapshot {
    pub side: Side,
    pub wallet: String,
    pub session_key: String,
    /// `None` while the account's live state is unavailable.
    pub account: Option<AccountSnapshot>,
    pub pending_intents: usize,
    /// The wallet's own key is on this machine, so the bot can withdraw and register for it.
    #[serde(default)]
    pub has_wallet_key: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    pub available_usd: String,
    pub queued_usd: String,
    /// Unix seconds; `None` when the bot's session key is not registered.
    pub session_key_expiry: Option<u64>,
    pub positions: Vec<PositionSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionSnapshot {
    pub id: String,
    pub is_long: bool,
    pub entry: String,
    pub bust: String,
    pub margin_usd: String,
    pub leverage: u32,
}

/// An 18-decimal amount with two decimal places, truncated.
pub fn format_amount(raw: U256) -> String {
    let unit = U256::from(10).pow(U256::from(USD_DECIMALS));
    let cents = (raw % unit) / U256::from(10).pow(U256::from(USD_DECIMALS - 2));
    format!("{}.{:02}", raw / unit, cents)
}

pub fn format_signed_amount(raw: I256) -> String {
    let sign = if raw.is_negative() { "-" } else { "" };
    format!("{sign}{}", format_amount(raw.unsigned_abs()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::deployment::usd;

    #[test]
    fn amounts_truncate_to_cents() {
        assert_eq!(format_amount(U256::ZERO), "0.00");
        assert_eq!(format_amount(usd(500)), "500.00");
        assert_eq!(format_amount(U256::from(673_377_893u64) * U256::from(10).pow(U256::from(12))), "673.37");
        assert_eq!(format_amount(U256::from(9_999_999_999_999_999u64)), "0.00");
    }

    #[test]
    fn signed_amounts_keep_their_sign() {
        let loss = I256::try_from(usd(100)).unwrap().wrapping_neg();
        assert_eq!(format_signed_amount(loss), "-100.00");
        assert_eq!(format_signed_amount(I256::try_from(usd(86)).unwrap()), "86.00");
    }
}
