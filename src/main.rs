use std::fs;
use std::io::{IsTerminal, Write as _};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, Signature};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use futures::FutureExt;
use papertrade::config::Config;
use papertrade::protocol::client::Client;
use papertrade::protocol::deployment::SESSION_KEY_TTL_SECONDS;
use papertrade::protocol::intents::{
    NonceSequence, RegisterSessionKey, register_session_key_document, register_session_key_from_document,
    seal_register_session_key,
};
use papertrade::store::Store;
use papertrade::strategy::decide::{Mode, Side};
use papertrade::{bot, session_keys, tui};

/// Two-account hedged PAPER farming bot for Papertrade.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the bot. With PT_DRY_RUN=true it decides and records but never submits.
    Run,
    /// Live terminal dashboard of what the bot sees, intends and has done.
    Tui,
    /// Print the bot's latest status once.
    Status,
    /// Finish the round that is open, start no new one.
    Pause,
    /// Go back to opening rounds.
    Resume,
    /// Close every position now and open nothing until resumed.
    Flatten,
    /// Create and register the session keys the bot signs with.
    #[command(subcommand)]
    SessionKey(SessionKeyCommand),
}

#[derive(Debug, Subcommand)]
enum SessionKeyCommand {
    /// Create the account's session key and a registration request for the master wallet to sign.
    New {
        side: Side,
        /// How long the signed registration stays submittable.
        #[arg(long, default_value_t = 60)]
        valid_for_minutes: u64,
    },
    /// Send the signed registration to the relayer.
    Submit {
        side: Side,
        /// The master wallet's 65-byte signature of the registration request.
        #[arg(long)]
        signature: Signature,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // A missing .env is fine: everything can come from the environment or flags.
    let _ = dotenvy::dotenv();
    let Cli { config, command } = Cli::parse();
    config.validate()?;

    match command {
        Command::Run => {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .init();
            let store = Store::open(&config.database()).await?;
            let _only_one = lock_data_dir(&config)?;
            let stop = shutdown_signal().shared();
            bot::keep_running(|| bot::start(&config, store.clone()), RESTART_DELAY, stop).await
        }
        Command::Tui => tui::run(&Store::open(&config.database()).await?).await,
        Command::Status => status(&Store::open(&config.database()).await?).await,
        Command::Pause => set_mode(&config, Mode::Pause).await,
        Command::Resume => set_mode(&config, Mode::Run).await,
        Command::Flatten => set_mode(&config, Mode::Flatten).await,
        Command::SessionKey(SessionKeyCommand::New { side, valid_for_minutes }) => {
            new_session_key(&config, side, valid_for_minutes)
        }
        Command::SessionKey(SessionKeyCommand::Submit { side, signature }) => {
            submit_session_key(&config, side, signature).await
        }
    }
}

/// Two bots on the same accounts would each open a round. The lock is held for the
/// life of the process and released by the operating system however it ends.
fn lock_data_dir(config: &Config) -> Result<fs::File> {
    let path = config.data_dir.join("papertrade.lock");
    let lock = fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    if lock.try_lock().is_err() {
        bail!("another `papertrade run` is already using {}", config.data_dir.display());
    }
    Ok(lock)
}

/// How long to wait before starting over after the bot stopped on its own.
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Resolves on Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing the SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

async fn set_mode(config: &Config, mode: Mode) -> Result<()> {
    Store::open(&config.database()).await?.set_mode(mode).await?;
    println!("mode set to {}", mode.as_str());
    Ok(())
}

/// Prints the bot's latest status. A closed pipe (`status | head`) is not an error.
async fn status(store: &Store) -> Result<()> {
    let report = tui::report(store).await?;
    let _ = std::io::stdout().write_all(report.as_bytes());
    Ok(())
}

fn registration_path(config: &Config, wallet: Address) -> std::path::PathBuf {
    config.keys_dir.join(format!("{wallet:#x}.registration.json"))
}

fn new_session_key(config: &Config, side: Side, valid_for_minutes: u64) -> Result<()> {
    if valid_for_minutes == 0 {
        bail!("--valid-for-minutes must be positive");
    }
    let wallet = config.wallet(side)?;
    let key = session_keys::ensure(&config.keys_dir, wallet)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let message = RegisterSessionKey {
        user: wallet,
        key: key.address(),
        expiry: now + SESSION_KEY_TTL_SECONDS,
        feeAddress: Address::ZERO,
        nonce: NonceSequence::random().allocate(),
        deadline: now + valid_for_minutes * 60,
    };
    let path = registration_path(config, wallet);
    fs::write(&path, serde_json::to_string_pretty(&register_session_key_document(&message))?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("Session key {} for {wallet}.", key.address());
    println!("Registration request written to {}.", path.display());
    println!("Sign it with the master wallet as EIP-712 typed data, for example:");
    println!("  cast wallet sign --data --from-file {} --interactive", path.display());
    println!("then: papertrade session-key submit {side} --signature 0x…");
    Ok(())
}

async fn submit_session_key(config: &Config, side: Side, signature: Signature) -> Result<()> {
    let wallet = config.wallet(side)?;
    let path = registration_path(config, wallet);
    let document = serde_json::from_str(
        &fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    let message = register_session_key_from_document(&document)
        .with_context(|| format!("{} is not a registration request", path.display()))?;
    let intent = seal_register_session_key(&message, signature)?;
    let receipt = Client::new()?.submit_intent(&intent).await?;
    println!("Relayer accepted the registration: {} ({}).", receipt.state, receipt.intent_id);
    println!("`papertrade status` shows the key as valid once it has executed.");
    Ok(())
}
