#![cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;
#[path = "support/pty.rs"]
mod pty;
mod support;

use pty::PtyChild;
use std::process::{Child, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use support::server::{Script, TestServer};

const SIGNALS: [(&str, i32); 3] = [("INT", 2), ("HUP", 1), ("TERM", 15)];
const FIXTURE_5S: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine-5s.flac");
const FIXTURE_SHORT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac");
const PATIENCE: Duration = Duration::from_secs(15);

#[allow(clippy::expect_used)] // A fixed shell command against a child this test just spawned.
fn send_signal(child: &Child, name: &str) {
    let status = std::process::Command::new("kill")
        .arg(format!("-{name}"))
        .arg(child.id().to_string())
        .status()
        .expect("kill");
    assert!(status.success());
}

fn wait_exit(child: &mut Child, patience: Duration) -> ExitStatus {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return status;
        }
        assert!(Instant::now() < deadline, "the process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[allow(clippy::expect_used)] // Fallible spawn of a fixed test binary.
fn next_invocation_acquires_the_profile(profile: &process::Profile) {
    let output = profile
        .command()
        .args(["play", FIXTURE_SHORT])
        .env("CONTINUO_AUDIO_OUTPUT", "null")
        .output()
        .expect("next");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn every_shutdown_signal_during_stalled_preparation_exits_128_plus_n() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
        let mut child = profile
            .command()
            .args(["play", &server.url("/a.mp3")])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        assert!(server.wait_until_stalled(Duration::from_secs(10)));
        send_signal(&child, name);
        let status = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + number), "SIG{name}");
        next_invocation_acquires_the_profile(&profile);
        server.shutdown();
    }
}

#[test]
fn every_shutdown_signal_during_playback_flushes_a_checkpoint() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let mut child = profile
            .command()
            .args(["play", FIXTURE_5S])
            .env("CONTINUO_AUDIO_OUTPUT", "null")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        std::thread::sleep(Duration::from_millis(1_500));
        send_signal(&child, name);
        let status = wait_exit(&mut child, Duration::from_secs(15));
        assert_eq!(status.code(), Some(128 + number), "SIG{name}");
        let secs = checkpoint_secs(&profile);
        assert!(secs >= 1, "SIG{name}: position {secs}");
        next_invocation_acquires_the_profile(&profile);
    }
}

/// The flushed checkpoint position of `FIXTURE_5S`, in whole seconds.
#[allow(clippy::expect_used)] // Reads the state file a test run just flushed.
fn checkpoint_secs(profile: &process::Profile) -> u64 {
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.state_file()).expect("flushed"))
            .expect("json");
    let key = format!(
        "local:{}",
        std::fs::canonicalize(FIXTURE_5S)
            .expect("fixture")
            .display()
    );
    state["checkpoints"][&key]["position"]["secs"]
        .as_u64()
        .expect("checkpoint written")
}

/// `play` of `FIXTURE_5S` on a PTY, so it runs in raw mode reading keys,
/// once its status line shows a second of playback.
#[allow(clippy::expect_used)] // Fallible spawn of a fixed test binary.
fn playing_under_a_pty(profile: &process::Profile) -> PtyChild {
    let child = PtyChild::spawn(
        profile.root(),
        &["play", FIXTURE_5S],
        &[("CONTINUO_AUDIO_OUTPUT", "null")],
        100,
        30,
    )
    .expect("spawn");
    assert!(child.wait_for("00:01", PATIENCE), "{}", child.output());
    child
}

#[test]
fn every_shutdown_signal_in_raw_mode_under_a_pty_flushes_a_checkpoint() {
    for (name, number) in SIGNALS {
        let profile = process::Profile::new().expect("profile");
        let mut child = playing_under_a_pty(&profile);
        let status = std::process::Command::new("kill")
            .arg(format!("-{name}"))
            .arg(child.pid().expect("pid").to_string())
            .status()
            .expect("kill");
        assert!(status.success());
        assert_eq!(
            child.wait_exit(PATIENCE),
            Some((128 + number).unsigned_abs()),
            "SIG{name}: {}",
            child.output()
        );
        let secs = checkpoint_secs(&profile);
        assert!(secs >= 1, "SIG{name}: position {secs}");
        next_invocation_acquires_the_profile(&profile);
    }
}

/// A closed pane types nothing: the hangup is all the key reader sees.
#[test]
fn a_silently_closed_pty_ends_play_and_flushes_a_checkpoint() {
    let profile = process::Profile::new().expect("profile");
    let mut child = playing_under_a_pty(&profile);
    child.close_master_silently();
    let code = child.wait_exit(PATIENCE);
    assert!(matches!(code, Some(129 | 1)), "exit {code:?}");
    let secs = checkpoint_secs(&profile);
    assert!(secs >= 1, "position {secs}");
    next_invocation_acquires_the_profile(&profile);
}

#[test]
fn exit_status_arithmetic() {
    use continuo::lifecycle::RunOutcome;
    assert_eq!(RunOutcome::Completed.exit_status(), 0);
    assert_eq!(RunOutcome::Signalled(15).exit_status(), 143);
    assert_eq!(RunOutcome::Signalled(200).exit_status(), 255);
}
