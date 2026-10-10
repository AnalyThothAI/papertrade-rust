//! The loop: gather what is known, ask `decide`, carry out the answer, record it.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, I256, U256};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};
use serde_json::json;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::market::{Quote, follow_price, format_price, price_decimals};
use crate::protocol::account::AccountState;
use crate::protocol::client::{Client, SubmitError};
use crate::protocol::intents::{
    Close, NonceSequence, OpenPosition, SignedIntent, sign_close, sign_open_position,
};
use crate::protocol::stream::follow_account;
use crate::protocol::wire::{IntentOutcome, IntentReceipt, TradingState};
use crate::session_keys;
use crate::status::{
    AccountSnapshot, LegSnapshot, PositionSnapshot, Snapshot, TradingSnapshot, format_amount,
    format_signed_amount,
};
use crate::store::{Store, now_ms};
use crate::strategy::decide::{
    CloseOrder, CloseReason, Decision, LegView, Market, Mode, Side, StrategyParams, View, decide,
};
use crate::strategy::inflight::{Inflight, track};

const TICK: Duration = Duration::from_millis(250);
const STATE_POLL: Duration = Duration::from_secs(5);
const STATUS_INTERVAL: Duration = Duration::from_secs(1);
const LEDGER_INTERVAL: Duration = Duration::from_secs(60);
/// A refused close is tried again, but not on every tick.
const CLOSE_RETRY: Duration = Duration::from_secs(5);

/// A value per account role.
struct PerSide<T> {
    long: T,
    short: T,
}

impl<T> PerSide<T> {
    fn get(&self, side: Side) -> &T {
        match side {
            Side::Long => &self.long,
            Side::Short => &self.short,
        }
    }

    fn get_mut(&mut self, side: Side) -> &mut T {
        match side {
            Side::Long => &mut self.long,
            Side::Short => &mut self.short,
        }
    }
}

/// A remote value with the moment it was loaded.
type Polled<T> = watch::Receiver<Option<(T, Instant)>>;

/// Keeps a remote value fresh in the background.
fn poll<T, F, Fut>(tasks: &mut JoinSet<()>, what: &'static str, load: F) -> Polled<T>
where
    T: Send + Sync + 'static,
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = reqwest::Result<T>> + Send,
{
    let (sender, receiver) = watch::channel(None);
    tasks.spawn(async move {
        let mut every = interval(STATE_POLL);
        every.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            match load().await {
                Ok(value) => drop(sender.send_replace(Some((value, Instant::now())))),
                Err(error) => warn!(%error, "{what} poll failed"),
            }
        }
    });
    receiver
}

/// The latest value, unless it has missed three refreshes in a row.
fn fresh<T: Clone>(polled: &Polled<T>) -> Option<T> {
    polled
        .borrow()
        .as_ref()
        .filter(|(_, loaded_at)| loaded_at.elapsed() <= 3 * STATE_POLL)
        .map(|(value, _)| value.clone())
}

struct Leg {
    wallet: Address,
    signer: PrivateKeySigner,
    nonces: NonceSequence,
    account: watch::Receiver<Option<AccountState>>,
}

struct Bot {
    dry_run: bool,
    coin: String,
    params: StrategyParams,
    ttl_seconds: u64,
    market: Market,
    price_decimals: u32,
    client: Client,
    store: Store,
    legs: PerSide<Leg>,
    outcomes: mpsc::Receiver<IntentOutcome>,
    quote: watch::Receiver<Option<Quote>>,
    trading: Polled<TradingState>,
    queue_total: Polled<U256>,
    inflight: Vec<Inflight>,
    /// An open attempt can surface late; it must not be answered by another
    /// open, so opens are held back for one intent lifetime after each attempt.
    open_allowed_at_ms: i64,
    close_allowed_at: Instant,
    reported: String,
    status_at: Instant,
    /// Wakes the ledger task when a position opens or settles.
    ledger_wake: Arc<Notify>,
    open_position_count: usize,
}

