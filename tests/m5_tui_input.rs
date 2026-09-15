#[path = "support/views.rs"]
mod views;

use continuo::application::runtime::{AppCommand, EnqueueItem};
use continuo::tui::input::{Effect, handle_key};
use continuo::tui::state::{Overlay, UiState};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use views::sample_view;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}

fn shift(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}

fn app(effects: &[Effect]) -> Vec<&AppCommand> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::App(c) => Some(c),
            _ => None,
        })
        .collect()
}

#[test]
fn selection_moves_without_touching_playback() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.reconcile(&view, None);
    assert_eq!(
        ui.selected,
        Some(view.rows[0].id),
        "default selection is the first row"
    );
    let effects = handle_key(key(KeyCode::Down), &mut ui, &view);
    assert!(app(&effects).is_empty());
    assert_eq!(ui.selected, Some(view.rows[1].id));
    handle_key(key(KeyCode::Char('k')), &mut ui, &view);
    assert_eq!(ui.selected, Some(view.rows[0].id));
}

#[test]
fn transport_keys_carry_the_selection_as_an_argument() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[2].id);
    let selected = ui.selected;
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char(' ')), &mut ui, &view))[..],
        [AppCommand::PlayPause { selected: s }] if *s == selected
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Enter), &mut ui, &view))[..],
        [AppCommand::PlayEntry(id)] if Some(*id) == selected
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char('p')), &mut ui, &view))[..],
        [AppCommand::Play { .. }]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Left), &mut ui, &view))[..],
        [AppCommand::SeekBy(-10)]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Home), &mut ui, &view))[..],
        [AppCommand::Restart]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char(']')), &mut ui, &view))[..],
        [AppCommand::Next { .. }]
    ));
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char('J')), &mut ui, &view))[..],
        [AppCommand::Move(_, continuo::queue::Direction::Down)]
    ));
}

#[test]
fn both_spellings_of_each_volume_key_work() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    for (code, delta) in [('-', -0.05), ('_', -0.05), ('+', 0.05), ('=', 0.05)] {
        let effects = handle_key(key(KeyCode::Char(code)), &mut ui, &view);
        assert!(
            matches!(app(&effects)[..], [AppCommand::AdjustVolume(d)] if (*d - delta).abs() < f32::EPSILON),
            "{code}"
        );
    }
}

#[test]
fn typing_a_url_never_triggers_shortcuts_and_enter_enqueues_it() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::Input);
    for c in "https://q.example/ p+.mp3".chars() {
        assert!(
            handle_key(key(KeyCode::Char(c)), &mut ui, &view).is_empty(),
            "{c}"
        );
    }
    let effects = handle_key(key(KeyCode::Enter), &mut ui, &view);
    assert!(matches!(
        app(&effects)[..],
        [AppCommand::Enqueue(items)] if matches!(&items[..], [EnqueueItem::Url(u)] if u == "https://q.example/ p+.mp3")
    ));
    assert_eq!(ui.overlay, Overlay::None);
}

#[test]
fn ctrl_c_quits_even_while_typing_and_esc_cancels_input() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    handle_key(key(KeyCode::Char('x')), &mut ui, &view);
    handle_key(key(KeyCode::Esc), &mut ui, &view);
    assert_eq!((ui.overlay, ui.input.as_str()), (Overlay::None, ""));
    handle_key(key(KeyCode::Char('a')), &mut ui, &view);
    assert!(matches!(
        handle_key(ctrl('c'), &mut ui, &view)[..],
        [Effect::Quit]
    ));
}

#[test]
fn clearing_requires_confirmation() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert!(app(&handle_key(key(KeyCode::Char('n')), &mut ui, &view)).is_empty());
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert!(matches!(
        app(&handle_key(key(KeyCode::Char('y')), &mut ui, &view))[..],
        [AppCommand::ClearQueue]
    ));
}

#[test]
fn mouse_toggle_and_full_redraw() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    assert!(matches!(
        handle_key(key(KeyCode::Char('m')), &mut ui, &view)[..],
        [Effect::SetMouseCapture(false)]
    ));
    assert!(!ui.mouse_capture);
    assert!(matches!(
        handle_key(ctrl('l'), &mut ui, &view)[..],
        [Effect::FullRedraw]
    ));
    let release = KeyEvent {
        code: KeyCode::Char('q'),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    };
    assert!(handle_key(release, &mut ui, &view).is_empty());
}

#[test]
fn enter_on_an_empty_queue_is_a_notice_not_a_command() {
    let mut view = sample_view();
    view.rows.clear();
    let mut ui = UiState::new(true);
    ui.selected = None;
    let effects = handle_key(key(KeyCode::Enter), &mut ui, &view);
    match &effects[..] {
        [Effect::Notice(message)] => {
            assert_eq!(*message, continuo::application::transport::QUEUE_EMPTY);
        }
        other => panic!("expected a single Notice effect, got {other:?}"),
    }
    assert!(app(&effects).is_empty());
}

#[test]
fn ctrl_and_alt_chords_never_fire_the_plain_letter_shortcut() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[0].id);

    // Ctrl-D would otherwise remove the selected row.
    assert!(handle_key(ctrl('d'), &mut ui, &view).is_empty());
    assert_eq!(ui.selected, Some(view.rows[0].id), "nothing was removed");

    // Ctrl-S would otherwise stop playback.
    assert!(handle_key(ctrl('s'), &mut ui, &view).is_empty());

    // Alt-D must be blocked the same way as Ctrl-D.
    assert!(handle_key(alt('d'), &mut ui, &view).is_empty());
    assert_eq!(
        ui.selected,
        Some(view.rows[0].id),
        "still nothing was removed"
    );
}

#[test]
fn shift_still_reaches_bindings_that_need_an_uppercase_or_symbol_key() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    ui.selected = Some(view.rows[1].id);
    assert!(matches!(
        app(&handle_key(shift('K'), &mut ui, &view))[..],
        [AppCommand::Move(_, continuo::queue::Direction::Up)]
    ));
}

#[test]
fn a_ctrl_chord_on_y_does_not_confirm_the_clear() {
    let view = sample_view();
    let mut ui = UiState::new(true);
    handle_key(key(KeyCode::Char('c')), &mut ui, &view);
    assert_eq!(ui.overlay, Overlay::ConfirmClear);
    assert!(app(&handle_key(ctrl('y'), &mut ui, &view)).is_empty());
    assert_eq!(ui.overlay, Overlay::None, "any key closes the confirmation");
}
