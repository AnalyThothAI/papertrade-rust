//! The wallets' own keys, and the three things they sign.
//!
//! A session key (see [`crate::session_keys`]) can open and close positions and
//! nothing else. Registering a session key and moving funds between the two
//! accounts take the key of the wallet itself. With `PT_WALLET_AUTOMATION` on
//! and that key unlocked (see [`crate::keystore`]) the bot does both by itself;
//! otherwise the operator does them by hand.
//!
//! A wallet's key can do anything the wallet can, so once the bot has it, it
//! stays in a [`WalletSigner`], which hands out no signer and signs exactly:
//!
//! 1. a session key registration for its own wallet,
//! 2. a withdrawal to its own wallet's HyperCore account,
//! 3. a Hyperliquid transfer to the deposit address of the other account.
//!
//! Only the third names a destination, and it cannot be given one: a
//! [`DepositSend`] is built from the two account roles and reads the address
//! from the deposit proxy factory on chain.

pub mod hypercore;

use std::fmt;

use alloy::primitives::{Address, Bytes, b128};
use alloy::signers::SignerSync;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use alloy::sol_types::{SolCall, SolStruct};
use anyhow::{Context, Result, ensure};
use hypersdk::Decimal;
use hypersdk::hypercore::types::api::{Action, ActionRequest};
use hypersdk::hypercore::{
    AssetTarget, Chain as HyperliquidChain, SendAsset, SendToken, SpotSend, SpotToken,
};

use crate::protocol::chain::Chain;
use crate::protocol::deployment::DEPOSIT_PROXY_FACTORY;
use crate::protocol::intents::{
    RegisterSessionKey, SealError, SignedIntent, WithdrawToCore, exchange_domain, seal_register_session_key,
    sign_withdraw_to_core,
};
use crate::strategy::decide::Side;

sol! {
    function computeProxyAddress(address user) external view returns (address);
}

/// A wallet's own private key. See the module documentation for what it signs.
#[derive(Clone)]
pub struct WalletSigner(PrivateKeySigner);

impl fmt::Debug for WalletSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "WalletSigner({})", self.address())
    }
}

impl WalletSigner {
    /// Takes the wallet's key as the keystore unlocked it. Refused unless it is
    /// the key of that wallet.
    pub fn new(wallet: Address, signer: PrivateKeySigner) -> Result<Self> {
        ensure!(
            signer.address() == wallet,
            "this is the key of {}, not of the wallet {wallet}",
            signer.address()
        );
        Ok(Self(signer))
    }

    /// The wallet this is the key of.
    pub fn address(&self) -> Address {
        self.0.address()
    }

    /// Signs a session key registration. Refused unless it is for this wallet.
    pub fn sign_register_session_key(&self, message: &RegisterSessionKey) -> Result<SignedIntent, SealError> {
        let digest = message.eip712_signing_hash(&exchange_domain());
        seal_register_session_key(message, self.0.sign_hash_sync(&digest)?)
    }

    /// Signs a withdrawal to this wallet's HyperCore account. Refused unless it
    /// withdraws from this wallet.
    pub fn sign_withdraw_to_core(&self, message: &WithdrawToCore) -> Result<SignedIntent, SealError> {
        sign_withdraw_to_core(&self.0, message)
    }

    /// Signs the Hyperliquid transfer. Refused unless it leaves this wallet.
    pub fn sign_deposit_send(&self, send: &DepositSend, via: SendVia) -> Result<ActionRequest> {
        ensure!(
            send.from == self.address(),
            "the transfer leaves {}, which this key ({}) does not control",
            send.from,
            self.address()
        );
        let (token, amount) = (SendToken(usdc()), Decimal::from(send.amount_usdc));
        let action: Action = match via {
            SendVia::SpotSend => SpotSend { destination: send.destination, token, amount, time: send.nonce }
                .into_action(HyperliquidChain::Mainnet)
                .into(),
            SendVia::SendAsset => SendAsset {
                destination: send.destination,
                source_dex: AssetTarget::Spot,
                destination_dex: AssetTarget::Spot,
                token,
                amount,
                from_sub_account: String::new(),
                nonce: send.nonce,
            }
            .into_action(HyperliquidChain::Mainnet)
            .into(),
        };
        action.sign_sync(&self.0, send.nonce, None, None, HyperliquidChain::Mainnet)
    }
}

