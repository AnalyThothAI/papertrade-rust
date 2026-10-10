//! Key files on disk: written encrypted, read back, never replaced, never quoted.
//! Every key here is random, made for the test, and has never held anything.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, hex};
use alloy::signers::local::PrivateKeySigner;
use clap::Parser;
use papertrade::config::Config;
use papertrade::keystore::{self, KeyRole, WalletKey};
use papertrade::session_keys;
use serde_json::Value;
use tempfile::TempDir;

const PASSWORD: &str = "correct horse battery staple";
const WRONG_PASSWORD: &str = "incorrect horse battery staple";

#[derive(Parser)]
struct Args {
    #[command(flatten)]
    config: Config,
}

/// A wallet with its own key, and where its key files go.
struct Account {
    wallet: PrivateKeySigner,
}

impl Account {
    fn random() -> Self {
        Self { wallet: PrivateKeySigner::random() }
    }

    fn owner(&self) -> Address {
        self.wallet.address()
    }

    fn secret(&self) -> [u8; 32] {
        self.wallet.to_bytes().0
    }
}

fn config(keys_dir: &Path, long: &Account, short: &Account, wallet_automation: bool) -> Config {
    Args::try_parse_from([
        "papertrade",
        "--long-account",
        &long.owner().to_string(),
        "--short-account",
        &short.owner().to_string(),
        "--keys-dir",
        keys_dir.to_str().unwrap(),
        "--wallet-automation",
        &wallet_automation.to_string(),
    ])
    .unwrap()
    .config
}

