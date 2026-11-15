// reference: in-repo prior art - `crates/jcode-build-support/src/tests.rs`
// (the `with_temp_jcode_home` harness that redirects `JCODE_HOME` into a
// temp dir) and `crates/jcode-build-support/src/landing.rs` (the transaction
// under test, with its own provenance declared there). These tests drive the
// REAL channel helpers (`update_current_symlink`,
// `update_shared_server_symlink`, `update_launcher_symlink_to_current`)
// against a temp home, so the assertions are about real filesystem state, not
// about a mock.
use super::landing::*;
use super::*;
use chrono::{Duration, Utc};
use std::path::PathBuf;

fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    // The lock is shared with the other test module: `JCODE_HOME` is
    // process-global, so two independent mutexes would let one module change
    // the home under the other.
    super::test_env_lock()
}

/// Run `f` with `JCODE_HOME` pointed at a fresh temp directory.
///
/// The landing state (journal, daemon identity, channels) lives under
/// `builds_dir()`, which honours `JCODE_HOME` first, so redirecting it
/// isolates the test completely from the production installation.
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

/// Install a fake "binary" for a version so the landing can find it.
///
/// The landing only checks that the version binary EXISTS; it never executes
/// it. The daemon identity is published separately, exactly as the real
/// daemon does.
fn install_fake_version(version: &str) {
    let path = version_binary_path(version).expect("version path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    std::fs::write(&path, b"fake binary").expect("write binary");
}

/// Publish an identity as if a daemon running `git_hash` had started.
fn publish_identity(git_hash: &str, started_at: chrono::DateTime<Utc>) {
    publish_daemon_identity(&DaemonIdentity {
        pid: 4242,
        exe: PathBuf::from("/fake/jcode"),
        git_hash: git_hash.to_string(),
        version: format!("v0.0.1-dev ({git_hash}, dirty)"),
        started_at,
    })
    .expect("publish identity");
}

#[test]
fn a_successful_landing_moves_both_channel_families_to_the_target() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        // The ecosystem starts on the OLD build.
        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        let mut handoff = |_target: &str| -> anyhow::Result<()> {
            // The daemon comes up on the new build and publishes its identity.
            publish_identity("bbb2222", Utc::now());
            Ok(())
        };

        let outcome = land_version_with_timeout(
            "bbb2222-dirty-3333",
            None,
            &mut handoff,
            std::time::Duration::from_secs(10),
        )
        .expect("landing runs");

        assert!(
            matches!(outcome, LandingOutcome::Landed { .. }),
            "expected a landed outcome, got {outcome:?}"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("bbb2222-dirty-3333")
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("bbb2222-dirty-3333")
        );
        // The journal is cleared: the transaction committed.
        assert!(load_landing_transaction().expect("load").is_none());
    });
}

#[test]
fn a_failed_handoff_leaves_the_client_channels_on_the_old_version() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // The handoff "succeeds" but the daemon never comes up on the target:
        // it publishes an identity for a DIFFERENT build, which is exactly
        // the case a reload command's exit code cannot detect.
        let mut handoff = |_target: &str| -> anyhow::Result<()> {
            publish_identity("ccc3333", Utc::now());
            Ok(())
        };

        let outcome = land_version_with_timeout(
            "bbb2222-dirty-3333",
            None,
            &mut handoff,
            std::time::Duration::from_millis(600),
        )
        .expect("landing runs");

        assert!(
            matches!(outcome, LandingOutcome::RolledBack { .. }),
            "expected a rolled-back outcome, got {outcome:?}"
        );
        // The CLIENT channel never moved: the client keeps the old build.
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a failed landing must leave the client on the old version"
        );
        // The daemon channel was restored too, so the two agree again.
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a rolled-back landing must restore the daemon channel"
        );
        assert!(load_landing_transaction().expect("load").is_none());
    });
}

#[test]
fn a_stale_identity_from_before_the_handoff_does_not_prove_the_target() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // The OLD daemon published its identity BEFORE the landing started,
        // and it happens to carry the target's commit (the two builds share
        // a commit but differ in the working tree). Only the timestamp
        // distinguishes it from the daemon the handoff produced.
        publish_identity("bbb2222", Utc::now() - Duration::minutes(5));

        let mut handoff = |_target: &str| -> anyhow::Result<()> {
            // The handoff does not bring up a new daemon: the stale identity
            // stays on disk.
            Ok(())
        };

        let outcome = land_version_with_timeout(
            "bbb2222-dirty-3333",
            None,
            &mut handoff,
            std::time::Duration::from_millis(600),
        )
        .expect("landing runs");

        assert!(
            matches!(outcome, LandingOutcome::RolledBack { .. }),
            "a stale identity must not be accepted as proof, got {outcome:?}"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222")
        );
    });
}

