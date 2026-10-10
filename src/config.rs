//! Configuration: command-line flags with `PT_*` environment variables behind them.

use std::path::PathBuf;

use alloy::primitives::Address;
use anyhow::{Context, Result, ensure};
use clap::{ArgAction, Args};

use crate::protocol::deployment::usd;
use crate::strategy::decide::{Side, StrategyParams};

/// The hard liquidation trigger sits this far before the zero-equity price.
const BUST_BUFFER_BPS: u32 = 5;

#[derive(Debug, Clone, Args)]
pub struct Config {
    /// Master wallet of the account that goes long.
    #[arg(long, env = "PT_LONG_ACCOUNT", global = true)]
    pub long_account: Option<Address>,

    /// Master wallet of the account that goes short.
    #[arg(long, env = "PT_SHORT_ACCOUNT", global = true)]
    pub short_account: Option<Address>,

    /// Where the keys live, encrypted: <wallet>.session.json, and
    /// <wallet>.wallet.json with the wallet automation on. `keys init` writes
    /// them and `run` reads them.
    #[arg(long, env = "PT_KEYS_DIR", default_value = "./secrets", global = true)]
    pub keys_dir: PathBuf,

    /// Where the database lives.
    #[arg(long, env = "PT_DATA_DIR", default_value = "./data", global = true)]
    pub data_dir: PathBuf,

    #[arg(long, env = "PT_COIN", default_value = "BTC", global = true)]
    pub coin: String,

    /// Position value per leg in USD.
    #[arg(long, env = "PT_NOTIONAL_USD", default_value_t = 10_000, global = true)]
    pub notional_usd: u64,

    #[arg(long, env = "PT_LEVERAGE", default_value_t = 20, global = true)]
    pub leverage: u32,

    /// Close both legs once price is this far from the entry.
    #[arg(long, env = "PT_CLOSE_BPS", default_value_t = 100, global = true)]
    pub close_bps: u32,

    /// Only open while the payout queue is empty.
    #[arg(long, env = "PT_REQUIRE_EMPTY_QUEUE", default_value_t = true, action = ArgAction::Set, global = true)]
    pub require_empty_queue: bool,

    /// Lifetime of each signed open or close. Short on purpose: it bounds how
    /// long an intent with an unknown outcome can still execute.
    #[arg(long, env = "PT_INTENT_TTL_SECONDS", default_value_t = 120, global = true)]
    pub intent_ttl_seconds: u64,

    /// Decide and record, never submit.
    #[arg(long, env = "PT_DRY_RUN", default_value_t = true, action = ArgAction::Set, global = true)]
    pub dry_run: bool,

    /// Let the bot sign with the two wallets' own keys: register and renew
    /// the session keys. Off, those keys are neither asked for nor read,
    /// whatever the keys directory holds.
    #[arg(long, env = "PT_WALLET_AUTOMATION", default_value_t = false, action = ArgAction::Set, global = true)]
    pub wallet_automation: bool,

