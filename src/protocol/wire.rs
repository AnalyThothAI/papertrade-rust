//! Shapes of the JSON the relayer and state API send.
//!
//! They follow the validators in the official frontend bundle: integers that
//! can exceed 2^53 arrive as decimal strings, and most records are positional
//! tuples. Tuples are read by index and tolerate extra trailing fields, so an
//! additive server change does not stop the bot.

use std::collections::BTreeSet;

use alloy::primitives::{Address, B256, I256, U256};
use serde::{Deserialize, Deserializer, de};
use serde_json::Value;

/// An unsigned integer sent as a decimal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dec(pub U256);

impl<'de> Deserialize<'de> for Dec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        parse_dec(&String::deserialize(deserializer)?).map(Dec).map_err(de::Error::custom)
    }
}

/// A signed integer sent as a decimal string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedDec(pub I256);

impl<'de> Deserialize<'de> for SignedDec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let digits = text.strip_prefix('-').unwrap_or(&text);
        if !is_canonical_decimal(digits) || text == "-0" {
            return Err(de::Error::custom(format!("not a decimal integer: {text:?}")));
        }
        I256::from_dec_str(&text).map(SignedDec).map_err(de::Error::custom)
    }
}

fn is_canonical_decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && (text == "0" || !text.starts_with('0'))
}

fn parse_dec(text: &str) -> Result<U256, String> {
    if !is_canonical_decimal(text) {
        return Err(format!("not a decimal integer: {text:?}"));
    }
    U256::from_str_radix(text, 10).map_err(|error| format!("{text:?}: {error}"))
}

/// Positional access to one wire tuple, with errors that name the field.
struct Fields<'a> {
    record: &'static str,
    values: &'a [Value],
}

impl Fields<'_> {
    fn get(&self, index: usize) -> Result<&Value, String> {
        self.values.get(index).ok_or_else(|| format!("{} is missing field {index}", self.record))
    }

    fn invalid(&self, index: usize, expected: &str) -> String {
        format!("{} field {index} is not {expected}", self.record)
    }

    fn text(&self, index: usize) -> Result<&str, String> {
        self.get(index)?.as_str().ok_or_else(|| self.invalid(index, "a string"))
    }

    fn flag(&self, index: usize) -> Result<bool, String> {
        self.get(index)?.as_bool().ok_or_else(|| self.invalid(index, "a boolean"))
    }

    fn number(&self, index: usize) -> Result<u64, String> {
        self.get(index)?.as_u64().ok_or_else(|| self.invalid(index, "a non-negative integer"))
    }

    fn small(&self, index: usize) -> Result<u32, String> {
        u32::try_from(self.number(index)?).map_err(|_| self.invalid(index, "a 32-bit integer"))
    }

    fn dec(&self, index: usize) -> Result<U256, String> {
        parse_dec(self.text(index)?)
    }

    fn dec_u64(&self, index: usize) -> Result<u64, String> {
        u64::try_from(self.dec(index)?).map_err(|_| self.invalid(index, "a 64-bit integer"))
    }

    fn hash(&self, index: usize) -> Result<B256, String> {
        self.text(index)?.parse().map_err(|_| self.invalid(index, "a 32-byte hash"))
    }
}

// --- /state/trading ----------------------------------------------------------

/// One listed market. Tuple layout (frontend `trading_instrument_tuple`):
/// 0 id, 1 symbol, 2 asset index, 3 price scale, 4 active, 5 paused,
/// 14 max leverage, 15 max position notional; the rest is not used here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<Value>")]
pub struct Instrument {
    pub id: u32,
    pub symbol: String,
    /// Wire price = USD price × scale.
    pub price_scale: u64,
    pub openable: bool,
    pub max_leverage: u32,
    pub max_position_notional: U256,
}

impl TryFrom<Vec<Value>> for Instrument {
    type Error = String;

    fn try_from(values: Vec<Value>) -> Result<Self, String> {
        let f = Fields { record: "instrument", values: &values };
        Ok(Self {
            id: f.small(0)?,
            symbol: f.text(1)?.to_owned(),
            price_scale: f.dec_u64(3)?,
            openable: f.flag(4)? && !f.flag(5)?,
            max_leverage: f.small(14)?,
            max_position_notional: f.dec(15)?,
        })
    }
}

