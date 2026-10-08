//! Classification of terminal events into the gestures the root component reacts to.
//!
//! Key gestures fire on press and on auto-repeat; releases never trigger an action.

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Position, Rect};

/// The key of a press or auto-repeat event.
fn pressed(event: &Event) -> Option<&KeyEvent> {
    match event {
        Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            Some(key)
        }
        _ => None,
    }
}

/// Whether `event` types `character` as text, which excludes Control and Alt chords.
fn types_character(event: &Event, character: char) -> bool {
    pressed(event).is_some_and(|key| {
        key.code == KeyCode::Char(character)
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    })
}

/// Whether `event` presses `code` without modifiers.
fn presses_plain(event: &Event, code: KeyCode) -> bool {
    pressed(event).is_some_and(|key| key.code == code && key.modifiers.is_empty())
}

pub(super) fn is_actions_trigger(event: &Event) -> bool {
    types_character(event, '/')
}

pub(super) fn is_file_finder_trigger(event: &Event) -> bool {
    types_character(event, '@')
}

pub(super) fn is_skill_picker_trigger(event: &Event) -> bool {
    types_character(event, '$')
}

pub(super) fn is_picker_navigation(event: &Event) -> bool {
    pressed(event).is_some_and(|key| {
        matches!(
            key.code,
            KeyCode::Enter | KeyCode::Tab | KeyCode::Up | KeyCode::Down | KeyCode::Esc
        )
    })
}

pub(super) fn is_mention_edit(event: &Event) -> bool {
    match event {
        Event::Paste(_) => true,
        _ => pressed(event).is_some_and(|key| {
            key.code == KeyCode::Backspace
                || matches!(key.code, KeyCode::Char(_))
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        }),
    }
}

pub(super) fn mention_edit_continues_query(event: &Event, valid: fn(char) -> bool) -> bool {
    match event {
        Event::Key(key) if key.code == KeyCode::Backspace => true,
        Event::Key(key) => {
            matches!(key.code, KeyCode::Char(character) if valid(character))
        }
        Event::Paste(text) => text.chars().all(valid),
        _ => false,
    }
}

pub(super) fn is_file_query_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
}

pub(super) fn is_skill_query_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '-'
}

pub(super) fn is_queue_shortcut(event: &Event) -> bool {
    pressed(event).is_some_and(|key| key.code == KeyCode::BackTab)
}

pub(super) fn is_focus_toggle(event: &Event) -> bool {
    pressed(event).is_some_and(|key| matches!(key.code, KeyCode::Tab | KeyCode::BackTab))
}

pub(super) fn is_left_click_in(event: &Event, area: Rect) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left)
            && area.contains(Position::new(mouse.column, mouse.row))
    )
}

pub(super) fn is_left_click(event: &Event) -> bool {
    matches!(
        event,
        Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left)
    )
}

pub(super) fn is_control_c(event: &Event) -> bool {
    is_control_key(event, 'c')
}

/// An auto-repeat of a key that asks for confirmation, which must not count as the confirming
/// second press.
pub(super) fn is_confirmation_key_repeat(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Repeat)
        && (is_control_c(event) || is_escape(event))
}

pub(super) fn is_key_release(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release)
}

pub(super) fn is_control_key(event: &Event, character: char) -> bool {
    pressed(event).is_some_and(|key| {
        key.code == KeyCode::Char(character) && key.modifiers.contains(KeyModifiers::CONTROL)
    })
}

pub(super) fn is_escape(event: &Event) -> bool {
    presses_plain(event, KeyCode::Esc)
}

pub(super) fn is_plain_enter(event: &Event) -> bool {
    presses_plain(event, KeyCode::Enter)
}
