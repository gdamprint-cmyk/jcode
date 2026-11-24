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

/// A transaction that crashed BEFORE the handoff was requested must roll
/// back, even when the daemon's identity names the target's commit.
///
/// MEASURED 2026-11-17: the git hash cannot make this call. The source
/// fingerprint that separates two builds of one commit lives only in the
/// version label, never in the binary and never in the identity, so a commit
/// match is not evidence that the daemon runs the target.
///
/// Without a handoff timestamp there is no way to tell the new daemon's
/// identity from the old daemon's leftover file, so the recovery must not
/// complete the landing forward.
#[test]
fn recovery_rolls_back_when_the_handoff_was_never_requested() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // A crash AFTER the daemon channel moved but BEFORE the handoff was
        // requested: the journal still carries no handoff timestamp.
        let mut transaction = begin_landing("bbb2222-dirty-3333", None).expect("begin");
        prepare_shared_server(&mut transaction, "bbb2222-dirty-3333").expect("prepare");
        assert!(
            transaction.handoff_requested_at.is_none(),
            "the handoff must not be recorded yet"
        );

        // The identity names the TARGET's commit - the match the old code
        // accepted on its own. It is still not proof.
        publish_identity("bbb2222", Utc::now());

        let recovery = recover_landing().expect("recover").expect("a recovery happened");
        assert!(
            matches!(recovery, LandingRecovery::RollBack { .. }),
            "an unproven daemon must never complete the landing forward, got {recovery:?}"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "recovery must restore the daemon channel"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "the client channels must stay on the old version"
        );
        assert!(load_landing_transaction().expect("load").is_none());
    });
}

/// The self-dev reload moves its channels through the landing transaction, so
/// a crash leaves a journal the next daemon can resolve.
///
/// This drives the REAL step functions the child landing runs (`begin_landing`
/// -> `prepare_shared_server`), then inspects the on-disk order: the journal
/// must exist BEFORE the daemon channel moved (otherwise a crash between them is
/// unrecoverable), and it must be gone after the client channels are activated.
#[test]
fn the_selfdev_reload_journals_before_moving_the_daemon_channel() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // begin_landing writes the journal. Nothing has moved yet.
        let mut transaction = begin_landing("bbb2222-dirty-3333", None).expect("begin");
        assert_eq!(
            load_landing_transaction().expect("load").as_ref(),
            Some(&transaction),
            "the journal must exist before any channel moves"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "beginning the landing must not move the daemon channel"
        );

        // prepare_shared_server is the FIRST channel move. It must leave the
        // journal naming the new phase, so recovery knows what to undo.
        prepare_shared_server(&mut transaction, "bbb2222-dirty-3333").expect("prepare");
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("bbb2222-dirty-3333")
        );
        let journal = load_landing_transaction().expect("load").expect("journal survives prepare");
        assert!(journal.shared_server_moved());
        assert!(!journal.clients_moved());

        // A crash here is recoverable: the journal exists, so the next daemon
        // start resolves forward or backward instead of finding a bare split.
        assert!(landing_journal_path().expect("path").exists());

        // Success clears the journal AFTER the client channels moved.
        record_handoff_requested(&mut transaction).expect("handoff");
        advance_landing(&mut transaction, LandingPhase::DaemonVerified, "verified").expect("advance");
        activate_client_channels(&mut transaction, "bbb2222-dirty-3333").expect("activate");
        clear_landing_transaction().expect("clear");
        assert!(load_landing_transaction().expect("load").is_none());
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("bbb2222-dirty-3333")
        );
    });
}

