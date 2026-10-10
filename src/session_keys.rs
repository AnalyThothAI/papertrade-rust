//! Session keys are the only secrets this bot holds. A session key can open and
//! close positions for its wallet but cannot withdraw, stake or claim, so the
//! worst case of a leaked key is losing the trading balance to bad trades.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use alloy::primitives::Address;
use alloy::signers::local::PrivateKeySigner;
use anyhow::{Context, Result};

pub fn key_path(keys_dir: &Path, wallet: Address) -> PathBuf {
    keys_dir.join(format!("{wallet:#x}.key"))
}

pub fn load(keys_dir: &Path, wallet: Address) -> Result<PrivateKeySigner> {
    let path = key_path(keys_dir, wallet);
    let key = fs::read_to_string(&path).with_context(|| {
        format!("no session key at {}; create one with `papertrade session-key new`", path.display())
    })?;
    key.trim().parse().with_context(|| format!("{} does not hold a private key", path.display()))
}

/// Loads the wallet's session key, creating it (owner-readable only) on first use.
pub fn ensure(keys_dir: &Path, wallet: Address) -> Result<PrivateKeySigner> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(keys_dir)
        .with_context(|| format!("creating {}", keys_dir.display()))?;
    // `create_new` fails if the file exists, so an existing key is never overwritten.
    match fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(key_path(keys_dir, wallet)) {
        Ok(mut file) => writeln!(file, "{:#x}", PrivateKeySigner::random().to_bytes())?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).context("creating the session key file"),
    }
    load(keys_dir, wallet)
}
