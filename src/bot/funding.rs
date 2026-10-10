//! The loop's side of funding: keeping the session keys registered, judging
//! whether funds should move between the two accounts, and carrying out a
//! transfer one recorded step at a time.
//!
//! What to do is decided by pure functions ([`funds`], [`hold`], [`next_step`]).
//! This module gathers what they look at, writes the record they hand back,
//! and only then acts. None of it waits for the legs, and the legs wait for
//! none of it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::{Address, B256, U256};
use anyhow::Result;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{error, info, warn};

use super::{Bot, PerSide, duration_ms, inflight, unix_seconds};
use crate::config::Config;
use crate::keystore::UnlockedKeys;
use crate::protocol::account::AccountState;
use crate::protocol::chain::Chain;
use crate::protocol::client::{Client, SubmitError};
use crate::protocol::deployment::{SESSION_KEY_TTL_SECONDS, usd};
use crate::protocol::intents::{RegisterSessionKey, SignedIntent, USDC_UNIT, WithdrawToCore};
use crate::protocol::wire::TradingState;
use crate::status::{FundsSnapshot, PlannedTransfer, Snapshot, TransferSnapshot, format_amount};
use crate::store::{Store, now_ms};
use crate::strategy::decide::{Side, has_queued_trade, session_key_needs_registration};
use crate::strategy::inflight::{Inflight, Kind};
use crate::strategy::rebalance::{Funds, Hold, Holding, Readiness, funds, hold, reserved, rounds_affordable};
use crate::strategy::transfer::{
    Limits, Observed, Reading, Stage, Step, Transfer, Withdrawal, holds_exit, next_step, wallet_guard,
};
use crate::wallet::hypercore::HyperCore;
use crate::wallet::{DepositSend, WalletSigner, Wallets};

/// An error of an action is kept with the transfer's record; this much of it.
const NOTE_LENGTH: usize = 200;

fn indexed_at_ms(account: &AccountState) -> i64 {
    i64::try_from(account.indexed_through_timestamp).unwrap_or(i64::MAX).saturating_mul(1_000)
}

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
    /// Each wallet's own key, where the automation is on and the key was unlocked.
    signers: PerSide<Option<WalletSigner>>,
    /// `PT_REBALANCE`: whether a transfer may be started. One that has started
    /// is carried on either way.
    rebalance: bool,
    /// The transfer that was last put on the timeline as called for but not started.
    reported_held: Option<String>,
    /// The unfinished transfer last written by the owner, or an id-zero claim
    /// while creation is awaiting storage. Both reserve the withdrawal amount.
    transfer: Option<Transfer>,
    /// What the transfer is waiting for, for the status snapshot.
    waiting_for: Option<String>,
    /// The wallet whose HyperCore balance is followed; `None` stops the polling.
    watched: watch::Sender<Option<Address>>,
    core: watch::Receiver<Option<CoreReading>>,
    register_allowed_at_ms: PerSide<i64>,
    commands: mpsc::Sender<Work>,
    updates: mpsc::UnboundedReceiver<Update>,
    busy: bool,
    pending: Work,
    journal: Vec<JournalUpdate>,
    owned_intents: HashSet<B256>,
}

#[derive(Default)]
struct Work {
    transfer: Option<TransferWork>,
    registrations: Vec<(Side, RegisterSessionKey)>,
    journal: Vec<JournalUpdate>,
    events: Vec<String>,
}

enum TransferWork {
    Start { from: Side, to: Side, amount_usdc: u64, seen: Observed, nonce: U256, deadline: u64 },
    Drive { transfer: Box<Transfer>, seen: Observed, in_flight: bool, nonce: U256, deadline: u64 },
    Unbound { transfer: Box<Transfer>, in_flight: bool, indexed_at_ms: Option<i64> },
    ReadStuck,
}

enum JournalUpdate {
    Outcome(B256, String),
    Seen(B256),
    Settled(B256),
}

enum Update {
    Transfer(Option<Transfer>),
    Waiting(Option<String>),
    Started(Inflight),
    Submission { id: B256, accepted: Option<U256>, rejected: bool },
    RegisterAllowed(Side, i64),
    Finished,
}

/// There is one execution owner for funding. It records and executes one batch
/// at a time; the decision loop exchanges only memory messages with it.
struct Executor {
    store: Store,
    client: Client,
    chain: Chain,
    hypercore: Arc<dyn HyperCore>,
    signers: PerSide<Option<WalletSigner>>,
    wallets: Wallets,
    limits: Limits,
    register_retry_ms: i64,
    updates: mpsc::UnboundedSender<Update>,
}

