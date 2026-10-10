//! The simulator's side of funding: HyperCore balances, withdrawals that land
//! there, Hyperliquid sends, and the relayer sweeping deposit addresses into
//! Papertrade balances. Also session key registration, which the same wallet
//! key signs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, U256, keccak256};
use alloy::sol_types::SolStruct;
use anyhow::{Result, bail};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
use papertrade::bot::FundingTiming;
use papertrade::protocol::deployment::{DEPOSIT_PROXY_FACTORY, usd};
use papertrade::protocol::intents::{RegisterSessionKey, WithdrawToCore, exchange_domain};
use papertrade::strategy::transfer::Limits;
use papertrade::wallet::hypercore::HyperCore;
use papertrade::wallet::{DepositSend, WalletSigner};
use serde_json::{Value, json};

use super::{Account, Behavior, Operation, Shared, Sim, World, rejection, unix_seconds};

/// The relayer's minimum for a withdrawal and for a sweep, in USDC.
const MINIMUM_USDC: u64 = 10;
/// What is held waits to be released; an hour is as good as forever here.
const HELD: Duration = Duration::from_secs(3_600);

/// How the simulated Hyperliquid treats the next send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendBehavior {
    /// Take it, move the funds, answer ok.
    Deliver,
    /// The request never arrives: an error, and nothing moves.
    Lose,
    /// Take it and move the funds, but the answer is lost: an error all the same.
    DeliverSilently,
    /// Answer with this error; nothing moves.
    Refuse(&'static str),
}

/// When the relayer credits funds that reached a deposit address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sweep {
    /// By itself, this long after they arrive.
    After(Duration),
    /// Only once it has been asked to check that wallet's deposit address.
    OnCheck,
    Never,
}

/// A send the bot made on the simulated Hyperliquid, whatever became of it.
#[derive(Debug, Clone)]
pub struct HlSend {
    pub from: Address,
    pub destination: Address,
    pub amount_usdc: u64,
    pub nonce: u64,
    pub at: Instant,
    /// Hyperliquid took it: the funds moved, or will once the send delay is over.
    pub taken: bool,
}

pub(super) struct Funding {
    /// USDC on HyperCore per address, wallets and deposit addresses alike. 18 decimals.
    core: HashMap<Address, U256>,
    /// Movements on HyperCore that are due later: `(when, from, to, amount)`. A
    /// withdrawal arriving has no `from`.
    due: Vec<(Instant, Option<Address>, Address, U256)>,
    /// When funds last arrived at each deposit address, and when the relayer was asked to check it.
    arrived: HashMap<Address, Instant>,
    checked: HashMap<Address, Instant>,
    check_count: HashMap<Address, usize>,
    sends: Vec<HlSend>,
    /// How often a HyperCore balance was asked for.
    balance_reads: usize,
    send_script: VecDeque<SendBehavior>,
    used_nonces: HashSet<(Address, u64)>,
    /// How long a withdrawal takes from the Exchange to the wallet's HyperCore
    /// account. `None`: until released.
    core_delay: Option<Duration>,
    /// What HyperCore keeps of a withdrawal on arrival, and the relayer of a deposit it sweeps.
    arrival_fee: U256,
    sweep_fee: U256,
    /// How long Hyperliquid takes to move the funds of a send it took. `None`: until released.
    send_delay: Option<Duration>,
    sweep: Sweep,
    /// What the relayer answers when asked for a wallet's deposit address, if not the truth.
    decoy: Option<Address>,
    /// Whether the deposit proxy factory answers; when not, it reverts.
    proxies_readable: bool,
}

impl Default for Funding {
    fn default() -> Self {
        Self {
            core: HashMap::new(),
            due: Vec::new(),
            arrived: HashMap::new(),
            checked: HashMap::new(),
            check_count: HashMap::new(),
            sends: Vec::new(),
            balance_reads: 0,
            send_script: VecDeque::new(),
            used_nonces: HashSet::new(),
            core_delay: Some(Duration::from_millis(20)),
            arrival_fee: U256::ZERO,
            sweep_fee: U256::ZERO,
            send_delay: Some(Duration::ZERO),
            sweep: Sweep::After(Duration::from_millis(40)),
            decoy: None,
            proxies_readable: true,
        }
    }
}

/// The deposit address the simulated factory derives for a wallet.
fn proxy_of(wallet: Address) -> Address {
    Address::from_slice(&keccak256([b"deposit proxy".as_slice(), wallet.as_slice()].concat())[12..])
}

