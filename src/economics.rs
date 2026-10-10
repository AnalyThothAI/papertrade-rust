//! What a round costs and what it mints, by the protocol's published rules.
//!
//! Everything here is a pure function of wire values: prices in wire scale,
//! amounts and rates as 18-decimal fixed point. No I/O, no clock, no floats.
//!
//! The results are an estimate of what the contract will do at settlement, not
//! a promise. [`close_value`] and [`payout`] follow the official frontend's
//! valuation module digit for digit; the mint rules follow the documentation.

use alloy::primitives::{I256, U256};

use crate::protocol::chain::FeeRates;
use crate::protocol::wire::{Impact, MintCurve, Trade};

/// One, in 18-decimal fixed point.
pub const ONE: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// A winning close is measured as if the exit were `entry / 50000` (0.2 bps)
/// less favourable; a smaller win is no win.
const DEADBAND_DIVISOR: u64 = 50_000;

/// `a × b ÷ divisor`, rounded down. `None` on overflow or a zero divisor.
fn mul_div(a: U256, b: U256, divisor: U256) -> Option<U256> {
    a.checked_mul(b)?.checked_div(divisor)
}

fn signed(amount: U256) -> Option<I256> {
    I256::try_from(amount).ok()
}

fn negative(amount: U256) -> Option<I256> {
    signed(amount)?.checked_neg()
}

/// One leg of a round, open or planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    pub is_long: bool,
    /// Wire scale.
    pub entry: u64,
    pub margin: U256,
    pub leverage: u32,
    /// Where the protocol liquidates it, wire scale. Known for an open position.
    pub bust: Option<u64>,
}

impl Leg {
    /// The two legs a round opens: same entry, margin and leverage, opposite directions.
    pub fn pair(entry: u64, margin: U256, leverage: u32) -> [Leg; 2] {
        [true, false].map(|is_long| Leg { is_long, entry, margin, leverage, bust: None })
    }

    fn is_liquidated_at(&self, price: u64) -> bool {
        self.bust.is_some_and(|bust| if self.is_long { price <= bust } else { price >= bust })
    }
}

/// A position's result at a price, before any fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Valuation {
    /// Margin × leverage × the move in the position's favour, uncapped.
    pub raw: I256,
    /// What the protocol settles: a gain after the deadband and the impact
    /// haircut, a loss capped at the margin.
    pub adjusted: I256,
}

/// Values a position closed at `exit`, as the official frontend does (its
/// function mirrors the contract's `applyMarketImpact`). The order of the
/// integer divisions is the frontend's, so the results agree to the last digit.
///
/// `None` where the frontend refuses the inputs (no entry price, no leverage,
/// a price beyond the contract's 64 bits) or the arithmetic leaves 256 bits.
pub fn close_value(leg: &Leg, exit: u64, impact: &Impact) -> Option<Valuation> {
    if leg.entry == 0 || leg.leverage == 0 {
        return None;
    }
    let deadband = leg.entry / DEADBAND_DIVISOR;
    // The contract keeps prices in 64 bits, the shifted one included.
    (if leg.is_long { leg.entry } else { exit }).checked_add(deadband)?;

    let entry = U256::from(leg.entry);
    let notional = leg.margin.checked_mul(U256::from(leg.leverage))?;
    let distance = exit.abs_diff(leg.entry);
    let in_favour = if leg.is_long { exit > leg.entry } else { exit < leg.entry };
    if !in_favour {
        let loss = mul_div(notional, U256::from(distance), entry)?;
        return Some(Valuation { raw: negative(loss)?, adjusted: negative(loss.min(leg.margin))? });
    }

    // The exit is moved against a winner by the deadband; a smaller move counts as none.
    let distance = U256::from(distance.saturating_sub(deadband));
    let raw = mul_div(notional, distance, entry)?;
    let gain = |adjusted: U256| Some(Valuation { raw: signed(raw)?, adjusted: signed(adjusted)? });
    if raw.is_zero() {
        return gain(U256::ZERO);
    }

    // scale = (1 − base rate) ÷ (1 + 1 ÷ (move × rate multiplier)
    //                              + reference notional ÷ (10⁶ × move × position multiplier))
    let moved = mul_div(distance, ONE, entry)?;
    let rate_term = mul_div(moved, impact.rate_multiplier, ONE)?;
    let size_term = mul_div(U256::from(1_000_000).checked_mul(moved)?, impact.position_multiplier, ONE)?;
    if rate_term.is_zero() || size_term.is_zero() {
        return gain(U256::ZERO);
    }
    let one_squared = ONE.checked_mul(ONE)?;
    let by_rate = one_squared / rate_term;
    let by_size = mul_div(impact.reference_notional, ONE, size_term)?;
    if by_rate > one_squared || by_size > one_squared {
        return gain(U256::ZERO);
    }
    let kept = ONE.checked_sub(impact.base_rate)?;
    let scale = mul_div(kept, ONE, ONE.checked_add(by_rate)?.checked_add(by_size)?)?;
    gain(mul_div(raw, scale, ONE)?)
}

