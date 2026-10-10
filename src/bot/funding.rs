//! The loop's side of funding: keeping the session keys registered, judging
//! whether funds should move between the two accounts, and carrying out a
//! transfer one recorded step at a time.
//!
//! What to do is decided by pure functions ([`funds`], [`hold`], [`next_step`]).
//! This module gathers what they look at, writes the record they hand back,
//! and only then acts. None of it waits for the legs, and the legs wait for
//! none of it.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use anyhow::Result;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use super::{Bot, PerSide, duration_ms, inflight, unix_seconds};
use crate::config::Config;
use crate::keystore::UnlockedKeys;
use crate::protocol::account::AccountState;
use crate::protocol::chain::Chain;
use crate::protocol::deployment::{SESSION_KEY_TTL_SECONDS, usd};
use crate::protocol::intents::{RegisterSessionKey, USDC_UNIT, WithdrawToCore};
use crate::protocol::wire::TradingState;
use crate::status::{FundsSnapshot, PlannedTransfer, Snapshot, TransferSnapshot, format_amount};
use crate::store::{Store, now_ms};
use crate::strategy::decide::{Side, has_queued_trade, session_key_needs_registration};
use crate::strategy::inflight::Kind;
use crate::strategy::rebalance::{Funds, Hold, Holding, Readiness, funds, hold, reserved, rounds_affordable};
use crate::strategy::transfer::{Limits, Observed, Reading, Stage, Step, Transfer, Withdrawal, next_step};
use crate::wallet::hypercore::HyperCore;
use crate::wallet::{DepositSend, WalletSigner, Wallets};

/// An error of an action is kept with the transfer's record; this much of it.
const NOTE_LENGTH: usize = 200;

#[derive(Debug, Clone, Copy)]
pub struct FundingTiming {
    /// The waits of a transfer between the two accounts.
    pub transfer: Limits,
    /// How often the sender's HyperCore balance is read while a transfer needs it.
    pub core_poll: Duration,
    /// A session key registration that did not take is not signed again sooner than this.
    pub register_retry: Duration,
}

impl Default for FundingTiming {
    fn default() -> Self {
        Self {
            transfer: Limits::default(),
            core_poll: Duration::from_secs(3),
            register_retry: Duration::from_secs(300),
        }
    }
}

/// A wallet's HyperCore USDC, with when the answer was asked for.
#[derive(Debug, Clone, Copy)]
struct CoreReading {
    wallet: Address,
    amount: U256,
    at_ms: i64,
}

pub(super) struct Funding {
    chain: Chain,
    hypercore: Arc<dyn HyperCore>,
    /// Each wallet's own key, where the automation is on and the key was unlocked.
    signers: PerSide<Option<WalletSigner>>,
    /// `PT_REBALANCE`: whether a transfer may be started. One that has started
    /// is carried on either way.
    rebalance: bool,
    /// The transfer that was last put on the timeline as called for but not started.
    reported_held: Option<String>,
    /// The unfinished transfer, exactly as it was last written to the database.
    transfer: Option<Transfer>,
    /// What the transfer is waiting for, for the status snapshot.
    waiting_for: Option<String>,
    /// The wallet whose HyperCore balance is followed; `None` stops the polling.
    watched: watch::Sender<Option<Address>>,
    core: watch::Receiver<Option<CoreReading>>,
    register_allowed_at_ms: PerSide<i64>,
}

