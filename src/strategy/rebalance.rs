//! Whether funds should be moved between the two accounts, as pure functions.
//!
//! Every round moves money from the losing account to the winning one, so the
//! two drift apart until one side can no longer post its margin. Moving some
//! of it back is a concern of its own: `decide` knows nothing of it, it is
//! judged whatever the legs hold, and neither waits for the other. One number
//! links the two, [`reserved`]: what a transfer has claimed from an account is
//! not there to open a position with.

use std::fmt;

use alloy::primitives::U256;

use super::decide::{Mode, Side};
use super::transfer::{Stage, Transfer};
use crate::protocol::account::AccountState;
use crate::protocol::deployment::usd;
use crate::protocol::wire::TradingState;

/// The relayer takes no smaller withdrawal and sweeps no smaller deposit.
pub const MIN_TRANSFER_USDC: u64 = 10;

/// What one account holds. Amounts are 18-decimal USD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holding {
    pub available: U256,
    /// Margin parked in open positions: the sum of their margins. Zero means
    /// that the account holds no position.
    pub locked: U256,
}

impl Holding {
    pub fn of(account: &AccountState) -> Self {
        Self {
            available: account.available_balance,
            locked: account.positions.iter().fold(U256::ZERO, |sum, position| sum + position.margin),
        }
    }

    /// What the account holds with its positions closed at their entry. Profit
    /// and loss that are not realised yet are not counted. Opening or closing
    /// a position at its entry leaves it unchanged, so a verdict made from it
    /// means the same whether a round is open or not.
    pub fn equity(&self) -> U256 {
        self.available + self.locked
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Funds {
    /// Nothing to move. Either both accounts hold the margin and one round's
    /// loss, or what could be moved is not worth a transfer. Whether each
    /// account can post its margin is still checked on its own.
    Enough,
    /// Move this many whole USDC from the richer account to the poorer one.
    Rebalance { from: Side, to: Side, amount_usdc: u64 },
    /// Together the accounts hold less than two margins. Moving funds cannot
    /// help; they need a deposit. This is only reported: nothing waits for it.
    Underfunded,
}

impl Funds {
    pub fn as_str(self) -> &'static str {
        match self {
            Funds::Enough => "enough",
            Funds::Rebalance { .. } => "rebalance",
            Funds::Underfunded => "underfunded",
        }
    }
}

/// Judges what the two accounts hold against what a round needs.
///
/// `margin` is what each leg posts; `buffer` is what the losing leg gives up
/// in one round. Both are 18-decimal USD.
pub fn funds(long: &Holding, short: &Holding, margin: U256, buffer: U256) -> Funds {
    let comfortable = margin + buffer;
    if long.equity() >= comfortable && short.equity() >= comfortable {
        return Funds::Enough;
    }
    if long.equity() + short.equity() < margin * U256::from(2) {
        return Funds::Underfunded;
    }
    let (from, to, sender, receiver) = if long.equity() >= short.equity() {
        (Side::Long, Side::Short, long, short)
    } else {
        (Side::Short, Side::Long, short, long)
    };
    // A sender without a position must still be able to open its own leg and
    // lose the round. One that has a position has posted its margin already,
    // and what is parked there cannot be withdrawn anyway.
    let reserve = if sender.locked.is_zero() { comfortable } else { U256::ZERO };
    let spare = sender.available.saturating_sub(reserve);
    // Half the difference levels them. Whole USDC, rounded down, so the richer
    // account never ends up the poorer one.
    let half = (sender.equity() - receiver.equity()) / U256::from(2);
    let amount_usdc = u64::try_from(half.min(spare) / usd(1)).unwrap_or(u64::MAX);
    if amount_usdc < MIN_TRANSFER_USDC {
        return Funds::Enough;
    }
    Funds::Rebalance { from, to, amount_usdc }
}

/// How much of an account's balance a transfer has claimed and may still take.
///
/// From the moment a transfer is planned until its withdrawal has been seen to
/// arrive on HyperCore, the amount can leave the sender's balance at any
/// time. An open that counted on that money would compete with the withdrawal
/// for it, and one of the two would be refused. So for that long the open
/// check does not count it.
///
/// Whether the withdrawal has left the balance already is not read off the
/// balance: with rounds going on, the balance also moves when a position
/// opens or closes. For the moment between the withdrawal executing and its
/// arrival being seen the amount is therefore held back twice, which can only
/// delay an open. A stuck transfer reserves nothing: its withdrawal can no
/// longer execute.
pub fn reserved(transfer: &Transfer, side: Side) -> U256 {
    if transfer.from == side && transfer.stage == Stage::Withdraw {
        usd(transfer.amount_usdc)
    } else {
        U256::ZERO
    }
}

/// Why a transfer that the funds call for is not started now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// One transfer at a time, a stuck one included.
    TransferActive,
    /// An open or a close of the sending account is in flight or about to be
    /// sent: what it holds is about to change.
    SenderBusy,
    /// A transfer is signed with both wallets' own keys, and they are not both here.
    NoWalletKeys,
    /// Only a bot that is opening rounds starts one.
    NotRunning(Mode),
    TradingStateUnavailable,
    /// A transfer starts with a withdrawal, and the relayer is not taking those.
    WithdrawalsPaused,
    DryRun,
}