fn names(keys_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(keys_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

fn key_file(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A directory with both accounts' session keys, and their wallet keys if asked for.
fn keys_dir(long: &Account, short: &Account, wallets: bool) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    for account in [long, short] {
        session_keys::create(dir.path(), account.owner(), PASSWORD).unwrap();
        if wallets {
            let (owner, secret) = (account.owner(), account.secret());
            keystore::create(dir.path(), owner, KeyRole::Wallet, &secret, PASSWORD).unwrap();
        }
    }
    dir
}

#[test]
fn a_new_session_key_is_stored_encrypted_and_decrypts_to_the_address_it_was_given() {
    let (dir, account) = (tempfile::tempdir().unwrap(), Account::random());
    let keys_dir = dir.path().join("secrets");
    assert!(!keystore::exists(&keys_dir, account.owner(), KeyRole::Session).unwrap());

    let address = session_keys::create(&keys_dir, account.owner(), PASSWORD).unwrap();
    let path = keystore::key_path(&keys_dir, account.owner(), KeyRole::Session);
    assert!(keystore::exists(&keys_dir, account.owner(), KeyRole::Session).unwrap());
    // Only the owner can list the directory or read the file, and no draft is left behind.
    assert_eq!((mode(&keys_dir), mode(&path)), (0o700, 0o600));
    assert_eq!(names(&keys_dir), [format!("{:#x}.session.json", account.owner())]);

    let loaded = keystore::load(&keys_dir, account.owner(), KeyRole::Session, PASSWORD).unwrap();
    assert_eq!(loaded.address(), address);
    // A session key is a key of its own, and what is on disk is not that key.
    assert_ne!(address, account.owner());
    let on_disk = fs::read_to_string(&path).unwrap();
    assert!(!on_disk.contains(&hex::encode(loaded.to_bytes())) && !on_disk.contains(PASSWORD));
}

#[test]
fn a_wallet_key_has_to_be_the_owners_own() {
    let (dir, owner, other) = (tempfile::tempdir().unwrap(), Account::random(), Account::random());
    let keys_dir = dir.path().join("secrets");

    let refused = keystore::create(&keys_dir, owner.owner(), KeyRole::Wallet, &other.secret(), PASSWORD);
    let refused = format!("{:#}", refused.unwrap_err());
    let expected = format!("this is the key of {}, not of the wallet {}", other.owner(), owner.owner());
    assert!(refused.contains(&expected), "{refused}");
    assert!(!keys_dir.exists(), "a refused key left something behind");

    let stored = keystore::create(&keys_dir, owner.owner(), KeyRole::Wallet, &owner.secret(), PASSWORD);
    assert_eq!(stored.unwrap(), owner.owner());

    // The same check on the way back: another wallet's file under this owner's name.
    keystore::create(&keys_dir, other.owner(), KeyRole::Wallet, &other.secret(), PASSWORD).unwrap();
    let path = keystore::key_path(&keys_dir, owner.owner(), KeyRole::Wallet);
    fs::rename(keystore::key_path(&keys_dir, other.owner(), KeyRole::Wallet), &path).unwrap();
    let refused = keystore::load(&keys_dir, owner.owner(), KeyRole::Wallet, PASSWORD).unwrap_err();
    assert!(format!("{refused:#}").contains(&expected), "{refused:#}");
}

#[test]
fn one_password_unlocks_two_files_or_four_and_no_salt_or_iv_is_used_twice() {
    let (long, short) = (Account::random(), Account::random());
    let dir = keys_dir(&long, &short, true);

    let sessions_only = keystore::unlock(&config(dir.path(), &long, &short, false), PASSWORD).unwrap();
    assert!(matches!(sessions_only.long.wallet, WalletKey::Disabled));
    assert!(matches!(sessions_only.short.wallet, WalletKey::Disabled));

    let all = keystore::unlock(&config(dir.path(), &long, &short, true), PASSWORD).unwrap();
    assert_eq!((all.long.owner, all.short.owner), (long.owner(), short.owner()));
    assert_eq!(all.long.wallet.signer().unwrap().address(), long.owner());
    assert_eq!(all.short.wallet.signer().unwrap().address(), short.owner());
    // The same session keys either way, and four different keys in all.
    assert_eq!(all.long.session.address(), sessions_only.long.session.address());
    assert_eq!(all.short.session.address(), sessions_only.short.session.address());
    let addresses = [all.long.session.address(), all.short.session.address(), long.owner(), short.owner()];
    assert_eq!(addresses.iter().collect::<std::collections::BTreeSet<_>>().len(), 4);

    // The same key under the same password, written once more elsewhere.
    let elsewhere = tempfile::tempdir().unwrap();
    keystore::create(elsewhere.path(), long.owner(), KeyRole::Wallet, &long.secret(), PASSWORD).unwrap();
    let mut files: Vec<PathBuf> = names(dir.path()).iter().map(|name| dir.path().join(name)).collect();
    files.push(keystore::key_path(elsewhere.path(), long.owner(), KeyRole::Wallet));
    assert_eq!(files.len(), 5);
    for field in ["/crypto/kdfparams/salt", "/crypto/cipherparams/iv", "/crypto/ciphertext", "/crypto/mac"] {
        let values: std::collections::BTreeSet<String> = files
            .iter()
            .map(|file| key_file(file).pointer(field).unwrap().as_str().unwrap().to_owned())
            .collect();
        assert_eq!(values.len(), 5, "{field} repeats across key files");
    }
}

#[test]
fn what_is_written_is_the_format_on_record() {
    let account = Account::random();
    let dir = tempfile::tempdir().unwrap();
    keystore::create(dir.path(), account.owner(), KeyRole::Wallet, &account.secret(), PASSWORD).unwrap();
    let file = key_file(&keystore::key_path(dir.path(), account.owner(), KeyRole::Wallet));

    assert_eq!(file["version"], 3);
    assert_eq!(file["crypto"]["cipher"], "aes-128-ctr");
    assert_eq!(file["crypto"]["kdf"], "scrypt");
    let kdf = &file["crypto"]["kdfparams"];
    assert_eq!(
        (&kdf["n"], &kdf["r"], &kdf["p"], &kdf["dklen"]),
        (&8192.into(), &8.into(), &1.into(), &32.into())
    );
    let bytes = |field: &str| hex::decode(file.pointer(field).unwrap().as_str().unwrap()).unwrap().len();
    assert_eq!(
        [
            bytes("/crypto/kdfparams/salt"),
            bytes("/crypto/cipherparams/iv"),
            bytes("/crypto/ciphertext"),
            bytes("/crypto/mac")
        ],
        [32, 16, 32, 32]
    );
    assert_eq!(keystore::FORMAT, "Web3 Secret Storage V3: aes-128-ctr, scrypt N=8192 r=8 p=1 dklen=32");

    // The parameters are the library's. If its version moves, look at what it
    // writes now, then bring `keystore::FORMAT`, the README and this test up to date.
    let lock = include_str!("../Cargo.lock");
    assert!(
        lock.contains("name = \"eth-keystore\"\nversion = \"0.5.0\"\n"),
        "eth-keystore is no longer 0.5.0"
    );
    assert!(
        lock.contains("name = \"alloy-signer-local\"\nversion = \"2.5.0\"\n"),
        "alloy is no longer 2.5.0"
    );
}

#[test]
fn a_wrong_password_or_a_damaged_file_is_refused_without_quoting_anything() {
    let account = Account::random();
    let dir = tempfile::tempdir().unwrap();
    keystore::create(dir.path(), account.owner(), KeyRole::Wallet, &account.secret(), PASSWORD).unwrap();
    let path = keystore::key_path(dir.path(), account.owner(), KeyRole::Wallet);
    let good = fs::read_to_string(&path).unwrap();
    let load = |password: &str| {
        format!("{:#}", keystore::load(dir.path(), account.owner(), KeyRole::Wallet, password).unwrap_err())
    };
    let undecrypted = format!("{} was not decrypted: wrong password, or the file is damaged", path.display());
    let unreadable = format!("{} is not a key file this version reads", path.display());

    assert_eq!(load(WRONG_PASSWORD), undecrypted);
    assert_eq!(load(""), undecrypted);

    // One digit of the ciphertext changed: the MAC no longer matches.
    let mut damaged = key_file(&path);
    let ciphertext = damaged["crypto"]["ciphertext"].as_str().unwrap().to_owned();
    let flipped = if ciphertext.starts_with('0') { "1" } else { "0" };
    damaged["crypto"]["ciphertext"] = format!("{flipped}{}", &ciphertext[1..]).into();
    fs::write(&path, damaged.to_string()).unwrap();
    assert_eq!(load(PASSWORD), undecrypted);

    // Cut short, a format the library would have accepted or choked on, and a
    // raw key someone saved under the encrypted file's name.
    let raw_key = hex::encode_prefixed(account.secret());
    let mut short_key = key_file(&path);
    short_key["crypto"]["kdfparams"]["dklen"] = 16.into();
    let mut other_version = key_file(&path);
    other_version["version"] = 1.into();
    for content in [&good[..good.len() / 2], &short_key.to_string(), &other_version.to_string(), &raw_key] {
        fs::write(&path, content).unwrap();
        let refused = load(PASSWORD);
        assert!(refused.starts_with(&unreadable), "{refused}");
        assert!(!refused.contains(&raw_key[2..]), "{refused}");
        assert_eq!(fs::read_to_string(&path).unwrap(), *content, "a refused file was changed");
    }

    fs::write(&path, &good).unwrap();
    assert_eq!(
        keystore::load(dir.path(), account.owner(), KeyRole::Wallet, PASSWORD).unwrap().address(),
        account.owner()
    );
}

#[test]
fn bytes_that_are_no_private_key_are_refused_and_not_quoted() {
    let (dir, account) = (tempfile::tempdir().unwrap(), Account::random());
    let keys_dir = dir.path().join("secrets");
    // Zero, and the order of the curve: the first value that is too large.
    let order = hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141").unwrap();
    for bytes in [[0u8; 32], <[u8; 32]>::try_from(order).unwrap()] {
        for role in [KeyRole::Session, KeyRole::Wallet] {
            let refused = keystore::create(&keys_dir, account.owner(), role, &bytes, PASSWORD).unwrap_err();
            assert_eq!(refused.to_string(), "not a private key: it is outside the curve's range");
        }
    }
    assert!(!keys_dir.exists());
}

#[test]
fn a_file_that_is_already_there_is_never_replaced() {
    let (dir, account, other) = (tempfile::tempdir().unwrap(), Account::random(), Account::random());
    let owner = account.owner();
    let first = session_keys::create(dir.path(), owner, PASSWORD).unwrap();
    let path = keystore::key_path(dir.path(), owner, KeyRole::Session);
    let before = fs::read(&path).unwrap();

    let refusal = format!("{} already exists; it is not replaced", path.display());
    let again = session_keys::create(dir.path(), owner, PASSWORD).unwrap_err();
    assert_eq!(again.to_string(), refusal);
    // Nor by another key, nor under another password.
    let imported = keystore::create(dir.path(), owner, KeyRole::Session, &other.secret(), WRONG_PASSWORD);
    assert_eq!(imported.unwrap_err().to_string(), refusal);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(names(dir.path()), [format!("{owner:#x}.session.json")]);
    assert_eq!(keystore::load(dir.path(), owner, KeyRole::Session, PASSWORD).unwrap().address(), first);

    // Whatever the file is: here not even a key file.
    let wallet = keystore::key_path(dir.path(), owner, KeyRole::Wallet);
    fs::write(&wallet, "notes to self").unwrap();
    assert!(keystore::create(dir.path(), owner, KeyRole::Wallet, &account.secret(), PASSWORD).is_err());
    assert_eq!(fs::read_to_string(&wallet).unwrap(), "notes to self");
}

#[test]
fn of_several_writers_of_the_same_key_file_exactly_one_wins() {
    let (dir, account) = (tempfile::tempdir().unwrap(), Account::random());
    let (keys_dir, owner) = (dir.path(), account.owner());
    // They all pass the look for an existing file before any of them has
    // written: what keeps the file whole is that giving it its name cannot replace.
    let results: Vec<_> = std::thread::scope(|scope| {
        let writers: Vec<_> =
            (0..8).map(|_| scope.spawn(|| session_keys::create(keys_dir, owner, PASSWORD))).collect();
        writers.into_iter().map(|writer| writer.join().unwrap()).collect()
    });
    let winners: Vec<Address> = results.iter().filter_map(|result| result.as_ref().ok().copied()).collect();
    assert_eq!(winners.len(), 1, "{results:?}");
    for lost in results.iter().filter_map(|result| result.as_ref().err()) {
        assert!(lost.to_string().ends_with("already exists; it is not replaced"), "{lost:#}");
    }
    // The file is the winner's, whole, and the losers' drafts are gone.
    assert_eq!(keystore::load(keys_dir, owner, KeyRole::Session, PASSWORD).unwrap().address(), winners[0]);
    assert_eq!(names(keys_dir), [format!("{owner:#x}.session.json")]);
}

#[test]
fn a_raw_key_in_a_file_is_not_read_in_place_of_an_encrypted_one() {
    let (dir, account) = (tempfile::tempdir().unwrap(), Account::random());
    let (keys_dir, owner) = (dir.path(), account.owner());
    // A raw key lying next to where the encrypted one belongs: there is no falling back to it.
    let raw = keys_dir.join(format!("{owner:#x}.key"));
    fs::write(&raw, format!("{}\n", hex::encode_prefixed(account.secret()))).unwrap();

    assert!(!keystore::exists(keys_dir, owner, KeyRole::Session).unwrap());
    let refused = keystore::load(keys_dir, owner, KeyRole::Session, PASSWORD).unwrap_err().to_string();
    let path = keystore::key_path(keys_dir, owner, KeyRole::Session);
    assert_eq!(
        refused,
        format!("no encrypted session key at {}; `papertrade keys init` sets it up", path.display())
    );
    assert_eq!(keystore::require(keys_dir, owner, KeyRole::Session).unwrap_err().to_string(), refused);
}

#[test]
fn without_both_session_keys_nothing_is_unlocked() {
    let (long, short) = (Account::random(), Account::random());
    let dir = keys_dir(&long, &short, false);
    let config = config(dir.path(), &long, &short, false);

    let refused = format!("{:#}", keystore::unlock(&config, WRONG_PASSWORD).unwrap_err());
    assert!(refused.ends_with("was not decrypted: wrong password, or the file is damaged"), "{refused}");
    assert!(!refused.contains(WRONG_PASSWORD));

    let missing = keystore::key_path(dir.path(), short.owner(), KeyRole::Session);
    fs::remove_file(&missing).unwrap();
    let refused = format!("{:#}", keystore::unlock(&config, PASSWORD).unwrap_err());
    assert_eq!(
        refused,
        format!("no encrypted session key at {}; `papertrade keys init` sets it up", missing.display())
    );
}

#[test]
fn a_wallet_key_that_does_not_load_costs_the_wallet_automation_and_not_the_session_keys() {
    let (long, short) = (Account::random(), Account::random());
    let dir = keys_dir(&long, &short, false);
    // The long wallet's key was never imported; the short one's is under another password.
    keystore::create(dir.path(), short.owner(), KeyRole::Wallet, &short.secret(), WRONG_PASSWORD).unwrap();

    let keys = keystore::unlock(&config(dir.path(), &long, &short, true), PASSWORD).unwrap();
    assert_ne!(keys.long.session.address(), keys.short.session.address());
    let WalletKey::Unavailable(missing) = &keys.long.wallet else { panic!("{:?}", keys.long.wallet) };
    let path = keystore::key_path(dir.path(), long.owner(), KeyRole::Wallet);
    assert_eq!(
        *missing,
        format!("no encrypted wallet key at {}; `papertrade keys init` sets it up", path.display())
    );
    let WalletKey::Unavailable(locked) = &keys.short.wallet else { panic!("{:?}", keys.short.wallet) };
    assert!(locked.ends_with("was not decrypted: wrong password, or the file is damaged"), "{locked}");
    assert!(keys.long.wallet.signer().is_none() && keys.short.wallet.signer().is_none());

    // What is shown of the keys is addresses and reasons, never a key.
    let shown = format!("{keys:?}");
    assert!(shown.contains(&format!("{:?}", keys.long.session.address())), "{shown}");
    for session in [&keys.long.session, &keys.short.session] {
        assert!(!shown.contains(&hex::encode(session.to_bytes())), "{shown}");
    }
}

#[test]
fn with_the_automation_off_a_wallet_file_is_not_even_read() {
    let (long, short) = (Account::random(), Account::random());
    let dir = keys_dir(&long, &short, false);
    // Files that could only be reported as unavailable, had anything opened them.
    for account in [&long, &short] {
        fs::write(keystore::key_path(dir.path(), account.owner(), KeyRole::Wallet), "damaged").unwrap();
    }
    let keys = keystore::unlock(&config(dir.path(), &long, &short, false), PASSWORD).unwrap();
    assert!(matches!(keys.long.wallet, WalletKey::Disabled), "{:?}", keys.long.wallet);
    assert!(matches!(keys.short.wallet, WalletKey::Disabled), "{:?}", keys.short.wallet);

    // Switched on, the same files are what stands between the bot and the wallets.
    let keys = keystore::unlock(&config(dir.path(), &long, &short, true), PASSWORD).unwrap();
    assert!(matches!(keys.long.wallet, WalletKey::Unavailable(_)), "{:?}", keys.long.wallet);
}