/// The two accounts' wallets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wallets {
    pub long: Address,
    pub short: Address,
}

impl Wallets {
    pub fn of(self, side: Side) -> Address {
        match side {
            Side::Long => self.long,
            Side::Short => self.short,
        }
    }
}

/// The wallet's deposit address: USDC that reaches this address's HyperCore
/// account is credited to the wallet's Papertrade balance, whoever sent it.
pub async fn deposit_address(chain: &Chain, wallet: Address) -> Result<Address> {
    let call = computeProxyAddressCall { user: wallet };
    let returned = chain
        .call(DEPOSIT_PROXY_FACTORY, Bytes::from(call.abi_encode()))
        .await
        .with_context(|| format!("reading the deposit address of {wallet}"))?;
    computeProxyAddressCall::abi_decode_returns(&returned)
        .with_context(|| format!("reading the deposit address of {wallet}: the factory returned {returned}"))
}

/// USDC going on Hyperliquid from one account's wallet to the other account's
/// deposit address. There is no other way to build one than
/// [`DepositSend::to_counterpart`], so there is no way to name another destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepositSend {
    from: Address,
    destination: Address,
    amount_usdc: u64,
    nonce: u64,
}

impl DepositSend {
    /// A transfer of whole USDC from the `from` account's wallet to the other
    /// account. The destination is that account's deposit address as the
    /// deposit proxy factory computes it, read from the chain here and now.
    ///
    /// Hyperliquid executes a nonce at most once per signer, so sending the same
    /// transfer again with the same nonce cannot pay twice.
    pub async fn to_counterpart(
        chain: &Chain,
        wallets: Wallets,
        from: Side,
        amount_usdc: u64,
        nonce: u64,
    ) -> Result<Self> {
        let (sender, receiver) = (wallets.of(from), wallets.of(from.other()));
        let destination = deposit_address(chain, receiver).await?;
        ensure!(
            destination != Address::ZERO && destination != sender && destination != receiver,
            "the deposit proxy factory gave {destination} for {receiver}; that cannot be a deposit address"
        );
        Ok(Self { from: sender, destination, amount_usdc, nonce })
    }

    /// The wallet the funds leave.
    pub fn from(&self) -> Address {
        self.from
    }

    /// The other account's deposit address.
    pub fn destination(&self) -> Address {
        self.destination
    }

    /// Whole USDC.
    pub fn amount_usdc(&self) -> u64 {
        self.amount_usdc
    }

    pub fn nonce(&self) -> u64 {
        self.nonce
    }
}

/// Which Hyperliquid action carries the transfer.
///
/// The official web app deposits from HyperCore with `spotSend` when the
/// sender's account is in the default mode and with `sendAsset` from spot to
/// spot when it is a unified or portfolio-margin account. This follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendVia {
    SpotSend,
    SendAsset,
}

impl SendVia {
    /// Picks the action from the `userAbstraction` info reply, as the web app
    /// does. `None` for a mode it does not know: then nothing is sent.
    pub fn for_account_mode(mode: &str) -> Option<Self> {
        match mode {
            "default" | "disabled" | "dexAbstraction" => Some(SendVia::SpotSend),
            "unifiedAccount" | "portfolioMargin" => Some(SendVia::SendAsset),
            _ => None,
        }
    }
}

