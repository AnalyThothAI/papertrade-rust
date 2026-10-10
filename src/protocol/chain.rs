//! Read-only calls to contracts on HyperEVM.

use std::time::Duration;

use alloy::primitives::{Address, Bytes};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::deployment::RPC_URL;

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
}
