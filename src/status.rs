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
    /// The open round's entry and where it closes. `None` unless hedged.
    #[serde(default)]
    pub round: Option<RoundSnapshot>,
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

/// A bot that is running rewrites its status about once a second. One this old
/// was left by a bot that is not running any more.
pub const STALE_MS: i64 = 5_000;

impl Snapshot {
    /// Whether the bot that wrote this is still running, as far as its status tells.
    pub fn is_live(&self, now_ms: i64) -> bool {
        now_ms - self.at_ms <= STALE_MS
    }

    /// When the oldest open position was opened, unix milliseconds, if the accounts say.
    pub fn held_since_ms(&self) -> Option<u64> {
        let positions = self.legs.iter().filter_map(|leg| leg.account.as_ref()).flat_map(|a| &a.positions);
        positions.filter_map(|position| position.opened_at_ms).min()
    }
}

/// The hedged round the two legs hold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoundSnapshot {
    /// Mean of the two entry prices: what the close distance is measured from.
    pub entry: String,
    /// Both legs are closed once the mid is at or below this …
    pub close_below: String,
    /// … or at or above this.
    pub close_above: String,
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
    /// The fee rates the figures were computed with.
    #[serde(default)]
    pub fees: Option<FeesSnapshot>,
}

/// The Exchange's fee rates as read from the chain, in percent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeesSnapshot {
    pub win_percent: String,
    pub loss_percent: String,
    pub liquidation_percent: String,
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

/// What the two accounts hold, the margins parked in their positions included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FundsSnapshot {
    pub total_usd: String,
    /// How much more the richer account holds.
    pub difference_usd: String,
    /// Rounds both accounts can still fund at the current margin, before any transfer.
    pub rounds_affordable: u32,
    /// `off` when `PT_REBALANCE` is off: nothing is judged and nothing is
    /// moved. Otherwise `enough`, `rebalance` or `underfunded`.
    pub verdict: String,
    /// Both wallet keys are present, so a transfer can be signed without the operator.
    pub can_rebalance: bool,
    /// The transfer the verdict calls for, while it has not started.
    #[serde(default)]
    pub planned: Option<PlannedTransfer>,
}

/// A transfer the bot would make and has not started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedTransfer {
    pub from: Side,
    pub to: Side,
    pub amount_usd: String,
    /// What it is waiting for. `None` when it starts at once.
    pub held_by: Option<String>,
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
    /// The wallet's own key is unlocked in the running bot, which can therefore
    /// sign for the wallet. Not whether a key file exists.
    #[serde(default)]
    pub has_wallet_key: bool,
    /// Why that key is not there although `PT_WALLET_AUTOMATION` is on.
    /// `None` when it is loaded, and when the automation is off.
    #[serde(default)]
    pub wallet_key_problem: Option<String>,
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
    /// Unix milliseconds; `None` when the account does not say.
    #[serde(default)]
    pub opened_at_ms: Option<u64>,
}

/// An 18-decimal amount with two decimal places, truncated.
pub fn format_amount(raw: U256) -> String {
    format_decimal(raw, 2)
}

pub fn format_signed_amount(raw: I256) -> String {
    format_signed_decimal(raw, 2)
}

/// An 18-decimal number with `places` decimal places (at most 18), truncated.
pub fn format_decimal(raw: U256, places: u8) -> String {
    let unit = U256::from(10).pow(U256::from(USD_DECIMALS));
    let fraction = (raw % unit) / U256::from(10).pow(U256::from(USD_DECIMALS - places));
    format!("{}.{fraction:0width$}", raw / unit, width = usize::from(places))
}