/// What a close credits or debits the trader beyond the returned margin: a gain
/// pays the win fee, a loss pays nothing on top.
pub fn payout(adjusted: I256, win_fee_rate: U256) -> Option<I256> {
    if !adjusted.is_positive() {
        return Some(adjusted);
    }
    let gain = adjusted.unsigned_abs();
    signed(gain.checked_sub(mul_div(gain, win_fee_rate, ONE)?)?)
}

/// PAPER minted per USD of mint basis right now, 18 decimals: flat while the
/// tracked LP is below the cliff (or underwater), then
/// `rate × (decay ÷ (decay + tail progress))²`.
pub fn mint_rate(curve: &MintCurve) -> Option<U256> {
    let below_cliff = curve.tracked_lp.is_negative() || curve.tracked_lp.unsigned_abs() < curve.cliff;
    if below_cliff {
        return Some(curve.rate);
    }
    let share = mul_div(curve.decay, ONE, curve.decay.checked_add(curve.tail_progress)?)?;
    mul_div(mul_div(curve.rate, share, ONE)?, share, ONE)
}

/// How a position lost money.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    /// Closed at a loss of this much.
    Closed(U256),
    /// Liquidated: the whole margin is gone.
    Liquidated { margin: U256 },
}

/// The part of a loss PAPER is minted on. A close while the payout queue is
/// empty mints on the loss less the loss fee, which the LP's side gives up; a
/// close while winners are queued pays no fee and mints on all of it; a
/// liquidation always mints on the full margin.
pub fn mint_basis(loss: Loss, queue_empty: bool, loss_fee_rate: U256) -> Option<U256> {
    match loss {
        Loss::Liquidated { margin } => Some(margin),
        Loss::Closed(amount) if queue_empty => amount.checked_sub(mul_div(amount, loss_fee_rate, ONE)?),
        Loss::Closed(amount) => Some(amount),
    }
}

/// What the protocol currently charges and mints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Terms {
    pub impact: Impact,
    pub fees: FeeRates,
    /// PAPER per USD of mint basis, see [`mint_rate`].
    pub mint_rate: U256,
    /// The LP owes nobody, see [`mint_basis`].
    pub queue_empty: bool,
}

/// Two legs closed at one price. Amounts are 18-decimal USD (or PAPER).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Round {
    /// What the losing leg loses.
    pub loss: U256,
    /// What the winning leg is paid after the impact haircut and the win fee.
    pub win: U256,
    /// `win − loss`. Negative: what the round costs.
    pub net: I256,
    pub paper: U256,
    /// USD spent per PAPER minted, 18 decimals. `None` when no PAPER is minted.
    pub usd_per_paper: Option<I256>,
}

