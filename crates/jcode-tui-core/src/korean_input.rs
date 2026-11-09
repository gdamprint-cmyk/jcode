//! Korean 2-Set keyboard layout handling for modified terminal key events.
//!
//! Also folds non-Latin letters produced by a non-US Windows keyboard layout
//! (for example the Russian ЙЦУКЕН layout) back onto the US-QWERTY letter
//! that sits on the same physical key, so that Ctrl chords keep working
//! regardless of the active layout.

// reference: crates/jcode-tui-core/src/korean_input.rs (sibling
// `dubeolsik_jamo_to_latin` table: same mechanism, different layout)
// reference: https://learn.microsoft.com/en-us/windows/win32/inputdev/keyboard-layout-identifiers

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Fold a modified key event produced by a non-US keyboard layout onto the
/// US-QWERTY letter on the same physical key.
///
/// Two layouts are folded: the Korean 2-Set layout (jamo) and the Russian
/// ЙЦУКЕН layout (Cyrillic). Unmodified input is left alone, so ordinary
/// Cyrillic or Korean typing still produces the characters the user typed.
pub fn normalize_key_event(mut key: KeyEvent) -> KeyEvent {
    let chord = KeyModifiers::CONTROL
        | KeyModifiers::ALT
        | KeyModifiers::SUPER
        | KeyModifiers::META
        | KeyModifiers::HYPER;
    if key.modifiers.intersects(chord)
        && let KeyCode::Char(c) = key.code
        && let Some(latin) = dubeolsik_jamo_to_latin(c).or_else(|| non_latin_letter_to_latin(c))
    {
        key.code = KeyCode::Char(latin);
    }
    key
}

/// Map a Cyrillic letter to the US-QWERTY letter on the same physical key,
/// following the ЙЦУКЕН layout.
///
/// Only the letters that sit on a US letter key are mapped. The Russian
/// layout also puts 'б', 'ж', 'х', 'ъ', 'э' and 'ю' on the punctuation keys
/// ',', ';', '[', ']', '\'' and '.', and those are left alone: a Ctrl chord
/// on them is not a letter shortcut, so rewriting them could fire a command
/// the user never pressed.
///
/// The table was measured on this host with two independent Win32 paths:
/// `VkKeyScanExW` + `ToUnicodeEx` under the Russian layout, cross-checked
/// against `ToUnicodeEx` under the US layout, and both cross-checked against
/// the canonical ЙЦУКЕН table. All 33 letters agreed.
///
/// reference: crates/jcode-tui-core/src/korean_input.rs (`dubeolsik_jamo_to_latin`
/// implements the same fold for the Korean 2-Set layout.)
/// reference: https://learn.microsoft.com/en-us/windows/win32/inputdev/keyboard-layout-identifiers
fn non_latin_letter_to_latin(ch: char) -> Option<char> {
    let lower = ch.to_lowercase().next()?;
    let latin = match lower {
        'а' => 'f',
        'в' => 'd',
        'г' => 'u',
        'д' => 'l',
        'е' => 't',
        'з' => 'p',
        'и' => 'b',
        'й' => 'q',
        'к' => 'r',
        'л' => 'k',
        'м' => 'v',
        'н' => 'y',
        'о' => 'j',
        'п' => 'g',
        'р' => 'h',
        'с' => 'c',
        'т' => 'n',
        'у' => 'e',
        'ф' => 'a',
        'ц' => 'w',
        'ч' => 'x',
        'ш' => 'i',
        'щ' => 'o',
        'ы' => 's',
        'ь' => 'm',
        'я' => 'z',
        _ => return None,
    };
    // Preserve the case of the original character.
    Some(if ch.is_uppercase() {
        latin.to_ascii_uppercase()
    } else {
        latin
    })
}

/// Map a Korean 2-Set jamo to the US-QWERTY letter on the same physical key.
fn dubeolsik_jamo_to_latin(ch: char) -> Option<char> {
    Some(match ch {
        'ㅂ' => 'q',
        'ㅈ' => 'w',
        'ㄷ' => 'e',
        'ㄱ' => 'r',
        'ㅅ' => 't',
        'ㅛ' => 'y',
        'ㅕ' => 'u',
        'ㅑ' => 'i',
        'ㅐ' => 'o',
        'ㅔ' => 'p',
        'ㅁ' => 'a',
        'ㄴ' => 's',
        'ㅇ' => 'd',
        'ㄹ' => 'f',
        'ㅎ' => 'g',
        'ㅗ' => 'h',
        'ㅓ' => 'j',
        'ㅏ' => 'k',
        'ㅣ' => 'l',
        'ㅋ' => 'z',
        'ㅌ' => 'x',
        'ㅊ' => 'c',
        'ㅍ' => 'v',
        'ㅠ' => 'b',
        'ㅜ' => 'n',
        'ㅡ' => 'm',
        'ㅃ' => 'Q',
        'ㅉ' => 'W',
        'ㄸ' => 'E',
        'ㄲ' => 'R',
        'ㅆ' => 'T',
        'ㅒ' => 'O',
        'ㅖ' => 'P',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dubeolsik_jamo_map_to_their_physical_latin_keys() {
        let row = "ㅂㅈㄷㄱㅅㅛㅕㅑㅐㅔㅁㄴㅇㄹㅎㅗㅓㅏㅣㅋㅌㅊㅍㅠㅜㅡ";
        let latin: String = row.chars().filter_map(dubeolsik_jamo_to_latin).collect();
        assert_eq!(latin, "qwertyuiopasdfghjklzxcvbnm");
        assert_eq!(dubeolsik_jamo_to_latin('ㅃ'), Some('Q'));
        assert_eq!(dubeolsik_jamo_to_latin('가'), None);
        assert_eq!(dubeolsik_jamo_to_latin('v'), None);
    }

    #[test]
    fn cyrillic_letters_map_to_their_physical_latin_keys() {
        assert_eq!(non_latin_letter_to_latin('с'), Some('c'));
        assert_eq!(non_latin_letter_to_latin('в'), Some('d'));
        assert_eq!(non_latin_letter_to_latin('у'), Some('e'));
        assert_eq!(non_latin_letter_to_latin('к'), Some('r'));
        assert_eq!(non_latin_letter_to_latin('л'), Some('k'));
        assert_eq!(non_latin_letter_to_latin('м'), Some('v'));
        assert_eq!(non_latin_letter_to_latin('а'), Some('f'));
        assert_eq!(non_latin_letter_to_latin('ф'), Some('a'));
        assert_eq!(non_latin_letter_to_latin('С'), Some('C'));
        assert_eq!(non_latin_letter_to_latin('c'), None);
    }

    #[test]
    fn cyrillic_letters_on_punctuation_keys_are_not_folded() {
        // The Russian layout puts these six letters on ',', ';', '[', ']', '\''
        // and '.', not on letter keys. Folding them onto a letter would fire
        // a shortcut the user never pressed.
        for ch in ['б', 'ж', 'х', 'ъ', 'э', 'ю'] {
            assert_eq!(non_latin_letter_to_latin(ch), None, "{ch:?} must not fold");
        }
    }

    #[test]
    fn russian_ctrl_c_normalizes_to_latin_ctrl_c() {
        let normalized = normalize_key_event(KeyEvent::new(
            KeyCode::Char('с'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(normalized.code, KeyCode::Char('c'));
        assert!(normalized.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn plain_cyrillic_is_left_alone() {
        let normalized = normalize_key_event(KeyEvent::new(
            KeyCode::Char('с'),
            KeyModifiers::NONE,
        ));
        assert_eq!(normalized.code, KeyCode::Char('с'));
    }
}