pub fn format_signed_decimal(raw: I256, places: u8) -> String {
    let sign = if raw.is_negative() { "-" } else { "" };
    format!("{sign}{}", format_decimal(raw.unsigned_abs(), places))
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
    fn a_snapshot_written_before_the_newer_sections_still_reads() {
        // The shape of the first release: no round, estimate, funds, transfer,
        // wallet key or opening time anywhere.
        let old = serde_json::json!({
            "at_ms": 1, "dry_run": false, "mode": "run", "coin": "BTC", "close_bps": 100,
            "phase": "hedged", "decision": "wait", "summary": "hedged; price is inside the target",
            "mid": "83100.5", "move_bps": 48.4, "trading": null, "queue_total_usd": "0.00",
            "legs": [{
                "side": "long", "wallet": "0x01", "session_key": "0x02", "pending_intents": 0,
                "account": {
                    "available_usd": "1000.00", "queued_usd": "0.00", "session_key_expiry": 1_800_000_000u64,
                    "positions": [{
                        "id": "7", "is_long": true, "entry": "82700.0", "bust": "78604.4",
                        "margin_usd": "500.00", "leverage": 20
                    }]
                }
            }]
        });
        let snapshot: Snapshot = serde_json::from_value(old).unwrap();
        assert_eq!(
            (&snapshot.round, &snapshot.estimate, &snapshot.funds, &snapshot.transfer),
            (&None, &None, &None, &None)
        );
        assert!(!snapshot.legs[0].has_wallet_key);
        assert_eq!(snapshot.legs[0].wallet_key_problem, None);
        assert_eq!(snapshot.legs[0].account.as_ref().unwrap().positions[0].opened_at_ms, None);
        assert_eq!(snapshot.held_since_ms(), None);

        // And an estimate written before the fee rates were recorded with it.
        let estimate: EstimateSnapshot = serde_json::from_value(serde_json::json!({
            "mint_rate": "100.00",
            "at_target": {
                "loser_usd": "100.00", "winner_usd": "86.38", "net_usd": "-13.61",
                "paper": "9800.00", "usd_per_paper": "0.001389"
            },
            "now": null
        }))
        .unwrap();
        assert_eq!(estimate.fees, None);

        // And the funds as they were written before a planned transfer was shown.
        let funds: FundsSnapshot = serde_json::from_value(serde_json::json!({
            "total_usd": "2300.00", "difference_usd": "1700.00", "rounds_affordable": 0,
            "verdict": "rebalance", "can_rebalance": true
        }))
        .unwrap();
        assert_eq!(funds.planned, None);
    }

    #[test]
    fn the_round_is_held_since_its_oldest_position_opened() {
        let leg = |side: Side, opened_at_ms: Option<u64>| LegSnapshot {
            side,
            wallet: String::new(),
            session_key: String::new(),
            account: Some(AccountSnapshot {
                available_usd: "0.00".into(),
                queued_usd: "0.00".into(),
                session_key_expiry: None,
                positions: vec![PositionSnapshot {
                    id: "1".into(),
                    is_long: side.is_long(),
                    entry: "80000.0".into(),
                    bust: "76000.0".into(),
                    margin_usd: "500.00".into(),
                    leverage: 20,
                    opened_at_ms,
                }],
            }),
            pending_intents: 0,
            has_wallet_key: false,
            wallet_key_problem: None,
        };
        let held = |long, short| {
            let mut snapshot: Snapshot = serde_json::from_value(serde_json::json!({
                "at_ms": 1, "dry_run": false, "mode": "run", "coin": "BTC", "close_bps": 100,
                "phase": "hedged", "decision": "wait", "summary": "", "mid": null, "move_bps": null,
                "trading": null, "queue_total_usd": null, "legs": []
            }))
            .unwrap();
            snapshot.legs = vec![leg(Side::Long, long), leg(Side::Short, short)];
            snapshot.held_since_ms()
        };
        assert_eq!(held(Some(2_000), Some(1_500)), Some(1_500));
        assert_eq!(held(None, Some(1_500)), Some(1_500));
        assert_eq!(held(None, None), None);
    }

    #[test]
    fn small_numbers_can_be_shown_with_more_places() {
        // 13.616… USD for 9,800 PAPER.
        let cost = U256::from(1_389_389_960_794_843u64);
        assert_eq!(format_decimal(cost, 6), "0.001389");
        assert_eq!(format_decimal(cost, 2), "0.00");
        assert_eq!(format_decimal(usd(100), 6), "100.000000");
        assert_eq!(format_signed_decimal(I256::try_from(cost).unwrap().wrapping_neg(), 6), "-0.001389");
    }

    #[test]
    fn signed_amounts_keep_their_sign() {
        let loss = I256::try_from(usd(100)).unwrap().wrapping_neg();
        assert_eq!(format_signed_amount(loss), "-100.00");
        assert_eq!(format_signed_amount(I256::try_from(usd(86)).unwrap()), "86.00");
    }
}