/// `[revision, minimum PAPER stake, maximum net open interest, paused actions]`
#[derive(Debug, Deserialize)]
#[serde(try_from = "Vec<Value>")]
struct Intake {
    paused_actions: BTreeSet<String>,
}

impl TryFrom<Vec<Value>> for Intake {
    type Error = String;

    fn try_from(values: Vec<Value>) -> Result<Self, String> {
        let f = Fields { record: "intake", values: &values };
        let actions = f.get(3)?.as_array().ok_or_else(|| f.invalid(3, "a list"))?;
        let paused_actions = actions
            .iter()
            .map(|action| action.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or_else(|| f.invalid(3, "a list of strings"))?;
        Ok(Self { paused_actions })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TradingStateWire {
    trading_paused: bool,
    minimum_trade_size_raw: Dec,
    minimum_open_notional_raw: Dec,
    instruments: Vec<Instrument>,
    intake: Option<Intake>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "TradingStateWire")]
pub struct TradingState {
    pub trading_paused: bool,
    pub minimum_trade_size: U256,
    pub minimum_open_notional: U256,
    pub instruments: Vec<Instrument>,
    /// Actions the relayer currently refuses to take in.
    pub paused_actions: BTreeSet<String>,
}

impl From<TradingStateWire> for TradingState {
    fn from(wire: TradingStateWire) -> Self {
        Self {
            trading_paused: wire.trading_paused,
            minimum_trade_size: wire.minimum_trade_size_raw.0,
            minimum_open_notional: wire.minimum_open_notional_raw.0,
            instruments: wire.instruments,
            paused_actions: wire.intake.map(|intake| intake.paused_actions).unwrap_or_default(),
        }
    }
}

impl TradingState {
    pub fn instrument(&self, symbol: &str) -> Option<&Instrument> {
        self.instruments.iter().find(|instrument| instrument.symbol == symbol)
    }
}

// --- /query/protocol/queue-summary -------------------------------------------

#[derive(Debug, Deserialize)]
pub struct QueueSummary {
    /// Total the LP owes to queued winners.
    pub total: Dec,
}

// --- POST /intents -----------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentReceipt {
    pub intent_id: B256,
    pub state: String,
    pub queue_revision: Dec,
}

#[derive(Debug, Default, Deserialize)]
pub struct IntentFailure {
    pub code: Option<String>,
    pub error: Option<String>,
    pub reason: Option<String>,
}

// --- /query/accounts/{wallet}/trades -----------------------------------------

/// One settled position from the account's trade history.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Trade {
    pub timestamp_ms: u64,
    pub tx_hash: B256,
    pub position_id: Dec,
    pub symbol: String,
    pub price_scale_raw: Dec,
    pub is_long: bool,
    pub entry_price_raw: Dec,
    pub margin_raw: Dec,
    pub leverage: u32,
    /// `PositionClosed` or `Liquidated`.
    pub kind: String,
    pub exit_price_raw: Dec,
    /// PnL after the protocol's impact haircut, before the win fee.
    pub adjusted_pnl_raw: SignedDec,
    pub user_paid_fee_raw: Dec,
    pub paper_minted_raw: Dec,
    pub paper_mint_basis_raw: Dec,
}

#[derive(Debug, Deserialize)]
pub struct TradePage {
    pub items: Vec<Trade>,
}

// --- /state/user/live --------------------------------------------------------

/// Tuple layout (frontend `position_tuple`): 0 id, 1 instrument, 2 is long,
/// 3 opened from queued balance, 4 entry price, 5 margin, 6 leverage,
/// 7 bust price, 8 opened at, 9 opening intent.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<Value>")]
pub struct Position {
    pub position_id: U256,
    pub instrument_id: u32,
    pub is_long: bool,
    /// Wire scale, see [`Instrument::price_scale`].
    pub entry_price: u64,
    pub margin: U256,
    pub leverage: u32,
    pub bust_price: u64,
    /// The intent that opened it.
    pub intent_id: Option<B256>,
}

impl TryFrom<Vec<Value>> for Position {
    type Error = String;

    fn try_from(values: Vec<Value>) -> Result<Self, String> {
        let f = Fields { record: "position", values: &values };
        Ok(Self {
            position_id: f.dec(0)?,
            instrument_id: f.small(1)?,
            is_long: f.flag(2)?,
            entry_price: f.dec_u64(4)?,
            margin: f.dec(5)?,
            leverage: f.small(6)?,
            bust_price: f.dec_u64(7)?,
            intent_id: if f.get(9)?.is_null() { None } else { Some(f.hash(9)?) },
        })
    }
}

