//! The keys this machine holds, encrypted at rest.
//!
//! Each account has a session key, which can open and close positions for it
//! and nothing else, and optionally the key of the wallet itself, which the
//! wallet automation signs with. Each is one file in the keys directory,
//! `<owner>.session.json` or `<owner>.wallet.json`, and all of them are
//! encrypted under the one password the operator types when the bot starts.
//!
//! The files are Web3 Secret Storage V3 as Alloy writes it; [`FORMAT`] records
//! the parameters. Those are the library's, not a choice made here, and they are
//! light: the password has to carry the weight. Encryption protects the files.
//! It does not protect a process that has been unlocked.
//!
//! Nothing here talks to the network or decides anything about trading.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256, hex};
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result, anyhow, bail, ensure};
use rand08::rngs::OsRng;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::config::Config;
use crate::strategy::decide::Side;

/// What a new key file is written as, by `eth-keystore` 0.5.0 through Alloy.
/// `tests/keystore.rs` holds a written file and the locked dependency to it.
pub const FORMAT: &str = "Web3 Secret Storage V3: aes-128-ctr, scrypt N=8192 r=8 p=1 dklen=32";

const VERSION: u64 = 3;
const CIPHER: &str = "aes-128-ctr";
const KDF: &str = "scrypt";
/// scrypt's cost as this version writes it. Nothing weaker is read.
const SCRYPT_N: u64 = 1 << 13;
/// The most expensive cost that is still read: what Geth writes by default.
const SCRYPT_N_MAX: u64 = 1 << 18;
const SCRYPT_R: u64 = 8;
const SCRYPT_P: u64 = 1;
const KEY_BYTES: usize = 32;
const IV_BYTES: usize = 16;

/// What a key is allowed to do, and so which file it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRole {
    /// Opens and closes positions for the owner once the owner has registered it.
    Session,
    /// The owner's own key.
    Wallet,
}

impl KeyRole {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyRole::Session => "session",
            KeyRole::Wallet => "wallet",
        }
    }
}

impl fmt::Display for KeyRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `<keys_dir>/<owner>.<role>.json`, the owner in lowercase.
pub fn key_path(keys_dir: &Path, owner: Address, role: KeyRole) -> PathBuf {
    keys_dir.join(format!("{owner:#x}.{role}.json"))
}

/// Where an earlier version kept the same key in plaintext.
pub fn plaintext_path(keys_dir: &Path, owner: Address, role: KeyRole) -> PathBuf {
    keys_dir.join(match role {
        KeyRole::Session => format!("{owner:#x}.key"),
        KeyRole::Wallet => format!("{owner:#x}.wallet.key"),
    })
}

/// What the keys directory holds of one key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    Encrypted,
    /// Only the plaintext file of an earlier version.
    Plaintext,
    Missing,
}

pub fn stored(keys_dir: &Path, owner: Address, role: KeyRole) -> Result<Stored> {
    let exists = |path: PathBuf| path.try_exists().with_context(|| format!("looking for {}", path.display()));
    Ok(if exists(key_path(keys_dir, owner, role))? {
        Stored::Encrypted
    } else if exists(plaintext_path(keys_dir, owner, role))? {
        Stored::Plaintext
    } else {
        Stored::Missing
    })
}

/// A private key as it is typed or as an earlier version stored it: 64 hex
/// digits, with or without `0x`. The error never quotes the input.
pub fn parse_secret(text: &str) -> Result<Zeroizing<[u8; KEY_BYTES]>> {
    let mut secret = Zeroizing::new([0u8; KEY_BYTES]);
    hex::decode_to_slice(text.trim(), secret.as_mut_slice())
        .map_err(|_| anyhow!("not a private key: 64 hex digits are expected"))?;
    Ok(secret)
}

