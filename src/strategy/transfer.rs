//! One transfer of USDC from one account to the other, as a pure state machine.
//!
//! Papertrade's collateral lives on HyperCore, so funds cannot go from one
//! account to the other directly. A transfer is three hops. Each is carried
//! out by someone else and shows up only as a balance changing:
//!
//! - `withdraw`: the sender's wallet signs a withdrawal, the relayer executes
//!   it, and the amount lands in that wallet's own HyperCore account.
//! - `send`: the sender's wallet sends it on Hyperliquid to the receiver's
//!   deposit address.
//! - `credit`: the relayer sweeps the deposit address into the receiver's
//!   Papertrade balance.
//!
//! [`next_step`] takes the transfer's record and what can be observed right now
//! and says what to do next. It never assumes that an action worked: a stage
//! is complete only when a balance shows it, and an action is repeated only
//! when the balances show that the earlier attempt moved nothing. Whenever it
//! asks for an action it also returns the record to write first, so that a
//! process killed at any point resumes from what is on disk.

use std::fmt;
use std::time::Duration;

use alloy::primitives::{B256, U256};

use super::decide::Side;
use super::inflight::is_failure;
use crate::protocol::deployment::usd;

/// HyperCore charges an address one USDC the first time it receives funds, so a
/// credit may fall this much short of the amount sent.
const CREDIT_TOLERANCE_USDC: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Withdraw,
    Send,
    Credit,
    /// The receiver holds the funds.
    Done,
    /// The bot will not go on by itself; see the record's note.
    Stuck,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Withdraw => "withdraw",
            Stage::Send => "send",
            Stage::Credit => "credit",
            Stage::Done => "done",
            Stage::Stuck => "stuck",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Stage::Withdraw, Stage::Send, Stage::Credit, Stage::Done, Stage::Stuck]
            .into_iter()
            .find(|stage| stage.as_str() == text)
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A transfer as it is kept in the database. Balances are 18-decimal USD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub id: i64,
    pub from: Side,
    pub to: Side,
    /// Whole USDC.
    pub amount_usdc: u64,
    pub stage: Stage,
    pub started_at_ms: i64,
    /// Attempts made in the current stage: withdrawals signed, sends made.
    pub attempts: u32,
    /// When the latest attempt was recorded; for `credit`, when the stage began.
    pub attempt_at_ms: i64,
    /// The sender's HyperCore USDC just before the latest withdrawal or send.
    pub core_baseline: Option<U256>,
    /// The sender's Papertrade balance just before the latest withdrawal.
    pub sender_baseline: Option<U256>,
    /// The receiver's Papertrade balance just before the first send.
    pub receiver_baseline: Option<U256>,
    /// The latest withdrawal intent.
    pub withdraw_intent: Option<B256>,
    /// Nonce of the Hyperliquid send. Every attempt of one transfer carries the
    /// same one: Hyperliquid executes a nonce once, so a repeat cannot pay twice.
    pub send_nonce: Option<u64>,
    /// When the relayer was last asked to check the receiver's deposit address.
    pub checked_at_ms: Option<i64>,
    /// Why it is stuck, or the latest error of an action.
    pub note: Option<String>,
}

impl Transfer {
    /// A transfer that has been decided and not yet started.
    pub fn new(id: i64, from: Side, to: Side, amount_usdc: u64, now_ms: i64) -> Self {
        Self {
            id,
            from,
            to,
            amount_usdc,
            stage: Stage::Withdraw,
            started_at_ms: now_ms,
            attempts: 0,
            attempt_at_ms: now_ms,
            core_baseline: None,
            sender_baseline: None,
            receiver_baseline: None,
            withdraw_intent: None,
            send_nonce: None,
            checked_at_ms: None,
            note: None,
        }
    }

    fn amount(&self) -> U256 {
        usd(self.amount_usdc)
    }
}

/// A balance with the moment it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub amount: U256,
    pub at_ms: i64,
}

/// What is known about the latest withdrawal intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withdrawal {
    /// It may still execute: queued, only just sent, or unanswered with its
    /// deadline not yet passed on the chain clock.
    InFlight,
    /// It can no longer execute.
    Ended {
        at_ms: i64,
        /// The relayer reported that it executed.
        succeeded: bool,
    },
}

