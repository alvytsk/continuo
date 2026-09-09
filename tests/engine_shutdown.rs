//! What `join` owes the caller: the position the shutdown captured, and every
//! event the application never got to drain.

use std::time::Duration;

use continuo::playback::command::PlaybackCommand;
use continuo::playback::event::PlaybackEvent;
use continuo::playback::state::PlaybackState;
use continuo::playback::volume::Volume;

mod support;

use support::TestEngine;

const TRACK: &str = "sine-5s.flac";

/// One flush pass: how long the worker needs, after `await_commands_taken`
/// confirms it has read a command, to emit that command's event and for
/// `flush_events` to hand it to the channel the test then drains via `join`.
const FLUSH_PASS: Duration = Duration::from_millis(50);

#[test]
fn join_returns_the_position_the_shutdown_captured() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_secs(1));
    let published = engine.position();

    let report = engine
        .shutdown_report()
        .expect("the engine was still running");

    assert!(
        report.progress.position >= published,
        "the captured position must be at least the last one published: {:?} < {:?}",
        report.progress.position,
        published
    );
    assert!(
        report.progress.position < Duration::from_secs(5),
        "and it must still be a position in this track: {:?}",
        report.progress.position
    );
}

#[test]
fn join_returns_the_events_the_application_never_drained() {
    let mut engine = TestEngine::start(TRACK);
    engine.play_for(Duration::from_secs(1));

    // From here the application is exactly as blind as `app::run` is between
    // pressing `q` and breaking out of its loop.
    engine.stop_draining_events();
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.5)));
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.25)));
    engine.send(PlaybackCommand::Stop);

    // The interrupt is checked before any command is read, so without this the
    // test would be asking after events the worker never produced.
    engine.await_commands_taken();
    // And one flush pass, so all three are in the channel the application is
    // no longer draining. Which side of the handoff they sit on is the next
    // test's subject, not this one's.
    std::thread::sleep(FLUSH_PASS);

    let report = engine
        .shutdown_report()
        .expect("the engine was still running");

    let volumes: Vec<f32> = report
        .events
        .iter()
        .filter_map(|event| match event {
            PlaybackEvent::VolumeChanged { volume, .. } => Some(volume.as_gain()),
            _ => None,
        })
        .collect();
    assert_eq!(
        volumes,
        vec![0.5, 0.25],
        "in emission order: {:?}",
        report.events
    );

    assert!(
        report.events.iter().any(|event| matches!(
            event,
            PlaybackEvent::StateChanged {
                state: PlaybackState::Stopped,
                ..
            }
        )),
        "the stop must survive the handoff too: {:?}",
        report.events
    );
}

#[test]
fn an_event_racing_the_shutdown_interrupt_still_arrives() {
    let mut engine = TestEngine::start(TRACK);
    engine.stop_draining_events();
    engine.send(PlaybackCommand::SetVolume(Volume::new(0.5)));

    // Wait only for the command to be *taken*, never for its event to be
    // flushed. That the event exists is the premise; whether it is still in the
    // worker's backlog or already in the channel when the interrupt lands is
    // the race, and the report is required to be indifferent to which.
    engine.await_commands_taken();
    let report = engine
        .shutdown_report()
        .expect("the engine was still running");

    assert!(
        report.events.iter().any(|event| matches!(
            event,
            PlaybackEvent::VolumeChanged { volume, .. } if volume.as_gain() == 0.5
        )),
        "an event emitted just before the interrupt is not the application's to lose: {:?}",
        report.events
    );
}

#[test]
fn a_load_reports_where_it_landed() {
    let mut engine = TestEngine::start_at(TRACK, Duration::from_secs(2));

    let loaded = engine.await_event(|event| matches!(event, PlaybackEvent::Loaded { .. }));
    let PlaybackEvent::Loaded { position, .. } = loaded else {
        panic!("await_event returned the wrong event");
    };
    assert!(
        position >= Duration::from_secs(2) && position < Duration::from_secs(3),
        "Loaded must carry the adopted landing, not zero: {position:?}"
    );
}
