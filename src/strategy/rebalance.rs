//! Whether the two accounts can fund the next round, as one pure function.
//!
//! Every round moves money from the losing account to the winning one, so the
//! balances drift apart until one side can no longer post its margin. This
//! says when that calls for moving funds back, and how much.

use alloy::primitives::U256;

use super::decide::Side;
use crate::protocol::deployment::usd;

/// The relayer takes no smaller withdrawal and sweeps no smaller deposit.
pub const MIN_TRANSFER_USDC: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Funds {
    /// Nothing to move. Either both accounts hold the margin and one round's
    /// loss, or they are so close to each other that moving the difference is
    /// not worth a transfer. Whether each account can post its margin is still
    /// checked on its own.
    Enough,
    /// Move this many whole USDC from the richer account to the poorer one.
    Rebalance { from: Side, to: Side, amount_usdc: u64 },
    /// Together the accounts hold less than two margins. Moving funds cannot
    /// help; they need a deposit.
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

/// Judges the two accounts' available balances against what a round needs.
///
/// `margin` is what each leg posts; `buffer` is what the losing leg gives up
/// in one round. All four amounts are 18-decimal USD.
pub fn funds(long: U256, short: U256, margin: U256, buffer: U256) -> Funds {
    let comfortable = margin + buffer;
    if long >= comfortable && short >= comfortable {
        return Funds::Enough;
    }
    if long + short < margin * U256::from(2) {
        return Funds::Underfunded;
    }
    let (from, to, richer, poorer) = if long >= short {
        (Side::Long, Side::Short, long, short)
    } else {
        (Side::Short, Side::Long, short, long)
    };
    // Half the difference levels them. Whole USDC, rounded down, so the richer
    // account never ends up the poorer one.
    let amount_usdc = u64::try_from((richer - poorer) / U256::from(2) / usd(1)).unwrap_or(u64::MAX);
    if amount_usdc < MIN_TRANSFER_USDC {
        return Funds::Enough;
    }
    Funds::Rebalance { from, to, amount_usdc }
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

    /// 10,000 USD at 20x closed 100 bps away: 500 margin, 100 lost per round.
    fn judge(long: u64, short: u64) -> Funds {
        funds(usd(long), usd(short), usd(500), usd(100))
    }

    fn rebalance(from: Side, to: Side, amount_usdc: u64) -> Funds {
        Funds::Rebalance { from, to, amount_usdc }
    }

    #[test]
    fn enough_when_both_hold_the_margin_and_one_rounds_loss() {
        assert_eq!(judge(600, 600), Funds::Enough);
        assert_eq!(judge(2_000, 600), Funds::Enough);
        // However far apart they are: nothing is moved while both can go on.
        assert_eq!(judge(600, 50_000), Funds::Enough);
    }

    #[test]
    fn one_account_short_of_the_buffer_is_topped_up_from_the_other() {
        // One unit below margin + buffer is no longer enough.
        let almost = usd(600) - U256::from(1);
        assert_eq!(funds(usd(2_000), almost, usd(500), usd(100)), rebalance(Side::Long, Side::Short, 700));
        assert_eq!(judge(2_000, 300), rebalance(Side::Long, Side::Short, 850));
        assert_eq!(judge(300, 2_000), rebalance(Side::Short, Side::Long, 850));
    }

    #[test]
    fn underfunded_when_the_total_is_below_two_margins() {
        assert_eq!(judge(600, 399), Funds::Underfunded);
        assert_eq!(judge(999, 0), Funds::Underfunded);
        assert_eq!(judge(0, 0), Funds::Underfunded);
        // Exactly two margins is not underfunded: after levelling, both can open once.
        assert_eq!(judge(1_000, 0), rebalance(Side::Long, Side::Short, 500));
        let one_short = usd(1_000) - U256::from(1);
        assert_eq!(funds(one_short, U256::ZERO, usd(500), usd(100)), Funds::Underfunded);
    }

    #[test]
    fn the_amount_is_half_the_difference_in_whole_usdc_rounded_down() {
        assert_eq!(judge(701, 500), rebalance(Side::Long, Side::Short, 100));
        // 100.99… is still 100.
        let richer = usd(702) - U256::from(1);
        assert_eq!(funds(richer, usd(500), usd(500), usd(100)), rebalance(Side::Long, Side::Short, 100));
        // The sender keeps at least half the total, so at least its own margin.
        let Funds::Rebalance { amount_usdc, .. } = judge(1_000, 0) else { panic!("expected a transfer") };
        assert!(usd(1_000) - usd(amount_usdc) >= usd(500));
    }

    #[test]
    fn a_difference_too_small_to_move_is_left_to_the_per_account_check() {
        // Half the difference is 9.5 USDC: below the 10 the relayer takes.
        assert_eq!(judge(519, 500), Funds::Enough);
        assert_eq!(judge(520, 500), rebalance(Side::Long, Side::Short, 10));
        // Also when one account is below its margin: no transfer can be made, so
        // the balance check that follows is what holds the open back.
        assert_eq!(judge(505, 495), Funds::Enough);
        // Two accounts that are level never ask for a transfer.
        assert_eq!(judge(550, 550), Funds::Enough);
    }

    #[test]
    fn levelling_once_is_enough() {
        // After a transfer the accounts differ by less than 2 USDC plus what the
        // credit may fall short, so the next verdict is never another transfer.
        for (long, short) in [(2_000, 300), (1_000, 0), (701, 500), (1_234, 567), (10, 5_000)] {
            let Funds::Rebalance { from, amount_usdc, .. } = judge(long, short) else {
                panic!("expected a transfer for {long}/{short}")
            };
            for shortfall in [0, 1] {
                let (long, short) = if from == Side::Long {
                    (long - amount_usdc, short + amount_usdc - shortfall)
                } else {
                    (long + amount_usdc - shortfall, short - amount_usdc)
                };
                assert!(!matches!(judge(long, short), Funds::Rebalance { .. }), "{long}/{short}");
            }
        }
    }

    #[test]
    fn the_verdict_has_a_name_for_the_dashboard() {
        assert_eq!(judge(600, 600).as_str(), "enough");
        assert_eq!(judge(2_000, 300).as_str(), "rebalance");
        assert_eq!(judge(1, 1).as_str(), "underfunded");
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