impl Hold {
    /// Whether it passes by itself, so that the transfer follows shortly and
    /// there is nothing to put on the timeline. The others last until the
    /// operator or the relayer changes something.
    pub fn is_passing(self) -> bool {
        matches!(self, Hold::TransferActive | Hold::SenderBusy | Hold::TradingStateUnavailable)
    }
}

impl fmt::Display for Hold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Hold::TransferActive => f.write_str("another transfer has not finished"),
            Hold::SenderBusy => f.write_str("the sending account has an intent in flight"),
            Hold::NoWalletKeys => f.write_str("the wallets' own keys are not both unlocked"),
            Hold::NotRunning(mode) => write!(f, "the bot is in {} mode", mode.as_str()),
            Hold::TradingStateUnavailable => f.write_str("trading state unavailable"),
            Hold::WithdrawalsPaused => f.write_str("the relayer has withdrawToCore paused"),
            Hold::DryRun => f.write_str("dry run"),
        }
    }
}

/// What starting a transfer depends on besides the funds.
#[derive(Debug, Clone, Copy)]
pub struct Readiness<'a> {
    /// A transfer is under way or stuck.
    pub transfer_active: bool,
    /// An open or a close of the account that would send is in flight, or is
    /// being sent in this very tick.
    pub sender_busy: bool,
    /// Both wallets' own keys are unlocked.
    pub wallet_keys: bool,
    pub mode: Mode,
    /// `None` when the trading state is not known right now.
    pub trading: Option<&'a TradingState>,
    pub dry_run: bool,
}

/// The first thing that stands in the way of starting a transfer, if anything
/// does. None of it is about whether a position can be opened. What would stop
/// a real run is named before the dry run is.
pub fn hold(ready: &Readiness<'_>) -> Option<Hold> {
    if ready.transfer_active {
        return Some(Hold::TransferActive);
    }
    if ready.sender_busy {
        return Some(Hold::SenderBusy);
    }
    if !ready.wallet_keys {
        return Some(Hold::NoWalletKeys);
    }
    if ready.mode != Mode::Run {
        return Some(Hold::NotRunning(ready.mode));
    }
    let Some(trading) = ready.trading else { return Some(Hold::TradingStateUnavailable) };
    if trading.paused_actions.contains("withdrawToCore") {
        return Some(Hold::WithdrawalsPaused);
    }
    ready.dry_run.then_some(Hold::DryRun)
}

