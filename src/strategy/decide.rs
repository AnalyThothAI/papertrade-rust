//! The whole strategy as one pure function of what is currently known.
//!
//! Two dedicated accounts hold the same market in opposite directions: one
//! long, one short, equal size. Whichever way price goes, one leg loses (and
//! mints PAPER) and the other wins about as much. Once price is far enough from
//! the entry, both are closed and the cycle starts again.
//!
//! Nothing is remembered between calls. Every answer is derived from the live
//! account state, so a restart, a liquidation or a manual close leads to the
//! right next step without any recovery logic.

use std::fmt;

use alloy::primitives::{Address, U256};
use serde::{Deserialize, Serialize};

use super::inflight::Awaiting;
use crate::protocol::account::AccountState;
use crate::protocol::wire::{Position, TradingState};

/// A session key must outlive a full round with room to spare before we open on it.
const MIN_SESSION_KEY_SECONDS_TO_OPEN: u64 = 24 * 60 * 60;
/// A session key with less than this left is registered again, when the
/// wallet's own key is there to sign for it.
const MIN_SESSION_KEY_SECONDS_TO_KEEP: u64 = 3 * 24 * 60 * 60;
/// One close intent carries at most this many positions.
const MAX_POSITIONS_PER_CLOSE: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    pub const BOTH: [Side; 2] = [Side::Long, Side::Short];

    pub fn as_str(self) -> &'static str {
        match self {
            Side::Long => "long",
            Side::Short => "short",
        }
    }

    pub fn is_long(self) -> bool {
        self == Side::Long
    }

    /// The account on the other side of the pair.
    pub fn other(self) -> Side {
        match self {
            Side::Long => Side::Short,
            Side::Short => Side::Long,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the operator wants the bot to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Open, hold and close rounds.
    Run,
    /// Finish the round that is open, start no new one.
    Pause,
    /// Close every position now, open nothing.
    Flatten,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Run => "run",
            Mode::Pause => "pause",
            Mode::Flatten => "flatten",
        }
    }
}

/// Where the pair of legs stands, read from the positions alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// At least one account's state is unavailable.
    Unknown,
    Flat,
    Hedged,
    /// Only this side holds a position.
    OneLegged(Side),
    /// Positions the strategy does not manage.
    Unexpected,
}

