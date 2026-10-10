//! A local stand-in for Papertrade's relayer and state API, plus a harness
//! that runs the real bot against it.
//!
//! The simulator speaks the wire protocol the bot uses in production: it
//! verifies intent signatures, queues and executes opens and closes at the
//! current mid, streams account snapshots and patches, and can be scripted to
//! misbehave the ways the real service might.

#![allow(dead_code)]

mod funding;

// Not every test binary that shares this module scripts the funding side.
#[allow(unused_imports)]
pub use funding::{SendBehavior, Sweep};

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, B256, Signature, U256, keccak256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::SolStruct;
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use futures::stream::{self, Stream};
use papertrade::bot::{self, Timing};
use papertrade::config::Config;
use papertrade::keystore::{AccountKeys, UnlockedKeys, WalletKey};
use papertrade::market::Quote;
use papertrade::protocol::chain::Chain;
use papertrade::protocol::client::Client;
use papertrade::protocol::deployment::{EXCHANGE, usd};
use papertrade::protocol::intents::{Close, OpenPosition, RegisterSessionKey, exchange_domain};
use papertrade::protocol::stream::StreamTiming;
use papertrade::status::Snapshot;
use papertrade::store::Store;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot, watch};
use tokio::task::JoinHandle;

const TRADING_STATE: &str = include_str!("../fixtures/trading-state.json");
const PROTOCOL_SUMMARY: &str = include_str!("../fixtures/protocol-summary.json");

/// How the simulator treats the next intent of one kind from one wallet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behavior {
    /// Queue it, answer with a receipt, execute it.
    Accept,
    /// Refuse it with this code and reason.
    Reject(&'static str, &'static str),
    /// Answer with a server error and do not take it: it never executes.
    Drop,
    /// Take it and execute it, but answer with a server error.
    AcceptSilently,
    /// Queue it, then let it fail when it executes.
    AcceptThenFail,
    /// Queue it and keep it queued until `release`.
    Hold,
}

/// An intent the bot sent, whatever became of it.
#[derive(Debug, Clone)]
pub struct Received {
    pub wallet: Address,
    pub action: String,
    pub intent_id: B256,
    pub at: Instant,
}

#[derive(Clone)]
enum Frame {
    Data(String),
    /// Ends every open stream of the account.
    Close,
}

struct Operation {
    intent_id: B256,
    action: String,
    signed: Value,
    deadline: u64,
    behavior: Behavior,
    accepted_at: Instant,
}

#[derive(Clone)]
struct Position {
    id: u64,
    is_long: bool,
    entry: u64,
    margin: U256,
    leverage: u32,
    intent_id: B256,
}

struct Account {
    address: Address,
    revision: u64,
    queue_revision: u64,
    balance: U256,
    session_keys: Vec<(Address, u64)>,
    positions: Vec<Position>,
    operations: Vec<Operation>,
    /// Opens that executed but whose position the indexer has not shown yet.
    unindexed: Vec<(Instant, Position)>,
    /// How long the indexer takes to show a new position of this account.
    position_lag: Duration,
    /// How far the chain clock this account reports runs behind real time, in seconds.
    clock_lag: u64,
    trades: Vec<Value>,
    frames: broadcast::Sender<Frame>,
}

struct World {
    mid: u64,
    paused: BTreeSet<String>,
    accounts: HashMap<Address, Account>,
    scripts: HashMap<(Address, String), VecDeque<Behavior>>,
    received: Vec<Received>,
    /// HyperCore balances, Hyperliquid sends and the relayer's sweeping.
    funding: funding::Funding,
    next_position_id: u64,
    stream_up: bool,
    held: bool,
    execution_delay: Duration,
    /// Whether the Exchange's fee getters answer; when not, they revert.
    fees_readable: bool,
    /// What the LP owes to queued winners.
    queue_total: U256,
}

