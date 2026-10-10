use std::fs;
use std::io::{IsTerminal, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy::primitives::{Address, Signature};
use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use futures::FutureExt;
use papertrade::config::Config;
use papertrade::keystore::{self, KeyRole, UnlockedKeys, WalletKey};
use papertrade::protocol::client::Client;
use papertrade::protocol::intents::{
    register_session_key_document, register_session_key_from_document, seal_register_session_key,
};
use papertrade::store::Store;
use papertrade::strategy::decide::{Mode, Side};
use papertrade::{bot, session_keys, tui};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tracing::{info, warn};
use zeroize::Zeroizing;

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
    /// Run the bot. Asks once for the password that unlocks its keys. With
    /// PT_DRY_RUN=true it decides and records but never signs or submits.
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
    /// Register a session key by hand: the wallet signs elsewhere, this submits.
    #[command(subcommand)]
    SessionKey(SessionKeyCommand),
    /// Keep this machine's keys encrypted under one password.
    #[command(subcommand)]
    Keys(KeysCommand),
    /// Deal with the transfer of funds between the two accounts.
    #[command(subcommand)]
    Rebalance(RebalanceCommand),
}

#[derive(Debug, Subcommand)]
enum KeysCommand {
    /// Encrypt the two accounts' keys under one password: create the session
    /// keys, take the wallets' own keys if PT_WALLET_AUTOMATION is on, keep
    /// what is already there. Asks at the terminal, works offline, sends nothing.
    Init,
}

#[derive(Debug, Subcommand)]
enum SessionKeyCommand {
    /// Write the request that registers the account's session key, for the
    /// account's wallet to sign. Asks for the password to read the key's
    /// address. The key itself is made by `keys init`; this sends nothing.
    New {
        side: Side,
        /// How long the signed registration stays submittable.
        #[arg(long, default_value_t = 60)]
        valid_for_minutes: u64,
    },
    /// Send the signed registration to the relayer. This is a real write
    /// whatever PT_DRY_RUN says: that setting is for `run`.
    Submit {
        side: Side,
        /// The master wallet's 65-byte signature of the registration request.
        #[arg(long)]
        signature: Signature,
    },
}

#[derive(Debug, Subcommand)]
enum RebalanceCommand {
    /// End a transfer that is stuck, once you have dealt with the funds yourself.
    Abandon,
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
            // Listening for Ctrl-C from here, before the password is asked for: one
            // pressed at the prompt or while the bot is still starting is not lost.
            let stop = shutdown_signal(interrupts()?).shared();
            // Once, here, outside the restarts below: they are handed the same keys.
            let keys = unlock_keys(&config, Terminal).await?;
            bot::keep_running(|| bot::start(&config, store.clone(), keys.clone()), RESTART_DELAY, stop).await
        }
        Command::Tui => tui::run(&Store::open(&config.database()).await?).await,
        Command::Status => status(&Store::open(&config.database()).await?).await,
        Command::Pause => set_mode(&config, Mode::Pause).await,
        Command::Resume => set_mode(&config, Mode::Run).await,
        Command::Flatten => set_mode(&config, Mode::Flatten).await,
        Command::SessionKey(SessionKeyCommand::New { side, valid_for_minutes }) => {
            require_terminal()?;
            new_session_key(&config, side, valid_for_minutes, &mut Terminal)
        }
        Command::SessionKey(SessionKeyCommand::Submit { side, signature }) => {
            submit_session_key(&config, side, signature, &Client::new()?).await
        }
        Command::Keys(KeysCommand::Init) => {
            require_terminal()?;
            init_keys(&config, &mut Terminal)
        }
        Command::Rebalance(RebalanceCommand::Abandon) => abandon_transfer(&config).await,
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

/// Resolves when the operator asks the bot to stop: Ctrl-C, SIGTERM, or the
/// terminal it runs in going away (SIGHUP). All three end the same way: the bot
/// finishes the tick it is in and exits cleanly. Being killed outright is safe
/// too, but then an intent could be left half-recorded for the next start to
/// sort out, so there is no reason to let a closed terminal do that.
///
/// `interrupt` is a Ctrl-C listener that is already listening: see [`interrupts`].
async fn shutdown_signal(mut interrupt: Signal) {
    let mut terminate = signal(SignalKind::terminate()).expect("installing the SIGTERM handler");
    let mut hangup = signal(SignalKind::hangup()).expect("installing the SIGHUP handler");
    tokio::select! {
        _ = interrupt.recv() => {}
        _ = terminate.recv() => {}
        _ = hangup.recv() => {}
    }
}