impl Phase {
    pub fn of(long: Option<&AccountState>, short: Option<&AccountState>, market: &Market) -> Self {
        let (Some(long), Some(short)) = (long, short) else { return Phase::Unknown };
        match (sole_leg(long, market, Side::Long), sole_leg(short, market, Side::Short)) {
            (Ok(None), Ok(None)) => Phase::Flat,
            (Ok(Some(_)), Ok(Some(_))) => Phase::Hedged,
            (Ok(Some(_)), Ok(None)) => Phase::OneLegged(Side::Long),
            (Ok(None), Ok(Some(_))) => Phase::OneLegged(Side::Short),
            _ => Phase::Unexpected,
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Phase::Unknown => f.write_str("unknown"),
            Phase::Flat => f.write_str("flat"),
            Phase::Hedged => f.write_str("hedged"),
            Phase::OneLegged(side) => write!(f, "one leg ({side})"),
            Phase::Unexpected => f.write_str("unexpected positions"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyParams {
    /// Target position value per leg, 18-decimal USD.
    pub notional: U256,
    pub leverage: u32,
    /// Close both legs once price is this far from the entry.
    pub close_bps: u32,
    /// Only open while the LP owes nobody: a queued win ties up the winning leg's profit.
    pub require_empty_queue: bool,
}

impl StrategyParams {
    /// What each leg posts. Rounded up so the position value never falls below the target.
    pub fn margin(&self) -> U256 {
        self.notional.div_ceil(U256::from(self.leverage))
    }

    /// What the losing leg gives up when the round closes at the target.
    pub fn round_loss(&self) -> U256 {
        self.notional * U256::from(self.close_bps) / U256::from(10_000)
    }
}

/// The market the bot trades; its id is fixed for the lifetime of the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Market {
    pub id: u32,
    pub symbol: String,
}

#[derive(Debug, Clone, Copy)]
pub struct LegView<'a> {
    /// `None` when the live state cannot be trusted right now.
    pub account: Option<&'a AccountState>,
    /// Address of the session key this bot signs with for the account.
    pub session_key: Address,
    /// How much the intents we sent for this account still hold the strategy back.
    pub awaiting: Awaiting,
    /// What a transfer between the two accounts has claimed from this
    /// account's balance and may still take: not there to open with. 18-decimal
    /// USD; see `strategy::rebalance::reserved`.
    pub reserved: U256,
}

#[derive(Debug, Clone, Copy)]
pub struct View<'a> {
    pub now_seconds: u64,
    pub mode: Mode,
    pub long: LegView<'a>,
    pub short: LegView<'a>,
    pub trading: Option<&'a TradingState>,
    /// Total the LP owes to queued winners, `None` when unknown.
    pub queue_total: Option<U256>,
    /// Hyperliquid mid in wire price scale, `None` when there is no fresh quote.
    pub mid: Option<u64>,
    /// False right after an open attempt; see the bot loop.
    pub may_open: bool,
}

impl<'a> View<'a> {
    fn leg(&self, side: Side) -> &LegView<'a> {
        match side {
            Side::Long => &self.long,
            Side::Short => &self.short,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseOrder {
    pub side: Side,
    pub position_ids: Vec<U256>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Price reached the close distance.
    Target,
    /// Only one leg exists.
    Unhedged,
    /// The operator asked for everything to be closed.
    Flatten,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Wait(Wait),
    Open {
        instrument_id: u32,
        margin: U256,
        leverage: u32,
    },
    Close {
        reason: CloseReason,
        /// In submission order.
        closes: Vec<CloseOrder>,
    },
}

impl Decision {
    /// Whether carrying it out sends an intent for this account.
    pub fn sends_for(&self, side: Side) -> bool {
        match self {
            Decision::Wait(_) => false,
            Decision::Open { .. } => true,
            Decision::Close { closes, .. } => closes.iter().any(|close| close.side == side),
        }
    }
}

/// Why the bot is not acting. Each reason is one concrete, checkable condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    AccountStateUnavailable,
    IntentPending,
    IntentUnresolved,
    UnexpectedPositions,
    ClosesPaused,
    NoSessionKey(Side),
    HedgedNoPrice,
    ImplausiblePrice,
    InsideTarget,
    OperatorPaused,
    Flattened,
    Settling,
    TradingStateUnavailable,
    NotListed,
    TradingPaused,
    MarketClosed,
    ActionPaused(&'static str),
    SessionKeyNotReady(Side),
    SizeOutsideLimits,
    QueueUnknown,
    QueueNotEmpty,
    InsufficientBalance(Side),
    FlatNoPrice,
}

impl fmt::Display for Wait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Wait::AccountStateUnavailable => f.write_str("account state unavailable"),
            Wait::IntentPending => f.write_str("an intent is still pending"),
            Wait::IntentUnresolved => f.write_str("flat; an intent that went unanswered could still execute"),
            Wait::UnexpectedPositions => f.write_str(
                "accounts hold positions this bot did not expect; resolve them manually or flatten",
            ),
            Wait::ClosesPaused => f.write_str("positions are open but the relayer is not accepting closes"),
            Wait::NoSessionKey(side) => {
                write!(f, "the {side} account has an open position but no valid session key")
            }
            Wait::HedgedNoPrice => f.write_str("hedged; no fresh price"),
            Wait::ImplausiblePrice => {
                f.write_str("hedged; price is implausibly far from the entry, check the price scale")
            }
            Wait::InsideTarget => f.write_str("hedged; price is inside the target"),
            Wait::OperatorPaused => f.write_str("flat; paused by the operator"),
            Wait::Flattened => f.write_str("flat; flatten mode opens nothing"),
            Wait::Settling => f.write_str("flat; settling before the next open"),
            Wait::TradingStateUnavailable => f.write_str("flat; trading state unavailable"),
            Wait::NotListed => f.write_str("flat; the market is not listed"),
            Wait::TradingPaused => f.write_str("flat; trading is paused"),
            Wait::MarketClosed => f.write_str("flat; the market is not open for new positions"),
            Wait::ActionPaused(action) => write!(f, "flat; the relayer has {action} paused"),
            Wait::SessionKeyNotReady(side) => {
                write!(f, "flat; the {side} account's session key is not registered or expires soon")
            }
            Wait::SizeOutsideLimits => {
                f.write_str("flat; the configured size or leverage is outside the market's limits")
            }
            Wait::QueueUnknown => f.write_str("flat; payout queue state unavailable"),
            Wait::QueueNotEmpty => f.write_str("flat; the payout queue is not empty"),
            Wait::InsufficientBalance(side) => {
                write!(f, "flat; the {side} account's balance is below the margin")
            }
            Wait::FlatNoPrice => f.write_str("flat; no fresh price"),
        }
    }
}

pub fn decide(view: &View<'_>, params: &StrategyParams, market: &Market) -> Decision {
    let (Some(long), Some(short)) = (view.long.account, view.short.account) else {
        return Decision::Wait(Wait::AccountStateUnavailable);
    };
    if view.long.awaiting == Awaiting::Expected
        || view.short.awaiting == Awaiting::Expected
        || has_queued_trade(long)
        || has_queued_trade(short)
    {
        return Decision::Wait(Wait::IntentPending);
    }

    if view.mode == Mode::Flatten {
        let closes: Vec<CloseOrder> = [(Side::Long, long), (Side::Short, short)]
            .into_iter()
            .filter(|(_, account)| !account.positions.is_empty())
            .map(|(side, account)| CloseOrder {
                side,
                position_ids: account
                    .positions
                    .iter()
                    .take(MAX_POSITIONS_PER_CLOSE)
                    .map(|position| position.position_id)
                    .collect(),
            })
            .collect();
        if closes.is_empty() {
            return Decision::Wait(Wait::Flattened);
        }
        return close(view, CloseReason::Flatten, closes);
    }

    let (Ok(long_leg), Ok(short_leg)) =
        (sole_leg(long, market, Side::Long), sole_leg(short, market, Side::Short))
    else {
        return Decision::Wait(Wait::UnexpectedPositions);
    };

    let order =
        |side: Side, position: &Position| CloseOrder { side, position_ids: vec![position.position_id] };
    match (long_leg, short_leg) {
        (None, None) => decide_open(view, params, market, long, short),
        // One leg is gone (rejected open, liquidation, manual close). The survivor
        // is plain directional risk, so it is closed whatever the price.
        (Some(survivor), None) => close(view, CloseReason::Unhedged, vec![order(Side::Long, survivor)]),
        (None, Some(survivor)) => close(view, CloseReason::Unhedged, vec![order(Side::Short, survivor)]),
        (Some(long_leg), Some(short_leg)) => {
            let Some(mid) = view.mid else { return Decision::Wait(Wait::HedgedNoPrice) };
            let entry = u128::from(long_leg.entry_price).midpoint(u128::from(short_leg.entry_price));
            let mid = u128::from(mid);
            let distance = mid.abs_diff(entry);
            if distance * u128::from(long_leg.leverage) > entry {
                // Both legs alive beyond the liquidation distance cannot happen. More
                // likely the feed and the positions disagree about the price scale.
                return Decision::Wait(Wait::ImplausiblePrice);
            }
            if distance * 10_000 < entry * u128::from(params.close_bps) {
                return Decision::Wait(Wait::InsideTarget);
            }
            // Loser first: with an empty queue its loss funds the LP that then pays the winner.
            let closes = if mid < entry {
                vec![order(Side::Long, long_leg), order(Side::Short, short_leg)]
            } else {
                vec![order(Side::Short, short_leg), order(Side::Long, long_leg)]
            };
            close(view, CloseReason::Target, closes)
        }
    }
}

