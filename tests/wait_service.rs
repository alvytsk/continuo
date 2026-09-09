//! Unit-level facts `WaitService` rests on: the hook and the main loop are one
//! implementation, and servicing with no transport preserves the retained
//! position instead of rewinding it to zero.
//!
//! The engine-level version of this fact - progress still rising while a real
//! read is blocked - needs a stalled server to stage, so it lives in Task 13
//! (H13). This is only the layer that fact rests on.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use continuo::http::channel::{SourceInterrupt, WaitHook};
use continuo::playback::engine::TransportCore;
use continuo::playback::event::{PlaybackEvent, Progress};
use continuo::playback::output::Nanos;
use continuo::playback::state::PlaybackState;
use continuo::playback::timeline::PositionQuality;
use continuo::playback::wait::{SessionFacts, WaitService};

/// The four wiring arguments that only the freeze tests care about, in the
/// inert configuration: nothing frozen, an empty backlog, a channel nobody
/// reads. A bare helper, so it handles its own error rather than unwrapping.
///
/// `type_complexity` is silenced rather than factored into a `type` alias:
/// this is a test-only tuple used at exactly two call sites, and a named
/// alias would only hide the same four `WaitService::new` arguments this is
/// wiring, not simplify them.
#[allow(clippy::type_complexity)]
fn inert() -> (
    Arc<SourceInterrupt>,
    crossbeam_channel::Sender<PlaybackEvent>,
    Arc<Mutex<std::collections::VecDeque<PlaybackEvent>>>,
    Arc<std::sync::atomic::AtomicBool>,
) {
    let (tx, rx) = crossbeam_channel::bounded(64);
    // Leak the receiver into the returned sender's lifetime by keeping it
    // alive here would be wrong; instead the caller keeps it. These tests do
    // not read events, so a disconnected channel is fine and try_send simply
    // fails, which `announce` already handles by using the outbox.
    drop(rx);
    (
        SourceInterrupt::new(1024),
        tx,
        Arc::new(Mutex::new(std::collections::VecDeque::new())),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    )
}

#[test]
fn servicing_publishes_the_position_the_timeline_reports() {
    let transport: Arc<Mutex<Option<TransportCore>>> = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::ZERO,
        quality: PositionQuality::Exact,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 4,
        media: None,
        position: Duration::from_secs(9),
        degraded: false,
        playing: true,
        frozen_by_hook: false,
    }));
    let (interrupt, events, outbox, backlog_empty) = inert();
    let service = WaitService::new(
        Arc::clone(&transport),
        Arc::clone(&progress),
        Arc::clone(&facts),
        interrupt,
        events,
        outbox,
        backlog_empty,
        Arc::new(|| Nanos(0)),
    );

    service.service();

    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.session_rev, 4);
    // With no transport the retained position stands: a blocked read must not
    // rewind the position to zero just because there is nothing to sample.
    assert_eq!(published.position, Duration::from_secs(9));
}

#[test]
fn servicing_a_freeze_parks_the_output_and_announces_paused() {
    // Section 9: pause must work during a stalled read. The worker is inside
    // pump_audio and will not reach its command loop until the read returns,
    // so if the hook does not do this, nothing does - the output keeps
    // draining and Paused is never emitted.
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
    let interrupt = SourceInterrupt::new(1024);
    let outbox = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let backlog_empty = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 3,
        media: None,
        position: Duration::from_secs(7),
        degraded: false,
        playing: true,
        frozen_by_hook: false,
    }));
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Progress {
            session_rev: 3,
            media: None,
            position: Duration::from_secs(7),
            quality: PositionQuality::Exact,
        })),
        Arc::clone(&facts),
        Arc::clone(&interrupt),
        events_tx,
        Arc::clone(&outbox),
        Arc::clone(&backlog_empty),
        Arc::new(|| Nanos(0)),
    );

    interrupt.freeze();
    service.service();
    match events_rx.try_recv() {
        Ok(PlaybackEvent::StateChanged {
            session_rev: 3,
            state: PlaybackState::Paused,
        }) => {}
        other => panic!("expected StateChanged{{Paused}}, got {other:?}"),
    }
    // Idempotent: a second slice must not re-announce.
    service.service();
    assert!(
        events_rx.try_recv().is_err(),
        "the freeze was announced twice"
    );

    interrupt.thaw();
    service.service();
    match events_rx.try_recv() {
        Ok(PlaybackEvent::StateChanged {
            session_rev: 3,
            state: PlaybackState::Playing,
        }) => {}
        other => panic!("expected StateChanged{{Playing}}, got {other:?}"),
    }
}

#[test]
fn a_hook_announcement_goes_to_the_outbox_when_the_workers_backlog_is_not_empty() {
    // Jumping a non-empty backlog would deliver Paused ahead of events emitted
    // before it. The outbox is drained into pending_events at the top of the
    // worker's next pass, so nothing is lost - only ordered.
    let (events_tx, events_rx) = crossbeam_channel::bounded(64);
    let interrupt = SourceInterrupt::new(1024);
    let outbox = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let backlog_empty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let service = WaitService::new(
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Progress {
            session_rev: 1,
            media: None,
            position: Duration::ZERO,
            quality: PositionQuality::Exact,
        })),
        Arc::new(Mutex::new(SessionFacts {
            session_rev: 1,
            media: None,
            position: Duration::ZERO,
            degraded: false,
            playing: true,
            frozen_by_hook: false,
        })),
        Arc::clone(&interrupt),
        events_tx,
        Arc::clone(&outbox),
        backlog_empty,
        Arc::new(|| Nanos(0)),
    );

    interrupt.freeze();
    service.service();
    assert!(
        events_rx.try_recv().is_err(),
        "the hook jumped a non-empty backlog"
    );
    let drained = service.take_outbox();
    assert!(
        matches!(
            drained.as_slice(),
            [PlaybackEvent::StateChanged {
                state: PlaybackState::Paused,
                ..
            }]
        ),
        "{drained:?}"
    );
}

#[test]
fn servicing_with_no_transport_is_harmless_and_repeatable() {
    let transport: Arc<Mutex<Option<TransportCore>>> = Arc::new(Mutex::new(None));
    let progress = Arc::new(Mutex::new(Progress {
        session_rev: 0,
        media: None,
        position: Duration::from_secs(3),
        quality: PositionQuality::Exact,
    }));
    let facts = Arc::new(Mutex::new(SessionFacts {
        session_rev: 1,
        media: None,
        position: Duration::from_secs(3),
        degraded: false,
        playing: false,
        frozen_by_hook: false,
    }));
    let (interrupt, events, outbox, backlog_empty) = inert();
    let service = WaitService::new(
        transport,
        Arc::clone(&progress),
        facts,
        interrupt,
        events,
        outbox,
        backlog_empty,
        Arc::new(|| Nanos(0)),
    );
    for _ in 0..100 {
        service.service();
    }
    let published = match progress.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    };
    assert_eq!(published.position, Duration::from_secs(3));
}
