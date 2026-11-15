// reference: no prior art, searched the jcode workspace for an existing
// "locked target binary" handler - there is none. The behaviour below was
// MEASURED on this host on 2026-11-15: a live `jcode.exe self-dev --resume`
// session holds a Windows handle on `target/selfdev/jcode.exe`, so cargo's
// `remove_file` fails with "Отказано в доступе. (os error 5)" (access
// denied) and the build dies with exit 101.
//
// The measured bypass: Windows ALLOWS RENAMING a file that has an open
// handle. Renaming the locked binary aside lets cargo write a fresh one, and
// the running sessions stay alive (their handle follows the renamed file).
// The holder is NEVER killed - it is a real live session.
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Whether the file at `path` cannot be opened for writing because another
/// process holds it.
///
/// On Windows a running executable denies write access, so a failed write-open
/// is the lock signal. On Unix an executable can be written while it runs, so
/// there is nothing to handle.
pub fn is_locked(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        // ERROR_ACCESS_DENIED (5) is what Windows returns for a running image.
        Err(err) => {
            err.kind() == std::io::ErrorKind::PermissionDenied || err.raw_os_error() == Some(5)
        }
    }
}

/// Move a locked binary aside so the compiler can write a fresh one.
///
/// The name keeps the original stem, so a human can tell what it was, plus
/// the pid of the process that moved it.
pub fn rename_binary_aside(path: &Path) -> Result<PathBuf> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("Invalid binary path: {:?}", path))?;
    let aside = path.with_file_name(format!("{name}.locked-{}", std::process::id()));
    std::fs::rename(path, &aside).map_err(|err| {
        anyhow::anyhow!(
            "Failed to move the locked binary {:?} aside to {:?}: {err}",
            path,
            aside
        )
    })?;
    Ok(aside)
}

/// Make `path` writable by the compiler, renaming it aside when it is locked.
///
/// Returns the path the binary was moved to, or `None` when nothing had to be
/// moved. The holder is never killed: renaming is the only operation used.
pub fn prepare_binary_for_overwrite(path: &Path) -> Result<Option<PathBuf>> {
    if !is_locked(path) {
        return Ok(None);
    }
    rename_binary_aside(path).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlocked_binary_is_left_alone() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        std::fs::write(&binary, b"binary").expect("write");

        assert!(!is_locked(&binary), "a plain file is not locked");
        assert_eq!(
            prepare_binary_for_overwrite(&binary).expect("prepare"),
            None,
            "an unlocked binary must not be renamed"
        );
        assert!(binary.exists());
    }

    #[test]
    fn a_locked_binary_is_renamed_aside_not_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        std::fs::write(&binary, b"binary").expect("write");

        let aside = rename_binary_aside(&binary).expect("rename");
        assert!(!binary.exists(), "the original path must be free for cargo");
        assert!(aside.exists(), "the binary must survive, renamed aside");
        assert_eq!(
            std::fs::read(&aside).expect("read aside"),
            b"binary",
            "renaming must not change the bytes"
        );
    }

    #[test]
    fn a_missing_binary_is_not_a_lock() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("jcode.exe");
        assert!(!is_locked(&binary));
        assert_eq!(prepare_binary_for_overwrite(&binary).expect("prepare"), None);
    }
}