/// Encrypts `secret` under `password` as the owner's key for `role` and returns
/// the key's address. A wallet key has to be the owner's own. A file that is
/// already there is never replaced.
pub fn create(
    keys_dir: &Path,
    owner: Address,
    role: KeyRole,
    secret: &[u8; KEY_BYTES],
    password: &str,
) -> Result<Address> {
    // The library's own message is left out: it must not get the chance to quote the key.
    let address = PrivateKeySigner::from_bytes(&B256::from(*secret))
        .map_err(|_| anyhow!("not a private key: it is outside the curve's range"))?
        .address();
    check_owner(owner, role, address).context("not stored")?;
    let path = key_path(keys_dir, owner, role);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(keys_dir)
        .with_context(|| format!("creating {}", keys_dir.display()))?;
    ensure!(!path.try_exists()?, "{} already exists; it is not replaced", path.display());

    // The library writes with `File::create`, which would replace a file. So it is
    // handed a temporary file of ours (mode 0600) in the same directory; what it
    // wrote is decrypted again, and only then given its name, by an operation
    // that fails if the name is taken. All that is ever on disk is ciphertext.
    let draft = tempfile::Builder::new()
        .prefix(".draft-")
        .tempfile_in(keys_dir)
        .with_context(|| format!("creating a file in {}", keys_dir.display()))?;
    let draft_name = draft.path().file_name().and_then(|name| name.to_str()).context("naming the draft")?;
    PrivateKeySigner::encrypt_keystore(keys_dir, &mut OsRng, secret, password, Some(draft_name))
        .map_err(|_| anyhow!("encrypting the {role} key of {owner} failed"))?;
    let written = decrypt(draft.path(), password)?.address();
    ensure!(written == address, "the {role} key of {owner} did not survive encryption; nothing was stored");
    draft.as_file().sync_all().context("flushing the new key file")?;
    draft.persist_noclobber(&path).map_err(|persist| match persist.error.kind() {
        ErrorKind::AlreadyExists => anyhow!("{} already exists; it is not replaced", path.display()),
        _ => anyhow!(persist.error).context(format!("writing {}", path.display())),
    })?;
    fs::File::open(keys_dir).and_then(|dir| dir.sync_all()).context("flushing the keys directory")?;
    Ok(address)
}

/// Encrypts the plaintext key an earlier version left for this owner, keeping
/// the key and so its address. The plaintext file is left exactly as it is.
pub fn migrate_plaintext(keys_dir: &Path, owner: Address, role: KeyRole, password: &str) -> Result<Address> {
    let path = plaintext_path(keys_dir, owner, role);
    let text =
        Zeroizing::new(fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?);
    let secret = parse_secret(&text).with_context(|| format!("{} was not migrated", path.display()))?;
    create(keys_dir, owner, role, &secret, password)
}

/// Decrypts the owner's key for `role`.
pub fn load(keys_dir: &Path, owner: Address, role: KeyRole, password: &str) -> Result<PrivateKeySigner> {
    let path = key_path(keys_dir, owner, role);
    if !path.try_exists().with_context(|| format!("looking for {}", path.display()))? {
        let plaintext = plaintext_path(keys_dir, owner, role);
        if plaintext.try_exists().unwrap_or(false) {
            bail!(
                "no encrypted {role} key at {}; {} is a plaintext key of an earlier version and is not read. \
                 `papertrade keys init` encrypts it",
                path.display(),
                plaintext.display()
            );
        }
        bail!("no encrypted {role} key at {}; `papertrade keys init` sets it up", path.display());
    }
    let signer = decrypt(&path, password)?;
    check_owner(owner, role, signer.address())
        .with_context(|| format!("{} was not loaded", path.display()))?;
    Ok(signer)
}

/// A wallet key is the owner's own; a session key is any key the owner registers.
fn check_owner(owner: Address, role: KeyRole, address: Address) -> Result<()> {
    ensure!(
        role == KeyRole::Session || address == owner,
        "this is the key of {address}, not of the wallet {owner}"
    );
    Ok(())
}

/// The one way a key file is read: its format is checked, then it is decrypted.
fn decrypt(path: &Path, password: &str) -> Result<PrivateKeySigner> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    check_format(&text)
        .with_context(|| format!("{} is not a key file this version reads", path.display()))?;
    // After the checks above the library fails on a MAC mismatch and on little else.
    PrivateKeySigner::decrypt_keystore(path, password)
        .map_err(|_| anyhow!("{} was not decrypted: wrong password, or the file is damaged", path.display()))
}

#[derive(Deserialize)]
struct KeyFile {
    version: u64,
    crypto: Crypto,
}

#[derive(Deserialize)]
struct Crypto {
    cipher: String,
    cipherparams: CipherParams,
    ciphertext: String,
    kdf: String,
    kdfparams: KdfParams,
    mac: String,
}

#[derive(Deserialize)]
struct CipherParams {
    iv: String,
}

#[derive(Deserialize)]
struct KdfParams {
    dklen: u64,
    n: u64,
    r: u64,
    p: u64,
    salt: String,
}

