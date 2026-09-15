//! What the terminal front end remembers between frames that the player
//! itself does not: the selected queue row, the open overlay, whether mouse
//! capture is on, the text being typed and how far the queue is scrolled.

use crate::application::view::PlayerView;
use crate::queue::QueueEntryId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Overlay {
    None,
    Help,
    ConfirmClear,
    Input,
    Browser,
}

#[derive(Clone, Debug)]
pub struct UiState {
    /// Independent of what is playing: moving it never touches playback.
    pub selected: Option<QueueEntryId>,
    pub overlay: Overlay,
    pub mouse_capture: bool,
    pub input: String,
    /// The first queue row the listing tries to show; drawing still scrolls
    /// as far as it must to keep the selection visible.
    pub queue_offset: usize,
}

impl UiState {
    pub fn new(mouse_capture: bool) -> Self {
        Self {
            selected: None,
            overlay: Overlay::None,
            mouse_capture,
            input: String::new(),
            queue_offset: 0,
        }
    }

    /// Keeps the selection pointing at a row that exists after the queue
    /// changed: the `hint` when it names a row (the runtime's choice after a
    /// removal, say), else the current selection while it survives, else the
    /// first row. An empty queue selects nothing and scrolls to the top.
    pub fn reconcile(&mut self, view: &PlayerView, hint: Option<QueueEntryId>) {
        let present =
            |id: Option<QueueEntryId>| id.filter(|id| view.rows.iter().any(|row| row.id == *id));
        self.selected = present(hint)
            .or_else(|| present(self.selected))
            .or_else(|| view.rows.first().map(|row| row.id));
        self.queue_offset = self.queue_offset.min(view.rows.len().saturating_sub(1));
    }
}