/// Starts listening for Ctrl-C. From then on it no longer ends the process by
/// itself: whoever holds the listener decides.
///
/// The hidden prompt needs that. While it reads, `rpassword` has the terminal's
/// own Ctrl-C handling switched off, and when it reads a Ctrl-C it raises the
/// interrupt itself, before it has put the terminal back. With nobody listening
/// that ends the process on the spot and leaves the terminal as it was: no
/// Ctrl-C for whatever runs in it next, and a shell does not necessarily repair
/// that (zsh does not). With a listener the prompt gets to return, the terminal
/// is put back, and the command ends by its own doing.
fn interrupts() -> Result<Signal> {
    signal(SignalKind::interrupt()).context("listening for Ctrl-C")
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

/// Asks for the password and decrypts what this run signs with, once. The keys
/// then stay in memory for as long as the process lives: a restart inside
/// `keep_running` neither asks again nor reads a file.
async fn unlock_keys(
    config: &Config,
    mut operator: impl Operator + Send + 'static,
) -> Result<Arc<UnlockedKeys>> {
    // Is there anything to unlock? Asked before the operator is asked for anything.
    for side in Side::BOTH {
        keystore::require(&config.keys_dir, config.wallet(side)?, KeyRole::Session)?;
    }
    // The prompt blocks and deriving the file key takes a moment: one blocking
    // task at start, so neither ever happens on the loop's threads.
    let unlocking = config.clone();
    let (keys, took) = tokio::task::spawn_blocking(move || -> Result<_> {
        let password = operator.secret(&format!("Password for {}: ", unlocking.keys_dir.display()))?;
        let started = Instant::now();
        Ok((keystore::unlock(&unlocking, &password)?, started.elapsed()))
    })
    .await??;

    info!(
        long = %keys.long.session.address(),
        short = %keys.short.session.address(),
        took = ?took,
        "session keys unlocked"
    );
    if !config.wallet_automation {
        info!("wallet automation is off: the wallets' own keys are not read");
    }
    for side in Side::BOTH {
        match &keys.get(side).wallet {
            WalletKey::Disabled => {}
            WalletKey::Loaded(_) => info!(%side, "wallet key unlocked"),
            WalletKey::Unavailable(reason) => warn!(
                %side,
                %reason,
                "wallet key unavailable: nothing is signed for this wallet; trading on the session keys goes on"
            ),
        }
    }
    Ok(Arc::new(keys))
}

fn new_session_key(
    config: &Config,
    side: Side,
    valid_for_minutes: u64,
    operator: &mut dyn Operator,
) -> Result<()> {
    if valid_for_minutes == 0 {
        bail!("--valid-for-minutes must be positive");
    }
    let wallet = config.wallet(side)?;
    keystore::require(&config.keys_dir, wallet, KeyRole::Session)?;
    let password = operator.secret(&format!("Password for {}: ", config.keys_dir.display()))?;
    let (key, path) = write_registration(config, side, valid_for_minutes, &password)?;
    operator.say(&format!("Session key {key} for {wallet}."));
    operator.say(&format!("Registration request written to {}. Nothing was sent.", path.display()));
    operator.say("Sign it with the account's wallet as EIP-712 typed data, for example:");
    operator.say(&format!("  cast wallet sign --data --from-file {} --interactive", path.display()));
    operator.say(&format!("then: papertrade session-key submit {side} --signature 0x…"));
    Ok(())
}

/// Writes the request that registers the account's session key and returns the
/// key's address. The address is only known by decrypting the key.
fn write_registration(
    config: &Config,
    side: Side,
    valid_for_minutes: u64,
    password: &str,
) -> Result<(Address, PathBuf)> {
    let wallet = config.wallet(side)?;
    let key = keystore::load(&config.keys_dir, wallet, KeyRole::Session, password)?.address();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let message = session_keys::registration(wallet, key, now, valid_for_minutes * 60);
    let path = session_keys::registration_path(&config.keys_dir, wallet);
    fs::write(&path, serde_json::to_string_pretty(&register_session_key_document(&message))?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok((key, path))
}

async fn submit_session_key(
    config: &Config,
    side: Side,
    signature: Signature,
    relayer: &Client,
) -> Result<()> {
    let wallet = config.wallet(side)?;
    let path = session_keys::registration_path(&config.keys_dir, wallet);
    let document = serde_json::from_str(
        &fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    let message = register_session_key_from_document(&document)
        .with_context(|| format!("{} is not a registration request", path.display()))?;
    let intent = seal_register_session_key(&message, signature)?;
    let receipt = relayer.submit_intent(&intent).await?;
    println!("Relayer accepted the registration: {} ({}).", receipt.state, receipt.intent_id);
    println!("This was a real request; PT_DRY_RUN applies to `run` only.");
    println!("`papertrade status` shows the key as valid once it has executed.");
    Ok(())
}

/// Ends a stuck transfer. This moves nothing: the funds stay where the transfer
/// left them, and the bot stops waiting for it.
async fn abandon_transfer(config: &Config) -> Result<()> {
    use papertrade::store::Abandoned;
    match Store::open(&config.database()).await?.abandon_stuck_transfer().await? {
        Abandoned::Ended(transfer) => {
            println!(
                "Transfer #{} abandoned: {} USDC from the {} account to the {} account.",
                transfer.id, transfer.amount_usdc, transfer.from, transfer.to
            );
            println!("It was {}.", transfer.note.as_deref().unwrap_or("stuck"));
        }
        Abandoned::NotStuck(transfer) => bail!(
            "transfer #{} is still under way (stage: {}); only a stuck transfer can be abandoned",
            transfer.id,
            transfer.stage
        ),
        Abandoned::Nothing => println!("There is no unfinished transfer."),
    }
    Ok(())
}

/// A new password is at least this long. A floor, not a measure of strength.
const MIN_PASSWORD_CHARS: usize = 16;

/// Whoever answers at the terminal. Secrets are typed and never shown.
trait Operator {
    fn say(&mut self, line: &str);
    fn secret(&mut self, prompt: &str) -> Result<Zeroizing<String>>;
}

/// The operator at the terminal the command was started from.
struct Terminal;

impl Operator for Terminal {
    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn secret(&mut self, prompt: &str) -> Result<Zeroizing<String>> {
        require_terminal()?;
        let _listening = interrupts()?;
        match rpassword::prompt_password(prompt) {
            Ok(typed) => Ok(Zeroizing::new(typed)),
            // Ctrl-C at the prompt. The terminal is whole again by now.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => bail!("interrupted"),
            Err(error) => Err(error).context("reading from the terminal"),
        }
    }
}

/// A password or a key is typed at a terminal. It is not taken from a pipe, an
/// argument or the environment, where it would end up in a script or a history.
/// Every hidden prompt asks this; the commands that would first print
/// something ask it before they do.
fn require_terminal() -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal(),
        "this asks for a password and needs a terminal; a password is not read from a pipe, \
         an argument or the environment"
    );
    Ok(())
}

/// `keys init`: brings the keys directory to one encrypted file per key the
/// configuration calls for, all under one password. What is already there is
/// checked against the password and kept; a missing session key is created; a
/// missing wallet key is typed in. It only ever adds files, so it can be run
/// again after an interruption or after turning the wallet automation on.
fn init_keys(config: &Config, operator: &mut dyn Operator) -> Result<()> {
    let dir = &config.keys_dir;
    let accounts = [(Side::Long, config.wallet(Side::Long)?), (Side::Short, config.wallet(Side::Short)?)];
    let roles: &[KeyRole] =
        if config.wallet_automation { &[KeyRole::Session, KeyRole::Wallet] } else { &[KeyRole::Session] };
    operator.say(&format!("Accounts: long {} / short {}", accounts[0].1, accounts[1].1));
    operator.say(&format!(
        "Keys directory: {} · wallet automation: {}",
        dir.display(),
        if config.wallet_automation { "on" } else { "off (session keys only)" }
    ));

    let mut keys = Vec::new();
    for (side, owner) in accounts {
        for &role in roles {
            keys.push((side, owner, role, keystore::exists(dir, owner, role)?));
        }
    }
    // With keys already there, the password is theirs and has to open every one
    // of them before anything is added. Otherwise it is chosen now.
    let password = if keys.iter().any(|&(.., exists)| exists) {
        operator.secret(&format!("Password for {}: ", dir.display()))?
    } else {
        operator.say(&format!(
            "Choose the password that unlocks these keys at every start: at least {MIN_PASSWORD_CHARS} \
             characters, best a random one from a password manager. It is stored nowhere, and without \
             it the keys cannot be recovered."
        ));
        let password = operator.secret("New password: ")?;
        ensure!(
            password.chars().count() >= MIN_PASSWORD_CHARS,
            "the password needs at least {MIN_PASSWORD_CHARS} characters; nothing was written"
        );
        ensure!(
            *operator.secret("Repeat the password: ")? == *password,
            "the two passwords differ; nothing was written"
        );
        password
    };
    let mut kept = Vec::new();
    for &(side, owner, role, exists) in &keys {
        if exists {
            let signer = keystore::load(dir, owner, role, &password)
                .map_err(|error| anyhow!("{error:#}; nothing was changed"))?;
            kept.push((side, role, signer.address()));
        }
    }
    for (side, role, address) in kept {
        operator.say(&format!("{side} {role} key {address}: already there, kept."));
    }

    for (side, owner, role, exists) in keys {
        match role {
            _ if exists => {}
            KeyRole::Session => {
                let address = session_keys::create(dir, owner, &password)?;
                operator.say(&format!(
                    "{side} session key {address}: created and encrypted. It is not registered: it signs \
                     for {owner} only once that wallet has registered it (`papertrade session-key new {side}`)."
                ));
            }
            KeyRole::Wallet => {
                let typed =
                    operator.secret(&format!("Private key of the {side} account's wallet {owner}: "))?;
                let address =
                    keystore::create(dir, owner, role, &*keystore::parse_secret(&typed)?, &password)?;
                operator.say(&format!("{side} wallet key {address}: encrypted."));
            }
        }
    }

    operator.say(&format!("Key files are {}.", keystore::FORMAT));
    operator.say("Nothing was sent: no registration, no withdrawal, no transfer, no order.");
    Ok(())
}

/// The simulated exchange of the scenario tests. The tests at the end of this
/// file take the commands above through it: from new keys to traded rounds.
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod support;

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::Path;

    use alloy::dyn_abi::TypedData;
    use alloy::primitives::hex;
    use alloy::signers::SignerSync;
    use alloy::signers::local::PrivateKeySigner;
    use alloy::sol_types::SolStruct;
    use papertrade::protocol::client::SubmitError;
    use papertrade::protocol::deployment::SESSION_KEY_TTL_SECONDS;
    use papertrade::protocol::intents::exchange_domain;
    use papertrade::strategy::decide::Phase;

    use super::support::{ENTRY, Harness, TARGET, TTL, eventually, eventually_stored, status_when};
    use super::*;

    const PASSWORD: &str = "correct horse battery staple";

    /// An operator who types prepared answers, and everything they were shown.
    #[derive(Default)]
    struct Script {
        secrets: VecDeque<String>,
        shown: Vec<String>,
    }

    impl Script {
        fn typing(secrets: &[&str]) -> Self {
            Self { secrets: secrets.iter().map(|&secret| secret.to_owned()).collect(), shown: Vec::new() }
        }

        fn shown(&self) -> String {
            self.shown.join("\n")
        }
    }

    impl Operator for Script {
        fn say(&mut self, line: &str) {
            self.shown.push(line.to_owned());
        }

        fn secret(&mut self, prompt: &str) -> Result<Zeroizing<String>> {
            self.shown.push(prompt.to_owned());
            Ok(Zeroizing::new(self.secrets.pop_front().unwrap_or_else(|| panic!("not expected: {prompt}"))))
        }
    }

    /// Two wallets nobody has funded, with their private keys as an operator would type them.
    struct Wallets {
        long: PrivateKeySigner,
        short: PrivateKeySigner,
    }

    impl Wallets {
        fn random() -> Self {
            Self { long: PrivateKeySigner::random(), short: PrivateKeySigner::random() }
        }

        fn typed(signer: &PrivateKeySigner) -> String {
            hex::encode_prefixed(signer.to_bytes())
        }

        fn config(&self, keys_dir: &Path, wallet_automation: bool) -> Config {
            Cli::try_parse_from([
                "papertrade",
                "--long-account",
                &self.long.address().to_string(),
                "--short-account",
                &self.short.address().to_string(),
                "--keys-dir",
                keys_dir.to_str().unwrap(),
                "--wallet-automation",
                &wallet_automation.to_string(),
                "keys",
                "init",
            ])
            .unwrap()
            .config
        }
    }

    /// Every file in the keys directory with its content, in name order.
    fn files(keys_dir: &Path) -> Vec<(String, String)> {
        let Ok(entries) = fs::read_dir(keys_dir) else { return Vec::new() };
        let mut files: Vec<(String, String)> = entries
            .map(|entry| entry.unwrap())
            .map(|entry| {
                (entry.file_name().into_string().unwrap(), fs::read_to_string(entry.path()).unwrap())
            })
            .collect();
        files.sort();
        files
    }

    fn names(keys_dir: &Path) -> Vec<String> {
        files(keys_dir).into_iter().map(|(name, _)| name).collect()
    }

    fn sorted<const N: usize>(mut names: [String; N]) -> [String; N] {
        names.sort();
        names
    }

    /// Neither what the operator was shown nor what was written carries a secret.
    fn assert_kept_secret(shown: &str, keys_dir: &Path, secrets: &[&str]) {
        for secret in secrets {
            let secret = secret.trim_start_matches("0x");
            assert!(!shown.contains(secret), "the operator was shown a secret:\n{shown}");
            for (name, content) in files(keys_dir) {
                assert!(!content.contains(secret), "{name} holds a secret");
            }
        }
    }

    #[test]
    fn the_first_run_sets_a_password_and_creates_both_session_keys() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        let keys_dir = dir.path().join("secrets");
        let config = wallets.config(&keys_dir, false);
        let mut operator = Script::typing(&[PASSWORD, PASSWORD]);
        init_keys(&config, &mut operator).unwrap();

        let (long, short) = (wallets.long.address(), wallets.short.address());
        assert_eq!(
            names(&keys_dir),
            sorted([format!("{long:#x}.session.json"), format!("{short:#x}.session.json")])
        );
        let shown = operator.shown();
        for owner in [long, short] {
            let session = keystore::load(&keys_dir, owner, KeyRole::Session, PASSWORD).unwrap().address();
            assert!(shown.contains(&format!("session key {session}: created and encrypted")), "{shown}");
            // A session key is its own key, not the wallet's.
            assert_ne!(session, owner);
        }
        // It says what it did not do, and that a new key is not a registered one.
        assert!(shown.contains("It is not registered"), "{shown}");
        assert!(shown.contains("Nothing was sent: no registration, no withdrawal, no transfer, no order."));
        assert!(shown.contains("Web3 Secret Storage V3: aes-128-ctr, scrypt N=8192 r=8 p=1"), "{shown}");
        assert_kept_secret(&shown, &keys_dir, &[PASSWORD]);
    }

    #[test]
    fn running_it_again_asks_for_the_password_once_and_keeps_every_key() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        let config = wallets.config(dir.path(), false);
        init_keys(&config, &mut Script::typing(&[PASSWORD, PASSWORD])).unwrap();
        let before = files(dir.path());

        // One answer is all it may ask for: a second prompt would panic.
        let mut again = Script::typing(&[PASSWORD]);
        init_keys(&config, &mut again).unwrap();
        assert_eq!(files(dir.path()), before);
        assert_eq!(again.shown().matches("already there, kept").count(), 2, "{}", again.shown());

        let mut wrong = Script::typing(&["not the password at all"]);
        let error = init_keys(&config, &mut wrong).unwrap_err().to_string();
        assert!(error.contains("wrong password") && error.ends_with("nothing was changed"), "{error}");
        assert_eq!(files(dir.path()), before);
    }

    #[test]
    fn an_interrupted_run_is_finished_by_the_next_without_touching_what_was_done() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        let config = wallets.config(dir.path(), false);
        // As if the first run had stopped after the long account's key.
        let long = session_keys::create(dir.path(), wallets.long.address(), PASSWORD).unwrap();
        let before = files(dir.path());

        let mut operator = Script::typing(&[PASSWORD]);
        init_keys(&config, &mut operator).unwrap();
        let after = files(dir.path());
        assert_eq!(after.len(), 2);
        assert!(after.contains(&before[0]), "the key that was there changed");
        let shown = operator.shown();
        assert!(shown.contains(&format!("long session key {long}: already there, kept.")), "{shown}");
        assert!(shown.contains("short session key") && shown.contains("created and encrypted"), "{shown}");
    }

    #[test]
    fn a_short_or_mistyped_new_password_writes_nothing() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        let keys_dir = dir.path().join("secrets");
        let config = wallets.config(&keys_dir, false);

        let error = init_keys(&config, &mut Script::typing(&["fifteen chars.."])).unwrap_err().to_string();
        assert!(error.contains("at least 16 characters"), "{error}");
        let error =
            init_keys(&config, &mut Script::typing(&[PASSWORD, "correct horse battery stapel"])).unwrap_err();
        assert!(error.to_string().contains("the two passwords differ"), "{error}");
        assert!(!keys_dir.exists());
    }

    #[test]
    fn turning_the_wallet_automation_on_later_adds_the_wallet_keys_and_keeps_the_sessions() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        init_keys(&wallets.config(dir.path(), false), &mut Script::typing(&[PASSWORD, PASSWORD])).unwrap();
        let sessions = files(dir.path());

        let (long_key, short_key) = (Wallets::typed(&wallets.long), Wallets::typed(&wallets.short));
        let mut operator = Script::typing(&[PASSWORD, &long_key, &short_key]);
        init_keys(&wallets.config(dir.path(), true), &mut operator).unwrap();

        for owner in [wallets.long.address(), wallets.short.address()] {
            let wallet = keystore::load(dir.path(), owner, KeyRole::Wallet, PASSWORD).unwrap();
            assert_eq!(wallet.address(), owner);
        }
        let after = files(dir.path());
        assert_eq!(after.len(), 4);
        assert!(sessions.iter().all(|session| after.contains(session)), "a session key file changed");
        assert_kept_secret(&operator.shown(), dir.path(), &[PASSWORD, &long_key, &short_key]);
    }

    #[test]
    fn with_the_automation_off_no_wallet_key_is_asked_for() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        // Two answers, both for the password: a prompt for a private key would panic.
        let mut operator = Script::typing(&[PASSWORD, PASSWORD]);
        init_keys(&wallets.config(dir.path(), false), &mut operator).unwrap();
        assert!(!operator.shown().contains("Private key"), "{}", operator.shown());
        assert!(names(dir.path()).iter().all(|name| name.ends_with(".session.json")));
    }

    #[test]
    fn a_wallet_key_that_is_not_the_accounts_own_is_refused() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        let config = wallets.config(dir.path(), true);
        // The short wallet's key, typed where the long wallet's is asked for.
        let short_key = Wallets::typed(&wallets.short);
        let mut operator = Script::typing(&[PASSWORD, PASSWORD, &short_key]);
        let error = format!("{:#}", init_keys(&config, &mut operator).unwrap_err());
        let (long, short) = (wallets.long.address(), wallets.short.address());
        assert!(error.contains(&format!("this is the key of {short}, not of the wallet {long}")), "{error}");
        assert_eq!(names(dir.path()), [format!("{long:#x}.session.json")]);
        assert_kept_secret(&format!("{error}\n{}", operator.shown()), dir.path(), &[PASSWORD, &short_key]);

        // Something that is no key at all is refused without being repeated.
        let mut operator = Script::typing(&[PASSWORD, "hunter2hunter2"]);
        let error = format!("{:#}", init_keys(&config, &mut operator).unwrap_err());
        assert_eq!(error, "not a private key: 64 hex digits are expected");
    }

    #[test]
    fn the_registration_request_names_the_encrypted_session_key_and_sends_nothing() {
        let (dir, wallets) = (tempfile::tempdir().unwrap(), Wallets::random());
        let config = wallets.config(dir.path(), false);
        let owner = wallets.short.address();

        // There is no key to register before `keys init` has made one, and no prompt for a password.
        let error = new_session_key(&config, Side::Short, 60, &mut Script::typing(&[])).unwrap_err();
        assert!(error.to_string().contains("`papertrade keys init` sets it up"), "{error}");
        assert!(names(dir.path()).is_empty(), "asking for a registration created something");

        init_keys(&config, &mut Script::typing(&[PASSWORD, PASSWORD])).unwrap();
        let keys = files(dir.path());
        let session = keystore::load(dir.path(), owner, KeyRole::Session, PASSWORD).unwrap().address();

        let mut wrong = Script::typing(&["not the password at all"]);
        let error = new_session_key(&config, Side::Short, 60, &mut wrong).unwrap_err().to_string();
        assert!(error.contains("wrong password"), "{error}");
        assert_eq!(files(dir.path()), keys);

        let mut operator = Script::typing(&[PASSWORD]);
        let before = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        new_session_key(&config, Side::Short, 60, &mut operator).unwrap();
        let path = session_keys::registration_path(dir.path(), owner);
        let document = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let request = register_session_key_from_document(&document).unwrap();
        assert_eq!((request.user, request.key, request.feeAddress), (owner, session, Address::ZERO));
        assert!((before + 3_600..=before + 3_605).contains(&request.deadline), "{request:?}");
        assert_eq!(request.expiry - request.deadline, SESSION_KEY_TTL_SECONDS - 3_600);
        let shown = operator.shown();
        assert!(shown.contains(&format!("Session key {session} for {owner}.")), "{shown}");
        assert!(shown.contains("Nothing was sent."), "{shown}");
        // The key files are as they were: asking again makes no second key.
        let after = files(dir.path());
        assert!(keys.iter().all(|key| after.contains(key)) && after.len() == keys.len() + 1);
        assert_kept_secret(&shown, dir.path(), &[PASSWORD]);
    }

    // From here on: the whole way, against the simulated exchange. The commands
    // are the ones above, each called as `main` calls it, with an operator who
    // types from a script and a relayer that is the simulator. The wallets and
    // every key are made for the run.

    const SOON: Duration = Duration::from_secs(5);

    /// What `cast wallet sign --data --from-file <request>` does: the wallet
    /// hashes the typed data it is shown and signs that.
    fn wallet_signs(request: &Path, wallet: &PrivateKeySigner) -> Signature {
        let typed: TypedData = serde_json::from_str(&fs::read_to_string(request).unwrap()).unwrap();
        wallet.sign_hash_sync(&typed.eip712_signing_hash().unwrap()).unwrap()
    }

    /// Nothing the bot wrote to its data directory holds one of these.
    fn assert_not_in_the_database(h: &Harness, secrets: &[&str]) {
        for entry in fs::read_dir(&h.config.data_dir).unwrap() {
            let (name, content) =
                (entry.as_ref().unwrap().file_name(), fs::read(entry.unwrap().path()).unwrap());
            let content = String::from_utf8_lossy(&content);
            for secret in secrets {
                assert!(!content.contains(secret.trim_start_matches("0x")), "{name:?} holds a secret");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn from_new_keys_to_traded_rounds_on_the_simulated_exchange() {
        // Two wallets made for this run, funded on the exchange, with no session
        // key registered. Their own keys stay with the operator: the wallets sign
        // the registrations below and nothing of theirs reaches the bot's side.
        let wallets = Wallets::random();
        let (long, short) = (wallets.long.address(), wallets.short.address());
        let mut h = Harness::unregistered(long, short).await;
        let dir = tempfile::tempdir().unwrap();
        h.config.keys_dir = dir.path().join("secrets");
        let relayer = Client::with_origin(&h.sim.url).unwrap();

        // `keys init`: one password, two new session keys, nothing sent.
        init_keys(&h.config, &mut Script::typing(&[PASSWORD, PASSWORD])).unwrap();

        // `run`: unlocked once. Its first start loses a feed, so the bot that goes on
        // is one started again inside the process. The exchange has not heard of the
        // session keys: it waits.
        h.keys = unlock_keys(&h.config, Script::typing(&[PASSWORD])).await.unwrap();
        h.start_self_healing_bot(1);
        let waiting = status_when(&h, "the wait for a registration", |status| {
            status.summary.ends_with("session key is not registered or expires soon")
        })
        .await;
        assert!(waiting.legs.iter().all(|leg| leg.account.as_ref().unwrap().session_key_expiry.is_none()));
        assert!(h.sim.received().is_empty(), "something was sent before any key was registered");

        // For each account: `session-key new`, the wallet's signature, `session-key submit`.
        for (side, wallet) in [(Side::Long, &wallets.long), (Side::Short, &wallets.short)] {
            new_session_key(&h.config, side, 60, &mut Script::typing(&[PASSWORD])).unwrap();
            let request = session_keys::registration_path(&h.config.keys_dir, wallet.address());
            submit_session_key(&h.config, side, wallet_signs(&request, wallet), &relayer).await.unwrap();
        }

        // The bot that was waiting, with the keys it unlocked when it started, finds
        // them registered and trades a round: opened, held, closed at the target, booked.
        h.hedged().await;
        let hedged = status_when(&h, "the open round", |status| status.phase == Phase::Hedged).await;
        for (leg, keys) in hedged.legs.iter().zip([&h.keys.long, &h.keys.short]) {
            assert_eq!(leg.session_key, format!("{:#x}", keys.session.address()));
            assert!(leg.account.as_ref().unwrap().session_key_expiry.is_some(), "{leg:?}");
            assert!(!leg.has_wallet_key && leg.wallet_key_problem.is_none(), "{leg:?}");
        }
        h.sim.set_mid(ENTRY + TARGET);
        h.flat().await;
        eventually_stored("both settled positions are in the ledger", SOON, || async {
            h.store.totals().await.unwrap().trades == 2
        })
        .await;
        assert!(h.opens() >= 2 && h.closes() == 2, "{} opens, {} closes", h.opens(), h.closes());

        // Stopped, and started as a new process would be: the password once more,
        // the same files, the same two keys, and the next round. Nothing is
        // registered a second time.
        h.stop_bot().await.unwrap();
        let before = [h.keys.long.session.address(), h.keys.short.session.address()];
        h.keys = unlock_keys(&h.config, Script::typing(&[PASSWORD])).await.unwrap();
        assert_eq!([h.keys.long.session.address(), h.keys.short.session.address()], before);
        h.start_bot();
        eventually("the next round opens", TTL + SOON, || h.opens() == 4).await;
        h.hedged().await;
        let registrations = |wallet| h.sim.count(wallet, "registerSessionKey");
        assert_eq!((registrations(long), registrations(short)), (1, 1));

        // What is on disk: two encrypted keys and the two public requests. No
        // password and no key, there or in the database.
        assert_eq!(names(&h.config.keys_dir).len(), 4, "{:?}", names(&h.config.keys_dir));
        let sessions = [&h.keys.long.session, &h.keys.short.session].map(|key| hex::encode(key.to_bytes()));
        let secrets = [PASSWORD, &sessions[0], &sessions[1]];
        assert_kept_secret("", &h.config.keys_dir, &secrets);
        assert_not_in_the_database(&h, &secrets);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_the_wallets_own_keys_unlocked_the_same_keys_register_and_trade() {
        let wallets = Wallets::random();
        let (long_key, short_key) = (Wallets::typed(&wallets.long), Wallets::typed(&wallets.short));
        let mut h = Harness::unregistered(wallets.long.address(), wallets.short.address()).await;
        let dir = tempfile::tempdir().unwrap();
        h.config.keys_dir = dir.path().join("secrets");
        h.config.wallet_automation = true;
        let relayer = Client::with_origin(&h.sim.url).unwrap();

        // `keys init` with the automation on: the session keys are made, the
        // wallets' own keys are typed in. Then one password unlocks all four.
        let mut operator = Script::typing(&[PASSWORD, PASSWORD, &long_key, &short_key]);
        init_keys(&h.config, &mut operator).unwrap();
        h.keys = unlock_keys(&h.config, Script::typing(&[PASSWORD])).await.unwrap();

        // The registration the wallet automation is to make by itself (issue #4),
        // made here by hand from the parts it will be handed: the unlocked wallet
        // key signs for the unlocked session key, and the exchange takes it.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        for side in Side::BOTH {
            let keys = h.keys.get(side);
            let wallet = keys.wallet.signer().expect("the wallet's own key is unlocked");
            let request = session_keys::registration(keys.owner, keys.session.address(), now, 3_600);
            let signature = wallet.sign_hash_sync(&request.eip712_signing_hash(&exchange_domain())).unwrap();
            let registration = seal_register_session_key(&request, signature).unwrap();

            // The exchange holds a registration to the wallet's signature: the same
            // one over another key is not that wallet's word.
            let mut forged = registration.clone();
            forged.signed["key"] = format!("{:#x}", Address::repeat_byte(0x66)).into();
            let refused = relayer.submit_intent(&forged).await.unwrap_err();
            assert!(
                matches!(&refused, SubmitError::Rejected { code, .. } if code == "unauthorized_intent"),
                "{refused:?}"
            );
            relayer.submit_intent(&registration).await.unwrap();
        }

        h.start_bot();
        h.hedged().await;
        let hedged = status_when(&h, "the open round", |status| status.phase == Phase::Hedged).await;
        assert!(hedged.legs.iter().all(|leg| leg.has_wallet_key && leg.wallet_key_problem.is_none()));
        h.sim.set_mid(ENTRY - TARGET);
        h.flat().await;
        assert_eq!((h.opens(), h.closes()), (2, 2));

        // Four encrypted files, and the wallets' keys nowhere in the clear.
        assert_eq!(names(&h.config.keys_dir).len(), 4, "{:?}", names(&h.config.keys_dir));
        let secrets = [PASSWORD, long_key.as_str(), short_key.as_str()];
        assert_kept_secret(&operator.shown(), &h.config.keys_dir, &secrets);
        assert_not_in_the_database(&h, &secrets);
    }
}