/// When the daemon identity cannot be proven, the client channels must NOT
/// move: a daemon that does not run the target must never drag `current` and
/// the launcher onto the target.
#[test]
fn the_client_channels_do_not_move_without_a_proven_daemon() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("bbb2222-dirty-3333");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        let mut transaction = begin_landing("bbb2222-dirty-3333", None).expect("begin");
        prepare_shared_server(&mut transaction, "bbb2222-dirty-3333").expect("prepare");
        record_handoff_requested(&mut transaction).expect("handoff");

        // The handoff "succeeded" but the daemon publishes an identity for a
        // DIFFERENT build - exactly the case a reload command's exit code
        // cannot detect.
        publish_identity("ccc3333", Utc::now());

        // This is the wait the CLI landing performs; it must fail.
        let error = wait_for_daemon_identity(
            "bbb2222-dirty-3333",
            transaction.handoff_requested_at,
            std::time::Duration::from_millis(400),
        )
        .expect_err("an unproven daemon must never satisfy the wait");
        assert!(
            error.to_string().contains("did not report running"),
            "unexpected error: {error}"
        );

        // The client channels were never touched by the failed wait.
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a failed landing must leave the client on the old version"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("bbb2222-dirty-3333"),
            "the daemon channel stays moved until rollback runs"
        );

        // Rollback restores it, using the same journal the crash path reads.
        rollback_landing(&transaction).expect("rollback");
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222")
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222")
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

/// Install a fake version whose binary carries a given build time.
///
/// The downgrade guard compares builds by their build time, which the landing
/// reads from the immutable version binary's mtime. So a test has to be able
/// to give two fake versions different times.
fn install_fake_version_at(version: &str, build_time: std::time::SystemTime) {
    let path = version_binary_path(version).expect("version path");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    std::fs::write(&path, b"fake binary").expect("write binary");
    // The mtime is set explicitly, because the guard's whole ordering rests on
    // it. A filesystem with coarse timestamp granularity would make two
    // versions written in the same second indistinguishable, so the test
    // spaces them a full day apart.
    //
    // reference: https://doc.rust-lang.org/std/fs/fn.File.html#method.set_times
    // (read 2026-11-15) - `File::set_times` "changes the access and modification
    // times of a file" and takes a `FileTimes`, which is the stdlib way to set
    // an mtime without pulling in an extra dependency.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open binary");
    file.set_times(std::fs::FileTimes::new().set_modified(build_time))
        .expect("set mtime");
}

#[test]
fn a_stale_staged_build_is_refused_as_a_downgrade() {
    with_temp_jcode_home(|| {
        let now = std::time::SystemTime::now();
        let older = now - std::time::Duration::from_secs(86_400);
        let newer = now;

        install_fake_version_at("aaa1111-dirty-2222", older);
        install_fake_version_at("bbb2222-dirty-3333", newer);

        // The ecosystem already runs the NEWER build.
        update_current_symlink("bbb2222-dirty-3333").expect("current");
        update_shared_server_symlink("bbb2222-dirty-3333").expect("shared");

        // The staged marker still names the OLDER build (it is never cleared).
        write_staged_version("aaa1111-dirty-2222").expect("stage");

        // Landing the stale staged build would move the whole ecosystem back.
        assert!(
            would_downgrade("aaa1111-dirty-2222", "bbb2222-dirty-3333").expect("compare"),
            "the older build must be recognised as a downgrade"
        );
        let error = guard_landing_target_is_not_a_downgrade("aaa1111-dirty-2222")
            .expect_err("a downgrade must be refused");
        assert!(
            error.to_string().contains("Refusing to land"),
            "unexpected error: {error}"
        );
    });
}

#[test]
fn a_newer_staged_build_is_not_a_downgrade() {
    with_temp_jcode_home(|| {
        let now = std::time::SystemTime::now();
        let older = now - std::time::Duration::from_secs(86_400);
        let newer = now;

        install_fake_version_at("aaa1111-dirty-2222", older);
        install_fake_version_at("bbb2222-dirty-3333", newer);

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");
        write_staged_version("bbb2222-dirty-3333").expect("stage");

        assert!(
            !would_downgrade("bbb2222-dirty-3333", "aaa1111-dirty-2222").expect("compare"),
            "the newer build must not be a downgrade"
        );
        guard_landing_target_is_not_a_downgrade("bbb2222-dirty-3333")
            .expect("a forward landing must be allowed");
    });
}