impl Withdrawal {
    /// Reads the intent journal's row for the withdrawal.
    pub fn from_journal(
        status: &str,
        outcome: Option<&str>,
        created_at_ms: i64,
        settled_at_ms: Option<i64>,
    ) -> Self {
        match status {
            // Refused when it was submitted.
            "rejected" => Withdrawal::Ended { at_ms: created_at_ms, succeeded: false },
            "settled" => Withdrawal::Ended {
                at_ms: settled_at_ms.unwrap_or(created_at_ms),
                succeeded: outcome == Some("succeeded"),
            },
            // A failure can be on record before the row is marked settled.
            _ if outcome.is_some_and(is_failure) => {
                Withdrawal::Ended { at_ms: settled_at_ms.unwrap_or(created_at_ms), succeeded: false }
            }
            _ => Withdrawal::InFlight,
        }
    }
}

/// Everything [`next_step`] looks at besides the record. `None` means "not
/// known right now", and nothing is done on a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub now_ms: i64,
    /// The sender's HyperCore USDC.
    pub sender_core: Option<Reading>,
    /// The sender's available Papertrade balance.
    pub sender_balance: Option<U256>,
    /// The receiver's available Papertrade balance.
    pub receiver_balance: Option<U256>,
    /// The latest withdrawal intent, while the transfer is in `withdraw`.
    pub withdrawal: Option<Withdrawal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// How long an attempt is given before balances that have not moved count
    /// as proof that it did nothing.
    pub settle: Duration,
    /// How often the relayer is asked to check the receiver's deposit address.
    pub credit_check: Duration,
    /// How long a stage may lack its evidence, with nothing left in flight,
    /// before a person has to look.
    pub stuck_after: Duration,
    /// A HyperCore reading older than this says nothing about now.
    pub reading_max_age: Duration,
    /// Withdrawals signed, or sends made, before giving up on a stage.
    pub max_attempts: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            settle: Duration::from_secs(30),
            credit_check: Duration::from_secs(15),
            stuck_after: Duration::from_secs(600),
            reading_max_age: Duration::from_secs(10),
            max_attempts: 3,
        }
    }
}

/// What to do about a transfer now. Every variant but `Wait` carries the
/// record to write before anything else happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Nothing to do yet; the text says what is awaited.
    Wait(String),
    /// Write the record, then sign and submit a withdrawal for it.
    Withdraw(Transfer),
    /// Write the record, then send on Hyperliquid with the record's nonce.
    Send(Transfer),
    /// Write the record, then ask the relayer to check the receiver's deposit address.
    CheckDeposit(Transfer),
    /// Write the record: the balances show that the stage is complete.
    Advance(Transfer),
    /// Write the record: the transfer needs a person.
    Stuck(Transfer),
}

pub fn next_step(transfer: &Transfer, seen: &Observed, limits: &Limits) -> Step {
    // A baseline taken from an old reading, or a conclusion drawn from one,
    // would be about a balance that may have changed since.
    let max_age = millis(limits.reading_max_age);
    let sender_core = seen.sender_core.filter(|reading| seen.now_ms - reading.at_ms <= max_age);
    let seen = &Observed { sender_core, ..*seen };
    match transfer.stage {
        Stage::Withdraw => withdraw(transfer, seen, limits),
        Stage::Send => send(transfer, seen, limits),
        Stage::Credit => credit(transfer, seen, limits),
        Stage::Done => Step::Wait("the transfer is complete".into()),
        Stage::Stuck => Step::Wait("the transfer is stuck".into()),
    }
}

fn withdraw(transfer: &Transfer, seen: &Observed, limits: &Limits) -> Step {
    let (from, amount) = (transfer.from, transfer.amount());
    let attempt = |core: Reading, held: U256| {
        Step::Withdraw(Transfer {
            attempts: transfer.attempts + 1,
            attempt_at_ms: seen.now_ms,
            core_baseline: Some(core.amount),
            sender_baseline: Some(held),
            withdraw_intent: None,
            ..transfer.clone()
        })
    };
    if transfer.attempts == 0 {
        return match (seen.sender_core, seen.sender_balance) {
            (Some(core), Some(held)) => attempt(core, held),
            _ => Step::Wait(format!("reading the {from} account's balances before withdrawing")),
        };
    }
    let (Some(baseline), Some(held_before)) = (transfer.core_baseline, transfer.sender_baseline) else {
        return incomplete(transfer);
    };
    if seen.sender_core.is_some_and(|core| core.amount >= baseline + amount) {
        return Step::Advance(Transfer {
            stage: Stage::Send,
            attempts: 0,
            attempt_at_ms: seen.now_ms,
            core_baseline: None,
            note: None,
            ..transfer.clone()
        });
    }
    let (at_ms, succeeded) = match seen.withdrawal {
        None => return Step::Wait("looking up the withdrawal".into()),
        Some(Withdrawal::InFlight) => return Step::Wait("the withdrawal is with the relayer".into()),
        Some(Withdrawal::Ended { at_ms, succeeded }) => (at_ms, succeeded),
    };
    // The withdrawal can no longer execute. It did nothing only if both of the
    // sender's balances say so: read well after it ended, HyperCore holds no
    // more than before and Papertrade holds no less.
    let core_unmoved = seen
        .sender_core
        .filter(|core| core.at_ms >= at_ms + millis(limits.settle) && core.amount <= baseline);
    let still_held = seen.sender_balance.filter(|held| *held >= held_before);
    if let (false, Some(core), Some(held)) = (succeeded, core_unmoved, still_held) {
        if transfer.attempts < limits.max_attempts {
            return attempt(core, held);
        }
        return stuck(
            transfer,
            format!(
                "the withdrawal did not go through in {} attempts; nothing has left the {from} account",
                transfer.attempts
            ),
        );
    }
    if seen.sender_core.is_some() && seen.now_ms - at_ms >= millis(limits.stuck_after) {
        return stuck(
            transfer,
            format!(
                "the withdrawal of {} USDC ended, but the {from} wallet's HyperCore balance did not rise by \
                 that much; compare the {from} account's Papertrade balance with its HyperCore account",
                transfer.amount_usdc
            ),
        );
    }
    Step::Wait(if succeeded {
        "the withdrawal executed; waiting for it to arrive on HyperCore".into()
    } else {
        "the withdrawal ended; waiting to see whether anything moved".into()
    })
}