/// Runs the strategy until `shutdown` resolves. The current tick always finishes.
pub async fn run(config: &Config, store: Store, shutdown: impl Future<Output = ()>) -> Result<()> {
    let client = Client::new()?;
    let trading_state = client.trading_state().await.context("fetching the trading state")?;
    let instrument = trading_state
        .instrument(&config.coin)
        .with_context(|| format!("{} is not listed on Papertrade", config.coin))?;
    let price_decimals = price_decimals(instrument.price_scale)
        .with_context(|| format!("price scale {} is not a power of ten", instrument.price_scale))?;

    let mut tasks = JoinSet::new();
    let (outcome_sender, outcomes) = mpsc::channel(64);
    let mut leg = |side: Side| -> Result<Leg> {
        let wallet = config.wallet(side)?;
        let (sender, account) = watch::channel(None);
        tasks.spawn(follow_account(client.account_stream_url(wallet), sender, outcome_sender.clone()));
        Ok(Leg {
            wallet,
            signer: session_keys::load(&config.keys_dir, wallet)?,
            nonces: NonceSequence::random(),
            account,
        })
    };
    let legs = PerSide { long: leg(Side::Long)?, short: leg(Side::Short)? };

    let (quote_sender, quote) = watch::channel(None);
    tasks.spawn(follow_price(config.coin.clone(), price_decimals, quote_sender));
    let trading = {
        let client = client.clone();
        poll(&mut tasks, "trading state", move || {
            let client = client.clone();
            async move { client.trading_state().await }
        })
    };
    let queue_total = {
        let client = client.clone();
        poll(&mut tasks, "queue summary", move || {
            let client = client.clone();
            async move { client.queue_total().await }
        })
    };

    let ledger_wake = Arc::new(Notify::new());
    tasks.spawn(follow_ledger(
        client.clone(),
        store.clone(),
        [(Side::Long, legs.long.wallet), (Side::Short, legs.short.wallet)],
        ledger_wake.clone(),
    ));

    let ttl_ms = i64::try_from(config.intent_ttl_seconds * 1000)?;
    let mut bot = Bot {
        dry_run: config.dry_run,
        coin: config.coin.clone(),
        params: config.strategy(),
        ttl_seconds: config.intent_ttl_seconds,
        market: Market { id: instrument.id, symbol: instrument.symbol.clone() },
        price_decimals,
        client,
        // Whatever an earlier run left in flight is tracked to its end.
        inflight: store.unresolved_intents().await?,
        open_allowed_at_ms: store.last_open_attempt_ms().await?.map_or(0, |at| at + ttl_ms),
        store,
        legs,
        outcomes,
        quote,
        trading,
        queue_total,
        close_allowed_at: Instant::now(),
        reported: String::new(),
        status_at: Instant::now(),
        ledger_wake,
        open_position_count: 0,
    };
    info!(
        coin = %bot.coin,
        dry_run = bot.dry_run,
        long = %bot.legs.long.wallet,
        short = %bot.legs.short.wallet,
        resumed_intents = bot.inflight.len(),
        "bot started"
    );

    let mut ticks = interval(TICK);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            _ = ticks.tick() => bot.tick().await?,
        }
    }
    if bot.open_position_count > 0 {
        warn!("stopping with open positions; nothing is watching them now");
    }
    Ok(())
}