/// Closing needs nothing but the positions: if the trading state is unknown, try anyway.
fn close(view: &View<'_>, reason: CloseReason, closes: Vec<CloseOrder>) -> Decision {
    if view.trading.is_some_and(|trading| trading.paused_actions.contains("closePositions")) {
        return Decision::Wait(Wait::ClosesPaused);
    }
    for order in &closes {
        if session_key_seconds_left(view.leg(order.side), view.now_seconds) == 0 {
            return Decision::Wait(Wait::NoSessionKey(order.side));
        }
    }
    Decision::Close { reason, closes }
}

fn decide_open(
    view: &View<'_>,
    params: &StrategyParams,
    market: &Market,
    long: &AccountState,
    short: &AccountState,
) -> Decision {
    let wait = |reason: Wait| Decision::Wait(reason);
    // An intent that may still execute must not be answered with a second open.
    if view.long.awaiting != Awaiting::Nothing || view.short.awaiting != Awaiting::Nothing {
        return wait(Wait::IntentUnresolved);
    }
    if view.mode == Mode::Pause {
        return wait(Wait::OperatorPaused);
    }
    if !view.may_open {
        return wait(Wait::Settling);
    }
    let Some(trading) = view.trading else { return wait(Wait::TradingStateUnavailable) };
    let Some(instrument) = trading.instrument(&market.symbol).filter(|instrument| instrument.id == market.id)
    else {
        return wait(Wait::NotListed);
    };
    if trading.trading_paused {
        return wait(Wait::TradingPaused);
    }
    if !instrument.openable {
        return wait(Wait::MarketClosed);
    }
    // Never open what could not be closed.
    for action in ["openPosition", "closePositions"] {
        if trading.paused_actions.contains(action) {
            return wait(Wait::ActionPaused(action));
        }
    }
    for side in Side::BOTH {
        if session_key_seconds_left(view.leg(side), view.now_seconds) < MIN_SESSION_KEY_SECONDS_TO_OPEN {
            return wait(Wait::SessionKeyNotReady(side));
        }
    }

    let margin = params.margin();
    let notional = margin * U256::from(params.leverage);
    if params.leverage > instrument.max_leverage
        || margin < trading.minimum_trade_size
        || notional < trading.minimum_open_notional
        || notional > instrument.max_position_notional
    {
        return wait(Wait::SizeOutsideLimits);
    }

    if params.require_empty_queue {
        let Some(queue_total) = view.queue_total else { return wait(Wait::QueueUnknown) };
        if !queue_total.is_zero() || !long.queued.is_zero() || !short.queued.is_zero() {
            return wait(Wait::QueueNotEmpty);
        }
    }
    // What a transfer may still withdraw is not there to post: an open that
    // counted on it would compete with the withdrawal, and one would be refused.
    for (side, account) in [(Side::Long, long), (Side::Short, short)] {
        if account.available_balance.saturating_sub(view.leg(side).reserved) < margin {
            return wait(Wait::InsufficientBalance(side));
        }
    }
    if view.mid.is_none() {
        return wait(Wait::FlatNoPrice);
    }
    Decision::Open { instrument_id: instrument.id, margin, leverage: params.leverage }
}

/// The account's single position in the expected direction; `Err` if it holds anything else.
fn sole_leg<'a>(account: &'a AccountState, market: &Market, side: Side) -> Result<Option<&'a Position>, ()> {
    match account.positions.as_slice() {
        [] => Ok(None),
        [only] if only.instrument_id == market.id && only.is_long == side.is_long() => Ok(Some(only)),
        _ => Err(()),
    }
}

/// Whether the relayer holds an open or a close of the account, from this run or an earlier one.
pub fn has_queued_trade(account: &AccountState) -> bool {
    account
        .operations
        .iter()
        .any(|operation| matches!(operation.action.as_str(), "openPosition" | "closePositions"))
}