#[test]
fn the_live_build_is_never_a_downgrade_of_itself() {
    with_temp_jcode_home(|| {
        let now = std::time::SystemTime::now();
        install_fake_version_at("aaa1111-dirty-2222", now);
        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        assert!(!would_downgrade("aaa1111-dirty-2222", "aaa1111-dirty-2222").expect("compare"));
        guard_landing_target_is_not_a_downgrade("aaa1111-dirty-2222")
            .expect("landing the live build must be allowed");
    });
}

#[test]
fn a_release_label_is_matched_by_the_semver_the_daemon_reports() {
    // A release label carries no commit, so the daemon's REPORTED version is
    // what proves it runs the release.
    let identity = DaemonIdentity {
        pid: 4242,
        exe: PathBuf::from("/fake/jcode"),
        git_hash: "abc1234".to_string(),
        version: "v1.2.3 (abc1234)".to_string(),
        started_at: Utc::now(),
    };
    assert!(daemon_identity_matches_version(&identity, "1.2.3"));
    assert!(!daemon_identity_matches_version(&identity, "1.2.4"));
}

#[test]
fn a_dev_build_does_not_prove_a_release_label() {
    // The dev build of the same semver reports `-dev`, so it is NOT the
    // release: accepting it would let a dev daemon pass as a landed release.
    let identity = DaemonIdentity {
        pid: 4242,
        exe: PathBuf::from("/fake/jcode"),
        git_hash: "abc1234".to_string(),
        version: "v1.2.3-dev (abc1234)".to_string(),
        started_at: Utc::now(),
    };
    assert!(!daemon_identity_matches_version(&identity, "1.2.3"));
}

#[test]
fn release_and_main_labels_are_recognized() {
    assert!(is_release_version_label("1.2.3"));
    assert!(is_release_version_label("v1.0.0-preview.7"));
    assert!(!is_release_version_label("abc1234-dirty-deadbeef"));
    assert!(!is_release_version_label("main-abc1234"));

    assert!(is_main_channel_label("main-abc1234"));
    assert!(!is_main_channel_label("1.2.3"));
}

#[test]
fn a_pinned_selfdev_daemon_refuses_the_release_landing_and_moves_nothing() {
    with_temp_jcode_home(|| {
        install_fake_version("aaa1111-dirty-2222");
        install_fake_version("1.2.3");

        // The daemon channel is deliberately pinned to a self-dev build.
        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");
        update_stable_symlink("aaa1111-dirty-2222").expect("stable");

        let plan = plan_release_landing("1.2.3").expect("plan");
        assert!(
            matches!(
                plan,
                ReleaseLandingPlan::RefusedPinned {
                    ref version,
                    ref pinned_version
                } if version == "1.2.3" && pinned_version == "aaa1111-dirty-2222"
            ),
            "a pinned self-dev daemon must refuse the release landing, got {plan:?}"
        );

        // The lander must NOT even be called.
        let mut called = false;
        let error = land_release_version("1.2.3", false, &mut |_version| {
            called = true;
            Ok(LandingOutcome::Landed {
                target_version: "1.2.3".to_string(),
                previous_current_version: None,
                previous_shared_server_version: None,
            })
        })
        .expect_err("a pinned daemon must refuse the landing");
        assert!(!called, "the lander must not run when the pin refuses");
        assert!(
            error.to_string().contains("pinned"),
            "unexpected error: {error}"
        );

        // NOTHING moved: the daemon channel, the client channel and the
        // stable channel all still name the pinned build.
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a refused landing must not move the daemon channel"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a refused landing must not move the client channel"
        );
        assert_eq!(
            read_stable_version().expect("read stable").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a refused landing must not move the stable channel"
        );
    });
}