/// Funding timings shrunk like the rest of the loop's, except the two that
/// decide when the bot gives up waiting: an attempt is not repeated and a
/// transfer does not become stuck merely because the machine running the
/// scenarios is busy. A scenario about a repeat or about being stuck sets its own.
pub fn fast() -> FundingTiming {
    FundingTiming {
        transfer: Limits {
            settle: Duration::from_secs(30),
            credit_check: Duration::from_millis(100),
            stuck_after: Duration::from_secs(30),
            reading_max_age: Duration::from_millis(200),
            max_attempts: 3,
        },
        core_poll: Duration::from_millis(25),
        register_retry: Duration::from_millis(600),
    }
}

// --- the relayer ---------------------------------------------------------------

/// Actions only the wallet's own key may sign; the others take a session key.
pub(super) fn signed_by_the_wallet(action: &str) -> bool {
    matches!(action, "withdrawToCore" | "registerSessionKey")
}

/// The EIP-712 digest of a funding intent, or `None` if it is not one or is malformed.
pub(super) fn digest(action: &str, signed: &Value) -> Option<B256> {
    let text = |field: &str| signed.get(field)?.as_str();
    Some(match action {
        "withdrawToCore" => WithdrawToCore {
            user: text("user")?.parse().ok()?,
            amount: text("amount")?.parse().ok()?,
            nonce: text("nonce")?.parse().ok()?,
            deadline: text("deadline")?.parse().ok()?,
        }
        .eip712_signing_hash(&exchange_domain()),
        "registerSessionKey" => RegisterSessionKey {
            user: text("user")?.parse().ok()?,
            key: text("key")?.parse().ok()?,
            expiry: text("expiry")?.parse().ok()?,
            feeAddress: text("feeAddress")?.parse().ok()?,
            nonce: text("nonce")?.parse().ok()?,
            deadline: text("deadline")?.parse().ok()?,
        }
        .eip712_signing_hash(&exchange_domain()),
        _ => return None,
    })
}

/// A withdrawal's amount in 18-decimal USD; the intent carries it with 6.
fn withdrawn(signed: &Value) -> U256 {
    signed["amount"].as_str().unwrap().parse::<U256>().unwrap() * U256::from(10).pow(U256::from(12))
}

/// The relayer's reasons to refuse a withdrawal before queueing it.
pub(super) fn refusal(action: &str, body: &Value, account: &Account) -> Option<Response> {
    if action != "withdrawToCore" {
        return None;
    }
    let invalid = |reason: &str| {
        Some(rejection(StatusCode::BAD_REQUEST, json!({ "code": "invalid_intent", "reason": reason })))
    };
    if body["withdrawal"] != json!({ "route": "hypercore" }) {
        return invalid("withdrawal route");
    }
    let amount = withdrawn(&body["signed"]);
    if amount < usd(MINIMUM_USDC) {
        return invalid("minimum");
    }
    if amount > account.balance {
        return Some(rejection(
            StatusCode::CONFLICT,
            json!({ "code": "admission_rejected", "reason": "funding" }),
        ));
    }
    None
}

impl Funding {
    /// Executes a queued withdrawal or registration. Any other operation is handed back.
    pub(super) fn execute(&mut self, account: &mut Account, operation: Operation) -> Option<Operation> {
        if !signed_by_the_wallet(&operation.action) {
            return Some(operation);
        }
        let id = operation.intent_id;
        account.queue_revision += 1;
        let ended = |outcome: &str| json!({ "removedOperationIds": [id], "intentOutcomes": [[id, outcome]] });
        if account.chain_seconds() > operation.deadline {
            account.publish(ended("expired"));
            return None;
        }
        if operation.behavior == Behavior::AcceptThenFail {
            account.publish(ended("failed"));
            return None;
        }
        let signed = &operation.signed;
        if operation.action == "registerSessionKey" {
            let key: Address = signed["key"].as_str().unwrap().parse().unwrap();
            let expiry: u64 = signed["expiry"].as_str().unwrap().parse().unwrap();
            account.session_keys.retain(|(registered, _)| *registered != key);
            account.session_keys.push((key, expiry));
            account.publish(ended("succeeded"));
            return None;
        }
        let amount = withdrawn(signed);
        if amount > account.balance {
            account.publish(ended("failed"));
            return None;
        }
        // It leaves the Exchange now and reaches the wallet's HyperCore account a little later.
        account.balance -= amount;
        let delay = self.core_delay.unwrap_or(HELD);
        self.due.push((Instant::now() + delay, None, account.address, amount - self.arrival_fee));
        account.publish(ended("succeeded"));
        None
    }
}