/// The result of closing every leg at `exit`. A leg at or beyond its bust
/// price, or one whose loss has eaten its margin, counts as liquidated.
pub fn round(legs: &[Leg], exit: u64, terms: &Terms) -> Option<Round> {
    let (mut loss, mut win, mut paper) = (U256::ZERO, U256::ZERO, U256::ZERO);
    for leg in legs {
        let result = payout(close_value(leg, exit, &terms.impact)?.adjusted, terms.fees.win)?;
        let liquidated = leg.is_liquidated_at(exit);
        if result.is_positive() && !liquidated {
            win = win.checked_add(result.unsigned_abs())?;
            continue;
        }
        let lost = result.unsigned_abs();
        let (lost, how) = if liquidated || (lost >= leg.margin && !lost.is_zero()) {
            (leg.margin, Loss::Liquidated { margin: leg.margin })
        } else {
            (lost, Loss::Closed(lost))
        };
        loss = loss.checked_add(lost)?;
        let basis = mint_basis(how, terms.queue_empty, terms.fees.loss)?;
        paper = paper.checked_add(mul_div(basis, terms.mint_rate, ONE)?)?;
    }
    let net = signed(win)?.checked_sub(signed(loss)?)?;
    let usd_per_paper = if paper.is_zero() {
        None
    } else {
        // Rounded towards zero, like every other division here.
        let magnitude = signed(mul_div(net.unsigned_abs(), ONE, paper)?)?;
        Some(if net.is_negative() { magnitude } else { magnitude.checked_neg()? })
    };
    Some(Round { loss, win, net, paper, usd_per_paper })
}

/// How far the mid has to be from the entry for a hedged round to be closed:
/// the smallest distance that is `close_bps` of the entry, which is where
/// `decide` stops waiting.
pub fn close_distance(entry: u64, close_bps: u32) -> Option<u64> {
    u64::try_from((u128::from(entry) * u128::from(close_bps)).div_ceil(10_000)).ok()
}

/// The price the close distance of a pair is measured from: the mean of the
/// two entries, rounded down as `decide` does.
pub fn mean_entry(legs: &[Leg; 2]) -> u64 {
    legs[0].entry.midpoint(legs[1].entry)
}

/// What a pair of legs is expected to cost and mint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// Closed at the close distance above the mean entry. For two equal legs a
    /// move down by the same distance gives the same figures with the legs'
    /// roles swapped, so one direction is enough.
    pub at_target: Round,
    /// Closed at the current mid, when one was given.
    pub now: Option<Round>,
}

/// `legs` is the open pair, or the pair the next round would open; `mid` is
/// given only for an open pair.
pub fn estimate(legs: &[Leg; 2], close_bps: u32, mid: Option<u64>, terms: &Terms) -> Option<Estimate> {
    let entry = mean_entry(legs);
    let target = entry.checked_add(close_distance(entry, close_bps)?)?;
    let now = match mid {
        Some(mid) => Some(round(legs, mid, terms)?),
        None => None,
    };
    Some(Estimate { at_target: round(legs, target, terms)?, now })
}

/// A settled position's net result for the trader, as the official frontend
/// books it: a liquidation costs the whole margin whatever PnL the record
/// carries; a close is the adjusted PnL less the fee the trader paid.
pub fn net_result(trade: &Trade) -> I256 {
    let signed = |amount: U256| I256::try_from(amount).unwrap_or(I256::MAX);
    if trade.is_liquidation() {
        return signed(trade.margin_raw.0).wrapping_neg();
    }
    trade.adjusted_pnl_raw.0.saturating_sub(signed(trade.user_paid_fee_raw.0))
}

#[cfg(test)]
mod tests {
    use alloy::primitives::B256;
    use serde::Deserialize;

    use super::*;
    use crate::protocol::account::AccountState;
    use crate::protocol::deployment::usd;
    use crate::protocol::wire::{Dec, Position, ProtocolSummary, SignedDec};
    use crate::status::{format_amount, format_signed_amount};
    use crate::strategy::decide::{
        CloseReason, Decision, LegView, Market, Mode, StrategyParams, View, Wait, decide,
    };
    use crate::strategy::inflight::Awaiting;
    use crate::testing::{KEY, NOW, PROTOCOL_SUMMARY, account, position, trading_state};

    const ENTRY: u64 = 800_000;

    /// BTC's parameters as production lists them.
    fn btc() -> Impact {
        trading_state(&[]).instrument("BTC").unwrap().impact.unwrap()
    }