fn send(transfer: &Transfer, seen: &Observed, limits: &Limits) -> Step {
    let (from, amount) = (transfer.from, transfer.amount());
    if transfer.attempts == 0 {
        let (Some(core), Some(receiver), Ok(nonce)) =
            (seen.sender_core, seen.receiver_balance, u64::try_from(seen.now_ms))
        else {
            return Step::Wait("reading both accounts' balances before sending on Hyperliquid".into());
        };
        return Step::Send(Transfer {
            attempts: 1,
            attempt_at_ms: seen.now_ms,
            core_baseline: Some(core.amount),
            receiver_baseline: Some(receiver),
            send_nonce: Some(nonce),
            ..transfer.clone()
        });
    }
    let Some(baseline) = transfer.core_baseline else { return incomplete(transfer) };
    let Some(core) = seen.sender_core else {
        return Step::Wait(format!("reading the {from} wallet's HyperCore balance"));
    };
    if core.amount + amount <= baseline {
        return Step::Advance(Transfer {
            stage: Stage::Credit,
            attempts: 0,
            attempt_at_ms: seen.now_ms,
            checked_at_ms: None,
            note: None,
            ..transfer.clone()
        });
    }
    if core.at_ms - transfer.attempt_at_ms < millis(limits.settle) {
        return Step::Wait("waiting for Hyperliquid to move the funds".into());
    }
    if core.amount < baseline {
        return stuck(
            transfer,
            format!(
                "the {from} wallet's HyperCore balance fell, but by less than the {} USDC that were sent; \
                 check where the funds went",
                transfer.amount_usdc
            ),
        );
    }
    if transfer.attempts < limits.max_attempts {
        // The balance has not moved, so the send did not happen. The nonce stays.
        return Step::Send(Transfer {
            attempts: transfer.attempts + 1,
            attempt_at_ms: seen.now_ms,
            core_baseline: Some(core.amount),
            ..transfer.clone()
        });
    }
    stuck(
        transfer,
        format!(
            "Hyperliquid did not move the {} USDC in {} attempts; they are in the {from} wallet's HyperCore \
             account",
            transfer.amount_usdc, transfer.attempts
        ),
    )
}

fn credit(transfer: &Transfer, seen: &Observed, limits: &Limits) -> Step {
    let (from, to) = (transfer.from, transfer.to);
    let Some(baseline) = transfer.receiver_baseline else { return incomplete(transfer) };
    let Some(balance) = seen.receiver_balance else {
        return Step::Wait(format!("reading the {to} account's balance"));
    };
    if balance + usd(CREDIT_TOLERANCE_USDC) >= baseline + transfer.amount() {
        return Step::Advance(Transfer { stage: Stage::Done, note: None, ..transfer.clone() });
    }
    if seen.now_ms - transfer.attempt_at_ms >= millis(limits.stuck_after) {
        return stuck(
            transfer,
            format!(
                "{} USDC left the {from} wallet but the {to} account was not credited; they should be at the \
                 {to} account's deposit address on HyperCore",
                transfer.amount_usdc
            ),
        );
    }
    if transfer.checked_at_ms.is_none_or(|at| seen.now_ms - at >= millis(limits.credit_check)) {
        return Step::CheckDeposit(Transfer { checked_at_ms: Some(seen.now_ms), ..transfer.clone() });
    }
    Step::Wait(format!("waiting for the relayer to credit the {to} account"))
}