#[test]
fn recovery_rolls_back_when_the_daemon_does_not_run_the_target() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // Simulate a crash AFTER the daemon channel moved but BEFORE the
        // client channels did.
        let mut transaction = begin_landing("bbb2222-dirty-3333", None).expect("begin");
        prepare_shared_server(&mut transaction, "bbb2222-dirty-3333").expect("prepare");
        record_handoff_requested(&mut transaction).expect("handoff");
        // The daemon never came up: the identity is from a different build.
        publish_identity("ccc3333", Utc::now());

        let recovery = recover_landing().expect("recover").expect("a recovery happened");
        assert!(
            matches!(recovery, LandingRecovery::RollBack { .. }),
            "expected a rollback, got {recovery:?}"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "recovery must restore the daemon channel"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222")
        );
        assert!(load_landing_transaction().expect("load").is_none());
    });
}

#[test]
fn recovery_completes_forward_when_the_daemon_already_runs_the_target() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // Simulate a crash AFTER the client channels were activated but
        // BEFORE the journal was cleared.
        let mut transaction = begin_landing("bbb2222-dirty-3333", None).expect("begin");
        prepare_shared_server(&mut transaction, "bbb2222-dirty-3333").expect("prepare");
        record_handoff_requested(&mut transaction).expect("handoff");
        advance_landing(&mut transaction, LandingPhase::DaemonVerified, "verified").expect("advance");
        activate_client_channels(&mut transaction, "bbb2222-dirty-3333").expect("activate");
        // The daemon really does run the target.
        publish_identity("bbb2222", Utc::now());

        let recovery = recover_landing().expect("recover").expect("a recovery happened");
        assert!(
            matches!(recovery, LandingRecovery::CompleteForward { .. }),
            "expected a forward completion, got {recovery:?}"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("bbb2222-dirty-3333")
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("bbb2222-dirty-3333")
        );
        assert!(load_landing_transaction().expect("load").is_none());
    });
}

#[test]
fn a_second_landing_cannot_start_while_one_holds_the_lock() {
    with_temp_jcode_home(|| {
        let lock = LandingLock::try_acquire().expect("acquire").expect("lock acquired");
        assert!(
            LandingLock::try_acquire().expect("second acquire").is_none(),
            "a second landing must not be able to take the lock"
        );
        drop(lock);
        assert!(
            LandingLock::try_acquire().expect("third acquire").is_some(),
            "the lock must be released when the holder drops it"
        );
    });
}

#[test]
fn an_invalid_version_label_is_refused_before_anything_moves() {
    with_temp_jcode_home(|| {
        update_current_symlink("aaa1111-dirty-2222").ok();
        for bad in ["", "  ", ".", "..", "a/b", "a\\b", "C:evil"] {
            assert!(
                !is_valid_version_label(bad),
                "{bad:?} must be refused as a version label"
            );
        }
        assert!(is_valid_version_label("abc1234-dirty-deadbeef"));
    });
}

#[test]
fn staging_a_build_moves_no_channel() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");

        // The ecosystem starts on the OLD build, on every channel.
        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // A BUILD stages the new build. It must not touch any channel: the
        // client channels move only inside a landing.
        write_staged_version("bbb2222-dirty-3333").expect("stage");

        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "staging must not move the client channel"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "staging must not move the daemon channel"
        );
        assert_eq!(
            read_staged_version().expect("read staged").as_deref(),
            Some("bbb2222-dirty-3333"),
            "staging must record the landing target"
        );
    });
}

#[test]
fn the_staged_build_is_the_landing_target_not_the_current_channel() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");
        write_staged_version("bbb2222-dirty-3333").expect("stage");

        // The landing moves EVERY channel onto the staged build, so the client
        // and the daemon end up on the same build.
        let mut handoff = |_target: &str| -> anyhow::Result<()> {
            publish_identity("bbb2222", Utc::now());
            Ok(())
        };
        let outcome = land_version_with_timeout(
            "bbb2222-dirty-3333",
            None,
            &mut handoff,
            std::time::Duration::from_secs(10),
        )
        .expect("landing runs");

        assert!(matches!(outcome, LandingOutcome::Landed { .. }));
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("bbb2222-dirty-3333")
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("bbb2222-dirty-3333")
        );
    });
}