    fn percent(points: u64) -> U256 {
        ONE * U256::from(points) / U256::from(100)
    }

    fn terms() -> Terms {
        Terms {
            impact: btc(),
            fees: FeeRates { win: percent(2), loss: percent(2), liquidation: percent(2) },
            mint_rate: usd(100),
            queue_empty: true,
        }
    }

    /// The legs of the default round: 10,000 USD at 20x, entered at 80,000.0.
    fn legs() -> [Leg; 2] {
        Leg::pair(ENTRY, usd(500), 20)
    }

    fn long() -> Leg {
        legs()[0]
    }

    fn short() -> Leg {
        legs()[1]
    }

    fn adjusted(leg: &Leg, exit: u64) -> I256 {
        close_value(leg, exit, &btc()).unwrap().adjusted
    }

    fn usd_signed(text: &str) -> I256 {
        I256::from_dec_str(text).unwrap()
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Golden {
        name: String,
        is_long: bool,
        entry: Dec,
        margin: Dec,
        leverage: u32,
        mid: Dec,
        rate_multiplier: Dec,
        position_multiplier: Dec,
        base_rate: Dec,
        reference_notional: Dec,
        win_fee: Dec,
        raw: Option<SignedDec>,
        adjusted: Option<SignedDec>,
        payout: Option<SignedDec>,
        error: Option<String>,
    }

    #[derive(Deserialize)]
    struct GoldenFile {
        cases: Vec<Golden>,
    }

    #[test]
    fn valuation_agrees_with_the_official_frontend_to_the_last_digit() {
        let file: GoldenFile =
            serde_json::from_str(include_str!("../tests/fixtures/valuation-golden.json")).unwrap();
        assert!(file.cases.len() > 90);
        let (mut gains, mut losses, mut refused) = (0, 0, 0);
        for case in file.cases {
            let name = &case.name;
            let impact = Impact {
                base_rate: case.base_rate.0,
                rate_multiplier: case.rate_multiplier.0,
                reference_notional: case.reference_notional.0,
                position_multiplier: case.position_multiplier.0,
            };
            let valued = u64::try_from(case.entry.0).ok().zip(u64::try_from(case.mid.0).ok()).and_then(
                |(entry, mid)| {
                    let leg = Leg {
                        is_long: case.is_long,
                        entry,
                        margin: case.margin.0,
                        leverage: case.leverage,
                        bust: None,
                    };
                    close_value(&leg, mid, &impact)
                },
            );
            if let Some(error) = &case.error {
                assert_eq!(valued, None, "{name}: the frontend refuses it ({error})");
                refused += 1;
                continue;
            }
            let expected = Valuation { raw: case.raw.unwrap().0, adjusted: case.adjusted.unwrap().0 };
            assert_eq!(valued, Some(expected), "{name}");
            assert_eq!(payout(expected.adjusted, case.win_fee.0), Some(case.payout.unwrap().0), "{name}");
            if expected.adjusted.is_positive() {
                gains += 1;
            } else if expected.adjusted.is_negative() {
                losses += 1;
            }
        }
        // The table really covers winners, losers and refused inputs.
        assert!(
            gains >= 20 && losses >= 20 && refused == 3,
            "{gains} gains, {losses} losses, {refused} refused"
        );
    }

    #[test]
    fn a_one_percent_win_on_btc_keeps_about_88_percent_and_a_tenth_of_a_percent_about_74() {
        // The documentation's figures: the share of the gain a 10,000 USD position
        // would have made without the deadband or the haircut, before the win fee.
        let one_percent = adjusted(&long(), ENTRY + 8_000);
        assert_eq!(format_signed_amount(one_percent), "88.14"); // of 100
        let a_tenth = adjusted(&long(), ENTRY + 800);
        assert_eq!(format_signed_amount(a_tenth), "7.39"); // of 10
        // The same moves the other way pay the short exactly the same.
        assert_eq!(adjusted(&short(), ENTRY - 8_000), one_percent);
        assert_eq!(adjusted(&short(), ENTRY - 800), a_tenth);
    }

    #[test]
    fn no_win_keeps_more_than_one_less_the_base_rate() {
        // A 4.9% move, just short of the bust, on 10,000 USD: the raw gain is 489.80.
        let valuation = close_value(&long(), ENTRY + 39_200, &btc()).unwrap();
        assert_eq!(format_signed_amount(valuation.raw), "489.80");
        let kept = valuation.adjusted.unsigned_abs() * ONE / valuation.raw.unsigned_abs();
        assert!(kept > percent(89) && kept < percent(90), "{kept}");
    }

    #[test]
    fn a_win_inside_the_deadband_pays_nothing_and_the_deadband_comes_off_a_larger_one() {
        // 800,000 / 50,000 = 16 price units.
        let nothing = Valuation { raw: I256::ZERO, adjusted: I256::ZERO };
        for distance in [1, 15, 16] {
            assert_eq!(close_value(&long(), ENTRY + distance, &btc()), Some(nothing), "long +{distance}");
            assert_eq!(close_value(&short(), ENTRY - distance, &btc()), Some(nothing), "short -{distance}");
        }
        // One unit beyond it is a gain measured over that one unit: 10,000 × 1 / 800,000.
        for (leg, exit) in [(long(), ENTRY + 17), (short(), ENTRY - 17)] {
            let valuation = close_value(&leg, exit, &btc()).unwrap();
            assert_eq!(valuation.raw, usd_signed("12500000000000000"));
            assert!(valuation.adjusted.is_positive() && valuation.adjusted < valuation.raw);
        }
    }

    #[test]
    fn a_loss_gets_no_deadband_and_no_haircut() {
        for (leg, exit) in [(long(), ENTRY - 1), (short(), ENTRY + 1)] {
            let lost = usd_signed("-12500000000000000");
            assert_eq!(close_value(&leg, exit, &btc()), Some(Valuation { raw: lost, adjusted: lost }));
        }
        assert_eq!(format_signed_amount(adjusted(&short(), ENTRY + 8_000)), "-100.00");
        assert_eq!(adjusted(&long(), ENTRY), I256::ZERO);
    }

    #[test]
    fn a_loss_is_capped_at_the_margin() {
        let margin = -signed(usd(500)).unwrap();
        // 5% against a 20x position is exactly the margin.
        assert_eq!(adjusted(&long(), ENTRY - 40_000), margin);
        let beyond = close_value(&long(), ENTRY - 40_001, &btc()).unwrap();
        assert_eq!(beyond.adjusted, margin);
        assert!(beyond.raw < margin);
        assert_eq!(adjusted(&short(), 2 * ENTRY), margin);
        assert_eq!(adjusted(&long(), 0), margin);
    }

    #[test]
    fn inputs_the_contract_could_not_hold_are_refused() {
        assert_eq!(close_value(&Leg { entry: 0, ..long() }, ENTRY, &btc()), None);
        assert_eq!(close_value(&Leg { leverage: 0, ..long() }, ENTRY, &btc()), None);
        assert_eq!(close_value(&short(), u64::MAX - 5, &btc()), None);
        assert_eq!(close_value(&Leg { margin: U256::MAX, ..long() }, ENTRY + 8_000, &btc()), None);
        // A base rate above one would turn a gain into a loss.
        let impact = Impact { base_rate: ONE + U256::from(1), ..btc() };
        assert_eq!(close_value(&long(), ENTRY + 8_000, &impact), None);
    }

    #[test]
    fn the_win_fee_comes_off_gains_only() {
        let gain = signed(usd(88)).unwrap();
        assert_eq!(payout(gain, percent(2)), signed(usd(8_624) / U256::from(100)));
        assert_eq!(payout(gain, U256::ZERO), Some(gain));
        assert_eq!(payout(-gain, percent(2)), Some(-gain));
        assert_eq!(payout(I256::ZERO, percent(2)), Some(I256::ZERO));
        assert_eq!(payout(gain, ONE + U256::from(1)), None);
    }

    fn curve() -> MintCurve {
        serde_json::from_str::<ProtocolSummary>(PROTOCOL_SUMMARY).unwrap().paper
    }

    #[test]
    fn the_mint_rate_is_flat_below_the_cliff() {
        // Production today: nothing tracked yet, 100 PAPER per USD.
        assert_eq!(mint_rate(&curve()), Some(usd(100)));
        // However far the tail has ever advanced, below the cliff the rate is the flat one.
        let advanced = MintCurve { tail_progress: usd(120_000_000), ..curve() };
        let just_below = signed(usd(2_000_000)).unwrap() - I256::ONE;
        assert_eq!(mint_rate(&MintCurve { tracked_lp: just_below, ..advanced }), Some(usd(100)));
        let underwater = -signed(usd(5_000_000)).unwrap();
        assert_eq!(mint_rate(&MintCurve { tracked_lp: underwater, ..advanced }), Some(usd(100)));
    }

    #[test]
    fn from_the_cliff_on_the_mint_rate_falls_with_the_square_of_the_tail() {
        let at_cliff = |tail_progress: U256| MintCurve {
            tracked_lp: signed(usd(2_000_000)).unwrap(),
            tail_progress,
            ..curve()
        };
        // At the cliff with no tail yet the formula still gives the full rate.
        assert_eq!(mint_rate(&at_cliff(U256::ZERO)), Some(usd(100)));
        // Tail progress equal to the decay scale halves the base: a quarter of the rate.
        assert_eq!(mint_rate(&at_cliff(usd(120_000_000))), Some(usd(25)));
        // The documentation's example: 100M past the cliff is 100 × (120/220)² ≈ 30.
        assert_eq!(format_amount(mint_rate(&at_cliff(usd(100_000_000))).unwrap()), "29.75");
        // Three times the decay scale: one sixteenth.
        assert_eq!(mint_rate(&at_cliff(usd(360_000_000))), Some(usd(625) / U256::from(100)));
        // And being far beyond the cliff changes nothing by itself: only the tail counts.
        let far = MintCurve { tracked_lp: signed(usd(900_000_000)).unwrap(), ..at_cliff(usd(120_000_000)) };
        assert_eq!(mint_rate(&far), Some(usd(25)));
    }

    #[test]
    fn a_mint_curve_that_cannot_be_evaluated_gives_no_rate() {
        let broken = MintCurve {
            tracked_lp: signed(usd(2_000_000)).unwrap(),
            decay: U256::ZERO,
            tail_progress: U256::ZERO,
            ..curve()
        };
        assert_eq!(mint_rate(&broken), None);
    }

    #[test]
    fn the_mint_basis_depends_on_how_the_position_lost() {
        let fee = percent(2);
        // Queue empty: the 2% loss fee comes out of the basis.
        assert_eq!(mint_basis(Loss::Closed(usd(100)), true, fee), Some(usd(98)));
        // Winners are queued: no fee, the whole loss counts.
        assert_eq!(mint_basis(Loss::Closed(usd(100)), false, fee), Some(usd(100)));
        // A liquidation mints on the full margin either way.
        for queue_empty in [true, false] {
            assert_eq!(mint_basis(Loss::Liquidated { margin: usd(500) }, queue_empty, fee), Some(usd(500)));
        }
        assert_eq!(mint_basis(Loss::Closed(U256::ZERO), true, fee), Some(U256::ZERO));
        assert_eq!(mint_basis(Loss::Closed(usd(100)), true, ONE + U256::from(1)), None);
    }

    #[test]
    fn a_round_at_the_default_target_costs_about_fourteen_dollars_for_9800_paper() {
        // 10,000 USD a leg, 100 bps, 2% win fee, 2% loss fee, 100 PAPER per USD.
        let round = round(&legs(), ENTRY + 8_000, &terms()).unwrap();
        assert_eq!(round.loss, usd(100));
        // 99.80 raw after the deadband, 88.146… after the haircut, less 2%.
        assert_eq!(round.win, U256::from(86_383_978_384_210_530_180u128));
        assert_eq!(round.net, usd_signed("-13616021615789469820"));
        assert_eq!(round.net, signed(round.win).unwrap() - signed(round.loss).unwrap());
        // The loser mints on 98 of its 100.
        assert_eq!(round.paper, usd(9_800));
        // 13.616… / 9,800.
        assert_eq!(round.usd_per_paper, Some(usd_signed("1389389960794843")));

        // A fall by the same distance swaps the roles and changes nothing else.
        assert_eq!(super::round(&legs(), ENTRY - 8_000, &terms()), Some(round));
    }

    #[test]
    fn a_queued_payout_and_a_decayed_rate_change_what_the_same_loss_mints() {
        let queued = Terms { queue_empty: false, ..terms() };
        let round_queued = round(&legs(), ENTRY + 8_000, &queued).unwrap();
        assert_eq!(round_queued.paper, usd(10_000));
        let decayed = Terms { mint_rate: usd(25), ..terms() };
        let round_decayed = round(&legs(), ENTRY + 8_000, &decayed).unwrap();
        assert_eq!(round_decayed.paper, usd(2_450));
        // The money side is the same in all three.
        let plain = round(&legs(), ENTRY + 8_000, &terms()).unwrap();
        assert_eq!((round_queued.net, round_decayed.net), (plain.net, plain.net));
        assert!(round_decayed.usd_per_paper > plain.usd_per_paper);
    }

    #[test]
    fn a_round_closed_where_it_was_opened_costs_and_mints_nothing() {
        let round = round(&legs(), ENTRY, &terms()).unwrap();
        assert_eq!(
            (round.loss, round.win, round.net, round.paper),
            (U256::ZERO, U256::ZERO, I256::ZERO, U256::ZERO)
        );
        assert_eq!(round.usd_per_paper, None);
    }

    #[test]
    fn a_leg_beyond_its_bust_price_is_counted_as_liquidated() {
        // The simulator's and production's busts sit a little before the whole margin is lost.
        let [long, short] = legs();
        let open = [Leg { bust: Some(760_400), ..long }, Leg { bust: Some(839_600), ..short }];
        // Just before the short's bust: a close, losing 494.98 and minting on 98% of it.
        let before = round(&open, 839_599, &terms()).unwrap();
        assert_eq!(format_amount(before.loss), "494.98");
        assert_eq!(before.paper, mul_div(before.loss, percent(98), ONE).unwrap() * U256::from(100));
        // At the bust: the whole margin is gone and all of it mints.
        let at = round(&open, 839_600, &terms()).unwrap();
        assert_eq!((at.loss, at.paper), (usd(500), usd(50_000)));
        assert!(at.win > before.win);
        // Planned legs carry no bust price; a loss of the whole margin still means liquidation.
        let planned = round(&legs(), ENTRY + 40_000, &terms()).unwrap();
        assert_eq!((planned.loss, planned.paper), (usd(500), usd(50_000)));
    }

    #[test]
    fn two_legs_entered_apart_can_both_lose() {
        // The long got in at 80,010.0 and the short at 79,990.0; the mid is in between.
        let [long, short] = legs();
        let apart = [Leg { entry: ENTRY + 100, ..long }, Leg { entry: ENTRY - 100, ..short }];
        assert_eq!(mean_entry(&apart), ENTRY);
        let round = round(&apart, ENTRY, &terms()).unwrap();
        assert_eq!(round.win, U256::ZERO);
        assert_eq!(format_amount(round.loss), "2.50");
        assert_eq!(format_signed_amount(round.net), "-2.50");
        assert_eq!(format_amount(round.paper), "245.00");
    }

    fn leg_view(account: &AccountState) -> LegView<'_> {
        LegView {
            account: Some(account),
            session_key: KEY,
            awaiting: Awaiting::Nothing,
            reserved: U256::ZERO,
        }
    }

