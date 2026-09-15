#![cfg(target_os = "linux")]
#[path = "support/process.rs"]
mod process;
#[path = "support/pty.rs"]
mod pty;
mod support;

use pty::PtyChild;
use std::time::Duration;

const CONTENDED: &str = "Another Continuo player is using this state profile";
const LEAVE_ALT: &str = "\x1b[?1049l";

#[test]
fn tui_opens_idle_on_an_empty_queue_and_q_restores_the_terminal() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(
        child.wait_for("Queue is empty", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    assert!(child.output().contains(LEAVE_ALT));
}

#[test]
fn tui_refuses_a_held_profile_before_entering_raw_mode() {
    let profile = process::Profile::new().expect("profile");
    let _held =
        continuo::lifecycle::lock::ProfileLock::acquire(&profile.state_file()).expect("hold");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    let code = child.wait_exit(Duration::from_secs(10));
    assert!(matches!(code, Some(c) if c != 0));
    let output = child.output();
    assert!(output.contains(CONTENDED), "{output}");
    assert!(
        !output.contains("\x1b[?1049h"),
        "never entered the alternate screen"
    );
}

#[test]
fn play_refuses_while_tui_holds_the_profile_and_tui_keeps_its_volume() {
    let profile = process::Profile::new().expect("profile");
    let mut tui = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(tui.wait_for("Queue is empty", Duration::from_secs(10)));
    tui.send(b"-");
    let play = profile
        .command()
        .args([
            "play",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"),
        ])
        .env("CONTINUO_AUDIO_OUTPUT", "null")
        .output()
        .expect("play");
    assert!(String::from_utf8_lossy(&play.stderr).contains(CONTENDED));
    tui.send(b"q");
    assert_eq!(tui.wait_exit(Duration::from_secs(10)), Some(0));
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(profile.state_file()).expect("flushed"))
            .expect("json");
    assert_eq!(state["volume"], 0.95);
}

#[test]
fn sigterm_restores_the_terminal_and_exits_143() {
    let profile = process::Profile::new().expect("profile");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for("Queue is empty", Duration::from_secs(10)));
    let pid = child.pid().expect("pid");
    assert!(
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .expect("kill")
            .success()
    );
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(143));
    assert!(child.output().contains(LEAVE_ALT));
}

#[test]
fn b_browses_the_working_directory_and_enter_enqueues_a_file() {
    let profile = process::Profile::new().expect("profile");
    // Sorts ahead of the profile's own state/data/cache/config directories.
    let music = profile.root().join("0-music");
    std::fs::create_dir(&music).expect("music dir");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sine.flac"),
        music.join("track.flac"),
    )
    .expect("copy fixture");
    let mut child = PtyChild::spawn(profile.root(), &["tui"], &[], 100, 30).expect("spawn");
    assert!(child.wait_for("Queue is empty", Duration::from_secs(10)));
    child.send(b"b");
    assert!(
        child.wait_for("0-music/", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"\r");
    assert!(
        child.wait_for("track.flac", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"\r");
    child.send(b"b");
    assert!(
        child.wait_for("queue 01", Duration::from_secs(10)),
        "{}",
        child.output()
    );
    child.send(b"q");
    assert_eq!(child.wait_exit(Duration::from_secs(10)), Some(0));
    let state = std::fs::read_to_string(profile.state_file()).expect("flushed");
    assert!(state.contains("track.flac"), "{state}");
}