impl Funding {
    /// Picks up the transfer an earlier run left unfinished, and when each
    /// account last tried to register its session key.
    pub(super) async fn start(
        tasks: &mut JoinSet<()>,
        store: &Store,
        keys: &UnlockedKeys,
        chain: Chain,
        hypercore: Arc<dyn HyperCore>,
        timing: &FundingTiming,
        config: &Config,
    ) -> Result<Self> {
        // The keystore hands over a signer; from here on it signs three things.
        // It has already checked that the key is the wallet's own. Should it not
        // be, the key is left unused: the session keys must still get to run.
        let signer = |side: Side| {
            let account = keys.get(side);
            let signer = account.wallet.signer()?.clone();
            WalletSigner::new(account.owner, signer)
                .inspect_err(|error| error!(%side, "the wallet key is not used: {error:#}"))
                .ok()
        };
        let (watched, watching) = watch::channel(None);
        let (reading, core) = watch::channel(None);
        tasks.spawn(follow_core_balance(hypercore.clone(), watching, timing.core_poll, reading));
        let retry = duration_ms(timing.register_retry);
        let last = |side: Side| store.last_attempt_ms(side, "registerSessionKey");
        let allowed_at = |last: Option<i64>| last.map_or(0, |at| at.saturating_add(retry));
        Ok(Self {
            chain,
            hypercore,
            signers: PerSide { long: signer(Side::Long), short: signer(Side::Short) },
            rebalance: config.rebalance,
            reported_held: None,
            transfer: store.active_transfer().await?,
            waiting_for: None,
            watched,
            core,
            register_allowed_at_ms: PerSide {
                long: allowed_at(last(Side::Long).await?),
                short: allowed_at(last(Side::Short).await?),
            },
        })
    }
}

/// Keeps `reading` at the watched wallet's HyperCore USDC, and at `None` while
/// no wallet is watched. Nothing is asked of Hyperliquid unless a transfer needs it.
async fn follow_core_balance(
    hypercore: Arc<dyn HyperCore>,
    mut watched: watch::Receiver<Option<Address>>,
    every: Duration,
    reading: watch::Sender<Option<CoreReading>>,
) {
    loop {
        let wallet = *watched.borrow_and_update();
        let Some(wallet) = wallet else {
            reading.send_replace(None);
            if watched.changed().await.is_err() {
                return;
            }
            continue;
        };
        // The answer is at least as new as the question, so the question's time
        // is the one that is safe to reason from.
        let asked_at_ms = now_ms();
        match hypercore.spot_usdc(wallet).await {
            Ok(amount) => {
                reading.send_replace(Some(CoreReading { wallet, amount, at_ms: asked_at_ms }));
            }
            Err(error) => {
                warn!(%wallet, error = %format_args!("{error:#}"), "reading the HyperCore balance failed");
            }
        }
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            changed = watched.changed() => if changed.is_err() { return },
        }
    }
}

/// What one tick makes of the two accounts' funds.
pub(super) struct FundsView {
    long: Holding,
    short: Holding,
    /// `None` with `PT_REBALANCE` off: nothing is judged, so nothing is moved.
    verdict: Option<Funds>,
    /// Why the transfer the verdict calls for is not started in this tick.
    hold: Option<Hold>,
}

impl FundsView {
    /// The transfer the verdict calls for and what holds it back, unless one
    /// is under way already: that one is what there is to show.
    fn planned(&self) -> Option<(Side, Side, u64, Option<Hold>)> {
        match (self.verdict?, self.hold) {
            (_, Some(Hold::TransferActive)) => None,
            (Funds::Rebalance { from, to, amount_usdc }, hold) => Some((from, to, amount_usdc, hold)),
            _ => None,
        }
    }

    /// The transfer to start now, as `(from, to, whole USDC)`.
    pub(super) fn to_start(&self) -> Option<(Side, Side, u64)> {
        match self.planned()? {
            (from, to, amount_usdc, None) => Some((from, to, amount_usdc)),
            _ => None,
        }
    }
}

impl Bot {
    /// What a transfer between the two accounts has claimed from this
    /// account's balance and may still take.
    pub(super) fn reserved(&self, side: Side) -> U256 {
        self.funding.transfer.as_ref().map_or(U256::ZERO, |transfer| reserved(transfer, side))
    }

    /// Both wallets' own keys are here, so a transfer can be signed from either side.
    pub(super) fn can_rebalance(&self) -> bool {
        Side::BOTH.iter().all(|&side| self.funding.signers.get(side).is_some())
    }