/// The stuck record: where it stopped, why, and the last error if there was one.
fn stuck(transfer: &Transfer, reason: String) -> Step {
    let last_error =
        transfer.note.as_deref().map(|note| format!(" (last error: {note})")).unwrap_or_default();
    Step::Stuck(Transfer {
        stage: Stage::Stuck,
        note: Some(format!("stuck in {}: {reason}{last_error}", transfer.stage)),
        ..transfer.clone()
    })
}

/// A record that lacks what its stage measures against cannot be judged.
fn incomplete(transfer: &Transfer) -> Step {
    stuck(transfer, "its record is incomplete, so nothing can be concluded from the balances".into())
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::intent_id;

    const START: i64 = 1_000_000;
    const SETTLE: i64 = 30_000;
    const STUCK_AFTER: i64 = 600_000;
    /// Long has too much: 850 USDC go from long to short.
    const AMOUNT: u64 = 850;

    fn limits() -> Limits {
        Limits::default()
    }

    fn fresh() -> Transfer {
        Transfer::new(7, Side::Long, Side::Short, AMOUNT, START)
    }

    /// Nothing is known.
    fn at(now_ms: i64) -> Observed {
        Observed { now_ms, sender_core: None, sender_balance: None, receiver_balance: None, withdrawal: None }
    }

    fn core(amount: u64, at_ms: i64) -> Option<Reading> {
        Some(Reading { amount: usd(amount), at_ms })
    }

    fn ended(at_ms: i64) -> Option<Withdrawal> {
        Some(Withdrawal::Ended { at_ms, succeeded: false })
    }

    fn step(transfer: &Transfer, seen: Observed) -> Step {
        next_step(transfer, &seen, &limits())
    }

    fn waits(transfer: &Transfer, seen: Observed) -> String {
        match step(transfer, seen) {
            Step::Wait(what) => what,
            other => panic!("expected to wait, got {other:?}"),
        }
    }

    fn stuck_note(transfer: &Transfer, seen: Observed) -> String {
        match step(transfer, seen) {
            Step::Stuck(record) => {
                assert_eq!(record.stage, Stage::Stuck);
                // Nothing else in the record changes: it shows where things stood.
                assert_eq!(
                    Transfer { stage: transfer.stage, note: transfer.note.clone(), ..record.clone() },
                    *transfer
                );
                record.note.unwrap()
            }
            other => panic!("expected to be stuck, got {other:?}"),
        }
    }

    /// The first withdrawal was signed at `START` with 5 USDC on HyperCore and
    /// 2,000 on Papertrade.
    fn withdrawing() -> Transfer {
        Transfer {
            attempts: 1,
            core_baseline: Some(usd(5)),
            sender_baseline: Some(usd(2_000)),
            withdraw_intent: Some(intent_id(1)),
            ..fresh()
        }
    }

    /// The first send went out at `START` with 855 USDC on HyperCore; the
    /// receiver held 300 on Papertrade.
    fn sending() -> Transfer {
        Transfer {
            stage: Stage::Send,
            attempts: 1,
            core_baseline: Some(usd(855)),
            receiver_baseline: Some(usd(300)),
            send_nonce: Some(START as u64),
            ..withdrawing()
        }
    }

    /// The send was seen to leave at `START`.
    fn crediting() -> Transfer {
        Transfer { stage: Stage::Credit, attempts: 0, ..sending() }
    }

    // --- withdraw --------------------------------------------------------------

    #[test]
    fn nothing_is_withdrawn_before_both_of_the_senders_balances_are_known() {
        for seen in [
            at(START),
            Observed { sender_core: core(5, START), ..at(START) },
            Observed { sender_balance: Some(usd(2_000)), ..at(START) },
        ] {
            assert_eq!(waits(&fresh(), seen), "reading the long account's balances before withdrawing");
        }
    }

    #[test]
    fn the_first_withdrawal_records_its_time_and_both_baselines_before_it_is_signed() {
        let seen =
            Observed { sender_core: core(5, START + 40), sender_balance: Some(usd(2_000)), ..at(START + 50) };
        assert_eq!(
            step(&fresh(), seen),
            Step::Withdraw(Transfer {
                attempts: 1,
                attempt_at_ms: START + 50,
                core_baseline: Some(usd(5)),
                sender_baseline: Some(usd(2_000)),
                ..fresh()
            })
        );
    }

    #[test]
    fn the_withdrawal_is_complete_once_hypercore_holds_the_amount_more() {
        let arrived = Observed { sender_core: core(855, START + 4_000), ..at(START + 4_100) };
        assert_eq!(
            step(&withdrawing(), arrived),
            Step::Advance(Transfer {
                stage: Stage::Send,
                attempts: 0,
                attempt_at_ms: START + 4_100,
                core_baseline: None,
                ..withdrawing()
            })
        );
        // Evidence is all it takes: whatever the relayer says about the intent.
        for withdrawal in [Some(Withdrawal::InFlight), ended(START + 10), None] {
            let seen = Observed { withdrawal, ..arrived };
            assert!(matches!(step(&withdrawing(), seen), Step::Advance(_)));
        }
    }

    #[test]
    fn one_unit_less_than_the_amount_is_not_an_arrival() {
        let short = Reading { amount: usd(855) - U256::from(1), at_ms: START + 4_000 };
        let seen = Observed {
            sender_core: Some(short),
            withdrawal: Some(Withdrawal::InFlight),
            ..at(START + 4_100)
        };
        assert_eq!(waits(&withdrawing(), seen), "the withdrawal is with the relayer");
    }

    #[test]
    fn a_withdrawal_that_may_still_execute_is_waited_for_however_long_it_takes() {
        let seen = Observed {
            sender_core: core(5, START + 3_000_000),
            sender_balance: Some(usd(2_000)),
            withdrawal: Some(Withdrawal::InFlight),
            ..at(START + 3_000_000)
        };
        assert_eq!(waits(&withdrawing(), seen), "the withdrawal is with the relayer");
        // Not knowing what became of it is no reason to sign another.
        let seen = Observed { withdrawal: None, ..seen };
        assert_eq!(waits(&withdrawing(), seen), "looking up the withdrawal");
    }

    /// The withdrawal ended at `START + 1_000` and, read after the settle time,
    /// neither of the sender's balances has moved.
    fn nothing_moved() -> Observed {
        let read_at = START + 1_000 + SETTLE;
        Observed {
            sender_core: core(5, read_at),
            sender_balance: Some(usd(2_000)),
            withdrawal: ended(START + 1_000),
            ..at(read_at + 10)
        }
    }

    #[test]
    fn a_withdrawal_that_ended_without_moving_anything_is_signed_again() {
        assert_eq!(
            step(&withdrawing(), nothing_moved()),
            Step::Withdraw(Transfer {
                attempts: 2,
                attempt_at_ms: START + 1_000 + SETTLE + 10,
                withdraw_intent: None,
                ..withdrawing()
            })
        );
    }

    #[test]
    fn a_withdrawal_is_not_signed_again_while_a_balance_could_still_show_it() {
        let cases = [
            // The HyperCore reading is from before the settle time was over.
            Observed { sender_core: core(5, START + SETTLE + 999), ..nothing_moved() },
            // HyperCore received something, though not the full amount.
            Observed { sender_core: core(6, START + 1_000 + SETTLE), ..nothing_moved() },
            // Papertrade no longer holds what it did: the withdrawal took it.
            Observed { sender_balance: Some(usd(2_000) - U256::from(1)), ..nothing_moved() },
            // A balance is unknown.
            Observed { sender_balance: None, ..nothing_moved() },
            Observed { sender_core: None, ..nothing_moved() },
        ];
        for seen in cases {
            assert_eq!(
                waits(&withdrawing(), seen),
                "the withdrawal ended; waiting to see whether anything moved",
                "{seen:?}"
            );
        }
    }

    #[test]
    fn a_withdrawal_the_relayer_reported_executed_is_never_signed_again() {
        let seen = Observed {
            withdrawal: Some(Withdrawal::Ended { at_ms: START + 1_000, succeeded: true }),
            ..nothing_moved()
        };
        assert_eq!(
            waits(&withdrawing(), seen),
            "the withdrawal executed; waiting for it to arrive on HyperCore"
        );
    }

    #[test]
    fn the_third_withdrawal_that_moves_nothing_is_the_last() {
        let third = Transfer { attempts: 3, ..withdrawing() };
        assert_eq!(
            stuck_note(&third, nothing_moved()),
            "stuck in withdraw: the withdrawal did not go through in 3 attempts; nothing has left the long \
             account"
        );
        let second = Transfer { attempts: 2, ..withdrawing() };
        assert!(matches!(step(&second, nothing_moved()), Step::Withdraw(record) if record.attempts == 3));
    }

    #[test]
    fn a_withdrawal_that_ended_without_evidence_either_way_becomes_stuck_in_time() {
        // It executed, by the relayer's word, but HyperCore never showed it.
        let ended_at = START + 1_000;
        let executed = |now_ms: i64| Observed {
            sender_core: core(5, now_ms),
            sender_balance: Some(usd(1_150)),
            withdrawal: Some(Withdrawal::Ended { at_ms: ended_at, succeeded: true }),
            ..at(now_ms)
        };
        assert!(matches!(step(&withdrawing(), executed(ended_at + STUCK_AFTER - 1)), Step::Wait(_)));
        assert_eq!(
            stuck_note(&withdrawing(), executed(ended_at + STUCK_AFTER)),
            "stuck in withdraw: the withdrawal of 850 USDC ended, but the long wallet's HyperCore balance did \
             not rise by that much; compare the long account's Papertrade balance with its HyperCore account"
        );
        // Only part of it arrived: the same, and never a second withdrawal.
        let partial =
            Observed { sender_core: core(854, ended_at + STUCK_AFTER), ..executed(ended_at + STUCK_AFTER) };
        assert!(
            stuck_note(&withdrawing(), partial).starts_with("stuck in withdraw: the withdrawal of 850 USDC")
        );
        // Without a HyperCore reading nothing is concluded, however late it is.
        let blind = Observed { sender_core: None, ..executed(ended_at + 10 * STUCK_AFTER) };
        assert!(matches!(step(&withdrawing(), blind), Step::Wait(_)));
    }

    // --- send ------------------------------------------------------------------

    fn arrived() -> Transfer {
        Transfer { stage: Stage::Send, attempts: 0, core_baseline: None, ..withdrawing() }
    }

    #[test]
    fn nothing_is_sent_before_the_senders_and_the_receivers_balances_are_known() {
        for seen in [
            at(START),
            Observed { sender_core: core(855, START), ..at(START) },
            Observed { receiver_balance: Some(usd(300)), ..at(START) },
        ] {
            assert_eq!(
                waits(&arrived(), seen),
                "reading both accounts' balances before sending on Hyperliquid"
            );
        }
    }

    #[test]
    fn the_first_send_records_its_time_its_baselines_and_its_nonce_before_it_goes_out() {
        let seen = Observed {
            sender_core: core(855, START + 90),
            receiver_balance: Some(usd(300)),
            ..at(START + 100)
        };
        assert_eq!(
            step(&arrived(), seen),
            Step::Send(Transfer {
                attempts: 1,
                attempt_at_ms: START + 100,
                core_baseline: Some(usd(855)),
                receiver_baseline: Some(usd(300)),
                send_nonce: Some(START as u64 + 100),
                ..arrived()
            })
        );
    }

    #[test]
    fn the_send_is_complete_once_hypercore_holds_the_amount_less() {
        // At once, well inside the settle time.
        let left = Observed { sender_core: core(5, START + 400), ..at(START + 500) };
        assert_eq!(
            step(&sending(), left),
            Step::Advance(Transfer {
                stage: Stage::Credit,
                attempts: 0,
                attempt_at_ms: START + 500,
                checked_at_ms: None,
                ..sending()
            })
        );
        // A fee on top only lowers it further.
        let seen = Observed { sender_core: core(4, START + 400), ..at(START + 500) };
        assert!(matches!(step(&sending(), seen), Step::Advance(_)));
    }

    #[test]
    fn a_send_is_given_the_settle_time_before_anything_is_concluded() {
        let unchanged = Observed { sender_core: core(855, START + SETTLE - 1), ..at(START + SETTLE + 5_000) };
        assert_eq!(waits(&sending(), unchanged), "waiting for Hyperliquid to move the funds");
        let unknown = at(START + 10 * SETTLE);
        assert_eq!(waits(&sending(), unknown), "reading the long wallet's HyperCore balance");
    }

    #[test]
    fn a_send_that_moved_nothing_by_the_settle_time_is_repeated_with_the_same_nonce() {
        let unchanged = Observed { sender_core: core(855, START + SETTLE), ..at(START + SETTLE + 20) };
        assert_eq!(
            step(&sending(), unchanged),
            Step::Send(Transfer { attempts: 2, attempt_at_ms: START + SETTLE + 20, ..sending() })
        );
        // More arrived meanwhile: still nothing left, and the new attempt measures from there.
        let more = Observed { sender_core: core(900, START + SETTLE), ..at(START + SETTLE + 20) };
        let Step::Send(record) = step(&sending(), more) else { panic!("expected another send") };
        assert_eq!((record.core_baseline, record.send_nonce), (Some(usd(900)), Some(START as u64)));
        assert_eq!(record.receiver_baseline, Some(usd(300)));
    }

    #[test]
    fn the_third_send_that_moves_nothing_is_the_last() {
        let third = Transfer { attempts: 3, note: Some("Insufficient spot balance".into()), ..sending() };
        let unchanged = Observed { sender_core: core(855, START + SETTLE), ..at(START + SETTLE + 20) };
        assert_eq!(
            stuck_note(&third, unchanged),
            "stuck in send: Hyperliquid did not move the 850 USDC in 3 attempts; they are in the long wallet's \
             HyperCore account (last error: Insufficient spot balance)"
        );
        let second = Transfer { attempts: 2, ..sending() };
        assert!(matches!(step(&second, unchanged), Step::Send(record) if record.attempts == 3));
    }

    #[test]
    fn a_balance_that_fell_by_less_than_the_amount_is_not_explained_and_not_retried() {
        let partial = Observed { sender_core: core(854, START + SETTLE), ..at(START + SETTLE + 20) };
        assert_eq!(
            stuck_note(&sending(), partial),
            "stuck in send: the long wallet's HyperCore balance fell, but by less than the 850 USDC that were \
             sent; check where the funds went"
        );
        // Inside the settle time it may simply not be finished.
        let early = Observed { sender_core: core(854, START + SETTLE - 1), ..at(START + SETTLE) };
        assert!(matches!(step(&sending(), early), Step::Wait(_)));
    }

    // --- credit ----------------------------------------------------------------

    #[test]
    fn the_relayer_is_asked_to_check_the_deposit_address_at_once_and_then_at_intervals() {
        let waiting = |now_ms: i64| Observed { receiver_balance: Some(usd(300)), ..at(now_ms) };
        assert_eq!(
            step(&crediting(), waiting(START + 10)),
            Step::CheckDeposit(Transfer { checked_at_ms: Some(START + 10), ..crediting() })
        );
        let checked = Transfer { checked_at_ms: Some(START + 10), ..crediting() };
        assert_eq!(
            waits(&checked, waiting(START + 10 + 14_999)),
            "waiting for the relayer to credit the short account"
        );
        assert_eq!(
            step(&checked, waiting(START + 10 + 15_000)),
            Step::CheckDeposit(Transfer { checked_at_ms: Some(START + 10 + 15_000), ..checked.clone() })
        );
    }

    #[test]
    fn the_credit_is_complete_once_the_receiver_holds_the_amount_more_less_one_usdc() {
        let done = Step::Advance(Transfer { stage: Stage::Done, ..crediting() });
        for balance in [usd(1_150), usd(1_149), usd(5_000)] {
            let seen = Observed { receiver_balance: Some(balance), ..at(START + 2_000) };
            assert_eq!(step(&crediting(), seen), done);
        }
        // One unit below the tolerance is not a credit.
        let seen = Observed { receiver_balance: Some(usd(1_149) - U256::from(1)), ..at(START + 2_000) };
        assert!(matches!(step(&crediting(), seen), Step::CheckDeposit(_)));
        // A credit is taken whenever it shows up, even after the time allowed.
        let late = Observed { receiver_balance: Some(usd(1_150)), ..at(START + 2 * STUCK_AFTER) };
        assert_eq!(step(&crediting(), late), done);
    }

    #[test]
    fn a_credit_that_does_not_come_in_time_becomes_stuck() {
        let checked = Transfer { checked_at_ms: Some(START + STUCK_AFTER - 2_000), ..crediting() };
        let waiting = |now_ms: i64| Observed { receiver_balance: Some(usd(300)), ..at(now_ms) };
        assert!(matches!(step(&checked, waiting(START + STUCK_AFTER - 1)), Step::Wait(_)));
        assert_eq!(
            stuck_note(&checked, waiting(START + STUCK_AFTER)),
            "stuck in credit: 850 USDC left the long wallet but the short account was not credited; they \
             should be at the short account's deposit address on HyperCore"
        );
        // While the receiver's balance is unknown, nothing is concluded.
        assert_eq!(waits(&checked, at(START + 2 * STUCK_AFTER)), "reading the short account's balance");
    }

    // --- readings --------------------------------------------------------------

    #[test]
    fn a_hypercore_reading_that_is_too_old_is_no_reading() {
        const MAX_AGE: i64 = 10_000;
        let now = START + 2 * STUCK_AFTER;
        let (aged, stale) = (now - MAX_AGE, now - MAX_AGE - 1);
        let balances =
            Observed { sender_balance: Some(usd(2_000)), receiver_balance: Some(usd(300)), ..at(now) };

        // No baseline is taken from it, for a withdrawal or for a send.
        let seen = Observed { sender_core: core(5, stale), ..balances };
        assert_eq!(waits(&fresh(), seen), "reading the long account's balances before withdrawing");
        assert!(matches!(
            step(&fresh(), Observed { sender_core: core(5, aged), ..balances }),
            Step::Withdraw(_)
        ));
        let seen = Observed { sender_core: core(855, stale), ..balances };
        assert_eq!(waits(&arrived(), seen), "reading both accounts' balances before sending on Hyperliquid");
        assert!(matches!(
            step(&arrived(), Observed { sender_core: core(855, aged), ..balances }),
            Step::Send(_)
        ));

        // Nothing is concluded from it: not that a withdrawal moved nothing, not
        // that it is lost, not that a send has to be repeated.
        let ended = Observed { withdrawal: ended(START + 1_000), ..balances };
        let seen = Observed { sender_core: core(5, stale), ..ended };
        assert_eq!(
            waits(&withdrawing(), seen),
            "the withdrawal ended; waiting to see whether anything moved"
        );
        assert!(matches!(
            step(&withdrawing(), Observed { sender_core: core(5, aged), ..ended }),
            Step::Withdraw(_)
        ));
        let executed =
            Observed { withdrawal: Some(Withdrawal::Ended { at_ms: START, succeeded: true }), ..balances };
        assert!(matches!(
            step(&withdrawing(), Observed { sender_core: core(5, stale), ..executed }),
            Step::Wait(_)
        ));
        assert!(matches!(
            step(&withdrawing(), Observed { sender_core: core(5, aged), ..executed }),
            Step::Stuck(_)
        ));
        let seen = Observed { sender_core: core(855, stale), ..balances };
        assert_eq!(waits(&sending(), seen), "reading the long wallet's HyperCore balance");
        assert!(matches!(
            step(&sending(), Observed { sender_core: core(855, aged), ..balances }),
            Step::Send(_)
        ));
    }

    // --- records ---------------------------------------------------------------

    #[test]
    fn a_record_without_its_baseline_is_stuck_rather_than_guessed_at() {
        let cases = [
            Transfer { core_baseline: None, ..withdrawing() },
            Transfer { sender_baseline: None, ..withdrawing() },
            Transfer { core_baseline: None, ..sending() },
            Transfer { receiver_baseline: None, ..crediting() },
        ];
        for transfer in cases {
            let everything = Observed {
                sender_core: core(5_000, START),
                sender_balance: Some(usd(5_000)),
                receiver_balance: Some(usd(5_000)),
                withdrawal: ended(START),
                ..at(START)
            };
            assert!(stuck_note(&transfer, everything).contains("its record is incomplete"), "{transfer:?}");
        }
    }

    #[test]
    fn a_finished_or_stuck_transfer_asks_for_nothing() {
        for stage in [Stage::Done, Stage::Stuck] {
            let transfer = Transfer { stage, ..crediting() };
            let seen = Observed { receiver_balance: Some(usd(300)), ..at(START + 2 * STUCK_AFTER) };
            assert!(matches!(step(&transfer, seen), Step::Wait(_)));
        }
    }

    #[test]
    fn the_journal_says_whether_a_withdrawal_can_still_execute() {
        let read = |status, outcome| Withdrawal::from_journal(status, outcome, 100, Some(900));
        for status in ["signed", "accepted", "unknown"] {
            assert_eq!(read(status, None), Withdrawal::InFlight);
            // Reported as executed, yet tracked until its deadline has passed.
            assert_eq!(read(status, Some("succeeded")), Withdrawal::InFlight);
        }
        assert_eq!(read("rejected", None), Withdrawal::Ended { at_ms: 100, succeeded: false });
        assert_eq!(read("settled", None), Withdrawal::Ended { at_ms: 900, succeeded: false });
        assert_eq!(read("settled", Some("expired")), Withdrawal::Ended { at_ms: 900, succeeded: false });
        assert_eq!(read("settled", Some("succeeded")), Withdrawal::Ended { at_ms: 900, succeeded: true });
        assert_eq!(read("accepted", Some("failed")), Withdrawal::Ended { at_ms: 900, succeeded: false });
        assert_eq!(
            Withdrawal::from_journal("settled", None, 100, None),
            Withdrawal::Ended { at_ms: 100, succeeded: false }
        );
    }

    #[test]
    fn stages_are_stored_by_name() {
        for stage in [Stage::Withdraw, Stage::Send, Stage::Credit, Stage::Done, Stage::Stuck] {
            assert_eq!(Stage::parse(stage.as_str()), Some(stage));
        }
        assert_eq!(Stage::parse("abandoned"), None);
        assert_eq!(Stage::Credit.to_string(), "credit");
    }
}