/// Carries out what is due on HyperCore, then lets the relayer sweep.
pub(super) fn step(world: &mut World) {
    let funding = &mut world.funding;
    let now = Instant::now();
    let (due, waiting): (Vec<_>, Vec<_>) =
        std::mem::take(&mut funding.due).into_iter().partition(|(at, ..)| *at <= now);
    funding.due = waiting;
    for (_, from, to, amount) in due {
        if let Some(from) = from {
            *funding.core.entry(from).or_default() -= amount;
        }
        *funding.core.entry(to).or_default() += amount;
        funding.arrived.insert(to, now);
    }

    for account in world.accounts.values_mut() {
        let proxy = proxy_of(account.address);
        let held = funding.core.get(&proxy).copied().unwrap_or_default();
        let Some(arrived) = funding.arrived.get(&proxy).copied() else { continue };
        let swept = held >= usd(MINIMUM_USDC)
            && match funding.sweep {
                Sweep::After(delay) => arrived + delay <= now,
                Sweep::OnCheck => {
                    funding.checked.get(&account.address).is_some_and(|asked| *asked >= arrived)
                }
                Sweep::Never => false,
            };
        if swept {
            funding.core.insert(proxy, U256::ZERO);
            funding.arrived.remove(&proxy);
            account.balance += held - funding.sweep_fee;
            account.publish(json!({}));
        }
    }
}

/// `computeProxyAddress(address)` on the deposit proxy factory.
pub(super) fn eth_call(world: &World, to: Address, data: &[u8]) -> Option<Vec<u8>> {
    let selector = &keccak256("computeProxyAddress(address)")[..4];
    if to != DEPOSIT_PROXY_FACTORY || data.len() != 36 || &data[..4] != selector {
        return None;
    }
    if !world.funding.proxies_readable {
        return None;
    }
    let proxy = proxy_of(Address::from_slice(&data[16..]));
    Some(proxy.into_word().to_vec())
}

/// `POST /funding/deposits/check`
pub(super) async fn check_deposit(State(world): State<Shared>, Json(body): Json<Value>) -> Response {
    let Some(wallet) = body["user"].as_str().and_then(|user| user.parse::<Address>().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut world = world.lock().unwrap();
    let funding = &mut world.funding;
    funding.checked.insert(wallet, Instant::now());
    *funding.check_count.entry(wallet).or_default() += 1;
    Json(json!({ "proxy": funding.decoy.unwrap_or_else(|| proxy_of(wallet)) })).into_response()
}

// --- Hyperliquid -----------------------------------------------------------------

/// HyperCore as the simulator keeps it.
struct SimHyperCore {
    world: Shared,
}

impl HyperCore for SimHyperCore {
    fn spot_usdc(&self, wallet: Address) -> BoxFuture<'_, Result<U256>> {
        let mut world = self.world.lock().unwrap();
        world.funding.balance_reads += 1;
        let held = world.funding.core.get(&wallet).copied().unwrap_or_default();
        Box::pin(async move { Ok(held) })
    }

    fn send<'a>(&'a self, key: &'a WalletSigner, send: &'a DepositSend) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut world = self.world.lock().unwrap();
            let funding = &mut world.funding;
            let behavior = funding.send_script.pop_front().unwrap_or(SendBehavior::Deliver);
            let (from, amount) = (send.from(), usd(send.amount_usdc()));
            let mut record = HlSend {
                from,
                destination: send.destination(),
                amount_usdc: send.amount_usdc(),
                nonce: send.nonce(),
                at: Instant::now(),
                taken: false,
            };
            let refused = match behavior {
                SendBehavior::Lose => Some("error sending request: connection reset"),
                SendBehavior::Refuse(reason) => Some(reason),
                // Hyperliquid's own checks, in its order: who signed, the nonce, the balance.
                _ if key.address() != from => Some("User or API Wallet does not exist"),
                _ if !funding.used_nonces.insert((from, send.nonce())) => Some("Nonce already used"),
                _ if funding.core.get(&from).copied().unwrap_or_default() < amount => {
                    Some("Insufficient spot balance")
                }
                _ => None,
            };
            if let Some(reason) = refused {
                funding.sends.push(record);
                bail!("{reason}");
            }
            record.taken = true;
            funding.sends.push(record);
            let delay = funding.send_delay.unwrap_or(HELD);
            funding.due.push((Instant::now() + delay, Some(from), send.destination(), amount));
            if behavior == SendBehavior::DeliverSilently {
                bail!("error decoding response body: operation timed out");
            }
            Ok(())
        })
    }
}

impl Sim {
    /// The simulator's HyperCore, for the bot to read balances from and send on.
    pub fn hypercore(&self) -> Arc<dyn HyperCore> {
        Arc::new(SimHyperCore { world: self.world.clone() })
    }