fn session_key_seconds_left(leg: &LegView<'_>, now_seconds: u64) -> u64 {
    leg.account
        .and_then(|account| account.session_keys.get(&leg.session_key))
        .map_or(0, |expiry| expiry.saturating_sub(now_seconds))
}

/// Whether the account's session key should be registered now: it is not
/// registered, or it expires soon, and no registration is already queued.
pub fn session_key_needs_registration(
    account: &AccountState,
    session_key: Address,
    now_seconds: u64,
) -> bool {
    let seconds_left =
        account.session_keys.get(&session_key).map_or(0, |expiry| expiry.saturating_sub(now_seconds));
    let queued = account.operations.iter().any(|operation| operation.action == "registerSessionKey");
    seconds_left < MIN_SESSION_KEY_SECONDS_TO_KEEP && !queued
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use alloy::primitives::address;

    use super::*;
    use crate::protocol::deployment::usd;
    use crate::protocol::wire::Operation;
    use crate::testing::{KEY, NOW, account, intent_id, position, trading_state};

    const ENTRY: u64 = 800_000;

    /// Everything `decide` looks at. The default is two funded, flat accounts on an open market.
    struct Scenario {
        long: Option<AccountState>,
        short: Option<AccountState>,
        awaiting: Option<(Side, Awaiting)>,
        trading: Option<TradingState>,
        queue_total: Option<U256>,
        mid: Option<u64>,
        may_open: bool,
        mode: Mode,
        params: StrategyParams,
        /// Whole USD a transfer has claimed from one account's balance.
        reserved: Option<(Side, u64)>,
    }

    impl Default for Scenario {
        fn default() -> Self {
            Self {
                long: Some(account()),
                short: Some(account()),
                awaiting: None,
                trading: Some(trading_state(&[])),
                queue_total: Some(U256::ZERO),
                mid: Some(ENTRY),
                may_open: true,
                mode: Mode::Run,
                params: StrategyParams {
                    notional: usd(10_000),
                    leverage: 20,
                    close_bps: 100,
                    require_empty_queue: true,
                },
                reserved: None,
            }
        }
    }

    impl Scenario {
        fn hedged(mid: u64) -> Self {
            Self {
                long: Some(holding(vec![position(7, true)])),
                short: Some(holding(vec![position(8, false)])),
                mid: Some(mid),
                ..Self::default()
            }
        }

        fn leg(&self, side: Side) -> LegView<'_> {
            let account = if side.is_long() { &self.long } else { &self.short };
            LegView {
                account: account.as_ref(),
                session_key: KEY,
                awaiting: match self.awaiting {
                    Some((awaited, how)) if awaited == side => how,
                    _ => Awaiting::Nothing,
                },
                reserved: match self.reserved {
                    Some((sender, amount)) if sender == side => usd(amount),
                    _ => U256::ZERO,
                },
            }
        }

        fn decide(&self) -> Decision {
            let view = View {
                now_seconds: NOW,
                mode: self.mode,
                long: self.leg(Side::Long),
                short: self.leg(Side::Short),
                trading: self.trading.as_ref(),
                queue_total: self.queue_total,
                mid: self.mid,
                may_open: self.may_open,
            };
            decide(&view, &self.params, &Market { id: 0, symbol: "BTC".into() })
        }

        fn wait(&self) -> Wait {
            match self.decide() {
                Decision::Wait(reason) => reason,
                other => panic!("expected to wait, got {other:?}"),
            }
        }
    }

    fn holding(positions: Vec<Position>) -> AccountState {
        AccountState { positions, ..account() }
    }

    fn funded(balance_usd: u64) -> AccountState {
        AccountState { available_balance: usd(balance_usd), ..account() }
    }

    fn with_key_expiry(mut account: AccountState, expiry: u64) -> AccountState {
        account.session_keys = BTreeMap::from([(KEY, expiry)]);
        account
    }

    fn close_of(side: Side, id: u64) -> CloseOrder {
        CloseOrder { side, position_ids: vec![U256::from(id)] }
    }

    // --- flat ----------------------------------------------------------------

    #[test]
    fn flat_opens_both_legs_with_equal_margin() {
        assert_eq!(
            Scenario::default().decide(),
            Decision::Open { instrument_id: 0, margin: usd(500), leverage: 20 }
        );
    }

    #[test]
    fn margin_rounds_up_so_the_position_never_falls_below_the_target_value() {
        let scenario = Scenario {
            params: StrategyParams { leverage: 30, ..Scenario::default().params },
            ..Scenario::default()
        };
        let Decision::Open { margin, .. } = scenario.decide() else { panic!("expected an open") };
        let notional = margin * U256::from(30);
        assert!(notional >= usd(10_000));
        assert!(notional - usd(10_000) < U256::from(30));
    }

    #[test]
    fn flat_waits_for_each_open_condition() {
        let cases: Vec<(Scenario, Wait)> = vec![
            (
                Scenario { trading: Some(trading_state(&["openPosition"])), ..Default::default() },
                Wait::ActionPaused("openPosition"),
            ),
            // Production today: opening is pointless if the position could not be closed again.
            (
                Scenario { trading: Some(trading_state(&["closePositions"])), ..Default::default() },
                Wait::ActionPaused("closePositions"),
            ),
            (
                Scenario {
                    trading: Some(TradingState { trading_paused: true, ..trading_state(&[]) }),
                    ..Default::default()
                },
                Wait::TradingPaused,
            ),
            (Scenario { trading: None, ..Default::default() }, Wait::TradingStateUnavailable),
            (
                Scenario {
                    trading: Some(TradingState { instruments: Vec::new(), ..trading_state(&[]) }),
                    ..Default::default()
                },
                Wait::NotListed,
            ),
            (Scenario { queue_total: Some(U256::from(1)), ..Default::default() }, Wait::QueueNotEmpty),
            (Scenario { queue_total: None, ..Default::default() }, Wait::QueueUnknown),
            (Scenario { mid: None, ..Default::default() }, Wait::FlatNoPrice),
            (Scenario { may_open: false, ..Default::default() }, Wait::Settling),
            (Scenario { mode: Mode::Pause, ..Default::default() }, Wait::OperatorPaused),
            (Scenario { mode: Mode::Flatten, ..Default::default() }, Wait::Flattened),
        ];
        for (scenario, expected) in cases {
            assert_eq!(scenario.wait(), expected);
        }
    }

    #[test]
    fn a_closed_market_is_not_opened() {
        let mut trading = trading_state(&[]);
        trading.instruments[0].openable = false;
        let scenario = Scenario { trading: Some(trading), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::MarketClosed);
    }

    #[test]
    fn the_payout_queue_can_be_ignored_on_request() {
        let scenario = Scenario {
            queue_total: Some(U256::from(5)),
            params: StrategyParams { require_empty_queue: false, ..Scenario::default().params },
            ..Default::default()
        };
        assert!(matches!(scenario.decide(), Decision::Open { .. }));
    }

    #[test]
    fn flat_waits_while_either_account_is_owed_a_queued_payout() {
        let scenario =
            Scenario { short: Some(AccountState { queued: usd(86), ..account() }), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::QueueNotEmpty);
    }

    #[test]
    fn flat_waits_when_one_account_cannot_fund_the_margin() {
        let scenario = Scenario { long: Some(funded(505)), short: Some(funded(495)), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::InsufficientBalance(Side::Short));
    }

    #[test]
    fn flat_waits_when_a_session_key_is_missing_or_close_to_expiry() {
        let unregistered = AccountState { session_keys: BTreeMap::new(), ..account() };
        let scenario = Scenario { long: Some(unregistered), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::SessionKeyNotReady(Side::Long));

        let expiring = with_key_expiry(account(), NOW + 3_600);
        let scenario = Scenario { short: Some(expiring), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::SessionKeyNotReady(Side::Short));
    }

    #[test]
    fn flat_waits_when_the_size_is_below_the_market_minimum() {
        let scenario = Scenario {
            params: StrategyParams { notional: usd(9_000), ..Scenario::default().params },
            ..Default::default()
        };
        assert_eq!(scenario.wait(), Wait::SizeOutsideLimits);
    }

    // --- unknown state -------------------------------------------------------

    #[test]
    fn waits_while_an_account_state_is_unavailable() {
        let scenario = Scenario { long: None, ..Default::default() };
        assert_eq!(scenario.wait(), Wait::AccountStateUnavailable);
    }

    #[test]
    fn waits_while_an_intent_we_sent_is_unresolved() {
        let scenario = Scenario { awaiting: Some((Side::Short, Awaiting::Expected)), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::IntentPending);
    }

    fn queued(action: &str) -> AccountState {
        let operation = Operation { intent_id: intent_id(1), action: action.into(), deadline: NOW + 60 };
        AccountState { operations: vec![operation], ..account() }
    }

    #[test]
    fn waits_while_the_relayer_still_holds_a_trade_intent_even_from_an_earlier_run() {
        let scenario = Scenario { long: Some(queued("openPosition")), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::IntentPending);
    }

    #[test]
    fn unrelated_queued_operations_do_not_block() {
        let scenario = Scenario { long: Some(queued("registerSessionKey")), ..Default::default() };
        assert!(matches!(scenario.decide(), Decision::Open { .. }));
    }

    #[test]
    fn unexpected_positions_are_left_alone() {
        let cases = [
            vec![position(7, true), position(9, true)],
            vec![position(7, false)],
            vec![Position { instrument_id: 1, ..position(7, true) }],
        ];
        for positions in cases {
            let scenario = Scenario { long: Some(holding(positions)), ..Default::default() };
            assert_eq!(scenario.wait(), Wait::UnexpectedPositions);
        }
    }

    // --- hedged --------------------------------------------------------------

    #[test]
    fn hedged_holds_while_price_is_inside_the_target() {
        assert_eq!(Scenario::hedged(ENTRY + 7_999).wait(), Wait::InsideTarget);
        assert_eq!(Scenario::hedged(ENTRY - 7_999).wait(), Wait::InsideTarget);
    }

    #[test]
    fn hedged_closes_the_loser_first_at_the_target() {
        assert_eq!(
            Scenario::hedged(ENTRY + 8_000).decide(),
            Decision::Close {
                reason: CloseReason::Target,
                closes: vec![close_of(Side::Short, 8), close_of(Side::Long, 7)],
            }
        );
        assert_eq!(
            Scenario::hedged(ENTRY - 8_000).decide(),
            Decision::Close {
                reason: CloseReason::Target,
                closes: vec![close_of(Side::Long, 7), close_of(Side::Short, 8)],
            }
        );
    }

    #[test]
    fn the_move_is_measured_from_the_mean_of_the_two_entries() {
        let scenario = Scenario {
            long: Some(holding(vec![Position { entry_price: 800_100, ..position(7, true) }])),
            short: Some(holding(vec![Position { entry_price: 799_900, ..position(8, false) }])),
            mid: Some(808_000),
            ..Default::default()
        };
        assert!(matches!(scenario.decide(), Decision::Close { reason: CloseReason::Target, .. }));
    }

    #[test]
    fn hedged_holds_without_a_fresh_price() {
        let scenario = Scenario { mid: None, ..Scenario::hedged(ENTRY + 8_000) };
        assert_eq!(scenario.wait(), Wait::HedgedNoPrice);
    }

    #[test]
    fn a_price_beyond_the_liquidation_distance_is_not_acted_on() {
        // 20x leverage liquidates near 5%; a 10% move with both legs alive means the prices disagree.
        assert_eq!(Scenario::hedged(ENTRY + 80_000).wait(), Wait::ImplausiblePrice);
    }

    #[test]
    fn hedged_closes_at_the_target_even_when_the_trading_state_is_unknown() {
        let scenario = Scenario { trading: None, ..Scenario::hedged(ENTRY + 8_000) };
        assert!(matches!(scenario.decide(), Decision::Close { .. }));
    }

    #[test]
    fn nothing_closes_while_the_relayer_refuses_closes() {
        let scenario =
            Scenario { trading: Some(trading_state(&["closePositions"])), ..Scenario::hedged(ENTRY + 8_000) };
        assert_eq!(scenario.wait(), Wait::ClosesPaused);
    }

    #[test]
    fn closing_needs_a_valid_session_key_but_not_a_long_lived_one() {
        let expired = with_key_expiry(holding(vec![position(7, true)]), NOW - 1);
        let scenario = Scenario { long: Some(expired), ..Scenario::hedged(ENTRY + 8_000) };
        assert_eq!(scenario.wait(), Wait::NoSessionKey(Side::Long));

        let soon = |positions| with_key_expiry(holding(positions), NOW + 600);
        let scenario = Scenario {
            long: Some(soon(vec![position(7, true)])),
            short: Some(soon(vec![position(8, false)])),
            ..Scenario::hedged(ENTRY + 8_000)
        };
        assert!(matches!(scenario.decide(), Decision::Close { .. }));
    }

    #[test]
    fn a_paused_bot_still_finishes_the_open_round() {
        let scenario = Scenario { mode: Mode::Pause, ..Scenario::hedged(ENTRY + 8_000) };
        assert!(matches!(scenario.decide(), Decision::Close { reason: CloseReason::Target, .. }));
        let scenario = Scenario { mode: Mode::Pause, ..Scenario::hedged(ENTRY) };
        assert_eq!(scenario.wait(), Wait::InsideTarget);
    }

    // --- one leg only --------------------------------------------------------

    #[test]
    fn a_lone_leg_is_closed_at_any_price() {
        let scenario = Scenario { long: Some(holding(vec![position(7, true)])), ..Default::default() };
        assert_eq!(
            scenario.decide(),
            Decision::Close { reason: CloseReason::Unhedged, closes: vec![close_of(Side::Long, 7)] }
        );
    }

    #[test]
    fn a_lone_leg_is_closed_without_price_queue_or_open_cooldown() {
        let scenario = Scenario {
            short: Some(holding(vec![position(8, false)])),
            mid: None,
            may_open: false,
            queue_total: None,
            mode: Mode::Pause,
            ..Default::default()
        };
        assert_eq!(
            scenario.decide(),
            Decision::Close { reason: CloseReason::Unhedged, closes: vec![close_of(Side::Short, 8)] }
        );
    }

    // --- flatten -------------------------------------------------------------

    #[test]
    fn flatten_closes_everything_inside_the_target() {
        let scenario = Scenario { mode: Mode::Flatten, ..Scenario::hedged(ENTRY) };
        assert_eq!(
            scenario.decide(),
            Decision::Close {
                reason: CloseReason::Flatten,
                closes: vec![close_of(Side::Long, 7), close_of(Side::Short, 8)],
            }
        );
    }

    #[test]
    fn flatten_also_closes_positions_the_bot_did_not_expect() {
        let scenario = Scenario {
            mode: Mode::Flatten,
            long: Some(holding(vec![position(7, false), position(9, true)])),
            mid: None,
            ..Default::default()
        };
        assert_eq!(
            scenario.decide(),
            Decision::Close {
                reason: CloseReason::Flatten,
                closes: vec![CloseOrder {
                    side: Side::Long,
                    position_ids: vec![U256::from(7), U256::from(9)]
                }],
            }
        );
    }

    #[test]
    fn flatten_sends_at_most_twelve_positions_per_close() {
        let many = (1..=15).map(|id| position(id, true)).collect();
        let scenario = Scenario { mode: Mode::Flatten, long: Some(holding(many)), ..Default::default() };
        let Decision::Close { closes, .. } = scenario.decide() else { panic!("expected a close") };
        assert_eq!(closes[0].position_ids.len(), 12);
    }

    #[test]
    fn flatten_still_waits_for_pending_intents_and_paused_closes() {
        let scenario = Scenario {
            mode: Mode::Flatten,
            awaiting: Some((Side::Long, Awaiting::Expected)),
            ..Scenario::hedged(ENTRY)
        };
        assert_eq!(scenario.wait(), Wait::IntentPending);
        let scenario = Scenario {
            mode: Mode::Flatten,
            trading: Some(trading_state(&["closePositions"])),
            ..Scenario::hedged(ENTRY)
        };
        assert_eq!(scenario.wait(), Wait::ClosesPaused);
    }

    // --- intents that went unanswered ---------------------------------------

    #[test]
    fn a_doubtful_intent_blocks_a_new_open_but_nothing_else() {
        let doubtful = Some((Side::Short, Awaiting::Doubtful));
        // Flat: the unanswered open could still land, so no second open.
        let scenario = Scenario { awaiting: doubtful, ..Default::default() };
        assert_eq!(scenario.wait(), Wait::IntentUnresolved);

        // One leg open while the other leg's open went unanswered: do not sit unhedged for it.
        let scenario = Scenario {
            awaiting: doubtful,
            long: Some(holding(vec![position(7, true)])),
            ..Default::default()
        };
        assert_eq!(
            scenario.decide(),
            Decision::Close { reason: CloseReason::Unhedged, closes: vec![close_of(Side::Long, 7)] }
        );

        // Hedged at the target while an earlier close went unanswered: close again.
        let scenario = Scenario { awaiting: doubtful, ..Scenario::hedged(ENTRY + 8_000) };
        assert!(matches!(scenario.decide(), Decision::Close { reason: CloseReason::Target, .. }));

        // Flatten is not held up either.
        let scenario = Scenario { awaiting: doubtful, mode: Mode::Flatten, ..Scenario::hedged(ENTRY) };
        assert!(matches!(scenario.decide(), Decision::Close { reason: CloseReason::Flatten, .. }));
    }

    #[test]
    fn an_expected_intent_holds_everything_even_with_one_leg_open() {
        let scenario = Scenario {
            awaiting: Some((Side::Short, Awaiting::Expected)),
            long: Some(holding(vec![position(7, true)])),
            ..Default::default()
        };
        assert_eq!(scenario.wait(), Wait::IntentPending);
    }

    // --- funds ----------------------------------------------------------------
    // Moving funds between the accounts is not decided here; see
    // `strategy::rebalance`. All that is left of it is the balance each account
    // can post, less what a transfer may still take from it.

    /// The long account has too much and the short too little.
    fn lopsided() -> Scenario {
        Scenario { long: Some(funded(2_000)), short: Some(funded(300)), ..Default::default() }
    }

    #[test]
    fn an_account_below_its_margin_is_the_only_thing_about_funds_that_holds_an_open_back() {
        assert_eq!(lopsided().wait(), Wait::InsufficientBalance(Side::Short));
        let mirrored = Scenario { long: Some(funded(300)), short: Some(funded(2_000)), ..Default::default() };
        assert_eq!(mirrored.wait(), Wait::InsufficientBalance(Side::Long));
        // Together below two margins: the same reason, named for the account that is short.
        let scenario = Scenario { long: Some(funded(600)), short: Some(funded(399)), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::InsufficientBalance(Side::Short));
        // One unit below the margin is below it; the margin itself is enough.
        let almost = AccountState { available_balance: usd(500) - U256::from(1), ..account() };
        let scenario = Scenario { short: Some(almost), ..Default::default() };
        assert_eq!(scenario.wait(), Wait::InsufficientBalance(Side::Short));
        let scenario = Scenario { short: Some(funded(500)), ..Default::default() };
        assert!(matches!(scenario.decide(), Decision::Open { .. }));
    }

    #[test]
    fn accounts_that_can_post_the_margin_open_however_uneven_they_are() {
        // The short account could not absorb a loss on top of its margin, and
        // the two are far apart. Neither waits for funds to be moved.
        for (long, short) in [(2_000, 550), (500, 50_000), (500, 500)] {
            let scenario =
                Scenario { long: Some(funded(long)), short: Some(funded(short)), ..Default::default() };
            assert!(matches!(scenario.decide(), Decision::Open { .. }), "{long}/{short}");
        }
        // Whether the relayer takes withdrawals is nothing to an open.
        let scenario = Scenario { trading: Some(trading_state(&["withdrawToCore"])), ..Default::default() };
        assert!(matches!(scenario.decide(), Decision::Open { .. }));
    }

    #[test]
    fn what_a_transfer_may_still_withdraw_is_not_counted_for_the_open() {
        // 1,500 held, the margin is 500: up to 1,000 can be on its way out.
        for side in Side::BOTH {
            let scenario = Scenario { reserved: Some((side, 1_000)), ..Default::default() };
            assert!(matches!(scenario.decide(), Decision::Open { .. }));
            let scenario = Scenario { reserved: Some((side, 1_001)), ..Default::default() };
            assert_eq!(scenario.wait(), Wait::InsufficientBalance(side));
            // More than the account holds leaves it nothing, not a negative balance.
            let scenario = Scenario { reserved: Some((side, 5_000)), ..Default::default() };
            assert_eq!(scenario.wait(), Wait::InsufficientBalance(side));
        }
        // It is the sender's balance that is claimed, not the other account's.
        let scenario = Scenario { reserved: Some((Side::Long, 1_800)), ..lopsided() };
        assert_eq!(scenario.wait(), Wait::InsufficientBalance(Side::Long));
        let scenario =
            Scenario { short: Some(funded(500)), reserved: Some((Side::Long, 1_500)), ..lopsided() };
        assert!(matches!(scenario.decide(), Decision::Open { .. }));
    }

    #[test]
    fn closing_is_never_held_back_by_funds_or_by_a_transfer() {
        let poor = |positions| AccountState { available_balance: U256::ZERO, ..holding(positions) };
        for reserved in [None, Some((Side::Long, 850)), Some((Side::Short, 850))] {
            let hedged = || Scenario {
                long: Some(poor(vec![position(7, true)])),
                short: Some(poor(vec![position(8, false)])),
                trading: Some(trading_state(&["withdrawToCore", "openPosition"])),
                reserved,
                ..Default::default()
            };
            let at_target = Scenario { mid: Some(ENTRY + 8_000), ..hedged() };
            assert!(matches!(at_target.decide(), Decision::Close { reason: CloseReason::Target, .. }));
            let inside = Scenario { mid: Some(ENTRY), ..hedged() };
            assert_eq!(inside.wait(), Wait::InsideTarget);
            let lone = Scenario { short: Some(poor(Vec::new())), ..hedged() };
            assert!(matches!(lone.decide(), Decision::Close { reason: CloseReason::Unhedged, .. }));
            let flatten = Scenario { mode: Mode::Flatten, ..hedged() };
            assert!(matches!(flatten.decide(), Decision::Close { reason: CloseReason::Flatten, .. }));
        }
    }

    #[test]
    fn a_decision_says_which_accounts_it_sends_an_intent_for() {
        let both = |decision: &Decision| (decision.sends_for(Side::Long), decision.sends_for(Side::Short));
        assert_eq!(both(&Scenario::default().decide()), (true, true));
        assert_eq!(both(&Scenario::hedged(ENTRY + 8_000).decide()), (true, true));
        assert_eq!(both(&Scenario::hedged(ENTRY).decide()), (false, false));
        let lone = Scenario { long: Some(holding(vec![position(7, true)])), ..Default::default() };
        assert_eq!(both(&lone.decide()), (true, false));
        assert_eq!(both(&lopsided().decide()), (false, false));
    }

    // --- session key registration ---------------------------------------------

    #[test]
    fn a_session_key_is_registered_when_it_is_missing_or_has_under_three_days_left() {
        let needs = |account: &AccountState| session_key_needs_registration(account, KEY, NOW);
        let three_days = 3 * 86_400;
        assert!(needs(&AccountState { session_keys: BTreeMap::new(), ..account() }));
        assert!(needs(&with_key_expiry(account(), NOW - 1)));
        assert!(needs(&with_key_expiry(account(), NOW + three_days - 1)));
        assert!(!needs(&with_key_expiry(account(), NOW + three_days)));
        assert!(!needs(&account()));
        // Another key being registered for the wallet does not make ours registered.
        let other = address!("0x00000000000000000000000000000000000000bb");
        assert!(session_key_needs_registration(&account(), other, NOW));
    }

    #[test]
    fn a_registration_already_queued_is_not_repeated() {
        let unregistered = AccountState { session_keys: BTreeMap::new(), ..queued("registerSessionKey") };
        assert!(!session_key_needs_registration(&unregistered, KEY, NOW));
        let trading = AccountState { session_keys: BTreeMap::new(), ..queued("openPosition") };
        assert!(session_key_needs_registration(&trading, KEY, NOW));
    }

    // --- phase ----------------------------------------------------------------

    #[test]
    fn the_phase_names_where_the_two_legs_stand() {
        let market = Market { id: 0, symbol: "BTC".into() };
        let (flat, long, short) =
            (account(), holding(vec![position(7, true)]), holding(vec![position(8, false)]));
        let phase =
            |long: Option<&AccountState>, short: Option<&AccountState>| Phase::of(long, short, &market);
        assert_eq!(phase(None, Some(&flat)), Phase::Unknown);
        assert_eq!(phase(Some(&flat), Some(&flat)), Phase::Flat);
        assert_eq!(phase(Some(&long), Some(&short)), Phase::Hedged);
        assert_eq!(phase(Some(&long), Some(&flat)), Phase::OneLegged(Side::Long));
        assert_eq!(phase(Some(&flat), Some(&short)), Phase::OneLegged(Side::Short));
        assert_eq!(phase(Some(&short), Some(&flat)), Phase::Unexpected);
        assert_eq!(Phase::OneLegged(Side::Short).to_string(), "one leg (short)");
    }

    #[test]
    fn every_wait_reason_reads_as_a_sentence() {
        assert_eq!(
            Wait::ActionPaused("openPosition").to_string(),
            "flat; the relayer has openPosition paused"
        );
        assert_eq!(
            Wait::InsufficientBalance(Side::Short).to_string(),
            "flat; the short account's balance is below the margin"
        );
    }
}
