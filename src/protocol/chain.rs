//! Read-only calls to contracts on HyperEVM.

use std::time::Duration;

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol;
use alloy::sol_types::SolCall;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};

use super::deployment::{EXCHANGE, RPC_URL};

sol! {
    function winFeeRate() external view returns (uint256);
    function lossFeeRate() external view returns (uint256);
    function liquidationFeeRate() external view returns (uint256);
}

/// What the Exchange charges, each a fraction of one scaled by 1e18.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeeRates {
    /// Taken from a winning close's gain after the impact haircut.
    pub win: U256,
    /// Carved from the LP's side of a losing close while the payout queue is
    /// empty. The trader pays nothing extra, but mints PAPER on that much less.
    pub loss: U256,
    /// Taken from a liquidated margin on the LP's side.
    pub liquidation: U256,
}

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Chain {
    http: reqwest::Client,
    rpc_url: String,
}

impl Chain {
    /// HyperEVM mainnet.
    pub fn new() -> reqwest::Result<Self> {
        Self::with_rpc(RPC_URL)
    }

    /// Any JSON-RPC endpoint, such as the test simulator's.
    pub fn with_rpc(rpc_url: &str) -> reqwest::Result<Self> {
        Ok(Self { http: reqwest::Client::builder().timeout(TIMEOUT).build()?, rpc_url: rpc_url.to_owned() })
    }

    /// `eth_call` against the latest block. Returns the raw return data.
    pub async fn call(&self, to: Address, data: Bytes) -> Result<Bytes> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_call",
            "params": [{ "to": to, "data": data }, "latest"],
        });
        let reply: Value =
            self.http.post(&self.rpc_url).json(&request).send().await?.error_for_status()?.json().await?;
        if let Some(error) = reply.get("error") {
            bail!("eth_call to {to} failed: {error}");
        }
        let result = reply.get("result").and_then(Value::as_str).context("eth_call reply has no result")?;
        result.parse().with_context(|| format!("eth_call returned {result:?}"))
    }

    pub async fn win_fee_rate(&self) -> Result<U256> {
        self.exchange_rate(winFeeRateCall::SELECTOR).await.context("reading winFeeRate()")
    }

    pub async fn loss_fee_rate(&self) -> Result<U256> {
        self.exchange_rate(lossFeeRateCall::SELECTOR).await.context("reading lossFeeRate()")
    }

    pub async fn liquidation_fee_rate(&self) -> Result<U256> {
        self.exchange_rate(liquidationFeeRateCall::SELECTOR).await.context("reading liquidationFeeRate()")
    }

    /// The Exchange's three fee rates as of the latest block.
    pub async fn fee_rates(&self) -> Result<FeeRates> {
        let (win, loss, liquidation) =
            tokio::try_join!(self.win_fee_rate(), self.loss_fee_rate(), self.liquidation_fee_rate())?;
        Ok(FeeRates { win, loss, liquidation })
    }

    /// Calls a parameterless Exchange getter that returns one `uint256`.
    async fn exchange_rate(&self, selector: [u8; 4]) -> Result<U256> {
        let returned = self.call(EXCHANGE, Bytes::copy_from_slice(&selector)).await?;
        ensure!(returned.len() == 32, "expected one 32-byte word, got {} bytes", returned.len());
        Ok(U256::from_be_slice(&returned))
    }
}

#[cfg(test)]
mod tests {
    use alloy::primitives::keccak256;

    use super::*;

    #[test]
    fn the_fee_getters_are_called_by_the_selectors_seen_on_chain() {
        // The first two are the selectors the deployed Exchange was read with (issue #4).
        assert_eq!(winFeeRateCall::SELECTOR, [0x48, 0xee, 0x5e, 0x08]);
        assert_eq!(lossFeeRateCall::SELECTOR, [0xf1, 0xa8, 0x78, 0x6c]);
        assert_eq!(liquidationFeeRateCall::SELECTOR, keccak256("liquidationFeeRate()")[..4]);
    }
}