    /// USDC an address holds on HyperCore, 18-decimal.
    pub fn core_usdc(&self, address: Address) -> U256 {
        self.world().funding.core.get(&address).copied().unwrap_or_default()
    }

    /// The wallet's deposit address, as the deposit proxy factory derives it.
    pub fn deposit_address(&self, wallet: Address) -> Address {
        proxy_of(wallet)
    }

    /// How often the bot has asked Hyperliquid for a HyperCore balance.
    pub fn hypercore_reads(&self) -> usize {
        self.world().funding.balance_reads
    }

    /// Every send the bot made on Hyperliquid, in order.
    pub fn sends(&self) -> Vec<HlSend> {
        self.world().funding.sends.clone()
    }

    /// Queues how the next Hyperliquid sends are treated.
    pub fn script_sends(&self, behaviors: &[SendBehavior]) {
        self.world().funding.send_script.extend(behaviors);
    }

    /// Makes Hyperliquid take sends without moving their funds until `release_sends`.
    pub fn hold_sends(&self) {
        self.world().funding.send_delay = None;
    }

    /// Moves the funds of every send that was held, and of later ones at once.
    pub fn release_sends(&self) {
        let mut world = self.world();
        world.funding.send_delay = Some(Duration::ZERO);
        let now = Instant::now();
        for (at, from, ..) in &mut world.funding.due {
            if from.is_some() {
                *at = now;
            }
        }
    }

    /// Keeps withdrawals that have left the Exchange from reaching HyperCore until `release_arrivals`.
    pub fn hold_arrivals(&self) {
        self.world().funding.core_delay = None;
    }

    /// Lets every withdrawal that was held arrive, and later ones at once.
    pub fn release_arrivals(&self) {
        let mut world = self.world();
        world.funding.core_delay = Some(Duration::ZERO);
        let now = Instant::now();
        for (at, from, ..) in &mut world.funding.due {
            if from.is_none() {
                *at = now;
            }
        }
    }

    pub fn set_sweep(&self, sweep: Sweep) {
        self.world().funding.sweep = sweep;
    }

    /// Makes a withdrawal reach HyperCore this many USDC short, as when the
    /// wallet's HyperCore account has to be activated first.
    pub fn set_arrival_fee(&self, usdc: u64) {
        self.world().funding.arrival_fee = usd(usdc);
    }

    /// Makes the relayer credit this many USDC less than it sweeps.
    pub fn set_sweep_fee(&self, usdc: u64) {
        self.world().funding.sweep_fee = usd(usdc);
    }

    /// Takes USDC out of an address's HyperCore account, as something else using that wallet would.
    pub fn take_from_core(&self, address: Address, usdc: u64) {
        *self.world().funding.core.get_mut(&address).unwrap() -= usd(usdc);
    }

    /// How often the relayer was asked to check the wallet's deposit address.
    pub fn deposit_checks(&self, wallet: Address) -> usize {
        self.world().funding.check_count.get(&wallet).copied().unwrap_or_default()
    }

    /// Makes the relayer name this address whenever it is asked for a deposit address.
    pub fn answer_deposit_checks_with(&self, decoy: Address) {
        self.world().funding.decoy = Some(decoy);
    }

    /// Makes the deposit proxy factory revert, or answer again.
    pub fn set_proxies_readable(&self, readable: bool) {
        self.world().funding.proxies_readable = readable;
    }

    /// Sets the wallet's Papertrade balance, as a deposit or withdrawal made elsewhere would.
    pub fn set_balance(&self, wallet: Address, balance_usd: u64) {
        let mut world = self.world();
        let account = world.accounts.get_mut(&wallet).unwrap();
        account.balance = usd(balance_usd);
        account.publish(json!({}));
    }

    /// Makes the wallet's registration of the session key run out at `expiry`.
    pub fn set_session_key_expiry(&self, wallet: Address, key: Address, expiry: u64) {
        let mut world = self.world();
        let account = world.accounts.get_mut(&wallet).unwrap();
        account.session_keys.retain(|(registered, _)| *registered != key);
        account.session_keys.push((key, expiry));
        account.publish(json!({}));
    }

    /// When the wallet's registration of the session key runs out, if it is registered.
    pub fn session_key_expiry(&self, wallet: Address, key: Address) -> Option<u64> {
        let world = self.world();
        let registered =
            world.accounts[&wallet].session_keys.iter().find(|(registered, _)| *registered == key);
        registered.map(|(_, expiry)| *expiry).filter(|expiry| *expiry > unix_seconds())
    }
}