impl Bot {
    async fn tick(&mut self) -> Result<()> {
        while let Ok(outcome) = self.outcomes.try_recv() {
            self.store.set_outcome(outcome.intent_id, &outcome.outcome).await?;
            if outcome.outcome != "succeeded" {
                warn!(intent_id = %outcome.intent_id, outcome = %outcome.outcome, "intent did not succeed");
            }
        }

        let accounts = PerSide {
            long: self.legs.long.account.borrow().clone(),
            short: self.legs.short.account.borrow().clone(),
        };
        self.track_inflight(&accounts).await?;

        let mode = self.store.mode().await?;
        let trading = fresh(&self.trading);
        let queue_total = fresh(&self.queue_total);
        let mid = (*self.quote.borrow()).and_then(Quote::fresh_mid);
        let leg = |side: Side| LegView {
            account: accounts.get(side).as_ref(),
            session_key: self.legs.get(side).signer.address(),
            awaiting_intent: self.inflight.iter().any(|entry| entry.side == side),
        };
        let view = View {
            now_seconds: unix_seconds(),
            mode,
            long: leg(Side::Long),
            short: leg(Side::Short),
            trading: trading.as_ref(),
            queue_total,
            mid,
            may_open: now_ms() >= self.open_allowed_at_ms,
        };
        let decision = decide(&view, &self.params, &self.market);

        let summary = self.describe(&decision);
        let changed = summary != self.reported;
        if changed {
            info!(mid = ?mid.map(|raw| format_price(raw, self.price_decimals)), "{summary}");
            let detail = json!({ "mode": mode.as_str(), "mid": mid });
            self.store.record_event("decision", &summary, Some(&detail)).await?;
            self.reported.clone_from(&summary);
        }
        if changed || self.status_at.elapsed() >= STATUS_INTERVAL {
            let snapshot =
                self.snapshot(mode, &decision, summary, &accounts, trading.as_ref(), queue_total, mid);
            self.store.write_status(&snapshot).await?;
            self.status_at = Instant::now();
        }

        let open_positions = Side::BOTH
            .iter()
            .filter_map(|&side| accounts.get(side).as_ref())
            .map(|a| a.positions.len())
            .sum();
        let positions_changed = open_positions != self.open_position_count;
        self.open_position_count = open_positions;
        if positions_changed {
            self.ledger_wake.notify_one();
        }

        if self.dry_run {
            return Ok(());
        }
        match decision {
            Decision::Wait(_) => Ok(()),
            Decision::Open { instrument_id, margin, leverage } => {
                self.open(instrument_id, margin, leverage).await
            }
            Decision::Close { closes, .. } if Instant::now() >= self.close_allowed_at => {
                self.close(closes).await
            }
            Decision::Close { .. } => Ok(()),
        }
    }

    /// Settles in-flight intents the accounts have caught up with.
    async fn track_inflight(&mut self, accounts: &PerSide<Option<AccountState>>) -> Result<()> {
        let mut still_inflight = Vec::with_capacity(self.inflight.len());
        for entry in std::mem::take(&mut self.inflight) {
            let (intent_id, side, was_seen) = (entry.intent_id, entry.side, entry.seen_queued);
            match track(entry, accounts.get(side).as_ref()) {
                Some(tracked) => {
                    if tracked.seen_queued && !was_seen {
                        self.store.mark_seen_queued(intent_id).await?;
                    }
                    still_inflight.push(tracked);
                }
                None => self.store.mark_settled(intent_id).await?,
            }
        }
        self.inflight = still_inflight;
        Ok(())
    }

    async fn open(&mut self, instrument_id: u32, margin: U256, leverage: u32) -> Result<()> {
        self.open_allowed_at_ms = now_ms() + i64::try_from(self.ttl_seconds * 1000)?;
        let deadline = unix_seconds() + self.ttl_seconds;
        let mut intents = Vec::with_capacity(2);
        for side in Side::BOTH {
            let leg = self.legs.get_mut(side);
            let message = OpenPosition {
                user: leg.wallet,
                instrumentId: instrument_id,
                isLong: side.is_long(),
                size: margin.try_into().context("margin exceeds uint128")?,
                leverage,
                useDebt: false,
                nonce: leg.nonces.allocate(),
                deadline,
            };
            let intent = sign_open_position(&leg.signer, &message)?;
            self.journal(side, &intent).await?;
            intents.push((side, intent));
        }
        // Both legs go out together so they have the best chance of the same block and price.
        let (first, second) =
            tokio::join!(self.client.submit_intent(&intents[0].1), self.client.submit_intent(&intents[1].1),);
        for ((side, intent), result) in intents.iter().zip([first, second]) {
            self.record_submission(*side, intent, result).await?;
        }
        Ok(())
    }

