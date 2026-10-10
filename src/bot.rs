//! The loop: gather what is known, ask `decide`, carry out the answer, record it.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, U256};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::config::Config;
use crate::economics;
use crate::market::{Quote, follow_price, format_price, price_decimals};
use crate::protocol::account::AccountState;
use crate::protocol::chain::Chain;
use crate::protocol::client::{Client, SubmitError};
use crate::protocol::intents::{
    Close, NonceSequence, OpenPosition, SignedIntent, sign_close, sign_open_position,
};
use crate::protocol::stream::{StreamTiming, follow_account};
use crate::protocol::wire::{IntentOutcome, IntentReceipt, TradingState};
use crate::session_keys;
use crate::status::{
    AccountSnapshot, LegSnapshot, PositionSnapshot, Snapshot, TradingSnapshot, format_amount,
    format_signed_amount,
};
use crate::store::{Store, now_ms};
use crate::strategy::decide::{
    CloseOrder, CloseReason, Decision, LegView, Market, Mode, Phase, Side, StrategyParams, View, decide,
};
use crate::strategy::inflight::{Awaiting, Inflight, Kind, awaiting, is_failure, track};

const STATUS_INTERVAL: Duration = Duration::from_secs(1);
/// Storage failures repeat every tick; say so this often.
const STORE_WARNING_INTERVAL: Duration = Duration::from_secs(30);

