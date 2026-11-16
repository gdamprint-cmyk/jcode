// reference: in-repo prior art - `tests.rs` in this same crate (the
// `with_temp_jcode_home` harness that redirects JCODE_HOME into a temp dir) and
// `crates/jcode-build-support/src/version_gc.rs` (the GC whose losses this
// ledger replaces).
use super::*;

/// Redirect `JCODE_HOME` into a temp dir for the duration of `f`.
///
/// `JCODE_HOME` is process-global, so the lock is the crate-wide one
/// (`super::test_env_lock`) - the same lock `tests.rs` and
/// `landing_tests.rs` use, so this module cannot race them.
fn with_temp_jcode_home<T>(f: impl FnOnce() -> T) -> T {
    let _guard = super::test_env_lock();
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

/// The publication ledger is APPEND-ONLY: recording a second version must not
/// remove the first, and the history truncation must not affect it.
///
/// WHY THIS MATTERS: the manifest history truncates to 20 rows
/// (`BuildManifest::add_to_history`) and `builds/versions/` is garbage
/// collected by the daily version GC. Measured 2026-11-16 on this host: a
/// landing that restarted the daemon ran that GC, which removed 9 version
/// directories and evicted 2 more rows from the truncated history, so four
/// Harbor queue entries from other sessions lost their ONLY publication proof
/// and the acceptance checker reported 8 failures for fixes that were
/// demonstrably live.
#[test]
fn publication_ledger_keeps_every_version() {
    with_temp_jcode_home(|| {
        record_publication(&PublicationRecord {
            version: "aaaa1111-dirty-000000000000".to_string(),
            source_fingerprint: "fp-one".to_string(),
            published_at: Utc::now(),
        })
        .expect("record first publication");
        record_publication(&PublicationRecord {
            version: "bbbb2222-dirty-111111111111".to_string(),
            source_fingerprint: "fp-two".to_string(),
            published_at: Utc::now(),
        })
        .expect("record second publication");

        let labels = published_versions_from_ledger();
        assert!(
            labels.contains(&"aaaa1111-dirty-000000000000".to_string()),
            "the ledger must keep the FIRST publication, an append-only ledger \
             never drops an earlier row: {labels:?}"
        );
        assert!(
            labels.contains(&"bbbb2222-dirty-111111111111".to_string()),
            "the ledger must contain the second publication: {labels:?}"
        );
    });
}

/// The NEGATIVE CONTROL: a malformed line must NOT make the whole ledger
/// unreadable, and must NOT be counted as a published version.
///
/// Without this case the test above could pass while the reader silently
/// returned an empty list on a corrupted ledger - the failure mode this test
/// exists to rule out.
#[test]
fn publication_ledger_skips_malformed_lines() {
    with_temp_jcode_home(|| {
        record_publication(&PublicationRecord {
            version: "cccc3333-dirty-222222222222".to_string(),
            source_fingerprint: "fp-three".to_string(),
            published_at: Utc::now(),
        })
        .expect("record publication");

        // Append a truncated line, as a crash mid-write would leave.
        let path = publication_ledger_path().expect("ledger path");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open ledger");
        use std::io::Write;
        writeln!(file, "{{\"version\": \"truncated").expect("append broken line");

        let labels = published_versions_from_ledger();
        assert!(
            labels.contains(&"cccc3333-dirty-222222222222".to_string()),
            "a malformed line must not hide the valid publications: {labels:?}"
        );
        assert!(
            !labels.iter().any(|l| l == "truncated"),
            "a malformed line must never be counted as a published version: {labels:?}"
        );
    });
}

/// The ledger path lives in the builds directory, which is where the version
/// directories and the manifest live - so all three are resolved from the same
/// root and cannot drift apart when JCODE_HOME changes.
#[test]
fn publication_ledger_lives_in_the_builds_directory() {
    with_temp_jcode_home(|| {
        let ledger = publication_ledger_path().expect("ledger path");
        let builds = builds_dir().expect("builds dir");
        assert_eq!(
            ledger.parent(),
            Some(builds.as_path()),
            "the ledger must live beside the manifest and the version directories"
        );
        assert_eq!(
            ledger.file_name().and_then(|n| n.to_str()),
            Some("published-versions.jsonl"),
            "the ledger is a JSON-lines file"
        );
    });
}

/// Installing a version MUST record a publication, whatever path installed it.
///
/// WHY THIS MATTERS: the ledger used to be written only by
/// `stage_local_current_build_for_source_with`, so a version installed by any
/// other path - `/rebuild` (`install_local_release`), the release updater
/// (`update.rs`), `landing::stage_build` - left NO durable trace. Measured
/// 2026-11-16 on this host: `ac6859a22-dirty-a2b2105c06c0` was provable ONLY
/// by its own garbage-collectible directory, in none of the manifest history,
/// the ledger or the build-request records.
///
/// The write now lives in `install_binary_at_version`, the single point where
/// a version directory is created. This test installs a version DIRECTLY (not
/// through the staging path) and requires a ledger row for it.
#[test]
fn installing_a_version_records_a_publication() {
    with_temp_jcode_home(|| {
        let version = "dddd4444-dirty-333333333333";
        let source = std::env::current_exe().expect("current exe");
        install_binary_at_version(&source, version).expect("install version");

        let labels = published_versions_from_ledger();
        assert!(
            labels.contains(&version.to_string()),
            "installing a version must record its publication in the durable \
             ledger, otherwise a version published by /rebuild or the release \
             updater leaves no durable trace: {labels:?}"
        );
    });
}

/// The NEGATIVE CONTROL for the test above: the ledger must record the version
/// that was actually installed, and NOT some other label.
///
/// Without this case the test above could pass while the writer recorded the
/// wrong version (for example the source's own label instead of the installed
/// one), which is the failure mode that would make the ledger useless as a
/// publication proof.
#[test]
fn the_ledger_records_the_installed_version_not_another_one() {
    with_temp_jcode_home(|| {
        let installed = "eeee5555-dirty-444444444444";
        let source = std::env::current_exe().expect("current exe");
        install_binary_at_version(&source, installed).expect("install version");

        let labels = published_versions_from_ledger();
        assert_eq!(
            labels.len(),
            1,
            "one installation must produce exactly one ledger row: {labels:?}"
        );
        assert_eq!(
            labels[0], installed,
            "the ledger must name the INSTALLED version: {labels:?}"
        );
    });
}

/// The fingerprint recorded for a published version must be the build's own
/// fingerprint, taken from the sidecar the build wrote next to the binary.
///
/// The sidecar is only trusted when it describes THIS version: a stale sidecar
/// left behind by an earlier build must never be recorded against a different
/// label.
#[test]
fn the_ledger_records_the_fingerprint_of_the_installed_build() {
    with_temp_jcode_home(|| {
        let version = "ffff6666-dirty-555555555555";
        let source_dir = tempfile::tempdir().expect("tempdir");
        let source = source_dir.path().join(binary_name());
        std::fs::write(&source, b"fake binary").expect("write fake binary");

        // A sidecar describing a DIFFERENT version must be ignored.
        write_dev_binary_source_metadata(
            &source,
            &SourceState {
                repo_scope: "repo".to_string(),
                worktree_scope: "worktree".to_string(),
                short_hash: "ffff6666".to_string(),
                full_hash: "ffff6666aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                dirty: true,
                fingerprint: "aaaaaaaaaaaaaaaa".to_string(),
                version_label: "some-other-version".to_string(),
                changed_paths: 0,
            },
        )
        .expect("write stale sidecar");

        install_binary_at_version(&source, version).expect("install version");

        let labels = published_versions_from_ledger();
        assert!(
            labels.contains(&version.to_string()),
            "the installation must be recorded: {labels:?}"
        );

        // With a stale sidecar the fingerprint must fall back to the label's own
        // fingerprint, never to the stale sidecar's value.
        let path = publication_ledger_path().expect("ledger path");
        let content = std::fs::read_to_string(&path).expect("read ledger");
        assert!(
            !content.contains("aaaaaaaaaaaaaaaa"),
            "a stale sidecar's fingerprint must never be recorded: {content}"
        );
    });
}