    /// Judges whether funds should move between the two accounts. It is asked
    /// in every tick and in every phase of the legs; `None` while an account's
    /// state is unavailable. `trading` says for which accounts the bot has an
    /// open or a close in flight or is sending one in this tick.
    pub(super) fn judge_funds(
        &self,
        accounts: &PerSide<Option<AccountState>>,
        trading: &PerSide<bool>,
        trading_state: Option<&TradingState>,
    ) -> Option<FundsView> {
        let (long, short) = (accounts.long.as_ref()?, accounts.short.as_ref()?);
        let view =
            |verdict, hold| FundsView { long: Holding::of(long), short: Holding::of(short), verdict, hold };
        if !self.funding.rebalance {
            return Some(view(None, None));
        }
        let verdict =
            funds(&Holding::of(long), &Holding::of(short), self.params.margin(), self.params.round_loss());
        let Funds::Rebalance { from, .. } = verdict else { return Some(view(Some(verdict), None)) };
        let sender = if from.is_long() { long } else { short };
        let held = hold(&Readiness {
            transfer_active: self.funding.transfer.is_some(),
            sender_busy: *trading.get(from) || has_queued_trade(sender),
            wallet_keys: self.can_rebalance(),
            mode: self.mode,
            trading: trading_state,
            dry_run: self.dry_run,
        });
        Some(view(Some(verdict), held))
    }

    /// Puts a transfer that is called for but cannot start on the timeline,
    /// once for as long as it stays the same. A dry run reports its plan this way.
    pub(super) async fn report_held_transfer(&mut self, funds: Option<&FundsView>) {
        let Some(funds) = funds else { return };
        let held = match funds.planned() {
            // It follows shortly, and is recorded when it starts.
            Some((.., Some(hold))) if hold.is_passing() => return,
            Some((from, to, amount_usdc, Some(hold))) => Some(format!(
                "transfer planned, not started ({hold}): {amount_usdc} USDC from the {from} account to the {to} \
                 account"
            )),
            _ => None,
        };
        if held == self.funding.reported_held {
            return;
        }
        if let Some(summary) = &held {
            info!("{summary}");
            let recorded = self.store.record_event("transfer", summary, None).await;
            self.best_effort("recording a planned transfer", recorded);
        }
        self.funding.reported_held = held;
    }

    /// Starts the transfer the funds call for: it exists once its record does.
    pub(super) async fn start_transfer(&mut self, from: Side, to: Side, amount_usdc: u64) {
        if self.funding.transfer.is_some() {
            return;
        }
        let created = self.store.create_transfer(from, to, amount_usdc).await;
        let Some(transfer) = self.best_effort("recording a new transfer", created) else { return };
        let what = format!("planned: {amount_usdc} USDC from the {from} account to the {to} account");
        self.transfer_event(&transfer, &what).await;
        self.funding.transfer = Some(transfer);
    }

    /// Only the operator ends a stuck transfer, and does so from another
    /// process: `papertrade rebalance abandon`. This notices.
    pub(super) async fn notice_abandoned_transfer(&mut self) {
        let Some(stuck) = self.funding.transfer.as_ref().filter(|transfer| transfer.stage == Stage::Stuck)
        else {
            return;
        };
        let id = stuck.id;
        let stored = self.store.active_transfer().await;
        if let Some(stored) = self.best_effort("reading the stuck transfer", stored)
            && stored.as_ref().map(|stored| stored.id) != Some(id)
        {
            info!(id, "the operator has ended the stuck transfer");
            self.funding.transfer = stored;
        }
    }

    /// Takes the unfinished transfer one step further, if the balances allow it.
    pub(super) async fn advance_transfer(&mut self, accounts: &PerSide<Option<AccountState>>) {
        let watch = self
            .funding
            .transfer
            .as_ref()
            .filter(|transfer| matches!(transfer.stage, Stage::Withdraw | Stage::Send))
            .map(|transfer| self.legs.get(transfer.from).wallet);
        self.funding.watched.send_if_modified(|watched| std::mem::replace(watched, watch) != watch);

        let Some(transfer) = self.funding.transfer.clone() else { return };
        if transfer.stage == Stage::Stuck {
            return;
        }
        let seen = self.observe(&transfer, accounts).await;
        match next_step(&transfer, &seen, &self.timing.funding.transfer) {
            Step::Wait(what) => self.funding.waiting_for = Some(what),
            Step::Withdraw(_) | Step::Send(_) if self.funding.signers.get(transfer.from).is_none() => {
                let note = format!(
                    "stuck in {}: the {} wallet's own key is not available to this run",
                    transfer.stage, transfer.from
                );
                self.record_stuck(Transfer { stage: Stage::Stuck, note: Some(note), ..transfer }).await;
            }
            Step::Withdraw(next) => self.withdraw(next).await,
            Step::Send(next) => self.send(next).await,
            Step::CheckDeposit(next) => self.check_deposit(next).await,
            Step::Advance(next) => self.record_progress(next).await,
            Step::Stuck(next) => self.record_stuck(next).await,
        }
    }

