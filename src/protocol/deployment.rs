//! Papertrade production deployment on HyperEVM mainnet.
//!
//! The values mirror the deployment record shipped in the official frontend
//! bundle (exchange.papertrade.xyz). The public docs still list the contract
//! addresses as "TBA", so the frontend is the only source for them.

use alloy::primitives::{Address, U256, address};

pub const ORIGIN: &str = "https://exchange.papertrade.xyz";
pub const CHAIN_ID: u64 = 999;
pub const EXCHANGE: Address = address!("0x6cd5661646289fb6e65ea5c032310fded797d0a2");
/// Derives each wallet's deposit address: `computeProxyAddress(address)`.
pub const DEPOSIT_PROXY_FACTORY: Address = address!("0x1629b7d46bef6e13ff754174fc9f32dea309b38f");
/// HyperEVM JSON-RPC, for read-only contract calls.
pub const RPC_URL: &str = "https://rpc.hyperliquid.xyz/evm";

/// Collateral and PAPER amounts on the wire are fixed-point with 18 decimals.
pub const USD_DECIMALS: u8 = 18;

/// Lifetime the official frontend gives a new session key.
pub const SESSION_KEY_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// Whole USD as an 18-decimal wire amount.
pub fn usd(amount: u64) -> U256 {
    U256::from(amount) * U256::from(10).pow(U256::from(USD_DECIMALS))
}
