// reference: https://sw.kovidgoyal.net/kitty/keyboard-protocol/ (CSI u encoding)
// reference: https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf (SGR 1006 mouse reports)
// reference: https://github.com/crossterm-rs/crossterm/blob/master/src/event/sys/unix/parse.rs (upstream parser that does decode these forms)

/// Strip terminal escape-sequence remnants from text headed for the composer.
///
/// Mouse reporting, bracketed paste, and focus events all arrive as CSI
/// sequences. When a read is torn mid-sequence (a fast wheel flick split across
/// two reads, a loaded machine, a slow SSH link), the terminal-event parser can
/// resynchronize partway through and hand the tail of the sequence back as
/// ordinary text, which then lands in the draft as noise like `[<65;50;24M`
/// (issue #540). Whether that happens depends on the terminal and on timing, so
/// rather than rely on the parser never mis-syncing, drop the remnants at the
/// single insertion boundary every input path shares.
///
/// Deliberately conservative: only ESC-introduced sequences and bare CSI-shaped
/// runs are removed, plus C0 control characters other than tab and newline.
///
/// A *bare* run (one whose ESC introducer was consumed by the torn read) is only
/// stripped when it looks unmistakably like a terminal report: `[`, at least one
/// parameter byte, and a final byte that terminals actually emit for the
/// sequences we enable (`M`/`m` mouse, `~` bracketed paste and special keys,
/// `R` cursor position, `I`/`O` focus). This is what keeps ordinary typed text
/// such as `array[0]`, `list[1]`, or `[TODO]` intact, since `]` and letters like
/// `O` only qualify with a preceding numeric parameter for the specific finals
/// listed. Anything else is left alone: a missed remnant is cosmetic, whereas
/// eating a user's real characters is not.
pub(in crate::tui::app) fn strip_terminal_control_sequences(
    text: &str,
) -> std::borrow::Cow<'_, str> {
    let looks_suspicious = text
        .chars()
        .any(|ch| ch == '\x1b' || ch == '\u{9b}' || (ch.is_control() && ch != '\t' && ch != '\n'));
    let has_csi_run = text.contains("\x1b[") || text.contains('\u{9b}') || {
        let bytes = text.as_bytes();
        bytes.iter().enumerate().any(|(index, byte)| {
            *byte == b'[' && bare_terminal_report_length(&bytes[index..]).is_some()
        })
    };
    // The same torn-read tail, without its introducer: an SGR mouse report. It is
    // text-shaped, so it is only detectable by its own grammar.
    let has_bare_tail = {
        let bytes = text.as_bytes();
        bytes.iter().enumerate().any(|(index, byte)| {
            *byte == b'<' && bare_sgr_mouse_tail_length(&bytes[index..]).is_some()
        })
    };
    if !looks_suspicious && !has_csi_run && !has_bare_tail {
        return std::borrow::Cow::Borrowed(text);
    }

    let bytes = text.as_bytes();
    let mut cleaned = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        // ESC-introduced sequence: skip the introducer, then the body.
        if byte == 0x1b {
            index += 1;
            if index < bytes.len() && bytes[index] == b'[' {
                match csi_run_length(&bytes[index..]) {
                    Some(len) => index += len,
                    // Unterminated: the rest is a truncated sequence, drop it.
                    None => index = bytes.len(),
                }
            } else if index < bytes.len() {
                // Two-character escape such as ESC O or ESC ].
                index += 1;
            }
            continue;
        }
        // 8-bit CSI introducer (UTF-8 encoded U+009B).
        if byte == 0xc2 && bytes.get(index + 1) == Some(&0x9b) {
            index += 2;
            if bytes.get(index) == Some(&b'[') {
                match csi_run_length(&bytes[index..]) {
                    Some(len) => index += len,
                    None => index = bytes.len(),
                }
            }
            continue;
        }
        // A bare report-shaped run left behind by a torn read.
        if byte == b'['
            && let Some(len) = bare_terminal_report_length(&bytes[index..])
        {
            index += len;
            continue;
        }
        // A bare SGR mouse-report tail left behind by a torn read: the
        // introducer and the `[` were consumed by the previous read, so only
        // `<button;column;rowM` remains.
        if byte == b'<'
            && let Some(len) = bare_sgr_mouse_tail_length(&bytes[index..])
        {
            index += len;
            continue;
        }
        // Drop stray C0 controls; keep tab and newline, which are meaningful.
        if byte < 0x20 && byte != b'\t' && byte != b'\n' {
            index += 1;
            continue;
        }
        // Copy one whole UTF-8 character.
        let char_len = text[index..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
        cleaned.push_str(&text[index..index + char_len]);
        index += char_len;
    }

    std::borrow::Cow::Owned(cleaned)
}

// reference: https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf (SGR 1006 mouse report format)
// reference: crates/jcode-tui/src/tui/app/input.rs `scrub_osc_color_replies` (the same
//   buffer-level scrub pattern: strip a complete reply out of the accumulated draft and
//   remap the cursor onto the cleaned text)
/// Length of a report-shaped run that starts at `bytes[0]`, or `None` when the
/// run could plausibly be text the user typed.
///
/// A bare report run is anchored on `[` (the CSI introducer whose ESC was
/// consumed by a torn read) or on `<` (the SGR mouse tail whose `[` was
/// consumed).
fn report_run_length(bytes: &[u8]) -> Option<usize> {
    match bytes.first()? {
        b'[' => bare_terminal_report_length(bytes),
        b'<' => bare_sgr_mouse_tail_length(bytes),
        _ => None,
    }
}

/// How far back a buffered report remnant may start and still be considered
/// part of the insertion that just happened.
const BUFFERED_REPORT_LOOKBACK: usize = 256;