/// How many rounds both accounts can still open if the same one loses every
/// time: the first needs the margin, each further one what a round loses.
pub fn rounds_affordable(long: U256, short: U256, margin: U256, buffer: U256) -> u32 {
    let rounds = |balance: U256| {
        if balance < margin {
            return 0;
        }
        if buffer.is_zero() {
            return u32::MAX;
        }
        u32::try_from((balance - margin) / buffer).unwrap_or(u32::MAX).saturating_add(1)
    };
    rounds(long).min(rounds(short))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{account, position, trading_state};

    /// An account with nothing open.
    fn flat(available: u64) -> Holding {
        Holding { available: usd(available), locked: U256::ZERO }
    }

    /// An account whose 500 margin is in a position.
    fn hedged(available: u64) -> Holding {
        Holding { locked: usd(500), ..flat(available) }
    }

    /// 10,000 USD at 20x closed 100 bps away: 500 margin, 100 lost per round.
    fn judge(long: Holding, short: Holding) -> Funds {
        funds(&long, &short, usd(500), usd(100))
    }

    fn rebalance(from: Side, to: Side, amount_usdc: u64) -> Funds {
        Funds::Rebalance { from, to, amount_usdc }
    }

    /// One unit less than this many USD, in an account with nothing open.
    fn just_under(available: u64) -> Holding {
        Holding { available: usd(available) - U256::from(1), ..flat(0) }
    }

    #[test]
    fn enough_when_both_hold_the_margin_and_one_rounds_loss() {
        assert_eq!(judge(flat(600), flat(600)), Funds::Enough);
        assert_eq!(judge(flat(2_000), flat(600)), Funds::Enough);
        // However far apart they are: nothing is moved while both can go on.
        assert_eq!(judge(flat(600), flat(50_000)), Funds::Enough);
    }

    #[test]
    fn one_account_short_of_the_buffer_is_topped_up_from_the_other() {
        // One unit below margin + buffer is no longer enough.
        assert_eq!(judge(flat(2_000), just_under(600)), rebalance(Side::Long, Side::Short, 700));
        assert_eq!(judge(flat(2_000), flat(300)), rebalance(Side::Long, Side::Short, 850));
        assert_eq!(judge(flat(300), flat(2_000)), rebalance(Side::Short, Side::Long, 850));
    }

    #[test]
    fn the_margin_parked_in_a_position_counts_as_held() {
        // 100 free and 500 posted is the margin and one round's loss.
        assert_eq!(judge(hedged(100), hedged(100)), Funds::Enough);
        assert_eq!(judge(hedged(1_500), hedged(100)), Funds::Enough);
        let short = Holding { locked: usd(500), ..just_under(100) };
        assert_eq!(judge(hedged(1_500), short), rebalance(Side::Long, Side::Short, 700));
        // The verdict is the same with the round open, with one leg open, and with none.
        let moved = rebalance(Side::Long, Side::Short, 750);
        assert_eq!(judge(flat(2_000), flat(500)), moved);
        assert_eq!(judge(hedged(1_500), hedged(0)), moved);
        assert_eq!(judge(hedged(1_500), flat(500)), moved);
        assert_eq!(judge(flat(2_000), hedged(0)), moved);
    }

    #[test]
    fn underfunded_when_the_total_is_below_two_margins() {
        assert_eq!(judge(flat(600), flat(399)), Funds::Underfunded);
        assert_eq!(judge(flat(999), flat(0)), Funds::Underfunded);
        assert_eq!(judge(flat(0), flat(0)), Funds::Underfunded);
        // A posted margin is part of the total: a lone leg and 499.
        assert_eq!(judge(hedged(0), flat(499)), Funds::Underfunded);
        // Exactly two margins is not underfunded.
        assert_eq!(judge(flat(1_000), flat(0)), rebalance(Side::Long, Side::Short, 400));
        assert_eq!(judge(just_under(1_000), flat(0)), Funds::Underfunded);
    }

    #[test]
    fn the_amount_is_half_the_difference_in_whole_usdc_rounded_down() {
        assert_eq!(judge(flat(1_001), flat(500)), rebalance(Side::Long, Side::Short, 250));
        // 250.99… is still 250.
        assert_eq!(judge(just_under(1_002), flat(500)), rebalance(Side::Long, Side::Short, 250));
    }

    #[test]
    fn a_sender_without_a_position_keeps_its_margin_and_one_rounds_loss() {
        // Half the difference is 450, but the long account keeps 600 of its 1,000.
        assert_eq!(judge(flat(1_000), flat(100)), rebalance(Side::Long, Side::Short, 400));
        // With exactly its reserve it has nothing to send.
        assert_eq!(judge(flat(600), flat(450)), Funds::Enough);
        // What it can spare is counted in whole USDC too: 610.99… spares 10.
        assert_eq!(judge(just_under(611), flat(400)), rebalance(Side::Long, Side::Short, 10));
    }

    #[test]
    fn a_sender_with_a_position_has_posted_its_margin_and_reserves_nothing() {
        // The same 1,000 against 100, but 500 of it are posted: all that is free can go.
        assert_eq!(judge(hedged(500), flat(100)), rebalance(Side::Long, Side::Short, 450));
        // It keeps nothing back even where a flat sender could send nothing at all.
        assert_eq!(judge(hedged(100), flat(400)), rebalance(Side::Long, Side::Short, 100));
        assert_eq!(judge(flat(600), flat(400)), Funds::Enough);
    }

    #[test]
    fn less_than_ten_usdc_is_not_worth_a_transfer() {
        // Half the difference is 9.5 USDC: below the 10 the relayer takes.
        assert_eq!(judge(flat(618), flat(599)), Funds::Enough);
        assert_eq!(judge(flat(619), flat(599)), rebalance(Side::Long, Side::Short, 10));
        // What the sender can spare is 9: the same.
        assert_eq!(judge(flat(609), flat(400)), Funds::Enough);
        assert_eq!(judge(flat(610), flat(400)), rebalance(Side::Long, Side::Short, 10));
        // Also when one account is below its margin: no transfer can be made,
        // and the balance check of the open is what holds that back.
        assert_eq!(judge(flat(505), flat(495)), Funds::Enough);
        // Two accounts that are level never ask for a transfer.
        assert_eq!(judge(flat(550), flat(550)), Funds::Enough);
        assert_eq!(judge(hedged(0), hedged(0)), Funds::Enough);
    }

    #[test]
    fn levelling_once_is_enough() {
        // After a transfer the next verdict is never another one, whatever the
        // credit falls short by.
        for (long, short) in [(2_000, 300), (1_000, 0), (701, 500), (1_234, 567), (10, 5_000)] {
            let Funds::Rebalance { from, amount_usdc, .. } = judge(flat(long), flat(short)) else {
                panic!("expected a transfer for {long}/{short}")
            };
            for shortfall in [0, 1] {
                let (long, short) = if from == Side::Long {
                    (long - amount_usdc, short + amount_usdc - shortfall)
                } else {
                    (long + amount_usdc - shortfall, short - amount_usdc)
                };
                let next = judge(flat(long), flat(short));
                assert!(!matches!(next, Funds::Rebalance { .. }), "{long}/{short}: {next:?}");
            }
        }
    }

    #[test]
    fn the_verdict_has_a_name_for_the_dashboard() {
        assert_eq!(judge(flat(600), flat(600)).as_str(), "enough");
        assert_eq!(judge(flat(2_000), flat(300)).as_str(), "rebalance");
        assert_eq!(judge(flat(1), flat(1)).as_str(), "underfunded");
    }

    #[test]
    fn a_holding_is_read_from_the_accounts_live_state() {
        let open = AccountState { positions: vec![position(7, true)], ..account() };
        assert_eq!(Holding::of(&account()), flat(1_500));
        assert_eq!(Holding::of(&open), hedged(1_500));
        assert_eq!(Holding::of(&open).equity(), usd(2_000));
    }

    #[test]
    fn a_transfer_reserves_its_amount_from_the_sender_until_the_withdrawal_has_arrived() {
        let planned = Transfer::new(1, Side::Long, Side::Short, 850, 0);
        assert_eq!(reserved(&planned, Side::Long), usd(850));
        assert_eq!(reserved(&planned, Side::Short), U256::ZERO);
        // Signed and submitted: still the whole amount, whatever the balance shows.
        let withdrawing = Transfer { attempts: 1, sender_baseline: Some(usd(2_000)), ..planned.clone() };
        assert_eq!(reserved(&withdrawing, Side::Long), usd(850));
        // Once it has reached HyperCore, or cannot leave any more, nothing.
        for stage in [Stage::Send, Stage::Credit, Stage::Done, Stage::Stuck] {
            let later = Transfer { stage, ..withdrawing.clone() };
            assert_eq!(
                (reserved(&later, Side::Long), reserved(&later, Side::Short)),
                (U256::ZERO, U256::ZERO)
            );
        }
    }

    #[test]
    fn a_transfer_starts_only_when_nothing_stands_in_its_way() {
        let open = trading_state(&[]);
        let ready = Readiness {
            transfer_active: false,
            sender_busy: false,
            wallet_keys: true,
            mode: Mode::Run,
            trading: Some(&open),
            dry_run: false,
        };
        assert_eq!(hold(&ready), None);
        assert_eq!(hold(&Readiness { transfer_active: true, ..ready }), Some(Hold::TransferActive));
        assert_eq!(hold(&Readiness { sender_busy: true, ..ready }), Some(Hold::SenderBusy));
        assert_eq!(hold(&Readiness { wallet_keys: false, ..ready }), Some(Hold::NoWalletKeys));
        assert_eq!(hold(&Readiness { mode: Mode::Pause, ..ready }), Some(Hold::NotRunning(Mode::Pause)));
        assert_eq!(hold(&Readiness { mode: Mode::Flatten, ..ready }), Some(Hold::NotRunning(Mode::Flatten)));
        assert_eq!(hold(&Readiness { trading: None, ..ready }), Some(Hold::TradingStateUnavailable));
        let refusing = trading_state(&["withdrawToCore"]);
        assert_eq!(hold(&Readiness { trading: Some(&refusing), ..ready }), Some(Hold::WithdrawalsPaused));
        assert_eq!(hold(&Readiness { dry_run: true, ..ready }), Some(Hold::DryRun));
        // What holds back an open does not hold back a transfer.
        let no_trading = trading_state(&["openPosition", "closePositions"]);
        assert_eq!(hold(&Readiness { trading: Some(&no_trading), ..ready }), None);
    }

    #[test]
    fn what_stands_in_the_way_is_named_in_order() {
        let ready = Readiness {
            transfer_active: true,
            sender_busy: true,
            wallet_keys: false,
            mode: Mode::Pause,
            trading: None,
            dry_run: true,
        };
        // A transfer that is under way comes first: it is what the operator is shown.
        assert_eq!(hold(&ready), Some(Hold::TransferActive));
        // A dry run names what would stop a real run before it names itself.
        assert_eq!(
            hold(&Readiness { transfer_active: false, sender_busy: false, ..ready }),
            Some(Hold::NoWalletKeys)
        );
        // Some pass by themselves; the others are worth telling the operator.
        for passing in [Hold::TransferActive, Hold::SenderBusy, Hold::TradingStateUnavailable] {
            assert!(passing.is_passing(), "{passing}");
        }
        for lasting in
            [Hold::NoWalletKeys, Hold::NotRunning(Mode::Pause), Hold::WithdrawalsPaused, Hold::DryRun]
        {
            assert!(!lasting.is_passing(), "{lasting}");
        }
        assert_eq!(Hold::NotRunning(Mode::Pause).to_string(), "the bot is in pause mode");
        assert_eq!(Hold::WithdrawalsPaused.to_string(), "the relayer has withdrawToCore paused");
        assert_eq!(Hold::DryRun.to_string(), "dry run");
    }

    #[test]
    fn affordable_rounds_count_the_poorer_account() {
        let rounds = |long: u64, short: u64| rounds_affordable(usd(long), usd(short), usd(500), usd(100));
        assert_eq!(rounds(499, 5_000), 0);
        assert_eq!(rounds(500, 5_000), 1);
        assert_eq!(rounds(599, 5_000), 1);
        assert_eq!(rounds(600, 5_000), 2);
        assert_eq!(rounds(2_000, 1_000), 6);
        assert_eq!(rounds_affordable(usd(500), usd(500), usd(500), U256::ZERO), u32::MAX);
    }
}
