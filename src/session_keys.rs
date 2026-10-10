//! Session keys: what the bot trades with. A session key can open and close
//! positions for the wallet that registered it and nothing else: it cannot
//! withdraw, stake or claim. It is created here, on this machine, and kept
//! encrypted like every other key (see [`crate::keystore`]).
//!
//! Creating one sends nothing. It signs for a wallet only once that wallet has
//! registered it, and [`registration`] is the request the wallet signs for that.

use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anyhow::Result;
use zeroize::Zeroizing;

use crate::keystore::{self, KeyRole};
use crate::protocol::deployment::SESSION_KEY_TTL_SECONDS;
use crate::protocol::intents::{NonceSequence, RegisterSessionKey};

/// Creates the owner's session key and stores it encrypted under `password`.
/// Returns its address, which is what the owner registers.
pub fn create(keys_dir: &Path, owner: Address, password: &str) -> Result<Address> {
    let secret = Zeroizing::new(PrivateKeySigner::random().to_bytes().0);
    keystore::create(keys_dir, owner, KeyRole::Session, &secret, password)
}

/// The request by which `owner` registers `key` as its session key, for the
/// owner's wallet to sign. It can be submitted for `valid_for_seconds` from `now`.
pub fn registration(owner: Address, key: Address, now: u64, valid_for_seconds: u64) -> RegisterSessionKey {
    RegisterSessionKey {
        user: owner,
        key,
        expiry: now + SESSION_KEY_TTL_SECONDS,
        feeAddress: Address::ZERO,
        nonce: NonceSequence::random().allocate(),
        deadline: now + valid_for_seconds,
    }
}

/// Where the registration request waits between `session-key new` and
/// `session-key submit`. It holds addresses and numbers, no secret.
pub fn registration_path(keys_dir: &Path, owner: Address) -> PathBuf {
    keys_dir.join(format!("{owner:#x}.registration.json"))
}
