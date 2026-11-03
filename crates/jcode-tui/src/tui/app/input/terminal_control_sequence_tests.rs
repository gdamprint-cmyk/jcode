// reference: https://sw.kovidgoyal.net/kitty/keyboard-protocol/ (CSI u encoding)
// reference: https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf (SGR 1006 mouse reports)
use super::strip_terminal_control_sequences;
use crate::tui::app::tests::create_test_app;

/// Remnants of terminal reports must never reach the composer (#540).
#[test]
fn strips_escape_and_bare_report_remnants() {
    for (input, expected) in [
        // Full mouse report, and the bare tail left by a torn read.
        ("\x1b[<65;50;24M", ""),
        ("[<65;50;24M", ""),
        ("hi[<65;50;24Mthere", "hithere"),
        ("[<65;50;24m", ""),

        // A torn SGR mouse report whose `[` was consumed by the previous read:
        // the mouse position arrives as bare text and must not reach the
        // composer.
        ("<35;120M", ""),
        ("hi<35;120Mthere", "hithere"),
        ("<35;120m", ""),
        // A torn Kitty key report whose `ESC` was consumed by the previous read:
        // the key tail arrives as bare text.
        ("[101;1:3u", ""),
        ("hi[101;1:3u there", "hi there"),
        ("[97;1u", ""),
        // Bracketed paste markers and cursor/focus reports.
        ("[200~", ""),
        ("[201~", ""),
        ("[12;40R", ""),
        ("[1I", ""),
        ("[1O", ""),
        // 8-bit CSI introducer.
        ("\u{9b}[<65;50;24M", ""),
        // Stray C0 controls, but tabs and newlines survive.
        ("a\x07b", "ab"),
        ("a\tb\nc", "a\tb\nc"),
        // Truncated escape with no final byte: drop the remnant.
        ("\x1b[<65;5", ""),
    ] {
        assert_eq!(
            strip_terminal_control_sequences(input),
            expected,
            "input {input:?} should sanitize to {expected:?}"
        );
    }
}

/// The bare-tail detectors must not eat text a user actually typed.
#[test]
fn preserves_ordinary_text_that_looks_sgr_like() {
    for input in [
        "a < b",
        "<3",
        "<3;4",
        "x<y",
        "version 1;2u",
        "3u",
        "42",
        "u",
        "http://example.com/a?b=1;c=2",
    ] {
        assert_eq!(
            strip_terminal_control_sequences(input),
            input,
            "input {input:?} must be preserved verbatim"
        );
    }
}

/// The guard must not eat text a user actually typed. Being too aggressive
/// here is worse than missing a remnant.
#[test]
fn preserves_ordinary_bracketed_text() {
    for input in [
        "array[0]",
        "list[1] = list[2]",
        "[TODO] fix this",
        "see docs[1] and notes[2]",
        "fn f(v: Vec<u8>) -> [u8; 4]",
        "a[b]c",
        "[]",
        "[",
        "]",
        "[abc]",
        "[1]",
        "[12;40]",
        "plain text with no brackets",
        "emoji 🎉 and accents café",
        "match x { [a, b] => a + b }",
    ] {
        assert_eq!(
            strip_terminal_control_sequences(input),
            input,
            "input {input:?} must be preserved verbatim"
        );
    }
}

/// Late OSC 11 replies typed into the composer key by key (#970).
#[test]
fn strips_late_osc_color_replies() {
    use super::strip_osc_color_replies;
    let strip = |input: &str| {
        strip_osc_color_replies(input, input.len()).map(|(text, cursor)| {
            assert_eq!(cursor, text.len(), "cursor must stay at the end");
            text
        })
    };
    assert_eq!(strip("11;rgb:3030/3434/4646").as_deref(), Some(""));
    assert_eq!(strip("hi11;rgb:3030/3434/4646").as_deref(), Some("hi"));
    assert_eq!(strip("]11;rgb:30/34/46\\hello").as_deref(), Some("hello"));
    assert_eq!(
        strip("10;rgb:cdcd/d6d6/f4f411;rgb:0000/0000/0000").as_deref(),
        Some("")
    );
    assert_eq!(strip("11;rgba:ffff/ffff/ffff/ffff").as_deref(), Some(""));

    // Half-arrived replies are left alone until the last component is
    // complete, so no tail is stranded.
    for partial in ["11;rgb:", "11;rgb:3030/34", "11;rgb:3030/3434/46"] {
        assert_eq!(strip(partial), None, "{partial:?}");
    }
    // Ordinary text survives.
    for text in ["rgb:3030/3434/4646", "111;rgb:30/34/46", "use rgb(1,2,3)"] {
        assert_eq!(strip(text), None, "{text:?}");
    }

    // A cursor in the middle of the draft is remapped across the removal.
    let input = "ab11;rgb:30/34/46cd";
    assert_eq!(
        strip_osc_color_replies(input, input.len() - 2),
        Some(("abcd".to_string(), 2))
    );
}

/// Non-suspicious text must not be reallocated.
#[test]
fn borrows_when_nothing_to_strip() {
    assert!(matches!(
        strip_terminal_control_sequences("array[0] = 1"),
        std::borrow::Cow::Borrowed(_)
    ));
}