/// USDC on HyperCore: token 0, with the id Hyperliquid's `spotMeta` lists for
/// it. Only the name and the id go into a transfer.
fn usdc() -> SpotToken {
    SpotToken {
        name: "USDC".into(),
        index: 0,
        token_id: b128!("0x6d1e7cde53ba9467b783cb7c530ce054"),
        evm_contract: None,
        cross_chain_address: None,
        sz_decimals: 8,
        wei_decimals: 8,
        evm_extra_decimals: 0,
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{U256, address};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::{Value, json};

    use super::*;
    use crate::protocol::intents::USDC_UNIT;

    // Well-known Anvil development keys; they hold nothing anywhere.
    const LONG_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
    const LONG: Address = address!("0x70997970C51812dc3A010C7d01b50e0d17dc79C8");
    const SHORT_KEY: &str = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
    const SHORT: Address = address!("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
    const WALLETS: Wallets = Wallets { long: LONG, short: SHORT };

    fn key(private_key: &str) -> WalletSigner {
        let signer: PrivateKeySigner = private_key.parse().unwrap();
        WalletSigner::new(signer.address(), signer).unwrap()
    }

    /// The deposit address the stub chain derives for a wallet.
    fn proxy_of(wallet: Address) -> Address {
        let mut bytes = wallet.into_array();
        bytes.reverse();
        Address::from(bytes)
    }

    /// A chain whose deposit proxy factory answers with `derive`.
    async fn chain_deriving(derive: fn(Address) -> Address) -> Chain {
        let rpc = move |Json(request): Json<Value>| async move {
            let call = &request["params"][0];
            let to: Address = call["to"].as_str().unwrap().parse().unwrap();
            let data: Bytes = call["data"].as_str().unwrap().parse().unwrap();
            assert_eq!((request["method"].as_str(), to), (Some("eth_call"), DEPOSIT_PROXY_FACTORY));
            let wallet = computeProxyAddressCall::abi_decode(&data).unwrap().user;
            let returned = Bytes::from(computeProxyAddressCall::abi_encode_returns(&derive(wallet)));
            Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": returned }))
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(
            async move { axum::serve(listener, Router::new().route("/", post(rpc))).await.unwrap() },
        );
        Chain::with_rpc(&url).unwrap()
    }

    fn send(from: Address, destination: Address) -> DepositSend {
        DepositSend { from, destination, amount_usdc: 850, nonce: 1_791_700_000_123 }
    }

    // --- the key --------------------------------------------------------------

    #[test]
    fn the_key_is_never_printed() {
        let printed = format!("{:?}", key(LONG_KEY));
        assert_eq!(printed, format!("WalletSigner({LONG})"));
    }

    #[test]
    fn a_signer_handed_over_is_taken_only_for_its_own_wallet() {
        let signer: PrivateKeySigner = LONG_KEY.parse().unwrap();
        assert_eq!(WalletSigner::new(LONG, signer.clone()).unwrap().address(), LONG);
        let error = WalletSigner::new(SHORT, signer).unwrap_err().to_string();
        assert_eq!(error, format!("this is the key of {LONG}, not of the wallet {SHORT}"));
    }

    // --- what it signs -----------------------------------------------------------

    fn registration(user: Address) -> RegisterSessionKey {
        RegisterSessionKey {
            user,
            key: address!("0x00000000000000000000000000000000000000aa"),
            expiry: 1_794_212_392,
            feeAddress: Address::ZERO,
            nonce: U256::from(256),
            deadline: 1_791_625_000,
        }
    }

    fn withdrawal(user: Address) -> WithdrawToCore {
        WithdrawToCore {
            user,
            amount: U256::from(850 * USDC_UNIT),
            nonce: U256::from(257),
            deadline: 1_791_625_000,
        }
    }

    #[test]
    fn it_signs_a_registration_and_a_withdrawal_for_its_own_wallet() {
        let registered = key(LONG_KEY).sign_register_session_key(&registration(LONG)).unwrap();
        assert_eq!((registered.action.as_str(), registered.user), ("registerSessionKey", LONG));
        let withdrawn = key(LONG_KEY).sign_withdraw_to_core(&withdrawal(LONG)).unwrap();
        assert_eq!((withdrawn.action.as_str(), withdrawn.user), ("withdrawToCore", LONG));
        assert_eq!(withdrawn.signed["amount"], "850000000");
    }

    #[test]
    fn it_signs_nothing_for_another_wallet() {
        let wrong = |result: Result<SignedIntent, SealError>| {
            assert!(matches!(result, Err(SealError::WrongSigner { expected, .. }) if expected == SHORT));
        };
        wrong(key(LONG_KEY).sign_register_session_key(&registration(SHORT)));
        wrong(key(LONG_KEY).sign_withdraw_to_core(&withdrawal(SHORT)));
        let error =
            key(LONG_KEY).sign_deposit_send(&send(SHORT, proxy_of(LONG)), SendVia::SpotSend).unwrap_err();
        assert!(error.to_string().contains("does not control"), "{error}");
    }

    #[test]
    fn the_hyperliquid_transfer_is_spot_usdc_to_the_destination_signed_by_the_wallet() {
        let send = send(LONG, proxy_of(SHORT));
        let destination = format!("{:#x}", proxy_of(SHORT));

        let request = key(LONG_KEY).sign_deposit_send(&send, SendVia::SpotSend).unwrap();
        assert_eq!(request.recover(HyperliquidChain::Mainnet).unwrap(), LONG);
        assert_eq!(request.nonce, 1_791_700_000_123);
        let action = serde_json::to_value(&request.action).unwrap();
        assert_eq!(
            action,
            json!({
                "type": "spotSend",
                "signatureChainId": "0xa4b1",
                "hyperliquidChain": "Mainnet",
                "destination": destination,
                "token": "USDC:0x6d1e7cde53ba9467b783cb7c530ce054",
                "amount": "850",
                "time": 1_791_700_000_123u64,
            })
        );

        let request = key(LONG_KEY).sign_deposit_send(&send, SendVia::SendAsset).unwrap();
        assert_eq!(request.recover(HyperliquidChain::Mainnet).unwrap(), LONG);
        let action = serde_json::to_value(&request.action).unwrap();
        assert_eq!(
            action,
            json!({
                "type": "sendAsset",
                "signatureChainId": "0xa4b1",
                "hyperliquidChain": "Mainnet",
                "destination": destination,
                "sourceDex": "spot",
                "destinationDex": "spot",
                "token": "USDC:0x6d1e7cde53ba9467b783cb7c530ce054",
                "amount": "850",
                "fromSubAccount": "",
                "nonce": 1_791_700_000_123u64,
            })
        );
    }

    #[test]
    fn the_same_transfer_signed_again_is_the_same_request() {
        // A repeat is byte for byte the first attempt, so Hyperliquid's nonce check catches it.
        let send = send(LONG, proxy_of(SHORT));
        let sign = || {
            serde_json::to_value(key(LONG_KEY).sign_deposit_send(&send, SendVia::SpotSend).unwrap()).unwrap()
        };
        assert_eq!(sign(), sign());
    }

    #[test]
    fn the_action_follows_the_senders_account_mode_like_the_web_app() {
        for mode in ["default", "disabled", "dexAbstraction"] {
            assert_eq!(SendVia::for_account_mode(mode), Some(SendVia::SpotSend));
        }
        for mode in ["unifiedAccount", "portfolioMargin"] {
            assert_eq!(SendVia::for_account_mode(mode), Some(SendVia::SendAsset));
        }
        assert_eq!(SendVia::for_account_mode("somethingNew"), None);
    }

    // --- where it sends ----------------------------------------------------------

    #[tokio::test]
    async fn a_transfer_goes_to_the_other_accounts_deposit_address_as_the_chain_derives_it() {
        let chain = chain_deriving(proxy_of).await;
        assert_eq!(deposit_address(&chain, SHORT).await.unwrap(), proxy_of(SHORT));

        let from_long = DepositSend::to_counterpart(&chain, WALLETS, Side::Long, 850, 7).await.unwrap();
        assert_eq!(
            from_long,
            DepositSend { from: LONG, destination: proxy_of(SHORT), amount_usdc: 850, nonce: 7 }
        );
        let from_short = DepositSend::to_counterpart(&chain, WALLETS, Side::Short, 10, 8).await.unwrap();
        assert_eq!(
            from_short,
            DepositSend { from: SHORT, destination: proxy_of(LONG), amount_usdc: 10, nonce: 8 }
        );
        // Each is signed by the wallet it leaves, and by no other.
        key(LONG_KEY).sign_deposit_send(&from_long, SendVia::SpotSend).unwrap();
        key(SHORT_KEY).sign_deposit_send(&from_short, SendVia::SpotSend).unwrap();
        assert!(key(SHORT_KEY).sign_deposit_send(&from_long, SendVia::SpotSend).is_err());
    }

    #[tokio::test]
    async fn an_address_that_cannot_be_a_deposit_address_is_refused() {
        // The zero address, the receiver's own wallet, the sender's own wallet.
        let derivations: [fn(Address) -> Address; 3] = [|_| Address::ZERO, |wallet| wallet, |_| LONG];
        for derive in derivations {
            let chain = chain_deriving(derive).await;
            let error = DepositSend::to_counterpart(&chain, WALLETS, Side::Long, 850, 7).await.unwrap_err();
            assert!(error.to_string().contains("cannot be a deposit address"), "{error}");
        }
    }

    /// Read-only calls to the deployed contracts. Run with `--ignored`.
    #[tokio::test]
    #[ignore = "reads HyperEVM mainnet"]
    async fn production_derives_a_deposit_address_per_wallet() {
        let chain = Chain::new().unwrap();
        let (long, short) =
            (deposit_address(&chain, LONG).await.unwrap(), deposit_address(&chain, SHORT).await.unwrap());
        assert!(long != short && long != Address::ZERO && long != LONG, "{long} {short}");
        assert_eq!(deposit_address(&chain, LONG).await.unwrap(), long);
        let send = DepositSend::to_counterpart(&chain, WALLETS, Side::Long, 10, 1).await.unwrap();
        assert_eq!(send.destination(), short);
    }

    /// The Exchange checks the signature before the balance, so simulating a
    /// withdrawal for a wallet that holds nothing shows whether it was accepted:
    /// signed by the wallet it fails on the balance, signed by anyone else on the signer.
    #[tokio::test]
    #[ignore = "reads HyperEVM mainnet"]
    async fn production_accepts_the_withdrawal_signature_of_the_wallet_and_of_nobody_else() {
        sol! {
            function withdrawToCoreBySig(address user, uint256 amount, uint256 nonce, uint64 deadline, uint8 v, bytes32 r, bytes32 s) external;
            error WithdrawalExceedsBalance();
        }
        use crate::protocol::deployment::EXCHANGE;
        use alloy::sol_types::SolError;

        let message = WithdrawToCore {
            user: LONG,
            amount: U256::from(10 * USDC_UNIT),
            nonce: U256::from(1_791_700_000_123u64),
            deadline: u64::try_from(crate::store::now_ms() / 1000).unwrap() + 600,
        };
        let simulate = |signer: &'static str| {
            let digest = message.eip712_signing_hash(&exchange_domain());
            let signature = signer.parse::<PrivateKeySigner>().unwrap().sign_hash_sync(&digest).unwrap();
            let call = withdrawToCoreBySigCall {
                user: message.user,
                amount: message.amount,
                nonce: message.nonce,
                deadline: message.deadline,
                v: 27 + u8::from(signature.v()),
                r: signature.r().into(),
                s: signature.s().into(),
            };
            async move { Chain::new().unwrap().call(EXCHANGE, Bytes::from(call.abi_encode())).await }
        };
        let no_balance = alloy::primitives::hex::encode(WithdrawalExceedsBalance::SELECTOR);
        let by_wallet = simulate(LONG_KEY).await.unwrap_err().to_string();
        assert!(by_wallet.contains(&no_balance), "{by_wallet}");
        let by_stranger = simulate(SHORT_KEY).await.unwrap_err().to_string();
        assert!(!by_stranger.contains(&no_balance), "{by_stranger}");
        // And the intent the bot would send carries exactly that signature.
        let intent = key(LONG_KEY).sign_withdraw_to_core(&message).unwrap();
        let digest = message.eip712_signing_hash(&exchange_domain());
        let signature = LONG_KEY.parse::<PrivateKeySigner>().unwrap().sign_hash_sync(&digest).unwrap();
        assert_eq!(intent.signed["r"], format!("{:#066x}", signature.r()));
    }

    #[tokio::test]
    async fn no_transfer_is_built_when_the_chain_cannot_be_read() {
        // A factory that reverts, and an answer that is not an address.
        let reverting = |Json(request): Json<Value>| async move {
            Json(
                json!({ "jsonrpc": "2.0", "id": request["id"], "error": { "code": 3, "message": "execution reverted" } }),
            )
        };
        let garbled = |Json(request): Json<Value>| async move {
            Json(json!({ "jsonrpc": "2.0", "id": request["id"], "result": "0x1234" }))
        };
        for rpc in [Router::new().route("/", post(reverting)), Router::new().route("/", post(garbled))] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let chain = Chain::with_rpc(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
            tokio::spawn(async move { axum::serve(listener, rpc).await.unwrap() });
            let error = DepositSend::to_counterpart(&chain, WALLETS, Side::Long, 850, 7).await.unwrap_err();
            assert!(format!("{error:#}").contains(&format!("the deposit address of {SHORT}")), "{error:#}");
        }
    }
}