    #[test]
    fn the_close_distance_is_where_decide_stops_waiting() {
        assert_eq!(close_distance(ENTRY, 100), Some(8_000));
        // 1% of 827,935 is 8,279.35: the first whole distance that reaches it is 8,280.
        assert_eq!(close_distance(827_935, 100), Some(8_280));
        assert_eq!(close_distance(827_935, 1), Some(83));
        assert_eq!(close_distance(u64::MAX, 20_000), None);

        let market = Market { id: 0, symbol: "BTC".into() };
        let trading = trading_state(&[]);
        for (entry, close_bps) in [(ENTRY, 100), (827_935, 100), (827_935, 37), (249_505, 250)] {
            let distance = close_distance(entry, close_bps).unwrap();
            let held = |is_long| {
                let mut account = account();
                account.positions = vec![Position { entry_price: entry, ..position(7, is_long) }];
                account
            };
            let (long, short) = (held(true), held(false));
            let params =
                StrategyParams { notional: usd(10_000), leverage: 20, close_bps, require_empty_queue: true };
            let decided = |mid: u64| {
                let view = View {
                    now_seconds: NOW,
                    mode: Mode::Run,
                    long: leg_view(&long),
                    short: leg_view(&short),
                    trading: Some(&trading),
                    queue_total: Some(U256::ZERO),
                    mid: Some(mid),
                    may_open: true,
                };
                decide(&view, &params, &market)
            };
            for inside in [entry + distance - 1, entry - distance + 1] {
                assert_eq!(decided(inside), Decision::Wait(Wait::InsideTarget), "{entry} {close_bps}");
            }
            for reached in [entry + distance, entry - distance] {
                assert!(
                    matches!(decided(reached), Decision::Close { reason: CloseReason::Target, .. }),
                    "{entry} {close_bps}"
                );
            }
        }
    }

