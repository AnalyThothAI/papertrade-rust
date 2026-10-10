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
