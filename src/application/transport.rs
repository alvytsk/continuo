//! The transport decision table (design doc §4): given a transport key, the
//! queue and the playback phase, what should happen. Pure and stateless -
//! no engine call, no clock read - so the runtime that drives the engine
//! can be tested against this table without a real device.

use std::time::Duration;

use crate::queue::{Direction, Queue, QueueEntryId};

pub const PLAY_BEFORE_SEEK: &str = "Play a track before seeking";
pub const QUEUE_EMPTY: &str = "Queue is empty";
pub const TRACK_ENDED: &str = "Track ended; press play to replay";
pub const STILL_LOADING: &str = "Still loading";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackPhase {
    Unloaded,
    Loading,
    LoadFailed,
    Playing,
    Paused,
    Stopped,
    Ended,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportInput {
    Space,
    Play,
    Enter,
    Home,
    SeekBy(i64),
    SeekTo(Duration),
    Previous,
    Next,
}

pub struct TransportSituation<'a> {
    pub queue: &'a Queue,
    pub selected: Option<QueueEntryId>,
    pub phase: PlaybackPhase,
    pub last_requested: Option<QueueEntryId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportDecision {
    Load(QueueEntryId),
    TogglePause,
    Play,
    SeekBy(i64),
    SeekTo(Duration),
    Restart,
    Notice(&'static str),
    Nothing,
}

/// The selected row, but only while it is still queued; a row removed from
/// under the selection falls back to the first row rather than resolving to
/// a `Load` that later queue registration would reject.
fn selection(situation: &TransportSituation<'_>) -> Option<QueueEntryId> {
    situation
        .selected
        .filter(|id| situation.queue.get(*id).is_some())
        .or_else(|| situation.queue.first())
}

/// An id, but only while it is still queued - used for the `last_requested`
/// retry, which must not resolve to an entry a later removal has erased.
fn still_queued(queue: &Queue, id: Option<QueueEntryId>) -> Option<QueueEntryId> {
    id.filter(|id| queue.get(*id).is_some())
}

/// A `Load` of `id`, or the empty-queue notice if there was nothing to load.
/// The notice never actually fires here: every call site is reached only
/// once the queue is known nonempty, so `selection` always returns `Some`.
/// The fallback exists so this stays a safe total function rather than a
/// panic on an invariant the type system cannot express.
fn load_or_notice(id: Option<QueueEntryId>) -> TransportDecision {
    id.map_or(
        TransportDecision::Notice(QUEUE_EMPTY),
        TransportDecision::Load,
    )
}

/// The entry Previous/Next steps from. A playing, paused or stopped track
/// anchors on the active entry alone: with none adopted there is nothing
/// currently playing to step away from, so the key is a no-op rather than a
/// jump to whatever row happens to be selected.
fn anchor(situation: &TransportSituation<'_>) -> Option<QueueEntryId> {
    match situation.phase {
        PlaybackPhase::Unloaded | PlaybackPhase::LoadFailed | PlaybackPhase::Ended => {
            situation.queue.active().or_else(|| selection(situation))
        }
        PlaybackPhase::Loading => still_queued(situation.queue, situation.last_requested)
            .or_else(|| situation.queue.active())
            .or_else(|| selection(situation)),
        PlaybackPhase::Playing | PlaybackPhase::Paused | PlaybackPhase::Stopped => {
            situation.queue.active()
        }
    }
}

fn navigate(situation: &TransportSituation<'_>, direction: Direction) -> TransportDecision {
    match anchor(situation) {
        Some(id) => situation
            .queue
            .neighbor(id, direction)
            .map_or(TransportDecision::Nothing, TransportDecision::Load),
        None => TransportDecision::Nothing,
    }
}

/// A playing or paused track keeps its engine controls even once its queue
/// has been emptied out from under it: Space still toggles pause, `p` is
/// still idempotent play, and a seek in flight still lands. Only Enter,
/// which would otherwise pick a row that no longer exists, reports the
/// queue as empty.
fn decide_engine_with_empty_queue(input: TransportInput) -> TransportDecision {
    match input {
        TransportInput::Space => TransportDecision::TogglePause,
        TransportInput::Play => TransportDecision::Play,
        TransportInput::Home => TransportDecision::Restart,
        TransportInput::SeekBy(n) => TransportDecision::SeekBy(n),
        TransportInput::SeekTo(d) => TransportDecision::SeekTo(d),
        TransportInput::Previous | TransportInput::Next => TransportDecision::Nothing,
        TransportInput::Enter => TransportDecision::Notice(QUEUE_EMPTY),
    }
}

pub fn decide(input: TransportInput, situation: &TransportSituation<'_>) -> TransportDecision {
    if situation.queue.is_empty() {
        return match situation.phase {
            // A playing or paused track was not necessarily fed by this now-empty
            // queue's current contents, so the engine keeps running it.
            PlaybackPhase::Playing | PlaybackPhase::Paused => decide_engine_with_empty_queue(input),
            // Stopped playback drops its adoption along with the entry that fed
            // it, so restarting would replay media the listener just removed.
            _ => match input {
                TransportInput::Previous | TransportInput::Next => TransportDecision::Nothing,
                _ => TransportDecision::Notice(QUEUE_EMPTY),
            },
        };
    }

    use PlaybackPhase::{Ended, LoadFailed, Loading, Paused, Playing, Stopped, Unloaded};
    use TransportInput::{Enter, Home, Next, Play, Previous, SeekBy, SeekTo, Space};

    match (situation.phase, input) {
        (Unloaded, Space | Play) => {
            load_or_notice(situation.queue.active().or_else(|| selection(situation)))
        }
        (Unloaded, Enter) => load_or_notice(selection(situation)),
        (Unloaded, Home | SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(PLAY_BEFORE_SEEK),
        (Unloaded, Previous) => navigate(situation, Direction::Up),
        (Unloaded, Next) => navigate(situation, Direction::Down),

        (LoadFailed, Space | Play) => {
            let retry = still_queued(situation.queue, situation.last_requested);
            load_or_notice(
                retry
                    .or_else(|| situation.queue.active())
                    .or_else(|| selection(situation)),
            )
        }
        (LoadFailed, Enter) => load_or_notice(selection(situation)),
        (LoadFailed, Home | SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(PLAY_BEFORE_SEEK),
        (LoadFailed, Previous) => navigate(situation, Direction::Up),
        (LoadFailed, Next) => navigate(situation, Direction::Down),

        (Loading, Space | Play) => TransportDecision::Nothing,
        (Loading, Enter) => load_or_notice(selection(situation)),
        (Loading, Home | SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(STILL_LOADING),
        (Loading, Previous) => navigate(situation, Direction::Up),
        (Loading, Next) => navigate(situation, Direction::Down),

        (Playing | Paused | Stopped, Space) => TransportDecision::TogglePause,
        (Playing | Paused | Stopped, Play) => TransportDecision::Play,
        (Playing | Paused | Stopped, Enter) => load_or_notice(selection(situation)),
        (Playing | Paused | Stopped, Home) => TransportDecision::Restart,
        (Playing | Paused | Stopped, SeekBy(n)) => TransportDecision::SeekBy(n),
        (Playing | Paused | Stopped, SeekTo(d)) => TransportDecision::SeekTo(d),
        (Playing | Paused | Stopped, Previous) => navigate(situation, Direction::Up),
        (Playing | Paused | Stopped, Next) => navigate(situation, Direction::Down),

        (Ended, Space | Play) => {
            load_or_notice(situation.queue.active().or_else(|| selection(situation)))
        }
        (Ended, Enter) => load_or_notice(selection(situation)),
        (Ended, Home) => TransportDecision::Restart,
        (Ended, SeekBy(_) | SeekTo(_)) => TransportDecision::Notice(TRACK_ENDED),
        (Ended, Previous) => navigate(situation, Direction::Up),
        (Ended, Next) => navigate(situation, Direction::Down),
    }
}