    /// Closes go out in the decided order, one after the other.
    async fn close(&mut self, closes: Vec<CloseOrder>) -> Result<()> {
        self.close_allowed_at = Instant::now() + CLOSE_RETRY;
        let deadline = unix_seconds() + self.ttl_seconds;
        for CloseOrder { side, position_ids } in closes {
            let leg = self.legs.get_mut(side);
            let message =
                Close { user: leg.wallet, positionIds: position_ids, nonce: leg.nonces.allocate(), deadline };
            let intent = sign_close(&leg.signer, &message)?;
            self.journal(side, &intent).await?;
            let result = self.client.submit_intent(&intent).await;
            self.record_submission(side, &intent, result).await?;
        }
        Ok(())
    }

    /// Write-ahead: the intent is on disk and in flight before it leaves.
    async fn journal(&mut self, side: Side, intent: &SignedIntent) -> Result<()> {
        self.store.journal_intent(side, intent).await?;
        self.inflight.push(Inflight {
            side,
            intent_id: intent.intent_id,
            deadline: intent.deadline,
            queue_revision: None,
            seen_queued: false,
        });
        Ok(())
    }

    async fn record_submission(
        &mut self,
        side: Side,
        intent: &SignedIntent,
        result: Result<IntentReceipt, SubmitError>,
    ) -> Result<()> {
        let (id, action) = (intent.intent_id, intent.action.as_str());
        match result {
            Ok(receipt) => {
                if receipt.intent_id != id {
                    error!(%id, relayer = %receipt.intent_id, "relayer derived a different intent id");
                }
                self.store.mark_accepted(id, receipt.queue_revision.0).await?;
                for entry in self.inflight.iter_mut().filter(|entry| entry.intent_id == id) {
                    entry.intent_id = receipt.intent_id;
                    entry.queue_revision = Some(receipt.queue_revision.0);
                }
                info!(%side, action, %id, state = %receipt.state, "intent accepted");
                self.store.record_event("intent", &format!("{side} {action} accepted"), None).await
            }
            Err(SubmitError::Rejected { code, reason }) => {
                let detail = reason.map_or_else(|| code.clone(), |reason| format!("{code}: {reason}"));
                self.store.mark_not_accepted(id, "rejected", &detail).await?;
                self.inflight.retain(|entry| entry.intent_id != id);
                warn!(%side, action, %id, %detail, "intent rejected");
                self.store.record_event("intent", &format!("{side} {action} rejected: {detail}"), None).await
            }
            Err(SubmitError::Unknown(detail)) => {
                self.store.mark_not_accepted(id, "unknown", &detail).await?;
                error!(%side, action, %id, %detail, "intent outcome unknown; waiting for the account state or its deadline");
                self.store
                    .record_event("intent", &format!("{side} {action} outcome unknown: {detail}"), None)
                    .await
            }
        }
    }

