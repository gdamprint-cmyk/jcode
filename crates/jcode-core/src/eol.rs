//! Line-ending handling for text files.
//!
//! Models emit LF. Windows working trees are CRLF. Writing the model's bytes
//! verbatim turns every write into a whole-file diff and makes a multi-line
//! `old_string` fail to match, so every file write normalizes the content to
//! the ending the file already uses.

use std::path::Path;

/// The line terminator a text file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// Unix line feed.
    Lf,
    /// Windows carriage return + line feed.
    Crlf,
}

impl LineEnding {
    /// The bytes this ending writes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::Crlf => "\r\n",
        }
    }
}

/// The dominant line ending of `content`.
///
/// CRLF wins only when more terminators are CRLF than bare LF; content without
/// any terminator is LF.
pub fn detect_line_ending(content: &str) -> LineEnding {
    let crlf = content.matches("\r\n").count();
    let bare_lf = content.matches('\n').count() - crlf;
    if crlf > bare_lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Rewrite every line terminator in `content` to `ending`.
///
/// A lone `\r` is content, not a terminator, so it is left alone.
pub fn normalize_line_endings(content: &str, ending: LineEnding) -> String {
    let unified = content.replace("\r\n", "\n");
    match ending {
        LineEnding::Lf => unified,
        LineEnding::Crlf => unified.replace('\n', "\r\n"),
    }
}

/// The ending a NEW file at `path` gets.
///
/// Git stores LF, so LF is the default for everything. Windows script
/// interpreters are the exception: `cmd.exe` and PowerShell read CRLF
/// reliably, so those extensions get CRLF.
pub fn default_line_ending(path: &Path) -> LineEnding {
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "bat" | "cmd" | "ps1" | "psm1" => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// The ending a file at `path` must use: its own dominant ending, or the
/// default for a new file.
///
/// A Windows script keeps CRLF even when its contents say otherwise: a
/// one-line `.bat` has no terminator to detect, and `cmd.exe` reads a bare
/// LF-terminated batch file unreliably.
pub fn resolve_line_ending(path: &Path, existing: Option<&str>) -> LineEnding {
    let required = default_line_ending(path);
    if required == LineEnding::Crlf {
        return LineEnding::Crlf;
    }
    existing.map(detect_line_ending).unwrap_or(LineEnding::Lf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_prefers_the_dominant_terminator() {
        assert_eq!(detect_line_ending("a\nb\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("a\r\nb\r\n"), LineEnding::Crlf);
        assert_eq!(detect_line_ending("a\r\nb\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("no terminator"), LineEnding::Lf);
        assert_eq!(detect_line_ending(""), LineEnding::Lf);
    }

    #[test]
    fn normalize_rewrites_every_terminator() {
        assert_eq!(normalize_line_endings("a\r\nb\n", LineEnding::Lf), "a\nb\n");
        assert_eq!(
            normalize_line_endings("a\nb\r\n", LineEnding::Crlf),
            "a\r\nb\r\n"
        );
        assert_eq!(normalize_line_endings("a\r\nb\n", LineEnding::Lf), "a\nb\n");
    }

    #[test]
    fn normalize_leaves_a_lone_carriage_return_as_content() {
        assert_eq!(normalize_line_endings("a\r\nb\r", LineEnding::Lf), "a\nb\r");
    }

    #[test]
    fn new_files_default_to_lf_except_windows_scripts() {
        assert_eq!(default_line_ending(Path::new("a/b.rs")), LineEnding::Lf);
        assert_eq!(default_line_ending(Path::new("a/b.md")), LineEnding::Lf);
        assert_eq!(default_line_ending(Path::new("a/b.bat")), LineEnding::Crlf);
        assert_eq!(default_line_ending(Path::new("a/b.PS1")), LineEnding::Crlf);
    }

    #[test]
    fn an_existing_file_keeps_its_own_ending() {
        assert_eq!(
            resolve_line_ending(Path::new("a/b.rs"), Some("a\r\nb\r\n")),
            LineEnding::Crlf
        );
        assert_eq!(
            resolve_line_ending(Path::new("a/b.rs"), Some("a\nb\n")),
            LineEnding::Lf
        );
        assert_eq!(
            resolve_line_ending(Path::new("a/b.rs"), None),
            LineEnding::Lf
        );
    }

    #[test]
    fn a_windows_script_keeps_crlf_even_without_a_terminator() {
        // A one-line .bat has no terminator to detect, and cmd.exe reads a
        // bare LF-terminated batch file unreliably.
        assert_eq!(
            resolve_line_ending(Path::new("a/run.bat"), Some("echo hi")),
            LineEnding::Crlf
        );
        assert_eq!(
            resolve_line_ending(Path::new("a/run.bat"), Some("echo hi\n")),
            LineEnding::Crlf
        );
        assert_eq!(
            resolve_line_ending(Path::new("a/run.ps1"), Some("Write-Host 1\n")),
            LineEnding::Crlf
        );
    }

    #[test]
    fn a_mixed_file_resolves_to_its_dominant_ending() {
        assert_eq!(
            resolve_line_ending(Path::new("a/b.rs"), Some("a\r\nb\ng\r\n")),
            LineEnding::Crlf
        );
        assert_eq!(
            resolve_line_ending(Path::new("a/b.rs"), Some("a\nb\r\ng\n")),
            LineEnding::Lf
        );
    }

    #[test]
    fn a_utf8_bom_is_content_and_survives_normalization() {
        let with_bom = "\u{feff}alpha\r\nbeta\r\n";
        assert_eq!(
            resolve_line_ending(Path::new("a/b.rs"), Some(with_bom)),
            LineEnding::Crlf
        );
        // The BOM stays at the front, and only the terminators change.
        let normalized = normalize_line_endings(with_bom, LineEnding::Lf);
        assert_eq!(normalized, "\u{feff}alpha\nbeta\n");
        assert!(normalized.starts_with('\u{feff}'));
    }
}
