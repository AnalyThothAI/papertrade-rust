//! The built program, started the way an operator starts it, but with no
//! terminal to type at: what has to work without one, and what must refuse.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const LONG: &str = "0x0000000000000000000000000000000000000001";
const SHORT: &str = "0x0000000000000000000000000000000000000002";

/// Runs `papertrade` in `dir` with nothing of this machine's own setup: an empty
/// environment, no `.env` in or above the directory, and no terminal on stdin.
fn papertrade(dir: &Path, arguments: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_papertrade"))
        .args(arguments)
        .current_dir(dir)
        .env_clear()
        .env("PT_LONG_ACCOUNT", LONG)
        .env("PT_SHORT_ACCOUNT", SHORT)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // A command that went on to wait for a password would never return.
    let started = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if started.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            panic!("`papertrade {}` is still running: is it waiting for input?", arguments.join(" "));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn keys_init_takes_a_password_from_a_terminal_only_and_writes_nothing_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let output = papertrade(dir.path(), &["keys", "init"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("this asks for a password and needs a terminal"), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(!dir.path().join("secrets").exists());

    // Nor is there a way to hand it one as an argument or through the environment.
    let help = papertrade(dir.path(), &["keys", "init", "--help"]);
    let help = String::from_utf8_lossy(&help.stdout).to_lowercase();
    assert!(!help.contains("--password") && !help.contains("pt_password"), "{help}");
    let refused = papertrade(dir.path(), &["keys", "init", "--password", "correct horse battery staple"]);
    assert!(stderr(&refused).contains("unexpected argument '--password'"), "{}", stderr(&refused));
}
