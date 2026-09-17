#![cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;
mod support;

use std::process::Stdio;
use std::time::{Duration, Instant};
use support::server::{Script, TestServer};

const CONTENDED: &str = "Another Tenuto player is using this state profile";

fn wait_exit(
    child: &mut std::process::Child,
    patience: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + patience;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_second_play_refuses_while_the_first_holds_the_profile() {
    let profile = process::Profile::new().expect("profile");
    let server = TestServer::start(Script::serving(b"x".to_vec()).stall_headers());
    let mut first = profile
        .command()
        .args(["play", &server.url("/a.mp3")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    // The load starts only after the lock and state load, so a stalled
    // request proves the first process owns the profile.
    assert!(server.wait_until_stalled(Duration::from_secs(10)));

    let second = profile
        .command()
        .args([
            "play",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"),
        ])
        .env("TENUTO_AUDIO_OUTPUT", "null")
        .output()
        .expect("second");
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains(CONTENDED));

    let _ = first.kill();
    let _ = wait_exit(&mut first, Duration::from_secs(10));
    server.shutdown();
}

#[test]
fn source_errors_are_reported_before_profile_contention() {
    let profile = process::Profile::new().expect("profile");
    let _held = tenuto::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let absent = profile
        .command()
        .args(["play", "/nonexistent/definitely-not-here.flac"])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&absent.stderr);
    assert!(
        text.contains("definitely-not-here.flac") && !text.contains(CONTENDED),
        "{text}"
    );
    let directory = profile
        .command()
        .args(["play", env!("CARGO_MANIFEST_DIR")])
        .output()
        .expect("run");
    let text = String::from_utf8_lossy(&directory.stderr);
    assert!(
        text.contains("regular file") && !text.contains(CONTENDED),
        "{text}"
    );
}

#[test]
fn probe_only_and_feed_listing_ignore_a_held_profile() {
    let profile = process::Profile::new().expect("profile");
    let _held = tenuto::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let probe = profile
        .command()
        .args([
            "play",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"),
            "--probe-only",
        ])
        .output()
        .expect("run");
    assert!(probe.status.success());
    assert!(
        profile
            .command()
            .arg("feeds")
            .output()
            .expect("run")
            .status
            .success()
    );
}