impl Funding {
    /// Picks up the transfer an earlier run left unfinished, and when each
    /// account last tried to register its session key.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start(
        tasks: &mut JoinSet<()>,
        store: &Store,
        client: Client,
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
        let signers = PerSide { long: signer(Side::Long), short: signer(Side::Short) };
        let (commands, work) = mpsc::channel(1);
        let (updated, updates) = mpsc::unbounded_channel();
        let executor = Executor {
            store: store.clone(),
            client,
            chain,
            hypercore,
            signers: PerSide { long: signers.long.clone(), short: signers.short.clone() },
            wallets: Wallets { long: keys.long.owner, short: keys.short.owner },
            limits: timing.transfer,
            register_retry_ms: retry,
            updates: updated,
        };
        tasks.spawn(executor.run(work));
        Ok(Self {
            signers,
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
            commands,
            updates,
            busy: false,
            pending: Work::default(),
            journal: Vec::new(),
            owned_intents: store
                .unresolved_intents()
                .await?
                .into_iter()
                .filter(|entry| !entry.kind.is_trade())
                .map(|entry| entry.intent_id)
                .collect(),
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
        self.funding
            .transfer
            .as_ref()
            .map_or(U256::ZERO, |transfer| reserved(transfer, self.legs.get(side).wallet))
    }

    /// Whether the transfer that is under way keeps a run the operator asked to
    /// stop from ending yet.
    pub(super) fn transfer_holds_exit(&self) -> bool {
        holds_exit(self.funding.transfer.as_ref().map(|transfer| transfer.stage), self.dry_run)
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
            cashflows_available: long.funding_totals.is_some() && short.funding_totals.is_some(),
            mode: self.mode,
            trading: trading_state,
            dry_run: self.dry_run,
        });
        Some(view(Some(verdict), held))
    }

    /// Applies the execution owner's updates without waiting for its database
    /// or requests. An intent joins the usual tracker before its request leaves.
    pub(super) fn collect_funding(&mut self) {
        while let Ok(update) = self.funding.updates.try_recv() {
            match update {
                Update::Transfer(transfer) => self.funding.transfer = transfer,
                Update::Waiting(note) => self.funding.waiting_for = note,
                Update::Started(entry) => {
                    self.funding.owned_intents.insert(entry.intent_id);
                    self.inflight.push(entry);
                }
                Update::Submission { id, accepted, rejected } => {
                    if rejected {
                        self.inflight.retain(|entry| entry.intent_id != id);
                    } else if let Some(revision) = accepted {
                        for entry in self.inflight.iter_mut().filter(|entry| entry.intent_id == id) {
                            entry.queue_revision = Some(revision);
                        }
                    }
                }
                Update::RegisterAllowed(side, at_ms) => {
                    *self.funding.register_allowed_at_ms.get_mut(side) = at_ms;
                }
                Update::Finished => self.funding.busy = false,
            }
        }
    }

    pub(super) fn is_funding_intent(&self, id: B256) -> bool {
        self.funding.owned_intents.contains(&id)
    }

    pub(super) fn funding_outcome(&mut self, id: B256, outcome: &str) {
        self.funding.journal.push(JournalUpdate::Outcome(id, outcome.to_owned()));
    }

    pub(super) fn funding_seen(&mut self, id: B256) {
        self.funding.journal.push(JournalUpdate::Seen(id));
    }

    pub(super) fn funding_settled(&mut self, id: B256) {
        self.funding.journal.push(JournalUpdate::Settled(id));
    }

    /// Puts a held plan on the same execution queue as funding's other writes.
    pub(super) fn report_held_transfer(&mut self, funds: Option<&FundsView>) {
        let Some(funds) = funds else { return };
        let held = match funds.planned() {
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
            self.funding.pending.events.push(summary.clone());
        }
        self.funding.reported_held = held;
    }

    /// Claims a transfer and its reservation as soon as work is dispatched,
    /// before the execution owner can wait on storage. A failed creation clears
    /// the claim; a cancelled owner stops this run and startup reloads the journal.
    pub(super) fn start_transfer(&mut self, from: Side, to: Side, amount_usdc: u64) {
        if self.funding.busy || self.funding.transfer.is_some() {
            return;
        }
        self.funding.transfer = Some(Transfer {
            from_wallet: Some(self.legs.get(from).wallet),
            to_wallet: Some(self.legs.get(to).wallet),
            ..Transfer::new(0, from, to, amount_usdc, now_ms())
        });
    }

    /// Takes the unfinished transfer one recorded step further in its owner.
    pub(super) fn advance_transfer(&mut self, accounts: &PerSide<Option<AccountState>>) {
        let watched = self
            .funding
            .transfer
            .as_ref()
            .filter(|transfer| {
                transfer
                    .matches_wallets(self.legs.get(transfer.from).wallet, self.legs.get(transfer.to).wallet)
            })
            .filter(|transfer| matches!(transfer.stage, Stage::Withdraw | Stage::Send))
            .map(|transfer| self.legs.get(transfer.from).wallet);
        self.funding.watched.send_if_modified(|current| std::mem::replace(current, watched) != watched);
        if self.funding.busy {
            return;
        }
        let Some(transfer) = self.funding.transfer.clone() else { return };
        if transfer.stage == Stage::Stuck {
            self.funding.pending.transfer = Some(TransferWork::ReadStuck);
            return;
        }
        let in_flight = transfer
            .withdraw_intent
            .is_some_and(|id| self.inflight.iter().any(|entry| entry.intent_id == id));
        if !transfer.matches_wallets(self.legs.get(transfer.from).wallet, self.legs.get(transfer.to).wallet) {
            // Without persisted owners, either current wallet might have sent
            // the old withdrawal. Both views must catch up before releasing it.
            let indexed_at_ms = accounts
                .long
                .as_ref()
                .zip(accounts.short.as_ref())
                .map(|(long, short)| indexed_at_ms(long).min(indexed_at_ms(short)));
            self.funding.pending.transfer =
                Some(TransferWork::Unbound { transfer: Box::new(transfer), in_flight, indexed_at_ms });
            return;
        }
        let seen = self.observe(&transfer, accounts);
        let nonce = if transfer.stage == Stage::Withdraw && !in_flight {
            self.legs.get_mut(transfer.from).nonces.allocate()
        } else {
            U256::ZERO
        };
        let deadline = unix_seconds() + self.ttl_seconds;
        self.funding.pending.transfer = Some(if transfer.id == 0 {
            TransferWork::Start {
                from: transfer.from,
                to: transfer.to,
                amount_usdc: transfer.amount_usdc,
                seen,
                nonce,
                deadline,
            }
        } else {
            TransferWork::Drive { transfer: Box::new(transfer), seen, in_flight, nonce, deadline }
        });
    }

    fn observe(&self, transfer: &Transfer, accounts: &PerSide<Option<AccountState>>) -> Observed {
        let sender = self.legs.get(transfer.from).wallet;
        let sender_core = (*self.funding.core.borrow())
            .filter(|reading| reading.wallet == sender)
            .map(|reading| Reading { amount: reading.amount, at_ms: reading.at_ms });
        let totals = |side: Side| accounts.get(side).as_ref()?.funding_totals.as_ref();
        Observed {
            now_ms: now_ms(),
            sender_core,
            sender_withdrawn: totals(transfer.from).map(|totals| totals.withdrawn),
            sender_indexed_at_ms: accounts.get(transfer.from).as_ref().map(indexed_at_ms),
            receiver_deposited: totals(transfer.to).map(|totals| totals.deposited),
            receiver_indexed_at_ms: accounts.get(transfer.to).as_ref().map(indexed_at_ms),
            withdrawal: None,
        }
    }

    /// Allocates from the same per-wallet nonce sequence as trading. The owner
    /// receives an exact request and never chooses another nonce while it waits.
    pub(super) fn register_session_keys(
        &mut self,
        accounts: &PerSide<Option<AccountState>>,
        trading: Option<&TradingState>,
    ) {
        if self.funding.busy
            || !trading.is_some_and(|state| !state.paused_actions.contains("registerSessionKey"))
        {
            return;
        }
        for side in Side::BOTH {
            let (Some(_), Some(account)) =
                (self.funding.signers.get(side).as_ref(), accounts.get(side).as_ref())
            else {
                continue;
            };
            let (now, key) = (unix_seconds(), self.keys.get(side).session.address());
            let pending =
                self.inflight.iter().any(|entry| entry.side == side && entry.kind == Kind::Register);
            if pending
                || now_ms() < *self.funding.register_allowed_at_ms.get(side)
                || !session_key_needs_registration(account, key, now)
            {
                continue;
            }
            *self.funding.register_allowed_at_ms.get_mut(side) =
                now_ms().saturating_add(duration_ms(self.timing.funding.register_retry));
            let leg = self.legs.get_mut(side);
            self.funding.pending.registrations.push((
                side,
                RegisterSessionKey {
                    user: leg.wallet,
                    key,
                    expiry: now + SESSION_KEY_TTL_SECONDS,
                    feeAddress: Address::ZERO,
                    nonce: leg.nonces.allocate(),
                    deadline: now + self.ttl_seconds,
                },
            ));
        }
    }

    pub(super) fn dispatch_funding(&mut self) {
        if self.funding.busy {
            return;
        }
        let mut work = std::mem::take(&mut self.funding.pending);
        work.journal = std::mem::take(&mut self.funding.journal);
        if work.transfer.is_none()
            && work.registrations.is_empty()
            && work.journal.is_empty()
            && work.events.is_empty()
        {
            return;
        }
        // A capacity-one channel plus this claim allows only one execution batch.
        match self.funding.commands.try_send(work) {
            Ok(()) => self.funding.busy = true,
            Err(error) => error!(%error, "the supervised funding owner cannot receive work"),
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

impl Executor {
    async fn run(self, mut work: mpsc::Receiver<Work>) {
        while let Some(work) = work.recv().await {
            for update in work.journal {
                let recorded = match update {
                    JournalUpdate::Outcome(id, outcome) => self.store.set_outcome(id, &outcome).await,
                    JournalUpdate::Seen(id) => self.store.mark_seen_queued(id).await,
                    JournalUpdate::Settled(id) => self.store.mark_settled(id).await,
                };
                if let Err(error) = recorded {
                    error!(%error, "updating the funding intent journal failed");
                }
            }
            for summary in work.events {
                if let Err(error) = self.store.record_event("transfer", &summary, None).await {
                    error!(%error, "recording a planned transfer failed");
                }
            }
            if let Some(transfer) = work.transfer {
                let result = self.transfer(transfer).await;
                if let Err(error) = result {
                    error!(error = %format_args!("{error:#}"), "the funding step failed; no unrecorded request is sent");
                }
            }
            for (side, message) in work.registrations {
                if let Err(error) = self.register(side, message).await {
                    error!(%side, error = %format_args!("{error:#}"), "registering the session key failed");
                }
                self.updated(Update::RegisterAllowed(side, now_ms().saturating_add(self.register_retry_ms)));
            }
            self.updated(Update::Finished);
        }
    }

    fn updated(&self, update: Update) {
        // Closing the receiver means this run is stopping. Its JoinSet cancels
        // this owner, leaving any journaled request unknown for startup to track.
        let _ = self.updates.send(update);
    }

    async fn transfer(&self, work: TransferWork) -> Result<()> {
        let (transfer, mut seen, in_flight, nonce, deadline) = match work {
            TransferWork::Start { from, to, amount_usdc, seen, nonce, deadline } => {
                let transfer = match self
                    .store
                    .create_transfer(from, to, amount_usdc, self.wallets.of(from), self.wallets.of(to))
                    .await
                {
                    Ok(transfer) => transfer,
                    Err(error) => {
                        self.updated(Update::Transfer(None));
                        return Err(error);
                    }
                };
                self.updated(Update::Transfer(Some(transfer.clone())));
                self.event(
                    &transfer,
                    &format!("planned: {amount_usdc} USDC from the {from} account to the {to} account"),
                )
                .await;
                (transfer, seen, false, nonce, deadline)
            }
            TransferWork::Drive { transfer, seen, in_flight, nonce, deadline } => {
                (*transfer, seen, in_flight, nonce, deadline)
            }
            TransferWork::Unbound { transfer, in_flight, indexed_at_ms } => {
                let withdrawal = self.withdrawal(&transfer, in_flight).await?;
                if let Some(step) = wallet_guard(
                    &transfer,
                    self.wallets.of(transfer.from),
                    self.wallets.of(transfer.to),
                    withdrawal,
                    indexed_at_ms,
                    now_ms(),
                    &self.limits,
                ) {
                    match step {
                        Step::Wait(note) => self.updated(Update::Waiting(Some(note))),
                        Step::Stuck(next) => self.progress(next).await?,
                        _ => unreachable!("wallet guard never authorizes a funding action"),
                    }
                }
                return Ok(());
            }
            TransferWork::ReadStuck => {
                self.updated(Update::Transfer(self.store.active_transfer().await?));
                return Ok(());
            }
        };
        if transfer.stage == Stage::Withdraw {
            seen.withdrawal = self.withdrawal(&transfer, in_flight).await?;
        }
        // Storage can be slow while the trade loop keeps going. Freshness is
        // judged when this owner acts, rather than at dispatch time.
        seen.now_ms = now_ms();
        match next_step(&transfer, &seen, &self.limits) {
            Step::Wait(note) => self.updated(Update::Waiting(Some(note))),
            Step::Withdraw(_) | Step::Send(_) if self.signers.get(transfer.from).is_none() => {
                let note = format!(
                    "stuck in {}: the {} wallet's own key is not available to this run",
                    transfer.stage, transfer.from
                );
                self.progress(Transfer { stage: Stage::Stuck, note: Some(note), ..transfer }).await?;
            }
            Step::Withdraw(next) => self.withdraw(next, nonce, deadline).await?,
            Step::Send(next) => self.send(next).await?,
            Step::CheckDeposit(next) => self.check_deposit(next).await?,
            Step::Advance(next) | Step::Stuck(next) => self.progress(next).await?,
        }
        Ok(())
    }

    async fn withdrawal(&self, transfer: &Transfer, in_flight: bool) -> Result<Option<Withdrawal>> {
        if in_flight {
            return Ok(Some(Withdrawal::InFlight));
        }
        let Some(id) = transfer.withdraw_intent else { return Ok(None) };
        Ok(self.store.intent_fate(id).await?.map(|fate| {
            Withdrawal::from_journal(
                &fate.status,
                fate.outcome.as_deref(),
                fate.created_at_ms,
                fate.settled_at_ms,
            )
        }))
    }

    async fn withdraw(&self, next: Transfer, nonce: U256, deadline: u64) -> Result<()> {
        let Some(key) = self.signers.get(next.from).as_ref() else { return Ok(()) };
        let message = WithdrawToCore {
            user: self.wallets.of(next.from),
            amount: U256::from(next.amount_usdc) * U256::from(USDC_UNIT),
            nonce,
            deadline,
        };
        let intent = key.sign_withdraw_to_core(&message)?;
        let next = Transfer { withdraw_intent: Some(intent.intent_id), ..next };
        // Local signing has no external effect. The attempt and signed intent
        // commit atomically before the request can leave this owner.
        self.store.begin_withdrawal(&next, &intent).await?;
        self.updated(Update::Transfer(Some(next.clone())));
        self.updated(Update::Started(inflight(next.from, Kind::Withdraw, &intent)));
        self.event(
            &next,
            &format!(
                "withdrawing {} USDC from the {} account to its wallet's HyperCore account (attempt {})",
                next.amount_usdc, next.from, next.attempts,
            ),
        )
        .await;
        let result = self.client.submit_intent(&intent).await;
        self.submission(next.from, &intent, result).await;
        Ok(())
    }

    async fn send(&self, next: Transfer) -> Result<()> {
        let (Some(key), Some(nonce)) = (self.signers.get(next.from).as_ref(), next.send_nonce) else {
            return Ok(());
        };
        let send = match DepositSend::to_counterpart(
            &self.chain,
            self.wallets,
            next.from,
            next.amount_usdc,
            nonce,
        )
        .await
        {
            Ok(send) => send,
            Err(error) => {
                warn!(error = %format_args!("{error:#}"), "the deposit address could not be read; not sending yet");
                self.updated(Update::Waiting(Some(format!(
                    "reading the {} account's deposit address",
                    next.to
                ))));
                return Ok(());
            }
        };
        self.store.save_transfer(&next).await?;
        self.updated(Update::Transfer(Some(next.clone())));
        self.event(&next, &format!(
            "sending {} USDC on Hyperliquid from the {} wallet to the {} account's deposit address {} (attempt {})",
            next.amount_usdc, next.from, next.to, send.destination(), next.attempts,
        )).await;
        if let Err(error) = self.hypercore.send(key, &send).await {
            let note: String = format!("{error:#}").chars().take(NOTE_LENGTH).collect();
            warn!(id = next.id, %note, "Hyperliquid did not confirm the send; the balance will tell");
            self.store.note_transfer(next.id, &note).await?;
            self.updated(Update::Transfer(Some(Transfer { note: Some(note.clone()), ..next.clone() })));
            self.event(&next, &format!("Hyperliquid did not confirm the send: {note}")).await;
        }
        Ok(())
    }

    async fn check_deposit(&self, next: Transfer) -> Result<()> {
        self.store.save_transfer(&next).await?;
        self.updated(Update::Waiting(Some(format!(
            "waiting for the relayer to credit the {} account",
            next.to
        ))));
        self.updated(Update::Transfer(Some(next.clone())));
        if let Err(error) = self.client.check_deposit(self.wallets.of(next.to)).await {
            warn!(%error, "asking the relayer to check the deposit address failed");
        }
        Ok(())
    }

    async fn progress(&self, next: Transfer) -> Result<()> {
        self.store.save_transfer(&next).await?;
        self.updated(Update::Waiting(None));
        self.updated(Update::Transfer((next.stage != Stage::Done).then_some(next.clone())));
        let (amount, from, to) = (next.amount_usdc, next.from, next.to);
        let what = match next.stage {
            Stage::Send => {
                format!("the withdrawal arrived: {amount} USDC are in the {from} wallet's HyperCore account")
            }
            Stage::Credit => format!(
                "{amount} USDC left the {from} wallet on Hyperliquid; waiting for the {to} account's credit"
            ),
            Stage::Done => format!(
                "done: the {to} account was credited, {} s after the transfer started",
                (now_ms() - next.started_at_ms) / 1000
            ),
            Stage::Stuck => {
                let note = next.note.clone().unwrap_or_else(|| "stuck".to_owned());
                error!(
                    id = next.id,
                    "transfer {note}; deal with the funds, then run `papertrade rebalance abandon`"
                );
                note
            }
            stage => format!("now in {stage}"),
        };
        self.event(&next, &what).await;
        Ok(())
    }

    async fn event(&self, transfer: &Transfer, what: &str) {
        let summary = format!("transfer #{} {what}", transfer.id);
        info!("{summary}");
        if let Err(error) = self.store.record_event("transfer", &summary, None).await {
            error!(%error, "recording a transfer event failed");
        }
    }

    async fn register(&self, side: Side, message: RegisterSessionKey) -> Result<()> {
        let Some(key) = self.signers.get(side).as_ref() else { return Ok(()) };
        let intent = key.sign_register_session_key(&message)?;
        self.store.journal_intent(side, &intent).await?;
        self.updated(Update::Started(inflight(side, Kind::Register, &intent)));
        info!(%side, session_key = %message.key, "registering the session key with the wallet's own key");
        let result = self.client.submit_intent(&intent).await;
        self.submission(side, &intent, result).await;
        Ok(())
    }

    /// Uses the same intent journal and tracking semantics as trading; only
    /// the owner of these requests waits for storage and the relayer.
    async fn submission(
        &self,
        side: Side,
        intent: &SignedIntent,
        result: Result<crate::protocol::wire::IntentReceipt, SubmitError>,
    ) {
        let id = intent.intent_id;
        let action = intent.action.as_str();
        let (recorded, summary, accepted, rejected) = match result {
            Ok(receipt) => {
                info!(%side, action, %id, state = %receipt.state, "intent accepted");
                (
                    self.store.mark_accepted(id, receipt.queue_revision.0).await,
                    format!("{side} {action} accepted"),
                    Some(receipt.queue_revision.0),
                    false,
                )
            }
            Err(SubmitError::Rejected { code, reason }) => {
                let detail = reason.map_or_else(|| code.clone(), |reason| format!("{code}: {reason}"));
                warn!(%side, action, %id, %detail, "intent rejected");
                (
                    self.store.mark_not_accepted(id, "rejected", &detail).await,
                    format!("{side} {action} rejected: {detail}"),
                    None,
                    true,
                )
            }
            Err(SubmitError::Unknown(detail)) => {
                error!(%side, action, %id, %detail, "intent outcome unknown; it may still execute until its deadline");
                (
                    self.store.mark_not_accepted(id, "unknown", &detail).await,
                    format!("{side} {action} outcome unknown: {detail}"),
                    None,
                    false,
                )
            }
        };
        self.updated(Update::Submission { id, accepted, rejected });
        if let Err(error) = recorded {
            error!(%error, "recording the funding submission failed");
        }
        if let Err(error) = self.store.record_event("intent", &summary, None).await {
            error!(%error, "recording a funding intent event failed");
        }
    }
}