/// Every wait and interval of the loop. Production uses the defaults.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub tick: Duration,
    /// How often the trading state and the payout queue are fetched.
    pub state_poll: Duration,
    pub ledger_interval: Duration,
    /// A refused close is tried again, but not on every tick.
    pub close_retry: Duration,
    /// How long a sent intent may stay out of the relayer's queue before it is
    /// treated as doubtful: it no longer keeps a leg waiting unhedged.
    pub grace: Duration,
    /// A quote older than this is treated as unknown.
    pub quote_max_age: Duration,
    pub stream: StreamTiming,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            tick: Duration::from_millis(250),
            state_poll: Duration::from_secs(5),
            ledger_interval: Duration::from_secs(60),
            close_retry: Duration::from_secs(5),
            grace: Duration::from_secs(10),
            quote_max_age: Duration::from_secs(30),
            stream: StreamTiming::default(),
        }
    }
}

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
fn poll<T, F, Fut>(tasks: &mut JoinSet<()>, what: &'static str, every: Duration, load: F) -> Polled<T>
where
    T: Send + Sync + 'static,
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = reqwest::Result<T>> + Send,
{
    let (sender, receiver) = watch::channel(None);
    tasks.spawn(async move {
        let mut every = interval(every);
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
fn fresh<T: Clone>(polled: &Polled<T>, every: Duration) -> Option<T> {
    polled
        .borrow()
        .as_ref()
        .filter(|(_, loaded_at)| loaded_at.elapsed() <= 3 * every)
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
    timing: Timing,
    market: Market,
    price_decimals: u32,
    client: Client,
    store: Store,
    legs: PerSide<Leg>,
    outcomes: mpsc::Receiver<IntentOutcome>,
    quote: watch::Receiver<Option<Quote>>,
    trading: Polled<TradingState>,
    queue_total: Polled<U256>,
    /// The operator's mode as last read; kept when the database cannot be read.
    mode: Mode,
    inflight: Vec<Inflight>,
    /// An open attempt can surface late; it must not be answered by another
    /// open, so opens are held back for one intent lifetime after each attempt.
    open_allowed_at_ms: i64,
    close_allowed_at: Instant,
    reported: String,
    status_at: Instant,
    /// The latest snapshot, picked up by the task that writes it to the database.
    status: watch::Sender<Option<Snapshot>>,
    store_warned_at: Option<Instant>,
    /// Wakes the ledger task when a position opens or settles.
    ledger_wake: Arc<Notify>,
    open_position_count: usize,
}

/// A bot that has started: its feeds are running and its state is loaded.
pub struct Running {
    bot: Bot,
    tasks: JoinSet<()>,
}

/// Starts the bot against production. Fails if it cannot start: a missing
/// session key, an unlisted market, an unreachable relayer.
pub async fn start(config: &Config, store: Store) -> Result<Running> {
    let coin = config.coin.clone();
    let price = move |price_decimals: u32, tasks: &mut JoinSet<()>| {
        let (sender, quote) = watch::channel(None);
        tasks.spawn(follow_price(coin, price_decimals, sender));
        quote
    };
    start_with(config, store, Client::new()?, Chain::new()?, price, Timing::default()).await
}

/// Keeps a bot running until `stop` resolves.
///
/// The first start has to succeed, so a wrong setting or a missing session key
/// is reported at once instead of being retried forever. After that, whenever
/// the bot cannot go on (a feed died, the relayer became unreachable) it is
/// started again after `restart_delay`. The bot takes over from the account
/// state and its journal on every start, so starting again is always safe; and
/// doing it in the same process means a terminal that stays open is all it needs.
pub async fn keep_running<S, Starting>(
    mut start: S,
    restart_delay: Duration,
    stop: impl Future<Output = ()> + Clone,
) -> Result<()>
where
    S: FnMut() -> Starting,
    Starting: Future<Output = Result<Running>>,
{
    let mut running = start().await?;
    loop {
        match running.until(stop.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) => error!("the bot stopped: {error:#}"),
        }
        running = loop {
            info!("starting again in {restart_delay:?}");
            tokio::select! {
                () = stop.clone() => return Ok(()),
                () = tokio::time::sleep(restart_delay) => {}
            }
            match start().await {
                Ok(running) => break running,
                Err(error) => error!("the bot could not start: {error:#}"),
            }
        };
    }
}

/// Starts the bot and runs it until `shutdown` resolves or it cannot go on.
pub async fn run_with(
    config: &Config,
    store: Store,
    client: Client,
    chain: Chain,
    price: impl FnOnce(u32, &mut JoinSet<()>) -> watch::Receiver<Option<Quote>>,
    timing: Timing,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    start_with(config, store, client, chain, price, timing).await?.until(shutdown).await
}

/// Starts the bot against any relayer and price source.
///
/// `price` is given the market's price decimals and returns the quote to follow;
/// it may spawn its feed onto the task set so the feed is supervised with the rest.
pub async fn start_with(
    config: &Config,
    store: Store,
    client: Client,
    // Read-only contract calls. Nothing uses it yet: the estimate reads fee
    // rates through it and rebalancing reads deposit addresses.
    _chain: Chain,
    price: impl FnOnce(u32, &mut JoinSet<()>) -> watch::Receiver<Option<Quote>>,
    timing: Timing,
) -> Result<Running> {
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
        tasks.spawn(follow_account(
            client.account_stream_url(wallet),
            timing.stream,
            sender,
            outcome_sender.clone(),
        ));
        Ok(Leg {
            wallet,
            signer: session_keys::load(&config.keys_dir, wallet)?,
            nonces: NonceSequence::random(),
            account,
        })
    };
    let legs = PerSide { long: leg(Side::Long)?, short: leg(Side::Short)? };

    let quote = price(price_decimals, &mut tasks);
    let trading = {
        let client = client.clone();
        poll(&mut tasks, "trading state", timing.state_poll, move || {
            let client = client.clone();
            async move { client.trading_state().await }
        })
    };
    let queue_total = {
        let client = client.clone();
        poll(&mut tasks, "queue summary", timing.state_poll, move || {
            let client = client.clone();
            async move { client.queue_total().await }
        })
    };
    let ledger_wake = Arc::new(Notify::new());
    tasks.spawn(follow_ledger(
        client.clone(),
        store.clone(),
        [(Side::Long, legs.long.wallet), (Side::Short, legs.short.wallet)],
        timing.ledger_interval,
        ledger_wake.clone(),
    ));

    let (status, latest_status) = watch::channel(None);
    tasks.spawn(follow_status(store.clone(), latest_status));

    let ttl_ms = i64::try_from(config.intent_ttl_seconds * 1000)?;
    let bot = Bot {
        dry_run: config.dry_run,
        coin: config.coin.clone(),
        params: config.strategy(),
        ttl_seconds: config.intent_ttl_seconds,
        timing,
        market: Market { id: instrument.id, symbol: instrument.symbol.clone() },
        price_decimals,
        client,
        mode: store.mode().await?,
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
        status,
        store_warned_at: None,
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

    Ok(Running { bot, tasks })
}

impl Running {
    /// Runs until `shutdown` resolves (`Ok`) or the bot cannot go on (`Err`).
    /// The tick in progress always finishes.
    pub async fn until(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let Running { mut bot, mut tasks } = self;
        let mut ticks = interval(bot.timing.tick);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                // Without its feeds the bot would wait forever on stale data. Better to
                // stop loudly: the caller starts a fresh run.
                Some(ended) = tasks.join_next() => bail!("a background task stopped: {ended:?}"),
                _ = ticks.tick() => bot.tick().await,
            }
        }
        if bot.open_position_count > 0 {
            warn!("stopping with open positions; nothing is watching them now");
        }
        Ok(())
    }
}

impl Bot {
    async fn tick(&mut self) {
        while let Ok(outcome) = self.outcomes.try_recv() {
            let failed = is_failure(&outcome.outcome);
            if failed {
                warn!(intent_id = %outcome.intent_id, outcome = %outcome.outcome, "intent did not succeed");
            }
            for entry in self.inflight.iter_mut().filter(|entry| entry.intent_id == outcome.intent_id) {
                entry.failed = failed;
            }
            let recorded = self.store.set_outcome(outcome.intent_id, &outcome.outcome).await;
            self.best_effort("recording an intent outcome", recorded);
        }

        let accounts = PerSide {
            long: self.legs.long.account.borrow().clone(),
            short: self.legs.short.account.borrow().clone(),
        };
        self.track_inflight(&accounts).await;

        let mode = self.store.mode().await;
        if let Some(mode) = self.best_effort("reading the operator's mode", mode) {
            self.mode = mode;
        }
        let trading = fresh(&self.trading, self.timing.state_poll);
        let queue_total = fresh(&self.queue_total, self.timing.state_poll);
        let max_age = self.timing.quote_max_age;
        let mid = (*self.quote.borrow()).and_then(|quote| quote.fresh_mid(max_age));
        let (now, grace_ms) = (now_ms(), duration_ms(self.timing.grace));
        let leg = |side: Side| LegView {
            account: accounts.get(side).as_ref(),
            session_key: self.legs.get(side).signer.address(),
            awaiting: self
                .inflight
                .iter()
                .filter(|entry| entry.side == side)
                .map(|entry| awaiting(entry, accounts.get(side).as_ref(), now, grace_ms))
                .max()
                .unwrap_or(Awaiting::Nothing),
        };
        let view = View {
            now_seconds: unix_seconds(),
            mode: self.mode,
            long: leg(Side::Long),
            short: leg(Side::Short),
            trading: trading.as_ref(),
            queue_total,
            mid,
            may_open: now >= self.open_allowed_at_ms,
        };
        let decision = decide(&view, &self.params, &self.market);

        let summary = self.describe(&decision);
        let changed = summary != self.reported;
        if changed {
            info!(mid = ?mid.map(|raw| format_price(raw, self.price_decimals)), "{summary}");
            let detail = json!({ "mode": self.mode.as_str(), "mid": mid });
            let recorded = self.store.record_event("decision", &summary, Some(&detail)).await;
            self.best_effort("recording a decision", recorded);
            self.reported.clone_from(&summary);
        }
        if changed || self.status_at.elapsed() >= STATUS_INTERVAL {
            let snapshot = self.snapshot(&decision, summary, &accounts, trading.as_ref(), queue_total, mid);
            self.status.send_replace(Some(snapshot));
            self.status_at = Instant::now();
        }

        let open_positions = Side::BOTH
            .iter()
            .filter_map(|&side| accounts.get(side).as_ref())
            .map(|account| account.positions.len())
            .sum();
        if open_positions != self.open_position_count {
            self.open_position_count = open_positions;
            self.ledger_wake.notify_one();
        }

        if self.dry_run {
            return;
        }
        match decision {
            Decision::Wait(_) => {}
            Decision::Open { instrument_id, margin, leverage } => {
                self.open(instrument_id, margin, leverage).await;
            }
            Decision::Close { closes, .. } => {
                if Instant::now() >= self.close_allowed_at {
                    self.close(closes).await;
                }
            }
        }
    }

    /// The database makes the bot durable and visible, but it must never stand
    /// between the bot and a close. Failures are reported and the loop goes on.
    fn best_effort<T>(&mut self, what: &'static str, result: Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                if self.store_warned_at.is_none_or(|at| at.elapsed() >= STORE_WARNING_INTERVAL) {
                    self.store_warned_at = Some(Instant::now());
                    error!(%error, "{what} failed; continuing without it");
                }
                None
            }
        }
    }

    /// Settles in-flight intents the accounts have caught up with.
    async fn track_inflight(&mut self, accounts: &PerSide<Option<AccountState>>) {
        let mut still_inflight = Vec::with_capacity(self.inflight.len());
        for entry in std::mem::take(&mut self.inflight) {
            let (intent_id, side, was_seen) = (entry.intent_id, entry.side, entry.seen_queued);
            let recorded = match track(entry, accounts.get(side).as_ref()) {
                Some(tracked) => {
                    let newly_seen = tracked.seen_queued && !was_seen;
                    still_inflight.push(tracked);
                    if !newly_seen {
                        continue;
                    }
                    self.store.mark_seen_queued(intent_id).await
                }
                None => self.store.mark_settled(intent_id).await,
            };
            self.best_effort("updating the intent journal", recorded);
        }
        self.inflight = still_inflight;
    }

    async fn open(&mut self, instrument_id: u32, margin: U256, leverage: u32) {
        self.open_allowed_at_ms = now_ms() + duration_ms(Duration::from_secs(self.ttl_seconds));
        let deadline = unix_seconds() + self.ttl_seconds;
        let Ok(size) = u128::try_from(margin) else {
            error!(%margin, "margin does not fit the open intent; not opening");
            return;
        };
        let mut intents = Vec::with_capacity(2);
        for side in Side::BOTH {
            let leg = self.legs.get_mut(side);
            let message = OpenPosition {
                user: leg.wallet,
                instrumentId: instrument_id,
                isLong: side.is_long(),
                size,
                leverage,
                useDebt: false,
                nonce: leg.nonces.allocate(),
                deadline,
            };
            match sign_open_position(&leg.signer, &message) {
                Ok(intent) => intents.push((side, intent)),
                Err(error) => {
                    error!(%side, %error, "signing the open failed; not opening");
                    return;
                }
            }
        }
        // Write-ahead, both legs or neither: an open that is not on disk must not leave,
        // because a restart could not know it may still execute.
        for (index, (side, intent)) in intents.iter().enumerate() {
            let journaled = self.store.journal_intent(*side, intent).await;
            if self.best_effort("journaling an open", journaled).is_none() {
                for (_, unsent) in &intents[..index] {
                    let marked = self.store.mark_not_accepted(unsent.intent_id, "rejected", "not sent").await;
                    self.best_effort("updating the intent journal", marked);
                }
                error!("the intent journal is not writable; not opening");
                return;
            }
        }
        for (side, intent) in &intents {
            self.inflight.push(inflight(*side, Kind::Open, intent));
        }
        // Both legs go out together so they have the best chance of the same block and price.
        let (first, second) =
            tokio::join!(self.client.submit_intent(&intents[0].1), self.client.submit_intent(&intents[1].1),);
        for ((side, intent), result) in intents.iter().zip([first, second]) {
            self.record_submission(*side, intent, result).await;
        }
    }

    /// Closes go out in the decided order, one after the other.
    async fn close(&mut self, closes: Vec<CloseOrder>) {
        self.close_allowed_at = Instant::now() + self.timing.close_retry;
        let deadline = unix_seconds() + self.ttl_seconds;
        for CloseOrder { side, position_ids } in closes {
            let leg = self.legs.get_mut(side);
            let message =
                Close { user: leg.wallet, positionIds: position_ids, nonce: leg.nonces.allocate(), deadline };
            let intent = match sign_close(&leg.signer, &message) {
                Ok(intent) => intent,
                Err(error) => {
                    error!(%side, %error, "signing the close failed");
                    continue;
                }
            };
            // A close goes out even if it cannot be journaled: closing twice is harmless,
            // not closing is not.
            let journaled = self.store.journal_intent(side, &intent).await;
            self.best_effort("journaling a close", journaled);
            self.inflight.push(inflight(side, Kind::Close, &intent));
            let result = self.client.submit_intent(&intent).await;
            self.record_submission(side, &intent, result).await;
        }
    }

    async fn record_submission(
        &mut self,
        side: Side,
        intent: &SignedIntent,
        result: Result<IntentReceipt, SubmitError>,
    ) {
        let (id, action) = (intent.intent_id, intent.action.as_str());
        let (journaled, summary) = match result {
            Ok(receipt) => {
                if receipt.intent_id != id {
                    error!(%id, relayer = %receipt.intent_id, "relayer derived a different intent id");
                }
                for entry in self.inflight.iter_mut().filter(|entry| entry.intent_id == id) {
                    entry.intent_id = receipt.intent_id;
                    entry.queue_revision = Some(receipt.queue_revision.0);
                }
                info!(%side, action, %id, state = %receipt.state, "intent accepted");
                (
                    self.store.mark_accepted(id, receipt.queue_revision.0).await,
                    format!("{side} {action} accepted"),
                )
            }
            Err(SubmitError::Rejected { code, reason }) => {
                let detail = reason.map_or_else(|| code.clone(), |reason| format!("{code}: {reason}"));
                self.inflight.retain(|entry| entry.intent_id != id);
                warn!(%side, action, %id, %detail, "intent rejected");
                (
                    self.store.mark_not_accepted(id, "rejected", &detail).await,
                    format!("{side} {action} rejected: {detail}"),
                )
            }
            Err(SubmitError::Unknown(detail)) => {
                error!(%side, action, %id, %detail, "intent outcome unknown; it may still execute until its deadline");
                (
                    self.store.mark_not_accepted(id, "unknown", &detail).await,
                    format!("{side} {action} outcome unknown: {detail}"),
                )
            }
        };
        self.best_effort("updating the intent journal", journaled);
        let recorded = self.store.record_event("intent", &summary, None).await;
        self.best_effort("recording an intent event", recorded);
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

    fn snapshot(
        &self,
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
                has_wallet_key: false,
            }
        };
        let phase = Phase::of(accounts.long.as_ref(), accounts.short.as_ref(), &self.market);
        let entries: Vec<u64> = Side::BOTH
            .iter()
            .filter_map(|&side| accounts.get(side).as_ref()?.positions.first())
            .map(|position| position.entry_price)
            .collect();
        let move_bps = match (mid, phase, entries.as_slice()) {
            (Some(mid), Phase::Hedged, [long, short]) => {
                let entry = (*long as f64 + *short as f64) / 2.0;
                Some((mid as f64 - entry) / entry * 10_000.0)
            }
            _ => None,
        };
        Snapshot {
            at_ms: now_ms(),
            dry_run: self.dry_run,
            mode: self.mode,
            coin: self.coin.clone(),
            close_bps: self.params.close_bps,
            phase,
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
            estimate: None,
            funds: None,
            transfer: None,
        }
    }
}