fn unix_seconds() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn unix_millis() -> u64 {
    u64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

impl Account {
    fn position_tuple(position: &Position) -> Value {
        let bust = if position.is_long { position.entry * 95 / 100 } else { position.entry * 105 / 100 };
        json!([
            position.id.to_string(),
            0,
            position.is_long,
            false,
            position.entry.to_string(),
            position.margin.to_string(),
            position.leverage,
            bust.to_string(),
            unix_millis(),
            position.intent_id
        ])
    }

    fn operation_tuple(operation: &Operation) -> Value {
        json!([
            operation.intent_id,
            operation.action,
            null,
            null,
            null,
            null,
            null,
            unix_millis(),
            operation.deadline.to_string(),
            null
        ])
    }

    /// The chain clock as this account's indexer reports it.
    fn chain_seconds(&self) -> u64 {
        unix_seconds() - self.clock_lag
    }

    fn scalars(&self) -> Value {
        json!({
            "indexedThroughTimestamp": self.chain_seconds(),
            "sessionKeys": self.session_keys.iter().map(|(key, expiry)| json!([key, expiry.to_string()])).collect::<Vec<_>>(),
            "queueRevision": self.queue_revision.to_string(),
            "balance": self.balance.to_string(),
            "queued": "0",
            "locked": "0",
            "funds": { "availableBalance": self.balance.to_string(), "availableQueued": "0" },
        })
    }

    fn snapshot(&self) -> String {
        let mut frame = self.scalars();
        let fields = frame.as_object_mut().unwrap();
        fields.insert("address".into(), json!(self.address));
        fields.insert("accountRevision".into(), json!(self.revision));
        fields.insert("operations".into(), self.operations.iter().map(Self::operation_tuple).collect());
        fields.insert("openPositions".into(), self.positions.iter().map(Self::position_tuple).collect());
        frame.to_string()
    }

    /// Advances the revision and streams a patch with the given changes.
    fn publish(&mut self, changes: Value) {
        let base = self.revision;
        self.revision += 1;
        let mut frame = json!({
            "address": self.address,
            "accountRevision": self.revision,
            "baseAccountRevision": base,
            "state": self.scalars(),
            "addedPositions": [],
            "removedPositionIds": [],
            "addedOperations": [],
            "removedOperationIds": [],
        });
        frame.as_object_mut().unwrap().extend(changes.as_object().unwrap().clone());
        let _ = self.frames.send(Frame::Data(frame.to_string()));
    }
}

impl World {
    /// Carries out whatever is due: queued operations and positions waiting to be indexed.
    fn step(&mut self) {
        let (mid, held, delay) = (self.mid, self.held, self.execution_delay);
        let next_id = &mut self.next_position_id;
        let funding = &mut self.funding;
        for account in self.accounts.values_mut() {
            let due: Vec<Position> = {
                let (ready, waiting) = std::mem::take(&mut account.unindexed)
                    .into_iter()
                    .partition(|(at, _)| *at <= Instant::now());
                account.unindexed = waiting;
                ready.into_iter().map(|(_, position): (Instant, Position)| position).collect()
            };
            for position in due {
                account.positions.push(position.clone());
                account.publish(json!({ "addedPositions": [Account::position_tuple(&position)] }));
            }

            let mut index = 0;
            while index < account.operations.len() {
                let operation = &account.operations[index];
                let expired = account.chain_seconds() > operation.deadline;
                // A held operation stays queued, but not past its deadline: then it is dropped.
                let waiting = operation.behavior == Behavior::Hold && held && !expired;
                if waiting || operation.accepted_at.elapsed() < delay {
                    index += 1;
                    continue;
                }
                let operation = account.operations.remove(index);
                // Withdrawals and registrations are the funding side's to execute.
                let Some(operation) = funding.execute(account, operation) else { continue };
                execute(account, operation, mid, next_id);
            }
        }
        funding::step(self);
    }
}

fn execute(account: &mut Account, operation: Operation, mid: u64, next_id: &mut u64) {
    let (id, lag) = (operation.intent_id, account.position_lag);
    account.queue_revision += 1;
    let ended = |outcome: &str| json!({ "removedOperationIds": [id], "intentOutcomes": [[id, outcome]] });
    if account.chain_seconds() > operation.deadline {
        return account.publish(ended("expired"));
    }
    if operation.behavior == Behavior::AcceptThenFail {
        return account.publish(ended("failed"));
    }
    let signed = &operation.signed;
    if operation.action == "registerSessionKey" {
        // From here on the key may trade for the account, until it expires.
        let key: Address = signed["key"].as_str().unwrap().parse().unwrap();
        let expiry: u64 = signed["expiry"].as_str().unwrap().parse().unwrap();
        account.session_keys.retain(|(known, _)| *known != key);
        account.session_keys.push((key, expiry));
        return account.publish(ended("succeeded"));
    }
    if operation.action == "openPosition" {
        let margin: U256 = signed["size"].as_str().unwrap().parse().unwrap();
        let position = Position {
            id: *next_id,
            is_long: signed["isLong"].as_bool().unwrap(),
            entry: mid,
            margin,
            leverage: signed["leverage"].as_str().unwrap().parse().unwrap(),
            intent_id: id,
        };
        *next_id += 1;
        account.balance -= margin;
        if lag.is_zero() {
            account.positions.push(position.clone());
            let mut changes = ended("succeeded");
            changes["addedPositions"] = json!([Account::position_tuple(&position)]);
            account.publish(changes);
        } else {
            // The relayer's queue and the chain indexer are separate sources: the
            // operation can disappear before the position shows up.
            account.unindexed.push((Instant::now() + lag, position));
            account.publish(ended("succeeded"));
        }
        return;
    }

    let wanted: Vec<u64> = signed["positionIds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap().parse().unwrap())
        .collect();
    let (closing, kept): (Vec<Position>, Vec<Position>) = std::mem::take(&mut account.positions)
        .into_iter()
        .partition(|position| wanted.contains(&position.id));
    account.positions = kept;
    if closing.is_empty() {
        return account.publish(ended("failed"));
    }
    for position in &closing {
        let (credit, trade) = settle(account.address, position, mid, "PositionClosed");
        account.balance += credit;
        account.trades.insert(0, trade);
    }
    let mut changes = ended("succeeded");
    changes["removedPositionIds"] = closing.iter().map(|position| json!(position.id.to_string())).collect();
    account.publish(changes);
}

/// What a position returns at `mid`, and its row in the trade history.
/// Winners keep 88% of the raw gain and pay a 2% fee on that; losers pay the raw loss.
fn settle(wallet: Address, position: &Position, mid: u64, kind: &str) -> (U256, Value) {
    let notional = position.margin * U256::from(position.leverage);
    let raw = notional * U256::from(mid.abs_diff(position.entry)) / U256::from(position.entry);
    let wins = (mid > position.entry) == position.is_long && mid != position.entry;
    let liquidated = kind == "Liquidated";
    let loss = if wins { U256::ZERO } else { raw.min(position.margin) };
    let lost = if loss.is_zero() { "0".to_owned() } else { format!("-{loss}") };
    let (credit, adjusted, fee, paper) = if liquidated {
        // The whole margin is gone and all of it mints, but the record carries only
        // the PnL at the price the liquidation fired at.
        (U256::ZERO, lost, U256::ZERO, position.margin * U256::from(100))
    } else if wins {
        let adjusted = raw * U256::from(88) / U256::from(100);
        let fee = adjusted * U256::from(2) / U256::from(100);
        (position.margin + adjusted - fee, adjusted.to_string(), fee, U256::ZERO)
    } else {
        (position.margin - loss, lost, U256::ZERO, loss * U256::from(98))
    };
    let trade = json!({
        "timestampMs": unix_millis(),
        "txHash": keccak256(format!("{wallet}{}", position.id)),
        "positionId": position.id.to_string(),
        "instrumentId": 0, "symbol": "BTC", "assetIndex": 0, "priceScaleRaw": "10",
        "terminal": false, "retired": false,
        "isLong": position.is_long,
        "entryPriceRaw": position.entry.to_string(),
        "marginRaw": position.margin.to_string(),
        "leverage": position.leverage,
        "kind": kind,
        "openedAtMs": unix_millis(),
        "exitPriceRaw": mid.to_string(),
        "adjustedPnlRaw": adjusted,
        "userPaidFeeRaw": fee.to_string(),
        "paperMintedRaw": paper.to_string(),
        "paperMintBasisRaw": "0",
        "paperMintBatchTotalRaw": null,
    });
    (credit, trade)
}

/// Recomputes the intent id the way the relayer does, and who signed it.
fn identify(action: &str, signed: &Value) -> Option<(B256, Address, u64)> {
    let text = |field: &str| signed.get(field)?.as_str();
    let deadline: u64 = text("deadline")?.parse().ok()?;
    let digest = match action {
        "openPosition" => OpenPosition {
            user: text("user")?.parse().ok()?,
            instrumentId: text("instrumentId")?.parse().ok()?,
            isLong: signed.get("isLong")?.as_bool()?,
            size: text("size")?.parse().ok()?,
            leverage: text("leverage")?.parse().ok()?,
            useDebt: signed.get("useDebt")?.as_bool()?,
            nonce: text("nonce")?.parse().ok()?,
            deadline,
        }
        .eip712_signing_hash(&exchange_domain()),
        "closePositions" => Close {
            user: text("user")?.parse().ok()?,
            positionIds: signed
                .get("positionIds")?
                .as_array()?
                .iter()
                .map(|id| id.as_str()?.parse().ok())
                .collect::<Option<_>>()?,
            nonce: text("nonce")?.parse().ok()?,
            deadline,
        }
        .eip712_signing_hash(&exchange_domain()),
        "registerSessionKey" => RegisterSessionKey {
            user: text("user")?.parse().ok()?,
            key: text("key")?.parse().ok()?,
            expiry: text("expiry")?.parse().ok()?,
            feeAddress: text("feeAddress")?.parse().ok()?,
            nonce: text("nonce")?.parse().ok()?,
            deadline,
        }
        .eip712_signing_hash(&exchange_domain()),
        _ => funding::digest(action, signed)?,
    };
    let signature = Signature::new(text("r")?.parse().ok()?, text("s")?.parse().ok()?, text("v")? == "28");
    let signer = signature.recover_address_from_prehash(&digest).ok()?;
    let tag = keccak256("papertrade:intent-id:v2");
    Some((keccak256([tag.as_slice(), digest.as_slice(), signer.as_slice()].concat()), signer, deadline))
}

type Shared = Arc<Mutex<World>>;

fn rejection(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

async fn intents(State(world): State<Shared>, Json(body): Json<Value>) -> Response {
    let (Some(action), Some(signed)) = (body["action"].as_str(), body.get("signed")) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let Some((intent_id, signer, deadline)) = identify(action, signed) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let wallet: Address = signed["user"].as_str().unwrap().parse().unwrap();
    let mut world = world.lock().unwrap();
    world.received.push(Received { wallet, action: action.to_owned(), intent_id, at: Instant::now() });
    if world.paused.contains(action) {
        return rejection(StatusCode::CONFLICT, json!({ "code": "intake_action_paused", "action": action }));
    }
    let behavior = world
        .scripts
        .get_mut(&(wallet, action.to_owned()))
        .and_then(VecDeque::pop_front)
        .unwrap_or(Behavior::Accept);
    let Some(account) = world.accounts.get_mut(&wallet) else {
        return rejection(
            StatusCode::CONFLICT,
            json!({ "code": "admission_rejected", "reason": "noDeposit" }),
        );
    };
    // A withdrawal or a registration takes the wallet's own key. A trade takes a
    // session key the wallet has registered, and nothing else: not even the
    // wallet's own key.
    let authorized = if funding::signed_by_the_wallet(action) {
        signer == wallet
    } else {
        account.session_keys.iter().any(|(key, expiry)| *key == signer && *expiry > unix_seconds())
    };
    if !authorized {
        return rejection(StatusCode::FORBIDDEN, json!({ "code": "unauthorized_intent" }));
    }
    if let Some(refusal) = funding::refusal(action, &body, account) {
        return refusal;
    }
    if action == "openPosition" && signed["size"].as_str().unwrap().parse::<U256>().unwrap() > account.balance
    {
        return rejection(StatusCode::CONFLICT, json!({ "code": "admission_rejected", "reason": "funding" }));
    }
    match behavior {
        Behavior::Reject(code, reason) => {
            return rejection(StatusCode::CONFLICT, json!({ "code": code, "reason": reason }));
        }
        Behavior::Drop => return (StatusCode::BAD_GATEWAY, "upstream connect error").into_response(),
        _ => {}
    }
    let operation = Operation {
        intent_id,
        action: action.to_owned(),
        signed: signed.clone(),
        deadline,
        behavior,
        accepted_at: Instant::now(),
    };
    account.queue_revision += 1;
    let added = Account::operation_tuple(&operation);
    account.operations.push(operation);
    account.publish(json!({ "addedOperations": [added] }));
    if behavior == Behavior::AcceptSilently {
        return (StatusCode::BAD_GATEWAY, "upstream connect error").into_response();
    }
    Json(json!({
        "intentId": intent_id,
        "action": action,
        "state": "queued",
        "queueRevision": account.queue_revision.to_string(),
    }))
    .into_response()
}

async fn trading_state(State(world): State<Shared>) -> Json<Value> {
    let mut state: Value = serde_json::from_str(TRADING_STATE).unwrap();
    state["intake"][3] = world.lock().unwrap().paused.iter().cloned().collect();
    Json(state)
}

async fn queue_summary(State(world): State<Shared>) -> Json<Value> {
    let total = world.lock().unwrap().queue_total;
    let count = u8::from(!total.is_zero());
    Json(json!({ "ok": true, "total": total.to_string(), "count": count, "largest": null, "front": null }))
}

/// The protocol summary as captured from production: nothing tracked yet, 100 PAPER per USD.
async fn protocol_summary() -> Json<Value> {
    Json(serde_json::from_str(PROTOCOL_SUMMARY).unwrap())
}

async fn trades(State(world): State<Shared>, Path(wallet): Path<Address>) -> Json<Value> {
    let world = world.lock().unwrap();
    let items = world.accounts.get(&wallet).map(|account| account.trades.clone()).unwrap_or_default();
    Json(json!({ "items": items, "nextCursor": null }))
}

/// The Exchange's win, loss and liquidation fee rates: 2% each, as on mainnet.
const FEE_RATE: u64 = 20_000_000_000_000_000;

/// What a contract call returns, or `None` to revert. Add an arm per call the bot makes.
fn eth_call(world: &World, to: Address, data: &[u8]) -> Option<Vec<u8>> {
    let selector = |signature: &str| keccak256(signature)[..4].to_vec();
    let fee_getters = ["winFeeRate()", "lossFeeRate()", "liquidationFeeRate()"];
    if to == EXCHANGE && fee_getters.iter().any(|getter| data == selector(getter)) {
        return world.fees_readable.then(|| U256::from(FEE_RATE).to_be_bytes_vec());
    }
    None
}

/// HyperEVM JSON-RPC, as far as the bot uses it: `eth_call` only.
async fn rpc(State(world): State<Shared>, Json(request): Json<Value>) -> Json<Value> {
    let call = &request["params"][0];
    let to = call["to"].as_str().and_then(|to| to.parse::<Address>().ok());
    let data = call["data"].as_str().and_then(|data| data.parse::<alloy::primitives::Bytes>().ok());
    let returned = match (request["method"].as_str(), to, data) {
        (Some("eth_call"), Some(to), Some(data)) => {
            let world = world.lock().unwrap();
            funding::eth_call(&world, to, &data).or_else(|| eth_call(&world, to, &data))
        }
        _ => None,
    };
    Json(match returned {
        Some(bytes) => {
            json!({ "jsonrpc": "2.0", "id": request["id"], "result": alloy::primitives::Bytes::from(bytes) })
        }
        None => {
            json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": 3, "message": "execution reverted" } })
        }
    })
}

async fn live(State(world): State<Shared>, Query(query): Query<HashMap<String, String>>) -> Response {
    let Some(wallet) = query.get("wallet").and_then(|wallet| wallet.parse::<Address>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let (snapshot, frames) = {
        let world = world.lock().unwrap();
        let Some(account) = world.accounts.get(&wallet).filter(|_| world.stream_up) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        (account.snapshot(), account.frames.subscribe())
    };
    let opening = stream::iter([
        Event::default().event("liveness").data(r#"{"heartbeatIntervalMs":60000}"#),
        Event::default().event("wallet").data(snapshot),
    ]);
    let patches = stream::unfold(frames, |mut frames| async move {
        match frames.recv().await {
            Ok(Frame::Data(frame)) => Some((Event::default().event("wallet").data(frame), frames)),
            Ok(Frame::Close) | Err(_) => None,
        }
    });
    let events: std::pin::Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>> =
        Box::pin(futures::StreamExt::map(futures::StreamExt::chain(opening, patches), Ok));
    Sse::new(events).into_response()
}

/// The simulated exchange.
#[derive(Clone)]
pub struct Sim {
    world: Shared,
    pub url: String,
    quote: Arc<watch::Sender<Option<Quote>>>,
}

impl Sim {
    pub async fn start(mid: u64) -> Self {
        let world = Arc::new(Mutex::new(World {
            mid,
            paused: BTreeSet::new(),
            accounts: HashMap::new(),
            scripts: HashMap::new(),
            received: Vec::new(),
            funding: funding::Funding::default(),
            next_position_id: 1,
            stream_up: true,
            held: true,
            execution_delay: Duration::from_millis(30),
            fees_readable: true,
            queue_total: U256::ZERO,
        }));
        let app = Router::new()
            .route("/intents", post(intents))
            .route("/funding/deposits/check", post(funding::check_deposit))
            .route("/state/trading", get(trading_state))
            .route("/state/user/live", get(live))
            .route("/query/protocol/queue-summary", get(queue_summary))
            .route("/query/protocol/summary", get(protocol_summary))
            .route("/query/accounts/{wallet}/trades", get(trades))
            .route("/rpc", post(rpc))
            .with_state(world.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let stepping = world.clone();
        tokio::spawn(async move {
            loop {
                stepping.lock().unwrap().step();
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        let (quote, _) = watch::channel(Some(Quote::new(mid)));
        Self { world, url, quote: Arc::new(quote) }
    }

    fn world(&self) -> MutexGuard<'_, World> {
        self.world.lock().unwrap()
    }

    /// A funded account whose wallet has registered no session key yet.
    pub fn add_account(&self, wallet: Address, balance_usd: u64) {
        let account = Account {
            address: wallet,
            revision: 1,
            queue_revision: 0,
            balance: usd(balance_usd),
            session_keys: Vec::new(),
            positions: Vec::new(),
            operations: Vec::new(),
            unindexed: Vec::new(),
            position_lag: Duration::ZERO,
            clock_lag: 0,
            trades: Vec::new(),
            frames: broadcast::channel(256).0,
        };
        self.world().accounts.insert(wallet, account);
    }

    /// A chain reader pointed at the simulator's JSON-RPC.
    pub fn chain(&self) -> Chain {
        Chain::with_rpc(&format!("{}/rpc", self.url)).unwrap()
    }

    pub fn quote(&self) -> watch::Receiver<Option<Quote>> {
        self.quote.subscribe()
    }

    /// Moves the market: both what executions get and what the bot's feed shows.
    pub fn set_mid(&self, mid: u64) {
        self.world().mid = mid;
        self.quote.send_replace(Some(Quote::new(mid)));
    }

    /// Stops the bot's price feed without moving the market.
    pub fn silence_quotes(&self) {
        self.quote.send_replace(None);
    }

    /// Queues how the next intents of this kind from this wallet are treated.
    pub fn script(&self, wallet: Address, action: &str, behaviors: &[Behavior]) {
        self.world().scripts.entry((wallet, action.to_owned())).or_default().extend(behaviors);
    }

    pub fn pause(&self, action: &str) {
        self.world().paused.insert(action.to_owned());
    }

    pub fn unpause(&self, action: &str) {
        self.world().paused.remove(action);
    }

    /// Lets operations queued with `Behavior::Hold` execute.
    pub fn release(&self) {
        self.world().held = false;
    }

    /// Makes `key` the wallet's one registered session key, or with `None`
    /// leaves the wallet with none, and tells whoever follows the account.
    pub fn set_session_key(&self, wallet: Address, key: Option<Address>) {
        let mut world = self.world();
        let account = world.accounts.get_mut(&wallet).unwrap();
        account.session_keys = key.into_iter().map(|key| (key, unix_seconds() + 30 * 86_400)).collect();
        account.publish(json!({}));
    }

    /// Makes the wallet's new positions show up this long after their open left the queue.
    pub fn set_position_lag(&self, wallet: Address, lag: Duration) {
        self.world().accounts.get_mut(&wallet).unwrap().position_lag = lag;
    }

    /// Makes the chain clock every account reports run this many seconds behind real time.
    pub fn set_clock_lag(&self, seconds: u64) {
        for account in self.world().accounts.values_mut() {
            account.clock_lag = seconds;
        }
    }

    /// Takes the account streams down (ending open ones) or brings them back.
    pub fn set_stream_up(&self, up: bool) {
        let mut world = self.world();
        world.stream_up = up;
        if !up {
            for account in world.accounts.values() {
                let _ = account.frames.send(Frame::Close);
            }
        }
    }

    /// Sets what the LP owes to queued winners: the payout queue is empty at zero.
    pub fn set_queue_total(&self, total: U256) {
        self.world().queue_total = total;
    }

    /// Makes the Exchange's fee getters revert, or answer again.
    pub fn set_fees_readable(&self, readable: bool) {
        self.world().fees_readable = readable;
    }

    /// Liquidates the wallet's positions: they vanish and the margin is gone.
    pub fn liquidate(&self, wallet: Address) {
        let mut world = self.world();
        let mid = world.mid;
        let account = world.accounts.get_mut(&wallet).unwrap();
        let positions = std::mem::take(&mut account.positions);
        for position in &positions {
            let (_, trade) = settle(wallet, position, mid, "Liquidated");
            account.trades.insert(0, trade);
        }
        let removed: Vec<Value> = positions.iter().map(|position| json!(position.id.to_string())).collect();
        account.publish(json!({ "removedPositionIds": removed }));
    }

    pub fn received(&self) -> Vec<Received> {
        self.world().received.clone()
    }

    /// How many intents of this kind the wallet has sent.
    pub fn count(&self, wallet: Address, action: &str) -> usize {
        self.world().received.iter().filter(|r| r.wallet == wallet && r.action == action).count()
    }

    /// The wallet's open positions as `(id, is_long)`, including ones not indexed yet.
    pub fn positions(&self, wallet: Address) -> Vec<(u64, bool)> {
        let world = self.world();
        let account = &world.accounts[&wallet];
        account
            .positions
            .iter()
            .chain(account.unindexed.iter().map(|(_, position)| position))
            .map(|position| (position.id, position.is_long))
            .collect()
    }

    pub fn queued(&self, wallet: Address) -> usize {
        self.world().accounts[&wallet].operations.len()
    }

    pub fn balance(&self, wallet: Address) -> U256 {
        self.world().accounts[&wallet].balance
    }
}

/// Polls until `done` holds, or panics with `what` after `within`.
pub async fn eventually(what: &str, within: Duration, mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < within, "timed out after {within:?} waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Like `eventually`, for conditions that have to ask the database.
pub async fn eventually_stored<F, Fut>(what: &str, within: Duration, mut done: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let started = Instant::now();
    while !done().await {
        assert!(started.elapsed() < within, "timed out after {within:?} waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The first stored status for which `wanted` holds. The bot rewrites its status
/// about once a second, so this can take that long.
pub async fn status_when(h: &Harness, what: &str, wanted: impl Fn(&Snapshot) -> bool) -> Snapshot {
    let (started, within) = (Instant::now(), Duration::from_secs(5));
    loop {
        if let Some(status) = h.store.read_status().await.unwrap().filter(&wanted) {
            return status;
        }
        assert!(started.elapsed() < within, "timed out after {within:?} waiting for a status with: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Asserts that `holds` stays true for the whole of `during`.
pub async fn steadily(what: &str, during: Duration, mut holds: impl FnMut() -> bool) {
    let started = Instant::now();
    while started.elapsed() < during {
        assert!(holds(), "no longer true after {:?}: {what}", started.elapsed());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    config: Config,
}

/// Loop timings shrunk so a scenario takes a second or two instead of minutes.
pub fn fast() -> Timing {
    Timing {
        tick: Duration::from_millis(15),
        state_poll: Duration::from_millis(60),
        estimate_poll: Duration::from_millis(60),
        ledger_interval: Duration::from_millis(150),
        close_retry: Duration::from_millis(150),
        grace: Duration::from_millis(400),
        quote_max_age: Duration::from_secs(60),
        stream: StreamTiming {
            resync: Duration::from_millis(300),
            retry_min: Duration::from_millis(30),
            retry_max: Duration::from_millis(120),
        },
        funding: funding::fast(),
    }
}

/// What a scenario starts from. The default is two accounts with 2,000 USD
/// each, registered session keys, no wallet keys and rebalancing off.
#[derive(Debug, Clone, Copy)]
pub struct Setup {
    pub long_usd: u64,
    pub short_usd: u64,
    /// The bot holds each wallet's own key: the wallet automation is on and
    /// both keys were unlocked.
    pub wallet_keys: bool,
    /// `PT_REBALANCE`: the bot may start transfers between the two accounts.
    pub rebalance: bool,
    /// Whether the bot's session keys are registered with the exchange.
    pub registered: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self { long_usd: 2_000, short_usd: 2_000, wallet_keys: false, rebalance: false, registered: true }
    }
}

/// Keys as a run holds them once they are unlocked, made here in memory: new
/// session keys, no wallet keys, no file and no password anywhere.
pub fn session_keys(long: Address, short: Address) -> UnlockedKeys {
    let account =
        |owner| AccountKeys { owner, session: PrivateKeySigner::random(), wallet: WalletKey::Disabled };
    UnlockedKeys { long: account(long), short: account(short) }
}

/// The same for two wallets whose own keys the bot holds as well. A wallet's
/// own key is the key of its address, so these accounts are new wallets.
pub fn keys_with_wallets() -> UnlockedKeys {
    let account = || {
        let wallet = PrivateKeySigner::random();
        let (owner, session) = (wallet.address(), PrivateKeySigner::random());
        AccountKeys { owner, session, wallet: WalletKey::Loaded(wallet) }
    };
    UnlockedKeys { long: account(), short: account() }
}

/// The simulator, two funded accounts with registered session keys, and the bot.
pub struct Harness {
    pub sim: Sim,
    pub long: Address,
    pub short: Address,
    pub store: Store,
    /// What the bot is started with; a scenario may change any of them before it starts the bot.
    pub config: Config,
    pub keys: Arc<UnlockedKeys>,
    pub timing: Timing,
    dir: TempDir,
    bot: Option<(oneshot::Sender<()>, JoinHandle<Result<()>>)>,
}

/// BTC at 80,000.0 in wire scale.
pub const ENTRY: u64 = 800_000;
/// One percent of `ENTRY`: the default close distance.
pub const TARGET: u64 = 8_000;
/// Lifetime of every intent in the scenarios, and so the pause between two opens.
pub const TTL: Duration = Duration::from_secs(2);

impl Harness {
    /// Starts the simulator and the accounts, but not the bot.
    pub async fn prepare() -> Self {
        Self::prepare_with(Setup::default()).await
    }

    /// Like `prepare`, from the given starting point.
    pub async fn prepare_with(setup: Setup) -> Self {
        let keys = match setup.wallet_keys {
            true => keys_with_wallets(),
            false => session_keys(Address::with_last_byte(1), Address::with_last_byte(2)),
        };
        let mut harness = Self::with_keys(keys, setup.long_usd, setup.short_usd).await;
        harness.config.wallet_automation = setup.wallet_keys;
        harness.config.rebalance = setup.rebalance;
        if setup.registered {
            for account in [&harness.keys.long, &harness.keys.short] {
                harness.sim.set_session_key(account.owner, Some(account.session.address()));
            }
        }
        harness
    }

    /// Starts the simulator with these two wallets' accounts: funded, but the
    /// exchange knows no session key of theirs. The keys the harness holds are
    /// as unknown to it as any; a scenario registers them or brings its own.
    pub async fn unregistered(long: Address, short: Address) -> Self {
        Self::with_keys(session_keys(long, short), 2_000, 2_000).await
    }

    /// The simulator with one funded account per wallet and no session key
    /// registered, plus everything a bot needs to be started against it.
    async fn with_keys(keys: UnlockedKeys, long_usd: u64, short_usd: u64) -> Self {
        let sim = Sim::start(ENTRY).await;
        let dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(keys);
        let (long, short) = (keys.long.owner, keys.short.owner);
        sim.add_account(long, long_usd);
        sim.add_account(short, short_usd);
        let config = Args::try_parse_from([
            "papertrade",
            "--long-account",
            &long.to_string(),
            "--short-account",
            &short.to_string(),
            // The bot is handed its keys; nothing may go looking for them on disk.
            "--keys-dir",
            dir.path().join("no-keys-here").to_str().unwrap(),
            "--data-dir",
            dir.path().join("data").to_str().unwrap(),
            "--dry-run",
            "false",
            "--intent-ttl-seconds",
            &TTL.as_secs().to_string(),
        ])
        .unwrap()
        .config;
        let store = Store::open(&config.database()).await.unwrap();
        Self { sim, long, short, store, config, keys, timing: fast(), dir, bot: None }
    }

    /// The simulator, the accounts and a running bot.
    pub async fn start() -> Self {
        let mut harness = Self::prepare().await;
        harness.start_bot();
        harness
    }

    pub fn start_bot(&mut self) {
        let (stop, stopped) = oneshot::channel::<()>();
        let (config, store, quote) = (self.config.clone(), self.store.clone(), self.sim.quote());
        let client = Client::with_origin(&self.sim.url).unwrap();
        let (chain, keys) = (self.sim.chain(), self.keys.clone());
        let (timing, hypercore) = (self.timing, self.sim.hypercore());
        let bot = tokio::spawn(async move {
            let shutdown = async {
                let _ = stopped.await;
            };
            let price = move |_, _: &mut tokio::task::JoinSet<()>| quote;
            bot::run_with(&config, store, client, chain, price, timing, keys, hypercore, shutdown).await
        });
        self.bot = Some((stop, bot));
    }

    /// Runs the bot with a price feed that ends at once, and returns how the bot ended.
    pub async fn run_with_a_dead_feed(&self) -> Result<()> {
        let client = Client::with_origin(&self.sim.url).unwrap();
        let price = |_, tasks: &mut tokio::task::JoinSet<()>| {
            tasks.spawn(async {});
            watch::channel(None).1
        };
        bot::run_with(
            &self.config,
            self.store.clone(),
            client,
            self.sim.chain(),
            price,
            fast(),
            self.keys.clone(),
            self.sim.hypercore(),
            std::future::pending(),
        )
        .await
    }

    /// Starts the bot the way the CLI does: restarted in-process when it cannot go on,
    /// every start with the keys that were unlocked once before the first.
    /// Its first `dead_feeds` starts get a price feed that ends at once.
    pub fn start_self_healing_bot(&mut self, dead_feeds: usize) {
        let (stop, stopped) = oneshot::channel::<()>();
        let (config, store, sim) = (self.config.clone(), self.store.clone(), self.sim.clone());
        let keys = self.keys.clone();
        let bot = tokio::spawn(async move {
            let stop = futures::FutureExt::shared(async {
                let _ = stopped.await;
            });
            let mut starts = 0;
            let start = || {
                starts += 1;
                let dead = starts <= dead_feeds;
                let (config, store, quote, chain) = (config.clone(), store.clone(), sim.quote(), sim.chain());
                let client = Client::with_origin(&sim.url).unwrap();
                let (keys, hypercore) = (keys.clone(), sim.hypercore());
                async move {
                    let price = move |_, tasks: &mut tokio::task::JoinSet<()>| {
                        if dead {
                            tasks.spawn(async {});
                        }
                        quote
                    };
                    bot::start_with(&config, store, client, chain, price, fast(), keys, hypercore).await
                }
            };
            bot::keep_running(start, Duration::from_millis(100), stop).await
        });
        self.bot = Some((stop, bot));
    }

    /// Stops the bot gracefully and returns how it ended.
    pub async fn stop_bot(&mut self) -> Result<()> {
        let (stop, bot) = self.bot.take().expect("the bot is running");
        let _ = stop.send(());
        bot.await.unwrap()
    }

    /// Kills the bot where it stands, as a crash or `kill -9` would: nothing it
    /// was doing gets to finish.
    pub async fn kill_bot(&mut self) {
        let (_stop, bot) = self.bot.take().expect("the bot is running");
        bot.abort();
        let _ = bot.await;
    }

    /// Both legs are open and visible on the exchange.
    pub async fn hedged(&self) {
        let (sim, long, short) = (&self.sim, self.long, self.short);
        eventually("both legs open", Duration::from_secs(5), || {
            sim.positions(long).len() == 1 && sim.positions(short).len() == 1
        })
        .await;
    }

    /// Neither account holds a position or has anything queued.
    pub async fn flat(&self) {
        let (sim, long, short) = (&self.sim, self.long, self.short);
        eventually("both accounts flat", Duration::from_secs(5), || {
            [long, short].iter().all(|&wallet| sim.positions(wallet).is_empty() && sim.queued(wallet) == 0)
        })
        .await;
    }

    pub fn opens(&self) -> usize {
        self.sim.count(self.long, "openPosition") + self.sim.count(self.short, "openPosition")
    }

    pub fn closes(&self) -> usize {
        self.sim.count(self.long, "closePositions") + self.sim.count(self.short, "closePositions")
    }
}