/// Remove terminal-report remnants that accumulated in the composer buffer
/// across several insertions.
///
/// `strip_terminal_control_sequences` only sees one insertion at a time, so a
/// report delivered character by character (a mouse report whose `<` lands in
/// one key event and whose `35;120M` lands in later ones) is never recognized:
/// every fragment is text-shaped on its own and the whole-run detector never
/// fires. The accumulated run, however, is complete in the buffer, so it is
/// recognized there instead.
///
/// Conservative by construction: only a run that includes the text just
/// inserted is removed, and only when it starts within
/// `BUFFERED_REPORT_LOOKBACK` bytes of that insertion. Text the user typed
/// earlier and far away from the insertion point is never rewritten, and a
/// run that does not include the new text was already complete (and already
/// sanitized) before this insertion.
///
/// Returns the cleaned text and the cursor remapped onto it, or `None` when
/// nothing matched.
pub(in crate::tui::app) fn scrub_buffered_terminal_reports(
    input: &str,
    cursor: usize,
    inserted_start: usize,
) -> Option<(String, usize)> {
    let bytes = input.as_bytes();
    // Cheap pre-check: a report run always starts with `[` or `<`.
    if !bytes.contains(&b'[') && !bytes.contains(&b'<') {
        return None;
    }
    let window_start = super::floor_char_boundary(
        input,
        inserted_start.saturating_sub(BUFFERED_REPORT_LOOKBACK),
    );
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut index = window_start;
    while index < bytes.len() {
        if let Some(len) = report_run_length(&bytes[index..]) {
            let end = index + len;
            if end > inserted_start {
                ranges.push((index, end));
                index = end;
                continue;
            }
        }
        index += 1;
    }
    if ranges.is_empty() {
        return None;
    }

    let mut cleaned = String::with_capacity(input.len());
    let mut new_cursor = cursor;
    let mut last = 0usize;
    for (start, end) in ranges {
        cleaned.push_str(&input[last..start]);
        if cursor >= end {
            new_cursor -= end - start;
        } else if cursor > start {
            new_cursor -= cursor - start;
        }
        last = end;
    }
    cleaned.push_str(&input[last..]);
    Some((cleaned, new_cursor))
}

/// Length of a CSI run starting at `bytes[0]` (the `[`), including its final
/// byte, or `None` when the run is unterminated.
///
/// A CSI body is parameter bytes `0x30..=0x3f`, then intermediate bytes
/// `0x20..=0x2f`, then one final byte `0x40..=0x7e`.
fn csi_run_length(bytes: &[u8]) -> Option<usize> {
    debug_assert_eq!(bytes.first(), Some(&b'['));
    let mut index = 1usize;
    while index < bytes.len() && (0x30..=0x3f).contains(&bytes[index]) {
        index += 1;
    }
    while index < bytes.len() && (0x20..=0x2f).contains(&bytes[index]) {
        index += 1;
    }
    let final_byte = *bytes.get(index)?;
    if (0x40..=0x7e).contains(&final_byte) {
        Some(index + 1)
    } else {
        None
    }
}

/// Length of a bare (ESC-less) run that is unmistakably a terminal report, or
/// `None` when the run could plausibly be text the user typed.
///
/// Requires at least one parameter byte and one of the final bytes emitted by
/// the reporting modes jcode enables, so `array[0]` and `[TODO]` are left alone
/// while `[<65;50;24M` and `[200~` are recognized.
fn bare_terminal_report_length(bytes: &[u8]) -> Option<usize> {
    debug_assert_eq!(bytes.first(), Some(&b'['));
    let len = csi_run_length(bytes)?;
    let params = &bytes[1..len - 1];
    if params.is_empty() {
        return None;
    }
    // Mouse/paste/cursor/focus reports carry digits, `;`, `:`, and an optional
    // leading `<`. Reject anything with other parameter bytes.
    if !params.iter().all(|byte| {
        byte.is_ascii_digit() || *byte == b';' || *byte == b':' || *byte == b'<'
    }) {
        return None;
    }
    // The finals terminals emit for the reporting modes jcode enables, plus
    // `u` for the Kitty keyboard protocol's `CSI number;modifiers:event-type u`.
    const REPORT_FINALS: [u8; 7] = *b"Mm~RIOu";
    REPORT_FINALS.contains(&bytes[len - 1]).then_some(len)
}

// reference: https://invisible-island.net/xterm/ctlseqs/ctlseqs.pdf (SGR 1006 mouse report format)
/// Length of a bare SGR mouse-report tail, or `None` when the run could
/// plausibly be text the user typed.
///
/// An SGR mouse report is `CSI < button ; column ; row (M|m)`. When a read
/// is torn between the introducer and the body, the tail arrives alone: the
/// `[` may have been consumed by the previous read, leaving `<35;120M` at the
/// start of the text. The `[`-anchored detector above cannot see that run, so
/// the mouse position lands in the composer as literal symbols.
///
/// This form is unmistakable: a leading `<`, at least one digit, `;`
/// separators, and a final `M`/`m`. Ordinary text never starts with `<`
/// followed by digits and ends with `M`, so the match stays narrow.
fn bare_sgr_mouse_tail_length(bytes: &[u8]) -> Option<usize> {
    debug_assert_eq!(bytes.first(), Some(&b'<'));
    let mut index = 1usize;
    let mut saw_digit = false;
    let mut saw_separator = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_digit() {
            saw_digit = true;
            index += 1;
            continue;
        }
        if byte == b';' {
            saw_separator = true;
            index += 1;
            continue;
        }
        break;
    }
    if !saw_digit || !saw_separator {
        return None;
    }
    match bytes.get(index) {
        Some(b'M') | Some(b'm') => Some(index + 1),
        _ => None,
    }
}