#[test]
fn an_unpinned_daemon_lands_the_release_and_moves_both_channel_families() {
    with_temp_jcode_home(|| {
        install_fake_version("1.2.3");
        install_fake_version("1.1.0");
        install_fake_version("aaa1111-dirty-2222");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("1.1.0").expect("shared");
        update_stable_symlink("1.1.0").expect("stable");

        let plan = plan_release_landing("1.2.3").expect("plan");
        assert!(
            matches!(plan, ReleaseLandingPlan::Land { .. }),
            "a release-pinned daemon channel must allow the landing, got {plan:?}"
        );

        let mut landed = None;
        land_release_version("1.2.3", false, &mut |version| {
            // The lander stands in for the landing transaction: it moves the
            // daemon channel, then the client channels, as one unit.
            update_shared_server_symlink(version).expect("shared");
            update_current_symlink(version).expect("current");
            landed = Some(version.to_string());
            Ok(LandingOutcome::Landed {
                target_version: version.to_string(),
                previous_current_version: None,
                previous_shared_server_version: None,
            })
        })
        .expect("landing succeeds");

        assert_eq!(landed.as_deref(), Some("1.2.3"));
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("1.2.3"),
            "the daemon channel must name the landed release"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("1.2.3"),
            "the client channel must name the landed release"
        );
        assert_eq!(
            read_stable_version().expect("read stable").as_deref(),
            Some("1.2.3"),
            "the stable channel must follow the landed release"
        );
    });
}

#[test]
fn a_rolled_back_landing_leaves_every_channel_on_the_previous_version() {
    with_temp_jcode_home(|| {
        install_fake_version("1.2.3");
        install_fake_version("1.1.0");
        install_fake_version("aaa1111-dirty-2222");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("1.1.0").expect("shared");
        update_stable_symlink("1.1.0").expect("stable");

        let error = land_release_version("1.2.3", false, &mut |_version| {
            // The landing rolled itself back: the daemon channel was restored
            // by the transaction, and the client channels never moved.
            Ok(LandingOutcome::RolledBack {
                target_version: "1.2.3".to_string(),
                reason: "the daemon did not come up".to_string(),
            })
        })
        .expect_err("a rolled-back landing is a failure");

        assert!(
            error.to_string().contains("rolled back"),
            "unexpected error: {error}"
        );
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("1.1.0"),
            "a rolled-back landing must leave the daemon channel alone"
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("aaa1111-dirty-2222"),
            "a rolled-back landing must leave the client channel alone"
        );
        assert_eq!(
            read_stable_version().expect("read stable").as_deref(),
            Some("1.1.0"),
            "a rolled-back landing must not advance the stable channel"
        );
    });
}

#[test]
fn the_pin_override_lets_the_main_channel_land_over_a_pinned_daemon() {
    with_temp_jcode_home(|| {
        install_fake_version("main-abc1234");
        install_fake_version("aaa1111-dirty-2222");

        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // The main channel is the development channel of the source tree the
        // user develops, so it MAY land over the pin - atomically, so there is
        // no split either way.
        let mut landed = false;
        land_release_version("main-abc1234", true, &mut |version| {
            landed = true;
            update_shared_server_symlink(version).expect("shared");
            update_current_symlink(version).expect("current");
            Ok(LandingOutcome::Landed {
                target_version: version.to_string(),
                previous_current_version: None,
                previous_shared_server_version: None,
            })
        })
        .expect("the main channel lands over the pin");

        assert!(landed, "the main channel must land over a pinned daemon");
        assert_eq!(
            read_shared_server_version().expect("read shared").as_deref(),
            Some("main-abc1234")
        );
        assert_eq!(
            read_current_version().expect("read current").as_deref(),
            Some("main-abc1234")
        );
    });
}

#[test]
fn an_unverifiable_build_is_never_treated_as_a_downgrade() {
    with_temp_jcode_home(|| {
        let now = std::time::SystemTime::now();
        install_fake_version_at("aaa1111-dirty-2222", now);
        update_current_symlink("aaa1111-dirty-2222").expect("current");
        update_shared_server_symlink("aaa1111-dirty-2222").expect("shared");

        // The candidate has no binary on disk, so its build time cannot be
        // read. An unjudgeable candidate must NOT be blocked: the guard must
        // never refuse a landing it cannot actually prove is a downgrade.
        assert!(
            !would_downgrade("ccc3333-dirty-4444", "aaa1111-dirty-2222").expect("compare"),
            "an unreadable build time must not be judged a downgrade"
        );
        guard_landing_target_is_not_a_downgrade("ccc3333-dirty-4444")
            .expect("an unverifiable candidate must be allowed through");
    });
}
