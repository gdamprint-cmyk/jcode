// reference: in-repo prior art - `crates/jcode-build-support/src/landing_tests.rs`
// (the same `with_temp_jcode_home` harness and the real channel helpers,
// `update_current_symlink` / `update_shared_server_symlink`, asserted against
// real filesystem state) and `crates/jcode-build-support/src/tests.rs` (the
// `create_git_repo_fixture` git repo fixture).
//
// reference: https://docs.rs/tempfile/latest/tempfile/struct.TempDir.html
// (read 2026-11-15) - "A directory in the filesystem that is automatically
// deleted when it goes out of scope"; `TempDir::path()` returns the path.
//
// These tests pin the BUILD half of the build/landing split:
//
// 1. a build STAGES a version and moves NO channel (`current`, the launcher,
//    `shared-server`),
// 2. a tree that moves during the compile is reported as SUPERSEDED, not as a
//    failure.
use super::*;
use std::path::PathBuf;

fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    // `JCODE_HOME` is process-global, so it is shared with the other test
    // modules in this crate.
    super::test_env_lock()
}

/// Run `f` with `JCODE_HOME` pointed at a fresh temp directory.
fn with_temp_jcode_home<T>(f: impl FnOnce() -> T) -> T {
    let _guard = test_env_lock();
    let temp_home = tempfile::tempdir().expect("tempdir");
    let prev_home = std::env::var_os("JCODE_HOME");
    jcode_core::env::set_var("JCODE_HOME", temp_home.path());
    let result = f();
    if let Some(prev_home) = prev_home {
        jcode_core::env::set_var("JCODE_HOME", prev_home);
    } else {
        jcode_core::env::remove_var("JCODE_HOME");
    }
    result
}

/// A git repo with one commit, so `current_source_state` resolves a real
/// fingerprint and a real version label.
fn git_repo_fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init"]);
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    std::fs::write(
        path.join("Cargo.toml"),
        "[package]\nname = \"jcode\"\nversion = \"0.0.0\"\n",
    )
    .expect("Cargo.toml");
    git(&["add", "Cargo.toml"]);
    git(&["commit", "-m", "init"]);
    temp
}

/// A fake dev binary at `target/selfdev/jcode`, which the staging step finds.
fn write_fake_dev_binary(repo: &Path) -> PathBuf {
    let dir = repo.join("target").join(SELFDEV_CARGO_PROFILE);
    std::fs::create_dir_all(&dir).expect("target dir");
    let binary = dir.join(binary_name());
    std::fs::write(&binary, b"fake dev binary").expect("write dev binary");
    binary
}

/// A smoke test that reports the binary as built from `short_hash`.
fn report_for_hash(short_hash: &str) -> impl Fn(&Path) -> Result<BinaryVersionReport> {
    move |_binary: &Path| {
        Ok(BinaryVersionReport {
            version: Some(format!("v0.0.0-dev ({short_hash}, dirty)")),
            git_hash: Some(short_hash.to_string()),
        })
    }
}

/// Install a fake version binary, so a channel can point at it.
fn install_fake_version(version: &str) {
    let path = version_binary_path(version).expect("version path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    std::fs::write(&path, b"fake binary").expect("write binary");
}

/// A BUILD must STAGE the version and move NO channel.
///
/// This is the load-bearing property of the whole split: if a build moved
/// `current` or `shared-server`, a client could attach to a daemon running a
/// different build, and the landing transaction would have nothing left to
/// prove.
#[test]
fn a_build_stages_the_version_and_moves_no_channel() {
    with_temp_jcode_home(|| {
        let repo = git_repo_fixture();
        write_fake_dev_binary(repo.path());
        let source = current_source_state(repo.path()).expect("source state");
        let short_hash = source.short_hash.clone();

        // The ecosystem starts on the OLD build, on every channel.
        install_fake_version("aaa1111-dirty-2222");
        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        let published = stage_local_current_build_for_source_with(
            repo.path(),
            &source,
            report_for_hash(&short_hash),
        )
        .expect("staging succeeds");

        assert_eq!(published.version, source.version_label);
        assert!(published.versioned_path.exists());
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a build must not move the client channel"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a build must not move the daemon channel"
        );
        assert_eq!(
            read_staged_version().expect("read staged").as_deref(),
            Some(source.version_label.as_str()),
            "a build must record the landing target"
        );
    });
}