/// An intent the relayer has accepted and not yet executed. Tuple layout
/// (frontend `operation_tuple`): 0 intent id, 1 action, 8 deadline.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<Value>")]
pub struct Operation {
    pub intent_id: B256,
    pub action: String,
    /// Unix seconds.
    pub deadline: u64,
}

impl TryFrom<Vec<Value>> for Operation {
    type Error = String;

    fn try_from(values: Vec<Value>) -> Result<Self, String> {
        let f = Fields { record: "operation", values: &values };
        Ok(Self { intent_id: f.hash(0)?, action: f.text(1)?.to_owned(), deadline: f.dec_u64(8)? })
    }
}

/// `[intent id, outcome]`: how the relayer says an intent ended.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "Vec<Value>")]
pub struct IntentOutcome {
    pub intent_id: B256,
    pub outcome: String,
}

impl TryFrom<Vec<Value>> for IntentOutcome {
    type Error = String;

    fn try_from(values: Vec<Value>) -> Result<Self, String> {
        let f = Fields { record: "intent outcome", values: &values };
        Ok(Self { intent_id: f.hash(0)?, outcome: f.text(1)?.to_owned() })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Funds {
    pub available_balance: Dec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletSnapshot {
    pub address: Address,
    pub account_revision: u64,
    /// Unix seconds of the last block the indexer has applied.
    pub indexed_through_timestamp: u64,
    /// `[session key address, expiry in unix seconds]`
    pub session_keys: Vec<(Address, Dec)>,
    /// How much of the relayer's intake queue this view already reflects.
    pub queue_revision: Dec,
    pub queued: Dec,
    pub funds: Funds,
    pub operations: Vec<Operation>,
    pub open_positions: Vec<Position>,
}

/// The part of a wallet's state a patch may replace; absent means unchanged.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletChanges {
    pub indexed_through_timestamp: Option<u64>,
    pub session_keys: Option<Vec<(Address, Dec)>>,
    pub queue_revision: Option<Dec>,
    pub queued: Option<Dec>,
    pub funds: Option<Funds>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletPatch {
    pub address: Address,
    pub account_revision: u64,
    pub base_account_revision: u64,
    pub state: WalletChanges,
    pub intent_outcomes: Option<Vec<IntentOutcome>>,
    pub added_positions: Vec<Position>,
    pub removed_position_ids: Vec<Dec>,
    pub added_operations: Vec<Operation>,
    pub removed_operation_ids: Vec<B256>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::protocol::deployment::usd;
    use crate::testing::TRADING_STATE;

    fn trading_json() -> Value {
        serde_json::from_str(TRADING_STATE).unwrap()
    }

    #[test]
    fn reads_the_production_market_list_and_limits() {
        let state: TradingState = serde_json::from_str(TRADING_STATE).unwrap();
        assert_eq!(state.instruments.iter().map(|i| i.symbol.as_str()).collect::<Vec<_>>(), ["BTC", "ETH"]);
        assert_eq!(
            state.instrument("BTC"),
            Some(&Instrument {
                id: 0,
                symbol: "BTC".into(),
                price_scale: 10,
                openable: true,
                max_leverage: 1000,
                max_position_notional: usd(10_000_000),
            })
        );
        assert_eq!(state.instrument("ETH").unwrap().price_scale, 100);
        assert_eq!(state.minimum_trade_size, usd(10));
        assert_eq!(state.minimum_open_notional, usd(10_000));
        assert!(!state.trading_paused);
        assert_eq!(state.instrument("DOGE"), None);
    }

    #[test]
    fn reads_which_actions_the_relayer_refuses() {
        let state: TradingState = serde_json::from_str(TRADING_STATE).unwrap();
        assert_eq!(state.paused_actions.iter().collect::<Vec<_>>(), ["closePositions", "openPosition"]);
    }

    #[test]
    fn a_missing_intake_record_means_nothing_is_paused() {
        let mut raw = trading_json();
        raw.as_object_mut().unwrap().remove("intake");
        let state: TradingState = serde_json::from_value(raw).unwrap();
        assert!(state.paused_actions.is_empty());
    }

    #[test]
    fn a_paused_market_is_not_open_for_entry() {
        let mut raw = trading_json();
        raw["instruments"][0][5] = json!(true);
        let state: TradingState = serde_json::from_value(raw).unwrap();
        assert!(!state.instrument("BTC").unwrap().openable);
    }

    #[test]
    fn fields_the_server_may_add_later_are_tolerated() {
        let mut raw = trading_json();
        raw["instruments"][0].as_array_mut().unwrap().push(json!("new-field"));
        raw["extra"] = json!(1);
        let state: TradingState = serde_json::from_value(raw).unwrap();
        assert_eq!(state.instrument("BTC").unwrap().id, 0);
    }

    #[test]
    fn a_malformed_record_names_the_offending_field() {
        let mut raw = trading_json();
        raw["instruments"][0][14] = json!("many");
        let error = serde_json::from_value::<TradingState>(raw).unwrap_err().to_string();
        assert!(error.contains("instrument field 14"), "{error}");
    }

    #[test]
    fn decimal_strings_must_be_canonical() {
        let dec = |text: &str| serde_json::from_value::<Dec>(json!(text)).map(|value| value.0);
        assert_eq!(dec("0").unwrap(), U256::ZERO);
        assert_eq!(
            dec("673377893000000000000").unwrap(),
            U256::from(673_377_893u64) * U256::from(10).pow(U256::from(12))
        );
        for bad in ["", "007", "-1", "1.5", "0x10", " 1"] {
            assert!(dec(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(serde_json::from_value::<Dec>(json!(5)).is_err());

        let signed = |text: &str| serde_json::from_value::<SignedDec>(json!(text)).map(|value| value.0);
        assert_eq!(signed("-100").unwrap(), I256::try_from(-100).unwrap());
        assert_eq!(signed("86").unwrap(), I256::try_from(86).unwrap());
        for bad in ["-0", "--1", "-", "+1"] {
            assert!(signed(bad).is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn reads_relayer_replies() {
        let queue: QueueSummary = serde_json::from_value(json!({
            "ok": true, "source": { "block": "48152673", "at": 1_791_619_891_000u64 },
            "total": "0", "count": 0, "largest": null, "front": null
        }))
        .unwrap();
        assert_eq!(queue.total.0, U256::ZERO);

        let id = format!("0x{}", "ab".repeat(32));
        let receipt: IntentReceipt = serde_json::from_value(json!({
            "intentId": id, "action": "openPosition", "state": "queued", "queueRevision": "7"
        }))
        .unwrap();
        assert_eq!((receipt.state.as_str(), receipt.queue_revision.0), ("queued", U256::from(7)));

        // A rejection is not mistaken for a receipt.
        let rejection = json!({ "code": "intake_action_paused", "action": "openPosition" });
        assert!(serde_json::from_value::<IntentReceipt>(rejection).is_err());
    }

    #[test]
    fn reads_a_trade_history_page() {
        let page: TradePage = serde_json::from_value(json!({
            "items": [{
                "timestampMs": 1_791_700_000_000u64,
                "txHash": format!("0x{}", "cd".repeat(32)),
                "positionId": "42", "instrumentId": 0, "symbol": "BTC", "assetIndex": 0,
                "priceScaleRaw": "10", "terminal": false, "retired": false, "isLong": true,
                "entryPriceRaw": "827935", "marginRaw": "500000000000000000000", "leverage": 20,
                "kind": "PositionClosed", "openedAtMs": 1_791_690_000_000u64, "exitPriceRaw": "819656",
                "adjustedPnlRaw": "-100000000000000000000", "userPaidFeeRaw": "0",
                "paperMintedRaw": "9800000000000000000000", "paperMintBasisRaw": "98000000000000000000",
                "paperMintBatchTotalRaw": null
            }],
            "nextCursor": null
        }))
        .unwrap();
        let trade = &page.items[0];
        assert_eq!(trade.position_id.0, U256::from(42));
        assert_eq!(trade.adjusted_pnl_raw.0, I256::try_from(usd(100)).unwrap().wrapping_neg());
        assert_eq!(trade.paper_minted_raw.0, usd(9_800));

        // Production today: no trades yet.
        let empty: TradePage = serde_json::from_value(json!({ "items": [], "nextCursor": null })).unwrap();
        assert!(empty.items.is_empty());
    }
}