/// Holds a file to [`FORMAT`] before the library sees it. The library reads a
/// version, a cipher name and lengths it does not check, and would derive a key
/// at whatever cost the file asks for.
fn check_format(text: &str) -> Result<()> {
    // serde's message names a field or a position, never the file's content.
    let KeyFile { version, crypto } = serde_json::from_str(text)
        .map_err(|error| anyhow!("not a V3 key file ({:?} at line {})", error.classify(), error.line()))?;
    ensure!(version == VERSION, "version {version}, not {VERSION}");
    ensure!(crypto.cipher == CIPHER, "cipher {}, not {CIPHER}", crypto.cipher);
    ensure!(crypto.kdf == KDF, "key derivation {}, not {KDF}", crypto.kdf);
    let KdfParams { dklen, n, r, p, salt } = crypto.kdfparams;
    ensure!(dklen == KEY_BYTES as u64, "derived key length {dklen}, not {KEY_BYTES}");
    ensure!(
        n.is_power_of_two() && (SCRYPT_N..=SCRYPT_N_MAX).contains(&n) && r == SCRYPT_R && p == SCRYPT_P,
        "scrypt N={n} r={r} p={p}; N from {SCRYPT_N} to {SCRYPT_N_MAX} with r={SCRYPT_R} p={SCRYPT_P} is read"
    );
    for (field, value, bytes) in [
        ("salt", &salt, KEY_BYTES),
        ("iv", &crypto.cipherparams.iv, IV_BYTES),
        ("ciphertext", &crypto.ciphertext, KEY_BYTES),
        ("mac", &crypto.mac, KEY_BYTES),
    ] {
        ensure!(
            value.len() == 2 * bytes && value.bytes().all(|digit| digit.is_ascii_hexdigit()),
            "{field} is not {bytes} bytes of hex"
        );
    }
    Ok(())
}

/// The wallet's own key, as far as this run has it.
pub enum WalletKey {
    /// `PT_WALLET_AUTOMATION` is off. The file, if there is one, was not read.
    Disabled,
    Loaded(PrivateKeySigner),
    /// The automation is on but the key could not be loaded, for this reason.
    Unavailable(String),
}

impl WalletKey {
    pub fn signer(&self) -> Option<&PrivateKeySigner> {
        match self {
            WalletKey::Loaded(signer) => Some(signer),
            WalletKey::Disabled | WalletKey::Unavailable(_) => None,
        }
    }
}

impl fmt::Debug for WalletKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WalletKey::Disabled => f.write_str("Disabled"),
            WalletKey::Loaded(signer) => write!(f, "Loaded({})", signer.address()),
            WalletKey::Unavailable(reason) => write!(f, "Unavailable({reason:?})"),
        }
    }
}

/// One account's keys, decrypted.
pub struct AccountKeys {
    /// The account: the wallet the positions belong to.
    pub owner: Address,
    pub session: PrivateKeySigner,
    pub wallet: WalletKey,
}

impl fmt::Debug for AccountKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountKeys")
            .field("owner", &self.owner)
            .field("session", &self.session.address())
            .field("wallet", &self.wallet)
            .finish()
    }
}

/// Everything a run signs with. Decrypted once, when the process starts, and
/// held in memory until it ends; it is neither written anywhere nor serialized.
#[derive(Debug)]
pub struct UnlockedKeys {
    pub long: AccountKeys,
    pub short: AccountKeys,
}

impl UnlockedKeys {
    pub fn get(&self, side: Side) -> &AccountKeys {
        match side {
            Side::Long => &self.long,
            Side::Short => &self.short,
        }
    }
}