/// A tree that moves during the compile is SUPERSEDED, not FAILED.
///
/// The carrier must be able to tell "the tree moved, run me again" from "the
/// code does not compile". A drift reported as a failure would make the
/// executor treat a healthy tree as a broken build.
#[test]
fn a_source_state_mismatch_is_superseded_not_failed() {
    with_temp_jcode_home(|| {
        let repo = git_repo_fixture();
        write_fake_dev_binary(repo.path());
        let source = current_source_state(repo.path()).expect("source state");
        let short_hash = source.short_hash.clone();

        // The tree moves while the "compile" is running: the compile closure
        // writes a file, which changes the fingerprint.
        let outcome = run_staging_build_with(
            repo.path(),
            &source,
            || {
                std::fs::write(repo.path().join("drifted.txt"), b"drift").expect("write drift");
                Ok(())
            },
            |_repo, _source| panic!("a drifted build must never be staged"),
        )
        .expect("the build reports an outcome");

        match &outcome {
            DeterministicBuildOutcome::Superseded {
                expected_fingerprint,
                current_fingerprint,
                ..
            } => {
                assert_eq!(
                    *expected_fingerprint, source.fingerprint,
                    "the superseded outcome must name the fingerprint the build started from"
                );
                assert_ne!(
                    *current_fingerprint, source.fingerprint,
                    "the superseded outcome must name the fingerprint the tree moved to"
                );
            }
            other => panic!("expected a superseded outcome, got {other:?}"),
        }
        assert!(
            outcome.is_retryable(),
            "a superseded build must be reported as retryable"
        );
        assert!(!outcome.is_staged());
    });
}

/// A compile error on a tree that did NOT move is a FAILURE, not a
/// superseded build.
///
/// This is the negative control for the test above: without it, "superseded"
/// would just mean "the build did not succeed".
#[test]
fn a_compile_error_on_a_stable_tree_is_a_failure() {
    with_temp_jcode_home(|| {
        let repo = git_repo_fixture();
        write_fake_dev_binary(repo.path());
        let source = current_source_state(repo.path()).expect("source state");

        let outcome = run_staging_build_with(
            repo.path(),
            &source,
            || Err(anyhow::anyhow!("compiler error: unresolved import")),
            |_repo, _source| panic!("a failed compile must never be staged"),
        )
        .expect("the build reports an outcome");

        match &outcome {
            DeterministicBuildOutcome::Failed { reason } => {
                assert!(
                    reason.contains("unresolved import"),
                    "the failure must carry the compiler's reason, got {reason}"
                );
            }
            other => panic!("expected a failed outcome, got {other:?}"),
        }
        assert!(!outcome.is_retryable());
    });
}

/// A build whose binary was built from a DIFFERENT commit is SUPERSEDED, not
/// staged: the binary describes a source state that no longer exists.
#[test]
fn a_binary_built_from_another_commit_is_superseded() {
    with_temp_jcode_home(|| {
        let repo = git_repo_fixture();
        write_fake_dev_binary(repo.path());
        let source = current_source_state(repo.path()).expect("source state");

        let outcome = run_staging_build_with(
            repo.path(),
            &source,
            || Ok(()),
            |repo_dir, source| {
                stage_local_current_build_for_source_with(repo_dir, source, report_for_hash("0ldhash"))
            },
        )
        .expect("the build reports an outcome");

        assert!(
            matches!(outcome, DeterministicBuildOutcome::Superseded { .. }),
            "a binary from another commit must be superseded, got {outcome:?}"
        );
    });
}