/// A mouse report delivered character by character must not reach the
/// composer as literal text.
///
/// `strip_terminal_control_sequences` runs per insertion, so no single
/// insertion ever holds the whole run and the per-string detector cannot
/// fire. The buffer-level scrub is what keeps the remnant out.
#[test]
fn per_character_mouse_report_never_reaches_the_composer() {
    // Feed the report the way the terminal does: one key event per
    // character, through the same function the key path uses.
    let mut app = create_test_app();
    for ch in "<35;120M".chars() {
        super::handle_text_input(&mut app, &ch.to_string());
    }
    assert_eq!(app.input, "", "the mouse report remnant must be scrubbed");
    assert_eq!(app.cursor_pos, 0, "the cursor must follow the scrub");

    // The same report behind real text: only the report disappears.
    let mut app = create_test_app();
    for ch in "hi<35;120M".chars() {
        super::handle_text_input(&mut app, &ch.to_string());
    }
    assert_eq!(app.input, "hi");
    assert_eq!(app.cursor_pos, 2);

    // The bracketed-paste and Kitty-report tails have the same per-character
    // shape and are caught by the same buffer-level scrub.
    for (report, expected) in [("[200~", ""), ("[101;1:3u", ""), ("<35;120m", "")] {
        let mut app = create_test_app();
        for ch in report.chars() {
            super::handle_text_input(&mut app, &ch.to_string());
        }
        assert_eq!(app.input, expected, "report {report:?}");
    }
}

/// The buffer-level scrub must never eat text the user actually typed.
#[test]
fn per_character_path_preserves_ordinary_text() {
    for text in ["array[0]", "a<1;b", "list[1] = list[2]", "[TODO] fix this"] {
        let mut app = create_test_app();
        for ch in text.chars() {
            super::handle_text_input(&mut app, &ch.to_string());
        }
        assert_eq!(app.input, text, "text {text:?} must survive verbatim");
        assert_eq!(app.cursor_pos, text.len());
    }
}

/// A paste of 5 or more lines takes the placeholder branch: only the
/// placeholder reaches the composer, and the payload is substituted back in
/// later - at expand time, or in place on a repeat paste. Neither of those
/// paths goes through `insert_input_text`, so the payload must be sanitized
/// where it is stored and wherever it is substituted, or a terminal report
/// that arrived inside a large paste reaches the model verbatim.
///
/// reference: https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf (SGR 1006 mouse report format)
#[test]
fn multi_line_paste_payload_is_sanitized_wherever_it_is_substituted() {
    // A 5-line payload whose first line carries a mouse report tail.
    let payload = "[<65;50;24M\nline2\nline3\nline4\nline5";

    // Stored payload is clean.
    let mut app = create_test_app();
    super::handle_text_paste(&mut app, payload.to_string());
    assert_eq!(app.pasted_contents.len(), 1);
    assert!(
        !app.pasted_contents[0].contains("[<65;50;24M"),
        "stored paste payload must not carry the report: {:?}",
        app.pasted_contents[0]
    );

    // The expanded text sent to the model is clean.
    let expanded = super::expand_paste_placeholders(&app, &app.input);
    assert!(
        !expanded.contains("[<65;50;24M"),
        "expanded paste content must not carry the report: {expanded:?}"
    );
    assert!(
        expanded.contains("line2") && expanded.contains("line5"),
        "the rest of the paste must survive: {expanded:?}"
    );

    // The in-place repeat-paste expansion is clean too.
    let mut app = create_test_app();
    super::handle_text_paste(&mut app, payload.to_string());
    let placeholder = app.input.clone();
    assert!(
        placeholder.contains("[pasted"),
        "expected a placeholder: {placeholder:?}"
    );
    super::handle_text_paste(&mut app, payload.to_string());
    assert!(
        !app.input.contains("[<65;50;24M"),
        "in-place expansion must not carry the report: {:?}",
        app.input
    );

    // Negative control: ordinary text in a multi-line paste survives.
    let ordinary = "array[0]\nlist[1] = list[2]\n[TODO] fix\nplain\nlines";
    let mut app = create_test_app();
    super::handle_text_paste(&mut app, ordinary.to_string());
    assert_eq!(
        app.pasted_contents[0], ordinary,
        "ordinary paste must survive verbatim"
    );
    let expanded = super::expand_paste_placeholders(&app, &app.input);
    assert!(
        expanded.contains("array[0]") && expanded.contains("[TODO] fix"),
        "ordinary paste content must survive: {expanded:?}"
    );
}
/// A focus report has NO parameter bytes: `CSI I` is just `[` + `I`. The
/// scrub grammar therefore cannot recognise it, and must NOT try to: a bare
/// `[I` / `[O` is ordinary text the user can type (a list index, a markdown
/// link, Rust array indexing).
///
/// The focus report is stopped at the source instead: on Windows the
/// focus-reporting mode is never enabled, so the terminal never sends the
/// report at all. See `FOCUS_REPORTING_ENABLED` in `crate::tui::mod`.
#[test]
fn focus_report_shape_is_ordinary_text_and_must_survive() {
    // The negative control: these are all real text a user can type, and
    // they must reach the composer verbatim, character by character.
    for text in ["[I]", "arr[I]", "x[O]", "[TODO]", "[I](https://example.com)"] {
        let mut app = create_test_app();
        for ch in text.chars() {
            super::handle_text_input(&mut app, &ch.to_string());
        }
        assert_eq!(app.input, text, "text {text:?} must survive verbatim");
        assert_eq!(app.cursor_pos, text.len());
    }
}
