//! HyperCore as far as moving funds between the two accounts needs it: one
//! balance to read and one transfer to send.

use std::time::Duration;

use alloy::primitives::{Address, U256};
use anyhow::{Context, Result, bail};
use futures::future::BoxFuture;
use hypersdk::Decimal;
use hypersdk::hypercore::{self, HttpClient};
use serde_json::json;

use super::{DepositSend, SendVia, WalletSigner};
use crate::protocol::deployment::USD_DECIMALS;

const INFO_URL: &str = "https://api.hyperliquid.xyz/info";
const TIMEOUT: Duration = Duration::from_secs(10);

/// The boundary to HyperCore. Production talks to Hyperliquid's API; the
/// tests' simulated exchange keeps the same books itself.
pub trait HyperCore: Send + Sync {
    /// The wallet's spot USDC on HyperCore, as 18-decimal USD.
    fn spot_usdc(&self, wallet: Address) -> BoxFuture<'_, Result<U256>>;

    /// Sends the transfer, signed with the key of the wallet it leaves.
    ///
    /// An error says nothing about whether the funds moved: the request may
    /// have executed and only its answer been lost. The caller finds out from
    /// the balance.
    fn send<'a>(&'a self, key: &'a WalletSigner, send: &'a DepositSend) -> BoxFuture<'a, Result<()>>;
}

/// Hyperliquid mainnet.
pub struct Mainnet {
    api: HttpClient,
    http: reqwest::Client,
}

impl Mainnet {
    pub fn new() -> reqwest::Result<Self> {
        Ok(Self { api: hypercore::mainnet(), http: reqwest::Client::builder().timeout(TIMEOUT).build()? })
    }

    /// The account's abstraction mode, which decides the action a transfer
    /// uses. Asked directly: the SDK's parser does not know `default`, the
    /// answer for an ordinary account.
    async fn account_mode(&self, wallet: Address) -> Result<String> {
        let request = json!({ "type": "userAbstraction", "user": format!("{wallet:#x}") });
        let reply = self.http.post(INFO_URL).json(&request).send().await?.error_for_status()?;
        Ok(reply.json().await?)
    }
}

impl HyperCore for Mainnet {
    fn spot_usdc(&self, wallet: Address) -> BoxFuture<'_, Result<U256>> {
        Box::pin(async move {
            let balances = self.api.user_balances(wallet).await?;
            // An account that never held USDC has no entry for it.
            let usdc = balances.iter().find(|balance| balance.coin == "USDC" || balance.token == Some(0));
            usdc.map_or(Ok(U256::ZERO), |balance| usd_from_decimal(balance.total))
        })
    }

    fn send<'a>(&'a self, key: &'a WalletSigner, send: &'a DepositSend) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mode = self.account_mode(send.from()).await.context("reading the sender's account mode")?;
            let via = SendVia::for_account_mode(&mode).with_context(|| {
                format!("the sender's Hyperliquid account is in mode {mode:?}; not sending")
            })?;
            let request = key.sign_deposit_send(send, via)?;
            self.api.send(request).await?.into_default()
        })
    }
}

/// A HyperCore balance as an exact 18-decimal amount.
fn usd_from_decimal(amount: Decimal) -> Result<U256> {
    let Ok(mantissa) = u128::try_from(amount.mantissa()) else { bail!("negative balance {amount}") };
    let ten = U256::from(10);
    let decimals = u32::from(USD_DECIMALS);
    Ok(match decimals.checked_sub(amount.scale()) {
        Some(missing) => U256::from(mantissa) * ten.pow(U256::from(missing)),
        None => U256::from(mantissa) / ten.pow(U256::from(amount.scale() - decimals)),
    })
}

#[cfg(test)]
mod tests {
    use hypersdk::dec;

    use super::*;
    use crate::protocol::deployment::usd;

    #[test]
    fn hypercore_balances_convert_exactly() {
        assert_eq!(usd_from_decimal(dec!(0)).unwrap(), U256::ZERO);
        assert_eq!(usd_from_decimal(dec!(850)).unwrap(), usd(850));
        assert_eq!(usd_from_decimal(dec!(850.0)).unwrap(), usd(850));
        // HyperCore keeps USDC to eight places.
        assert_eq!(
            usd_from_decimal(dec!(22.40272782)).unwrap(),
            U256::from(2_240_272_782u64) * U256::from(10).pow(U256::from(10))
        );
        assert!(usd_from_decimal(dec!(-1)).is_err());
    }

    /// Read-only requests to Hyperliquid's info endpoint. Run with `--ignored`.
    #[tokio::test]
    #[ignore = "reads Hyperliquid mainnet"]
    async fn mainnet_reports_spot_usdc_and_an_account_mode_the_transfer_knows() {
        let mainnet = Mainnet::new().unwrap();
        // The burn address holds some USDC; an address nobody uses holds none and is still answered for.
        let burn = alloy::primitives::address!("0x000000000000000000000000000000000000dEaD");
        assert!(mainnet.spot_usdc(burn).await.unwrap() > U256::ZERO);
        let unused = alloy::primitives::address!("0x1234567890abcdef1234567890abcdef12345678");
        assert_eq!(mainnet.spot_usdc(unused).await.unwrap(), U256::ZERO);
        for wallet in [burn, unused] {
            let mode = mainnet.account_mode(wallet).await.unwrap();
            assert!(SendVia::for_account_mode(&mode).is_some(), "unknown account mode {mode:?}");
        }
    }
}