    fn describe(&self, decision: &Decision) -> String {
        match decision {
            Decision::Wait(reason) => reason.to_string(),
            Decision::Open { margin, leverage, .. } => {
                format!("open both legs: margin {} USD at {leverage}x", format_amount(*margin))
            }
            Decision::Close { reason, closes } => {
                let order: Vec<&str> = closes.iter().map(|close| close.side.as_str()).collect();
                let order = order.join(" then ");
                match reason {
                    CloseReason::Target => format!("price reached the target; close {order}"),
                    CloseReason::Unhedged => format!("only the {order} leg is open; close it"),
                    CloseReason::Flatten => format!("flatten: close {order}"),
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn snapshot(
        &self,
        mode: Mode,
        decision: &Decision,
        summary: String,
        accounts: &PerSide<Option<AccountState>>,
        trading: Option<&TradingState>,
        queue_total: Option<U256>,
        mid: Option<u64>,
    ) -> Snapshot {
        let price = |raw: u64| format_price(raw, self.price_decimals);
        let leg = |side: Side| {
            let leg = self.legs.get(side);
            LegSnapshot {
                side,
                wallet: format!("{:#x}", leg.wallet),
                session_key: format!("{:#x}", leg.signer.address()),
                account: accounts.get(side).as_ref().map(|account| AccountSnapshot {
                    available_usd: format_amount(account.available_balance),
                    queued_usd: format_amount(account.queued),
                    session_key_expiry: account.session_keys.get(&leg.signer.address()).copied(),
                    positions: account
                        .positions
                        .iter()
                        .map(|position| PositionSnapshot {
                            id: position.position_id.to_string(),
                            is_long: position.is_long,
                            entry: price(position.entry_price),
                            bust: price(position.bust_price),
                            margin_usd: format_amount(position.margin),
                            leverage: position.leverage,
                        })
                        .collect(),
                }),
                pending_intents: self.inflight.iter().filter(|entry| entry.side == side).count(),
            }
        };
        let entries: Vec<u64> = Side::BOTH
            .iter()
            .filter_map(|&side| accounts.get(side).as_ref()?.positions.first())
            .map(|position| position.entry_price)
            .collect();
        let move_bps = match (mid, entries.as_slice()) {
            (Some(mid), [long, short]) => {
                let entry = (*long as f64 + *short as f64) / 2.0;
                Some((mid as f64 - entry) / entry * 10_000.0)
            }
            _ => None,
        };
        Snapshot {
            at_ms: now_ms(),
            dry_run: self.dry_run,
            mode,
            coin: self.coin.clone(),
            close_bps: self.params.close_bps,
            decision: match decision {
                Decision::Wait(_) => "wait",
                Decision::Open { .. } => "open",
                Decision::Close { .. } => "close",
            }
            .to_owned(),
            summary,
            mid: mid.map(price),
            move_bps,
            trading: trading.map(|trading| TradingSnapshot {
                trading_paused: trading.trading_paused,
                paused_actions: trading.paused_actions.iter().cloned().collect(),
            }),
            queue_total_usd: queue_total.map(format_amount),
            legs: vec![leg(Side::Long), leg(Side::Short)],
        }
    }
}

/// Copies newly settled positions from the accounts' trade history into the
/// ledger, once a minute and whenever the bot sees a position open or settle.
/// It runs beside the decision loop so a slow history request never delays a close.
async fn follow_ledger(client: Client, store: Store, wallets: [(Side, Address); 2], wake: Arc<Notify>) {
    let mut every = interval(LEDGER_INTERVAL);
    every.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = every.tick() => {}
            () = wake.notified() => {}
        }
        for (side, wallet) in wallets {
            if let Err(error) = sync_ledger(&client, &store, side, wallet).await {
                warn!(%side, %error, "ledger sync failed");
            }
        }
    }
}

async fn sync_ledger(client: &Client, store: &Store, side: Side, wallet: Address) -> Result<()> {
    let trades = client.trades(wallet).await?;
    for trade in store.record_trades(wallet, side, &trades).await? {
        let net = trade.adjusted_pnl_raw.0 - I256::try_from(trade.user_paid_fee_raw.0).unwrap_or_default();
        let summary = format!(
            "{side} position {} {}: net {} USD, {} PAPER",
            trade.position_id.0,
            if trade.kind == "Liquidated" { "liquidated" } else { "closed" },
            format_signed_amount(net),
            format_amount(trade.paper_minted_raw.0),
        );
        info!("{summary}");
        store.record_event("trade", &summary, None).await?;
    }
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
