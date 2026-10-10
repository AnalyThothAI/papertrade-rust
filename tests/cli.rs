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

    // `stop` is asked of a running bot. With none running it is refused, so that
    // it cannot lie in wait and end the next start instead.
    let output = papertrade(dir.path(), &["stop"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("no bot is running on ./data: there is nothing to stop"),
        "{}",
        stderr(&output)
    );
    let status = stdout(&papertrade(dir.path(), &["status"]));
    assert!(!status.to_lowercase().contains("stop"), "{status}");
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

/// Runs `papertrade <arguments>` in a terminal of its own, presses `keys` once
/// `prompt` shows, and returns everything that terminal showed: the program's
/// output, then how it ended, then the terminal's settings afterwards as `stty`
/// reports them. The terminal is made by `script` (util-linux), which every
/// Linux has; the test has no other way to one without unsafe code.
#[cfg(target_os = "linux")]
fn in_a_terminal(dir: &Path, arguments: &str, prompt: &str, keys: &[u8]) -> String {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    let program = env!("CARGO_BIN_EXE_papertrade");
    let command = format!("{program} {arguments}; echo \"ended with $?\"; stty -a; echo settings shown");
    let mut child = Command::new("script")
        .args(["-qec", &command, "/dev/null"])
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("PT_LONG_ACCOUNT", LONG)
        .env("PT_SHORT_ACCOUNT", SHORT)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("`script` from util-linux is needed to give the program a terminal");
    let shown = Arc::new(Mutex::new(String::new()));
    let (mut output, sink) = (child.stdout.take().unwrap(), shown.clone());
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        while let Ok(read @ 1..) = output.read(&mut chunk) {
            sink.lock().unwrap().push_str(&String::from_utf8_lossy(&chunk[..read]));
        }
    });
    let wait_for = |text: &str| {
        let started = Instant::now();
        while !shown.lock().unwrap().contains(text) {
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "{text:?} never showed in:\n{}",
                shown.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    wait_for(prompt);
    // The prompt is printed a moment before the terminal is set up to read the answer.
    std::thread::sleep(Duration::from_millis(300));
    let mut keyboard = child.stdin.take().unwrap();
    keyboard.write_all(keys).unwrap();
    keyboard.flush().unwrap();
    wait_for("settings shown");
    drop(keyboard);
    let _ = child.wait();
    shown.lock().unwrap().clone()
}

/// Whether the terminal still echoes, edits lines and turns Ctrl-C into an
/// interrupt: what the next program started in it will find.
#[cfg(target_os = "linux")]
fn terminal_is_whole(shown: &str) -> bool {
    let settings = shown.rsplit("ended with").next().unwrap();
    let on = |flag: &str| settings.split([' ', ';', '\n', '\r']).any(|word| word == flag);
    on("isig") && on("icanon") && on("echo")
}

#[test]
#[cfg(target_os = "linux")]
fn ctrl_c_at_the_password_prompt_ends_run_and_leaves_the_terminal_whole() {
    let dir = tempfile::tempdir().unwrap();
    for wallet in [LONG, SHORT] {
        let keys_dir = dir.path().join("secrets");
        session_keys::create(&keys_dir, wallet.parse().unwrap(), "correct horse battery staple").unwrap();
    }
    let shown = in_a_terminal(dir.path(), "run", "Password for ./secrets: ", b"\x03");
    // It ended by its own doing, saying why: not killed where it stood, which
    // would leave the terminal without Ctrl-C for whatever runs in it next.
    assert!(shown.contains("Error: interrupted"), "{shown}");
    assert!(shown.contains("ended with 1"), "{shown}");
    assert!(terminal_is_whole(&shown), "the terminal was left changed:\n{shown}");
    assert!(!shown.contains("bot started"), "{shown}");
}

#[test]
#[cfg(target_os = "linux")]
fn ctrl_c_at_a_prompt_of_keys_init_writes_nothing_and_leaves_the_terminal_whole() {
    let dir = tempfile::tempdir().unwrap();
    let shown = in_a_terminal(dir.path(), "keys init", "New password: ", b"\x03");
    assert!(shown.contains("Error: interrupted"), "{shown}");
    assert!(shown.contains("ended with 1"), "{shown}");
    assert!(terminal_is_whole(&shown), "the terminal was left changed:\n{shown}");
    assert!(!dir.path().join("secrets").exists());

    // A password typed in full still goes through: the listener does not get in its way.
    let typed = b"correct horse battery staple\rcorrect horse battery staple\r";
    let shown = in_a_terminal(dir.path(), "keys init", "New password: ", typed);
    assert!(shown.contains("Nothing was sent") && shown.contains("ended with 0"), "{shown}");
    assert!(terminal_is_whole(&shown), "the terminal was left changed:\n{shown}");
    assert!(!shown.contains("correct horse"), "the password was echoed:\n{shown}");
}
