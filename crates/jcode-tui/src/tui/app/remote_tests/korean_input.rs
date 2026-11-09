use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

use super::create_test_app;

#[test]
fn local_korean_jamo_ctrl_chord_runs_the_latin_shortcut() {
    let mut app = create_test_app();
    app.set_input_for_test("hello world again");
    app.handle_key(KeyCode::Left, KeyModifiers::CONTROL)
        .unwrap();
    app.handle_key_press_event(jcode_tui_core::korean_input::normalize_key_event(
        KeyEvent::new(KeyCode::Char('ㅏ'), KeyModifiers::CONTROL),
    ))
    .unwrap();
    assert_eq!(app.input(), "hello world ");
}

/// A Windows Russian-layout keyboard (ЙЦУКЕН, layout id 00000419) reports
/// Ctrl+C as the Cyrillic letter 'с' (U+0441) on the physical C key, not the
/// Latin 'c'. crossterm turns that Win32 KEY_EVENT into KeyCode::Char('с'),
/// so the global Ctrl+C handler never sees a Latin 'c' and the running turn is
/// not interrupted.
///
/// reference: https://learn.microsoft.com/en-us/windows/win32/inputdev/keyboard-layout-identifiers
#[test]
fn russian_layout_ctrl_c_interrupts_the_running_turn() {
    let mut app = create_test_app();
    app.is_processing = true;

    app.handle_key_press_event(jcode_tui_core::korean_input::normalize_key_event(
        KeyEvent::new(KeyCode::Char('\u{0441}'), KeyModifiers::CONTROL),
    ))
    .unwrap();

    assert!(
        app.cancel_requested,
        "Ctrl+C under the Russian layout must interrupt the running turn"
    );
}

/// The same fold must apply to every other letter the Russian layout puts on
/// a US letter key, so Ctrl+R, Ctrl+L and friends keep working.
#[test]
fn russian_layout_ctrl_letters_map_to_their_physical_key() {
    for (cyrillic, latin) in [
        ('\u{0440}', 'h'), // р -> H
        ('\u{0434}', 'l'), // д -> L
        ('\u{043A}', 'r'), // к -> R
        ('\u{043B}', 'k'), // л -> K
        ('\u{0443}', 'e'), // у -> E
        ('\u{0444}', 'a'), // ф -> A
        ('\u{0430}', 'f'), // а -> F
    ] {
        let folded = jcode_tui_core::korean_input::normalize_key_event(KeyEvent::new(
            KeyCode::Char(cyrillic),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(
            folded.code,
            KeyCode::Char(latin),
            "Ctrl+{cyrillic:?} must fold onto the physical {latin:?} key"
        );
    }
}

/// Negative control: the Cyrillic 'ъ' (U+044A) sits on the physical ']' key,
/// not on any letter key, so Ctrl+ъ must not be folded onto a Latin shortcut.
#[test]
fn russian_layout_other_letters_are_not_folded_onto_shortcuts() {
    let mut app = create_test_app();

    app.handle_key_press_event(jcode_tui_core::korean_input::normalize_key_event(
        KeyEvent::new(KeyCode::Char('\u{044A}'), KeyModifiers::CONTROL),
    ))
    .unwrap();

    assert!(
        !app.cancel_requested,
        "Ctrl+ъ is the physical ']' key and must not interrupt the turn"
    );
}

#[test]
fn local_korean_jamo_cmd_chord_does_not_type_the_jamo() {
    let mut app = create_test_app();
    app.set_input_for_test("hi");
    app.handle_key_press_event(jcode_tui_core::korean_input::normalize_key_event(
        KeyEvent::new(KeyCode::Char('ㅍ'), KeyModifiers::SUPER),
    ))
    .unwrap();
    assert_eq!(app.input(), "hi");
}

#[test]
fn local_plain_korean_jamo_still_types() {
    let mut app = create_test_app();
    app.handle_key_press_event(jcode_tui_core::korean_input::normalize_key_event(
        KeyEvent::new(KeyCode::Char('ㅍ'), KeyModifiers::NONE),
    ))
    .unwrap();
    assert_eq!(app.input(), "ㅍ");
}

#[test]
fn connected_remote_korean_chord_and_plain_jamo() {
    let mut app = create_test_app();
    app.set_input_for_test("hello world again");
    app.handle_key(KeyCode::Left, KeyModifiers::CONTROL)
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();

    rt.block_on(super::super::apply_terminal_event(
        &mut app,
        &mut terminal,
        &mut remote,
        Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('ㅏ'),
            KeyModifiers::CONTROL,
        )))),
    ))
    .unwrap();
    assert_eq!(app.input(), "hello world ");

    rt.block_on(super::super::apply_terminal_event(
        &mut app,
        &mut terminal,
        &mut remote,
        Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('ㅍ'),
            KeyModifiers::NONE,
        )))),
    ))
    .unwrap();
    assert_eq!(app.input(), "hello world ㅍ");
}

#[test]
fn disconnected_remote_korean_chord_and_plain_jamo() {
    let mut app = create_test_app();
    app.set_input_for_test("hello world again");
    app.handle_key(KeyCode::Left, KeyModifiers::CONTROL)
        .unwrap();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();

    super::super::handle_terminal_event_while_disconnected(
        &mut app,
        &mut terminal,
        Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('ㅏ'),
            KeyModifiers::CONTROL,
        )))),
    )
    .unwrap();
    assert_eq!(app.input(), "hello world ");

    super::super::handle_terminal_event_while_disconnected(
        &mut app,
        &mut terminal,
        Some(Ok(Event::Key(KeyEvent::new(
            KeyCode::Char('ㅍ'),
            KeyModifiers::NONE,
        )))),
    )
    .unwrap();
    assert_eq!(app.input(), "hello world ㅍ");
}