    #[test]
    fn the_estimate_gives_the_target_and_the_current_price() {
        let flat = estimate(&legs(), 100, None, &terms()).unwrap();
        assert_eq!(flat.at_target, round(&legs(), ENTRY + 8_000, &terms()).unwrap());
        assert_eq!(flat.now, None);

        // Half way to the target the loser is down 50 and has minted on 49.
        let hedged = estimate(&legs(), 100, Some(ENTRY + 4_000), &terms()).unwrap();
        assert_eq!(hedged.at_target, flat.at_target);
        let now = hedged.now.unwrap();
        assert_eq!((now.loss, now.paper), (usd(50), usd(4_900)));
        // A smaller move keeps less of the gain, so half the distance costs more per PAPER.
        assert!(now.usd_per_paper > flat.at_target.usd_per_paper);

        // Without the market's parameters there is nothing to say.
        let unreadable = Terms { impact: Impact { base_rate: ONE + U256::from(1), ..btc() }, ..terms() };
        assert_eq!(estimate(&legs(), 100, None, &unreadable), None);
    }

    fn trade(kind: &str, adjusted: I256, fee: U256) -> Trade {
        Trade {
            timestamp_ms: 1_791_700_000_000,
            tx_hash: B256::with_last_byte(1),
            position_id: Dec(U256::from(7)),
            symbol: "BTC".into(),
            price_scale_raw: Dec(U256::from(10)),
            is_long: true,
            entry_price_raw: Dec(U256::from(827_935)),
            margin_raw: Dec(usd(500)),
            leverage: 20,
            kind: kind.into(),
            exit_price_raw: Dec(U256::from(819_656)),
            adjusted_pnl_raw: SignedDec(adjusted),
            user_paid_fee_raw: Dec(fee),
            paper_minted_raw: Dec(U256::ZERO),
            paper_mint_basis_raw: Dec(U256::ZERO),
        }
    }

    #[test]
    fn a_close_nets_its_adjusted_pnl_less_the_fee_and_a_liquidation_the_whole_margin() {
        let gain = signed(usd(88)).unwrap();
        assert_eq!(net_result(&trade("PositionClosed", gain, usd(2))), signed(usd(86)).unwrap());
        assert_eq!(net_result(&trade("PositionClosed", -gain, U256::ZERO)), -gain);
        // A liquidation is booked at minus the margin even if its record carries
        // the PnL at the liquidation price and a fee.
        let reported = -signed(usd(480)).unwrap();
        assert_eq!(net_result(&trade("Liquidated", reported, usd(5))), -signed(usd(500)).unwrap());
        assert_eq!(net_result(&trade("Liquidated", I256::ZERO, U256::ZERO)), -signed(usd(500)).unwrap());
    }
}