    async fn observe(&mut self, transfer: &Transfer, accounts: &PerSide<Option<AccountState>>) -> Observed {
        let now_ms = now_ms();
        let sender = self.legs.get(transfer.from).wallet;
        let sender_core = (*self.funding.core.borrow())
            .filter(|reading| reading.wallet == sender)
            .map(|reading| Reading { amount: reading.amount, at_ms: reading.at_ms });
        // What the account holds, the margins parked in its positions included.
        // Rounds go on while a transfer is under way, and the available balance
        // alone moves by a whole margin whenever a position opens or closes,
        // which would pass for the transfer's money leaving or arriving.
        // What a round wins or loses when it closes still shows in it: a loss
        // the receiver realises while its credit is awaited makes the credit
        // look short, and that transfer ends up stuck for the operator to check.
        let balance = |side: Side| accounts.get(side).as_ref().map(|account| Holding::of(account).equity());
        let withdrawal = match (transfer.stage, transfer.withdraw_intent) {
            (Stage::Withdraw, Some(intent_id)) => self.withdrawal(intent_id).await,
            _ => None,
        };
        Observed {
            now_ms,
            sender_core,
            sender_balance: balance(transfer.from),
            receiver_balance: balance(transfer.to),
            withdrawal,
        }
    }

    /// Whether the withdrawal can still execute: from the intents being
    /// tracked, and after a restart or once it has ended, from the journal.
    async fn withdrawal(&mut self, intent_id: B256) -> Option<Withdrawal> {
        if self.inflight.iter().any(|entry| entry.intent_id == intent_id) {
            return Some(Withdrawal::InFlight);
        }
        let fate = self.store.intent_fate(intent_id).await;
        let fate = self.best_effort("reading the withdrawal's journal entry", fate)??;
        Some(Withdrawal::from_journal(
            &fate.status,
            fate.outcome.as_deref(),
            fate.created_at_ms,
            fate.settled_at_ms,
        ))
    }

    async fn withdraw(&mut self, next: Transfer) {
        let ttl_seconds = self.ttl_seconds;
        let Some(key) = self.funding.signers.get(next.from).clone() else { return };
        let leg = self.legs.get_mut(next.from);
        let message = WithdrawToCore {
            user: leg.wallet,
            amount: U256::from(next.amount_usdc) * U256::from(USDC_UNIT),
            nonce: leg.nonces.allocate(),
            deadline: unix_seconds() + ttl_seconds,
        };
        let intent = match key.sign_withdraw_to_core(&message) {
            Ok(intent) => intent,
            Err(error) => {
                error!(%error, "signing the withdrawal failed; not withdrawing");
                return;
            }
        };
        let next = Transfer { withdraw_intent: Some(intent.intent_id), ..next };
        // Write-ahead: the attempt and its intent are on disk, together, before
        // the request leaves. Without that record nothing is sent.
        let recorded = self.store.begin_withdrawal(&next, &intent).await;
        if self.best_effort("recording a withdrawal", recorded).is_none() {
            return;
        }
        self.inflight.push(inflight(next.from, Kind::Withdraw, &intent));
        self.funding.transfer = Some(next.clone());
        let what = format!(
            "withdrawing {} USDC from the {} account to its wallet's HyperCore account (attempt {})",
            next.amount_usdc, next.from, next.attempts
        );
        self.transfer_event(&next, &what).await;
        let result = self.client.submit_intent(&intent).await;
        self.record_submission(next.from, &intent, result).await;
    }