    /// Let the bot move funds between the two accounts when one of them runs
    /// short. A transfer is signed with the wallets' own keys, so this needs
    /// PT_WALLET_AUTOMATION=true. Off, no transfer is started; one that has
    /// started already is still carried to its end.
    #[arg(long, env = "PT_REBALANCE", default_value_t = false, action = ArgAction::Set, global = true)]
    pub rebalance: bool,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.leverage >= 1 && self.close_bps >= 1, "leverage and close distance must be positive");
        // The losing leg must still be far from liquidation when the target is hit,
        // because closing takes a relayer round trip.
        ensure!(
            2 * self.close_bps + BUST_BUFFER_BPS <= 10_000 / self.leverage,
            "PT_CLOSE_BPS must be at most half the distance to liquidation at PT_LEVERAGE"
        );
        ensure!(
            (30..=3600).contains(&self.intent_ttl_seconds),
            "PT_INTENT_TTL_SECONDS must be between 30 and 3600"
        );
        ensure!(
            self.long_account.is_none() || self.long_account != self.short_account,
            "PT_LONG_ACCOUNT and PT_SHORT_ACCOUNT must be different wallets"
        );
        ensure!(
            !self.rebalance || self.wallet_automation,
            "PT_REBALANCE=true needs PT_WALLET_AUTOMATION=true: a transfer between the two accounts is signed \
             with the wallets' own keys"
        );
        Ok(())
    }

    pub fn strategy(&self) -> StrategyParams {
        StrategyParams {
            notional: usd(self.notional_usd),
            leverage: self.leverage,
            close_bps: self.close_bps,
            require_empty_queue: self.require_empty_queue,
        }
    }

    pub fn wallet(&self, side: Side) -> Result<Address> {
        match side {
            Side::Long => self.long_account.context("PT_LONG_ACCOUNT is not set"),
            Side::Short => self.short_account.context("PT_SHORT_ACCOUNT is not set"),
        }
    }

    pub fn database(&self) -> PathBuf {
        self.data_dir.join("papertrade.db")
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Debug, Parser)]
    struct Cli {
        #[command(flatten)]
        config: Config,
    }

    const A: &str = "0x00000000000000000000000000000000000000Aa";
    const B: &str = "0x00000000000000000000000000000000000000bb";

    fn parse(args: &[&str]) -> Config {
        Cli::try_parse_from(std::iter::once("papertrade").chain(args.iter().copied())).unwrap().config
    }

    #[test]
    fn defaults_to_a_dry_run_of_the_minimum_size_btc_round() {
        let config = parse(&[]);
        config.validate().unwrap();
        assert!(config.dry_run && !config.wallet_automation && !config.rebalance);
        assert_eq!((config.coin.as_str(), config.intent_ttl_seconds), ("BTC", 120));
        assert_eq!(
            config.strategy(),
            StrategyParams { notional: usd(10_000), leverage: 20, close_bps: 100, require_empty_queue: true }
        );
        assert!(config.wallet(Side::Long).is_err());
        assert_eq!(config.database(), PathBuf::from("./data/papertrade.db"));
    }

    #[test]
    fn switches_take_explicit_values() {
        let config = parse(&[
            "--dry-run",
            "false",
            "--require-empty-queue",
            "false",
            "--wallet-automation",
            "true",
            "--long-account",
            A,
        ]);
        assert!(!config.dry_run && !config.require_empty_queue && config.wallet_automation);
        assert_eq!(config.wallet(Side::Long).unwrap(), A.parse::<Address>().unwrap());
    }

    #[test]
    fn rebalancing_is_a_switch_of_its_own_that_needs_the_wallet_automation() {
        // The wallet automation alone moves no funds.
        let keys_only = parse(&["--wallet-automation", "true"]);
        keys_only.validate().unwrap();
        assert!(keys_only.wallet_automation && !keys_only.rebalance);

        let both = parse(&["--wallet-automation", "true", "--rebalance", "true"]);
        both.validate().unwrap();
        assert!(both.rebalance);

        // Without the wallets' own keys nothing could sign a transfer.
        for args in [&["--rebalance", "true"][..], &["--rebalance", "true", "--wallet-automation", "false"]] {
            let error = parse(args).validate().unwrap_err().to_string();
            assert!(error.contains("PT_REBALANCE=true needs PT_WALLET_AUTOMATION=true"), "{error}");
        }
    }

    #[test]
    fn the_same_wallet_cannot_be_on_both_sides() {
        let same = parse(&["--long-account", A, "--short-account", &A.to_lowercase()]);
        assert!(same.validate().unwrap_err().to_string().contains("different wallets"));
        parse(&["--long-account", A, "--short-account", B]).validate().unwrap();
    }

    #[test]
    fn leverage_that_would_liquidate_the_loser_near_the_target_is_refused() {
        // 50x liquidates about 195 bps from entry; a 100 bps target needs twice that distance.
        let error = parse(&["--leverage", "50"]).validate().unwrap_err().to_string();
        assert!(error.contains("half the distance to liquidation"), "{error}");
        parse(&["--leverage", "45"]).validate().unwrap();
        assert!(parse(&["--leverage", "20", "--close-bps", "300"]).validate().is_err());
    }

    #[test]
    fn the_intent_lifetime_is_bounded() {
        assert!(parse(&["--intent-ttl-seconds", "10"]).validate().is_err());
        assert!(parse(&["--intent-ttl-seconds", "7200"]).validate().is_err());
    }
}