/// Decrypts what a run needs. Both session keys have to load. The wallets' own
/// keys are read only with `PT_WALLET_AUTOMATION` on, and one that does not
/// load is reported in its place instead of stopping the run: the session keys
/// can still manage and close what is open.
pub fn unlock(config: &Config, password: &str) -> Result<UnlockedKeys> {
    let account = |side: Side| -> Result<AccountKeys> {
        let (keys_dir, owner) = (&config.keys_dir, config.wallet(side)?);
        let session = load(keys_dir, owner, KeyRole::Session, password)?;
        let wallet = if config.wallet_automation {
            match load(keys_dir, owner, KeyRole::Wallet, password) {
                Ok(signer) => WalletKey::Loaded(signer),
                Err(error) => WalletKey::Unavailable(format!("{error:#}")),
            }
        } else {
            WalletKey::Disabled
        };
        Ok(AccountKeys { owner, session, wallet })
    };
    Ok(UnlockedKeys { long: account(Side::Long)?, short: account(Side::Short)? })
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    /// A file as this version writes it; the values are placeholders of the right shape.
    fn file() -> Value {
        json!({
            "crypto": {
                "cipher": "aes-128-ctr",
                "cipherparams": { "iv": "0".repeat(32) },
                "ciphertext": "0".repeat(64),
                "kdf": "scrypt",
                "kdfparams": { "dklen": 32, "n": 8192, "p": 1, "r": 8, "salt": "0".repeat(64) },
                "mac": "0".repeat(64),
            },
            "id": "6c4485f3-3cc0-4081-848e-8bf489f2c262",
            "version": 3,
        })
    }

    fn refusal(change: impl FnOnce(&mut Value)) -> String {
        let mut file = file();
        change(&mut file);
        check_format(&file.to_string()).unwrap_err().to_string()
    }

    #[test]
    fn the_written_format_is_read_and_so_is_the_cost_geth_writes() {
        check_format(&file().to_string()).unwrap();
        let mut geth = file();
        geth["crypto"]["kdfparams"]["n"] = json!(262_144);
        geth["address"] = json!("00000398232e2064f896018496b4b44b3d62751f");
        check_format(&geth.to_string()).unwrap();
    }

    #[test]
    fn anything_else_is_refused_before_the_library_sees_it() {
        assert_eq!(refusal(|file| file["version"] = json!(4)), "version 4, not 3");
        assert_eq!(
            refusal(|file| file["crypto"]["cipher"] = json!("aes-256-gcm")),
            "cipher aes-256-gcm, not aes-128-ctr"
        );
        let pbkdf2 = refusal(|file| {
            file["crypto"]["kdf"] = json!("pbkdf2");
            file["crypto"]["kdfparams"] = json!({ "c": 1, "dklen": 32, "prf": "hmac-sha256", "salt": "00" });
        });
        assert!(pbkdf2.starts_with("not a V3 key file"), "{pbkdf2}");
        assert_eq!(
            refusal(|file| file["crypto"]["kdf"] = json!("pbkdf2")),
            "key derivation pbkdf2, not scrypt"
        );
        // The library would slice a derived key this short out of range.
        assert_eq!(
            refusal(|file| file["crypto"]["kdfparams"]["dklen"] = json!(16)),
            "derived key length 16, not 32"
        );
        for (field, value) in [("n", 4096), ("n", 8193), ("n", 1 << 20), ("r", 1), ("p", 6)] {
            let refused = refusal(|file| file["crypto"]["kdfparams"][field] = json!(value));
            assert!(refused.starts_with("scrypt N="), "{field}={value}: {refused}");
        }
        assert_eq!(
            refusal(|file| file["crypto"]["cipherparams"]["iv"] = json!("00")),
            "iv is not 16 bytes of hex"
        );
        assert_eq!(
            refusal(|file| file["crypto"]["ciphertext"] = json!("zz".repeat(32))),
            "ciphertext is not 32 bytes of hex"
        );
        assert_eq!(refusal(|file| file["crypto"]["mac"] = json!("")), "mac is not 32 bytes of hex");
        assert_eq!(
            refusal(|file| file["crypto"]["kdfparams"]["salt"] = json!("0".repeat(62))),
            "salt is not 32 bytes of hex"
        );
        assert!(check_format("0x59c6995e998f97a5").unwrap_err().to_string().starts_with("not a V3 key file"));
    }

    #[test]
    fn a_file_that_is_not_json_is_refused_without_being_quoted() {
        let key = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        let refused = check_format(key).unwrap_err().to_string();
        assert!(!refused.contains("59c6"), "{refused}");
    }

    #[test]
    fn a_private_key_is_64_hex_digits_and_a_refusal_does_not_quote_it() {
        let key = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
        let parsed = parse_secret(key).unwrap();
        assert_eq!(parse_secret(&format!("  0x{key}\n")).unwrap(), parsed);
        assert_eq!(hex::encode(parsed.as_slice()), key);
        for wrong in [&key[..62], &format!("{key}00"), &key.replace('5', "g"), ""] {
            let refused = parse_secret(wrong).unwrap_err().to_string();
            assert_eq!(refused, "not a private key: 64 hex digits are expected");
        }
    }

    #[test]
    fn keys_are_named_after_their_owner_in_lowercase() {
        let owner: Address = "0x70997970C51812dc3A010C7d01b50e0d17dc79C8".parse().unwrap();
        let name = |path: PathBuf| path.to_str().unwrap().to_owned();
        let dir = Path::new("secrets");
        assert_eq!(
            name(key_path(dir, owner, KeyRole::Session)),
            "secrets/0x70997970c51812dc3a010c7d01b50e0d17dc79c8.session.json"
        );
        assert_eq!(
            name(key_path(dir, owner, KeyRole::Wallet)),
            "secrets/0x70997970c51812dc3a010c7d01b50e0d17dc79c8.wallet.json"
        );
        assert_eq!(
            name(plaintext_path(dir, owner, KeyRole::Session)),
            "secrets/0x70997970c51812dc3a010c7d01b50e0d17dc79c8.key"
        );
        assert_eq!(
            name(plaintext_path(dir, owner, KeyRole::Wallet)),
            "secrets/0x70997970c51812dc3a010c7d01b50e0d17dc79c8.wallet.key"
        );
    }
}