    async fn send(&mut self, next: Transfer) {
        let (Some(key), Some(nonce)) = (self.funding.signers.get(next.from).clone(), next.send_nonce) else {
            return;
        };
        // Where the funds go is read from the chain, for the account on the other
        // side, every time. Until that has worked nothing is recorded or sent.
        let wallets = Wallets { long: self.legs.long.wallet, short: self.legs.short.wallet };
        let chain = &self.funding.chain;
        let send = match DepositSend::to_counterpart(chain, wallets, next.from, next.amount_usdc, nonce).await
        {
            Ok(send) => send,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the deposit address could not be read; not sending yet");
                self.funding.waiting_for = Some(format!("reading the {} account's deposit address", next.to));
                return;
            }
        };
        let recorded = self.store.save_transfer(&next).await;
        if self.best_effort("recording a Hyperliquid send", recorded).is_none() {
            return;
        }
        self.funding.transfer = Some(next.clone());
        let what = format!(
            "sending {} USDC on Hyperliquid from the {} wallet to the {} account's deposit address {} \
             (attempt {})",
            next.amount_usdc,
            next.from,
            next.to,
            send.destination(),
            next.attempts
        );
        self.transfer_event(&next, &what).await;

        let hypercore = self.funding.hypercore.clone();
        let Err(error) = hypercore.send(&key, &send).await else { return };
        // Not proof that nothing moved: the balance decides. The error is kept
        // so that a transfer that ends up stuck can say what Hyperliquid answered.
        let note: String = format!("{error:#}").chars().take(NOTE_LENGTH).collect();
        warn!(id = next.id, %note, "Hyperliquid did not confirm the send; the balance will tell");
        let noted = self.store.note_transfer(next.id, &note).await;
        self.best_effort("recording a send error", noted);
        self.transfer_event(&next, &format!("Hyperliquid did not confirm the send: {note}")).await;
        if let Some(transfer) = &mut self.funding.transfer {
            transfer.note = Some(note);
        }
    }

    async fn check_deposit(&mut self, next: Transfer) {
        let recorded = self.store.save_transfer(&next).await;
        if self.best_effort("recording a deposit check", recorded).is_none() {
            return;
        }
        let receiver = self.legs.get(next.to).wallet;
        self.funding.waiting_for = Some(format!("waiting for the relayer to credit the {} account", next.to));
        self.funding.transfer = Some(next);
        if let Err(error) = self.client.check_deposit(receiver).await {
            warn!(%error, "asking the relayer to check the deposit address failed");
        }
    }

    async fn record_progress(&mut self, next: Transfer) {
        let recorded = self.store.save_transfer(&next).await;
        if self.best_effort("recording a transfer's progress", recorded).is_none() {
            return;
        }
        let (amount, from, to) = (next.amount_usdc, next.from, next.to);
        let what = match next.stage {
            Stage::Send => {
                format!("the withdrawal arrived: {amount} USDC are in the {from} wallet's HyperCore account")
            }
            Stage::Credit => {
                format!(
                    "{amount} USDC left the {from} wallet on Hyperliquid; waiting for the {to} account's credit"
                )
            }
            Stage::Done => {
                let seconds = (now_ms() - next.started_at_ms) / 1000;
                format!("done: the {to} account was credited, {seconds} s after the transfer started")
            }
            stage => format!("now in {stage}"),
        };
        self.transfer_event(&next, &what).await;
        self.funding.waiting_for = None;
        self.funding.transfer = (next.stage != Stage::Done).then_some(next);
    }

    async fn record_stuck(&mut self, next: Transfer) {
        let recorded = self.store.save_transfer(&next).await;
        if self.best_effort("recording a stuck transfer", recorded).is_none() {
            return;
        }
        let note = next.note.clone().unwrap_or_else(|| "stuck".to_owned());
        error!(id = next.id, "transfer {note}; deal with the funds, then run `papertrade rebalance abandon`");
        self.transfer_event(&next, &note).await;
        self.funding.waiting_for = None;
        self.funding.transfer = Some(next);
    }

    async fn transfer_event(&mut self, transfer: &Transfer, what: &str) {
        let summary = format!("transfer #{} {what}", transfer.id);
        info!("{summary}");
        let recorded = self.store.record_event("transfer", &summary, None).await;
        self.best_effort("recording a transfer event", recorded);
    }

    /// Registers the bot's session key for each account whose wallet key is
    /// here, when the key is not registered or is about to expire.
    pub(super) async fn register_session_keys(
        &mut self,
        accounts: &PerSide<Option<AccountState>>,
        trading: Option<&TradingState>,
    ) {
        // Only while the relayer is known to take registrations.
        if !trading.is_some_and(|trading| !trading.paused_actions.contains("registerSessionKey")) {
            return;
        }
        for side in Side::BOTH {
            let (Some(key), Some(account)) =
                (self.funding.signers.get(side).clone(), accounts.get(side).as_ref())
            else {
                continue;
            };
            let (now, session_key) = (unix_seconds(), self.keys.get(side).session.address());
            let pending =
                self.inflight.iter().any(|entry| entry.side == side && entry.kind == Kind::Register);
            if pending
                || now_ms() < *self.funding.register_allowed_at_ms.get(side)
                || !session_key_needs_registration(account, session_key, now)
            {
                continue;
            }
            // One attempt per interval, whatever becomes of it.
            let retry_ms = duration_ms(self.timing.funding.register_retry);
            *self.funding.register_allowed_at_ms.get_mut(side) = now_ms().saturating_add(retry_ms);
            let ttl_seconds = self.ttl_seconds;
            let leg = self.legs.get_mut(side);
            let message = RegisterSessionKey {
                user: leg.wallet,
                key: session_key,
                expiry: now + SESSION_KEY_TTL_SECONDS,
                feeAddress: Address::ZERO,
                nonce: leg.nonces.allocate(),
                deadline: now + ttl_seconds,
            };
            let intent = match key.sign_register_session_key(&message) {
                Ok(intent) => intent,
                Err(error) => {
                    error!(%side, %error, "signing the session key registration failed");
                    continue;
                }
            };
            let journaled = self.store.journal_intent(side, &intent).await;
            if self.best_effort("journaling a session key registration", journaled).is_none() {
                continue;
            }
            self.inflight.push(inflight(side, Kind::Register, &intent));
            info!(%side, %session_key, "registering the session key with the wallet's own key");
            let result = self.client.submit_intent(&intent).await;
            self.record_submission(side, &intent, result).await;
            // The interval runs from when the attempt was over, however long it took.
            *self.funding.register_allowed_at_ms.get_mut(side) = now_ms().saturating_add(retry_ms);
        }
    }

    /// Fills in what the snapshot says about funds and the transfer.
    pub(super) fn describe_funding(&self, snapshot: &mut Snapshot, funds: Option<&FundsView>) {
        snapshot.funds = funds.map(|view| {
            // What an account holds with its positions closed at their entry: while a
            // round is open the margins are only parked.
            let (long, short) = (view.long.equity(), view.short.equity());
            let (margin, buffer) = (self.params.margin(), self.params.round_loss());
            FundsSnapshot {
                total_usd: format_amount(long + short),
                difference_usd: format_amount(long.abs_diff(short)),
                rounds_affordable: rounds_affordable(long, short, margin, buffer),
                verdict: view.verdict.map_or("off", Funds::as_str).to_owned(),
                can_rebalance: self.can_rebalance(),
                planned: view.planned().map(|(from, to, amount_usdc, hold)| PlannedTransfer {
                    from,
                    to,
                    amount_usd: format_amount(usd(amount_usdc)),
                    held_by: hold.map(|hold| hold.to_string()),
                }),
            }
        });
        snapshot.transfer = self.funding.transfer.as_ref().map(|transfer| TransferSnapshot {
            from: transfer.from,
            to: transfer.to,
            amount_usd: format_amount(usd(transfer.amount_usdc)),
            stage: transfer.stage.as_str().to_owned(),
            started_at_ms: transfer.started_at_ms,
            note: match transfer.stage {
                Stage::Stuck => transfer.note.clone(),
                _ => self.funding.waiting_for.clone().or_else(|| transfer.note.clone()),
            },
        });
    }
}
