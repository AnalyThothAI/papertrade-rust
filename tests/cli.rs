//! The built program, started the way an operator starts it, but with no
//! terminal to type at: what has to work without one, and what must refuse.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use papertrade::session_keys;

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

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const NEEDS_A_TERMINAL: &str = "this asks for a password and needs a terminal";

#[test]
fn watching_and_steering_need_no_key_no_password_and_no_terminal() {
    let dir = tempfile::tempdir().unwrap();
    // Where the keys would be, there is something no command here has any business reading.
    std::fs::write(dir.path().join("secrets"), "not a directory").unwrap();

    let status = papertrade(dir.path(), &["status"]);
    assert!(status.status.success(), "{}", stderr(&status));
    assert!(stdout(&status).contains("papertrade run"), "{}", stdout(&status));
    for (command, mode) in [("pause", "pause"), ("flatten", "flatten"), ("resume", "run")] {
        let output = papertrade(dir.path(), &[command]);
        assert!(output.status.success(), "{command}: {}", stderr(&output));
        assert_eq!(stdout(&output), format!("mode set to {mode}\n"));
        assert!(!stderr(&output).contains("assword"), "{command}: {}", stderr(&output));
    }
    assert_eq!(std::fs::read_to_string(dir.path().join("secrets")).unwrap(), "not a directory");
}

#[test]
fn run_stops_before_anything_else_when_it_has_no_keys_or_nobody_to_ask() {
    let dir = tempfile::tempdir().unwrap();
    let keys_dir = dir.path().join("secrets");

    // No keys: it says how to get them. It does not make any.
    let output = papertrade(dir.path(), &["run"]);
    assert!(!output.status.success());
    let expected = format!(
        "no encrypted session key at ./secrets/{LONG}.session.json; `papertrade keys init` sets it up"
    );
    assert!(stderr(&output).contains(&expected), "{}", stderr(&output));
    assert!(!keys_dir.exists());

    // A raw key lying in the directory is not a key to it: there is no falling back.
    std::fs::create_dir(&keys_dir).unwrap();
    let raw = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    std::fs::write(keys_dir.join(format!("{LONG}.key")), raw).unwrap();
    let output = papertrade(dir.path(), &["run"]);
    assert!(stderr(&output).contains(&expected), "{}", stderr(&output));

    // Keys, but no terminal to type the password at: refused, not waited for.
    for wallet in [LONG, SHORT] {
        session_keys::create(&keys_dir, wallet.parse().unwrap(), "correct horse battery staple").unwrap();
    }
    let output = papertrade(dir.path(), &["run"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains(NEEDS_A_TERMINAL), "{}", stderr(&output));
    // It got no further: the bot never started.
    assert!(!stderr(&output).contains("bot started"), "{}", stderr(&output));
}

#[test]
fn a_password_cannot_be_handed_over_as_an_argument_or_through_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    for command in [&["run"][..], &["keys", "init"], &["session-key", "new", "long"]] {
        let help = papertrade(dir.path(), &[command, &["--help"]].concat());
        let help = stdout(&help).to_lowercase();
        assert!(!help.contains("--password") && !help.contains("pt_password"), "{command:?}: {help}");
        let refused =
            papertrade(dir.path(), &[command, &["--password", "correct horse battery staple"]].concat());
        assert!(stderr(&refused).contains("unexpected argument '--password'"), "{}", stderr(&refused));
    }
    // And the one command that sends without `run` says so where it is looked up.
    let help = stdout(&papertrade(dir.path(), &["session-key", "submit", "--help"]));
    assert!(help.contains("real write") && help.contains("PT_DRY_RUN"), "{help}");
}

#[test]
fn preparing_a_registration_takes_the_password_from_a_terminal_only() {
    let dir = tempfile::tempdir().unwrap();
    let output = papertrade(dir.path(), &["session-key", "new", "long"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains(NEEDS_A_TERMINAL), "{}", stderr(&output));
    assert!(!dir.path().join("secrets").exists());
}

#[test]
fn keys_init_takes_a_password_from_a_terminal_only_and_writes_nothing_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let output = papertrade(dir.path(), &["keys", "init"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains(NEEDS_A_TERMINAL), "{}", stderr(&output));
    assert!(output.stdout.is_empty());
    assert!(!dir.path().join("secrets").exists());
}
