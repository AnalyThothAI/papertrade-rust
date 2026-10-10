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
/// 12 rate multiplier, 13 position multiplier, 14 max leverage,
/// 15 max position notional, 17 base rate, 18 reference notional; the rest is
/// not used here.
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
    /// `None` when the record does not carry readable impact parameters. Only
    /// the estimate needs them, so their absence must not stop trading.
    pub impact: Option<Impact>,
}

/// How much of its raw gain a winning close keeps: the market's parameters of
/// the protocol's impact haircut, each a decimal scaled by 1e18.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Impact {
    /// The share of a gain that is never paid out, however large the move.
    pub base_rate: U256,
    pub rate_multiplier: U256,
    pub reference_notional: U256,
    pub position_multiplier: U256,
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
            impact: Impact::read(&f).ok(),
        })
    }
}

impl Impact {
    fn read(f: &Fields<'_>) -> Result<Self, String> {
        Ok(Self {
            base_rate: f.dec(17)?,
            rate_multiplier: f.dec(12)?,
            reference_notional: f.dec(18)?,
            position_multiplier: f.dec(13)?,
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

// --- /query/protocol/summary ---------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MintCurveWire {
    tracked_lp: SignedDec,
    cliff: Dec,
    rate: Dec,
    decay: Dec,
    tail_progress: Dec,
}

/// Where the PAPER mint curve stands. Amounts are 18-decimal USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(from = "MintCurveWire")]
pub struct MintCurve {
    /// What the LP has gained so far; negative while it is underwater.
    pub tracked_lp: I256,
    /// The mint rate is flat while the tracked LP is below this.
    pub cliff: U256,
    /// PAPER minted per USD of mint basis in the flat region, 18 decimals.
    pub rate: U256,
    /// How slowly the rate falls beyond the cliff.
    pub decay: U256,
    /// How far the LP has ever been beyond the cliff. It never goes back down.
    pub tail_progress: U256,
}

impl From<MintCurveWire> for MintCurve {
    fn from(wire: MintCurveWire) -> Self {
        Self {
            tracked_lp: wire.tracked_lp.0,
            cliff: wire.cliff.0,
            rate: wire.rate.0,
            decay: wire.decay.0,
            tail_progress: wire.tail_progress.0,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ProtocolSummary {
    pub paper: MintCurve,
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

impl Trade {
    /// The position was liquidated rather than closed.
    pub fn is_liquidation(&self) -> bool {
        self.kind == "Liquidated"
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradePage {
    pub items: Vec<Trade>,
    // Official frontend index-Tu4xzYeT.js, history schema `mg` and request
    // `_g`: a nullable opaque cursor, required even on the last page. The
    // empty production reply captured 2026-10-10 is trades-empty.json.
    #[serde(deserialize_with = "history_cursor")]
    pub next_cursor: Option<String>,
}

fn history_cursor<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let cursor = Option::<String>::deserialize(deserializer)?;
    if cursor.as_ref().is_some_and(|cursor| cursor.is_empty() || cursor.len() > 2048) {
        return Err(de::Error::custom("history cursor must contain 1 to 2048 bytes"));
    }
    Ok(cursor)
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
    /// Unix milliseconds; `None` if the record does not say. Only shown, never decided on.
    pub opened_at_ms: Option<u64>,
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
            opened_at_ms: f.number(8).ok(),
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

/// Actual cash credited to and debited from the Exchange, from the account's
/// lifetime statistics. Trades and their fees do not change these counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(from = "FundingTotalsWire")]
pub struct FundingTotals {
    pub deposited: U256,
    pub withdrawn: U256,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FundingTotalsWire {
    deposited_raw: Dec,
    withdrawn_raw: Dec,
}

impl From<FundingTotalsWire> for FundingTotals {
    fn from(wire: FundingTotalsWire) -> Self {
        Self { deposited: wire.deposited_raw.0, withdrawn: wire.withdrawn_raw.0 }
    }
}

/// Absent means unchanged on a patch; an explicit null is not a cash reading.
fn funding_totals<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<FundingTotals>, D::Error> {
    FundingTotals::deserialize(deserializer).map(Some)
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
    /// Missing cash evidence disables funding actions, not management of positions.
    #[serde(default, rename = "lifetime", deserialize_with = "funding_totals")]
    pub funding_totals: Option<FundingTotals>,
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
    /// Top-level on a patch, exactly as in the official frontend's account fold.
    #[serde(default, rename = "lifetime", deserialize_with = "funding_totals")]
    pub funding_totals: Option<FundingTotals>,
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
    use crate::testing::{PROTOCOL_SUMMARY, TRADING_STATE};

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
                // 10%, 15,000, 100,000 USD and 814.598: the figures the documentation gives for BTC.
                impact: Some(Impact {
                    base_rate: usd(1) / U256::from(10),
                    rate_multiplier: usd(15_000),
                    reference_notional: usd(100_000),
                    position_multiplier: usd(814_598) / U256::from(1_000),
                }),
            })
        );
        let eth = state.instrument("ETH").unwrap().impact.unwrap();
        assert_eq!(eth.position_multiplier, usd(483_979) / U256::from(1_000));
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
    fn a_market_without_readable_impact_parameters_is_still_a_market() {
        // The estimate needs them; opening and closing do not.
        let mut raw = trading_json();
        raw["instruments"][0][17] = json!(null);
        raw["instruments"][1].as_array_mut().unwrap().truncate(16);
        let state: TradingState = serde_json::from_value(raw).unwrap();
        for symbol in ["BTC", "ETH"] {
            let instrument = state.instrument(symbol).unwrap();
            assert!(instrument.openable && instrument.max_leverage == 1000);
            assert_eq!(instrument.impact, None);
        }
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
    fn reads_the_production_mint_curve() {
        let summary: ProtocolSummary = serde_json::from_str(PROTOCOL_SUMMARY).unwrap();
        assert_eq!(
            summary.paper,
            MintCurve {
                tracked_lp: I256::ZERO,
                cliff: usd(2_000_000),
                rate: usd(100),
                decay: usd(120_000_000),
                tail_progress: U256::ZERO,
            }
        );
    }

    #[test]
    fn the_tracked_lp_may_be_negative() {
        let mut raw: Value = serde_json::from_str(PROTOCOL_SUMMARY).unwrap();
        raw["paper"]["trackedLp"] = json!("-5000000000000000000");
        let summary: ProtocolSummary = serde_json::from_value(raw).unwrap();
        assert_eq!(summary.paper.tracked_lp, I256::try_from(usd(5)).unwrap().wrapping_neg());
    }

    #[test]
    fn a_position_says_when_it_was_opened_if_the_record_does() {
        let tuple = |opened_at: Value| {
            json!(["42", 0, true, false, "827935", "500000000000000000000", 20, "786932", opened_at, null])
        };
        let position: Position = serde_json::from_value(tuple(json!(1_791_620_900_000u64))).unwrap();
        assert_eq!(position.opened_at_ms, Some(1_791_620_900_000));
        // An unreadable timestamp costs the display its "held for", never the position.
        let position: Position = serde_json::from_value(tuple(json!(null))).unwrap();
        assert_eq!((position.opened_at_ms, position.entry_price), (None, 827_935));
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
        let empty: TradePage =
            serde_json::from_str(include_str!("../../tests/fixtures/trades-empty.json")).unwrap();
        assert!(empty.items.is_empty());
        assert_eq!(empty.next_cursor, None);
    }

    #[test]
    fn a_history_page_requires_an_explicit_valid_cursor() {
        assert!(serde_json::from_value::<TradePage>(json!({ "items": [] })).is_err());
        for cursor in ["".to_owned(), "x".repeat(2049)] {
            assert!(
                serde_json::from_value::<TradePage>(json!({ "items": [], "nextCursor": cursor })).is_err()
            );
        }
        let page: TradePage =
            serde_json::from_value(json!({ "items": [], "nextCursor": "opaque+/=&?" })).unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("opaque+/=&?"));
    }
}
