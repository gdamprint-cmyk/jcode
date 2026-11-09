// reference: no prior art; searched crates/jcode-app-core/src/tool for tests of
// "path points at a file" and for the exact-file filter; the existing suite
// covers only the directory-root case. These tests pin the two defects found
// on 2026-11-08:
//
//   1. a file-scoped search silently returned ZERO matches because the result
//      paths are relative (".\util.rs") while the caller's request is absolute,
//      and the filter compared the two strings directly;
//   2. a non-existent path failed with the raw OS error 267 ("Неверно задано
//      имя папки") because the path was used as the ripgrep working directory.
//
// The negative control is the directory-root case, which must keep working and
// must keep returning the same matches.

use super::*;
use std::fs;

/// The sibling `agentgrep_tests.rs` module keeps its helpers private, so this
/// module carries its own copies rather than depending on another test
/// module's visibility.
fn scope_test_ctx(root: &Path) -> ToolContext {
    ToolContext {
        session_id: "test".to_string(),
        message_id: "test".to_string(),
        tool_call_id: "test".to_string(),
        working_dir: Some(root.to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: super::super::ToolExecutionMode::Direct,
    }
}

fn scoped_input(query: &str, path: &str) -> AgentGrepInput {
    AgentGrepInput {
        mode: "grep".to_string(),
        query: Some(query.to_string()),
        file: None,
        terms: None,
        regex: Some(false),
        path: Some(path.to_string()),
        glob: None,
        file_type: None,
        hidden: None,
        no_ignore: None,
        max_files: None,
        max_regions: None,
        full_region: None,
        debug_plan: None,
        debug_score: None,
        paths_only: None,
    }
}

fn dir_input(query: &str) -> AgentGrepInput {
    AgentGrepInput {
        path: None,
        ..scoped_input(query, "")
    }
}

#[test]
fn grep_scoped_to_one_file_returns_that_files_matches() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wanted = temp.path().join("wanted.rs");
    let other = temp.path().join("other.rs");
    fs::write(&wanted, "fn target() { marker_fn(); }\n").expect("write wanted");
    fs::write(&other, "fn target() { marker_fn(); }\n").expect("write other");

    let output = execute_linked_agentgrep(
        &scoped_input("marker_fn", wanted.to_str().expect("path")),
        &scope_test_ctx(temp.path()),
        None,
    )
    .expect("agentgrep execute")
    .output;

    // The file-scoped search must find the matches of the requested file, and
    // must not silently report an empty result.
    assert!(
        output.contains("marker_fn"),
        "a file-scoped search must report the matches it found: {output}"
    );
    assert!(
        !output.contains("other.rs"),
        "a file-scoped search must not include other files: {output}"
    );
}

#[test]
fn grep_scoped_to_one_file_matches_the_directory_scoped_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    let wanted = temp.path().join("wanted.rs");
    fs::write(&wanted, "fn target() { marker_fn(); }\n").expect("write wanted");

    let by_file = execute_linked_agentgrep(
        &scoped_input("marker_fn", wanted.to_str().expect("path")),
        &scope_test_ctx(temp.path()),
        None,
    )
    .expect("agentgrep execute by file")
    .output;

    let by_dir = execute_linked_agentgrep(
        &dir_input("marker_fn"),
        &scope_test_ctx(temp.path()),
        None,
    )
    .expect("agentgrep execute by dir")
    .output;

    let count = |text: &str| text.matches("marker_fn").count();
    assert!(
        count(&by_file) >= 1,
        "file-scoped search found nothing: {by_file}"
    );
    assert_eq!(
        count(&by_file),
        count(&by_dir),
        "file-scoped and directory-scoped searches must agree on the same file"
    );
}

#[test]
fn grep_scoped_to_one_file_ignores_a_same_named_file_in_a_subdirectory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sub = temp.path().join("sub");
    fs::create_dir_all(&sub).expect("create sub");
    let wanted = temp.path().join("wanted.rs");
    fs::write(&wanted, "fn target() { marker_fn(); }\n").expect("write wanted");
    fs::write(sub.join("wanted.rs"), "fn target() { marker_fn(); }\n").expect("write sub");

    let output = execute_linked_agentgrep(
        &scoped_input("marker_fn", wanted.to_str().expect("path")),
        &scope_test_ctx(temp.path()),
        None,
    )
    .expect("agentgrep execute")
    .output;

    // The glob handed to ripgrep is the bare file name and it is RECURSIVE, so
    // the sibling `sub\wanted.rs` also matches the glob. Only the requested
    // file may survive the filter.
    assert!(
        output.contains("marker_fn"),
        "a file-scoped search must report the matches it found: {output}"
    );
    assert!(
        !output.contains("sub"),
        "a file-scoped search must not include a same-named file from a subdirectory: {output}"
    );
}

#[test]
fn grep_with_a_nonexistent_path_reports_the_path_instead_of_os_error_267() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing = temp.path().join("does-not-exist");

    let error = execute_linked_agentgrep(
        &scoped_input("marker_fn", missing.to_str().expect("path")),
        &scope_test_ctx(temp.path()),
        None,
    )
    .expect_err("a nonexistent path must be rejected");

    let message = error.to_string();
    assert!(
        !message.contains("os error 267"),
        "the raw OS error must not leak: {message}"
    );
    assert!(
        message.contains("does not exist") || message.contains("not a directory"),
        "the error must name the problem: {message}"
    );
}

#[test]
fn grep_with_a_file_as_the_root_reports_a_clear_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("wanted.rs");
    fs::write(&file, "fn target() { marker_fn(); }\n").expect("write wanted");

    // A file path is split into root+glob by `resolved_search_scope`, so the
    // root handed to ripgrep is the parent directory and this call succeeds.
    // What must never happen is the raw OS error.
    let result = execute_linked_agentgrep(
        &scoped_input("marker_fn", file.to_str().expect("path")),
        &scope_test_ctx(temp.path()),
        None,
    );
    match result {
        Ok(output) => {
            let text = output.output;
            assert!(
                text.contains("marker_fn"),
                "a file-scoped search must report its matches: {text}"
            );
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                !message.contains("os error 267"),
                "the raw OS error must not leak: {message}"
            );
        }
    }
}