fn inflight(side: Side, kind: Kind, intent: &SignedIntent) -> Inflight {
    Inflight {
        side,
        kind,
        intent_id: intent.intent_id,
        deadline: intent.deadline,
        sent_at_ms: now_ms(),
        queue_revision: None,
        seen_queued: false,
        failed: false,
    }
}

/// Writes the latest status snapshot to the database whenever it changes. The
/// snapshot is rewritten every second for the viewers' benefit; doing that beside
/// the loop means a slow disk can make the dashboard lag, never a decision.
async fn follow_status(store: Store, mut latest: watch::Receiver<Option<Snapshot>>) {
    let mut warned_at: Option<Instant> = None;
    while latest.changed().await.is_ok() {
        let Some(snapshot) = latest.borrow_and_update().clone() else { continue };
        if let Err(error) = store.write_status(&snapshot).await
            && warned_at.is_none_or(|at| at.elapsed() >= STORE_WARNING_INTERVAL)
        {
            warned_at = Some(Instant::now());
            warn!(%error, "writing the status failed");
        }
    }
}

/// Copies newly settled positions from the accounts' trade history into the
/// ledger, at an interval and whenever the bot sees a position open or settle.
/// It runs beside the decision loop so a slow history request never delays a close.
async fn follow_ledger(
    client: Client,
    store: Store,
    wallets: [(Side, Address); 2],
    every: Duration,
    wake: Arc<Notify>,
) {
    let mut every = interval(every);
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
        let summary = format!(
            "{side} position {} {}: net {} USD, {} PAPER",
            trade.position_id.0,
            if trade.is_liquidation() { "liquidated" } else { "closed" },
            format_signed_amount(economics::net_result(&trade)),
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

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
