// reference: https://github.com/jjant/ini-edit - a syntax-preserving editor that
// takes the new lines' ending from the document instead of the writer's platform.
use super::*;
use std::path::Path;

fn eol_context(dir: &Path) -> ToolContext {
    ToolContext {
        session_id: "eol".into(),
        message_id: "m".into(),
        tool_call_id: "t".into(),
        working_dir: Some(dir.to_path_buf()),
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: super::super::ToolExecutionMode::Direct,
    }
}

#[tokio::test]
async fn edit_matches_across_line_endings_and_keeps_the_files_ending() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.rs");
    // A CRLF file on disk, an LF `old_string` from the model.
    std::fs::write(&file, "fn a() {\r\n    old();\r\n}\r\n").unwrap();
    EditTool::new()
        .execute(
            json!({
                "file_path": file.to_string_lossy(),
                "edits": [{
                    "old_string": "fn a() {\n    old();\n}",
                    "new_string": "fn a() {\n    new();\n}"
                }],
            }),
            eol_context(dir.path()),
        )
        .await
        .expect("the edit must match a CRLF file with an LF template");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"fn a() {\r\n    new();\r\n}\r\n".to_vec()
    );
}

#[tokio::test]
async fn edit_leaves_an_lf_file_in_lf() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.rs");
    std::fs::write(&file, "fn a() {\n    old();\n}\n").unwrap();
    EditTool::new()
        .execute(
            json!({
                "file_path": file.to_string_lossy(),
                "edits": [{
                    "old_string": "fn a() {\n    old();\n}",
                    "new_string": "fn a() {\n    new();\n}"
                }],
            }),
            eol_context(dir.path()),
        )
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"fn a() {\n    new();\n}\n".to_vec()
    );
}

#[tokio::test]
async fn write_keeps_the_ending_the_file_already_uses() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("f.rs");
    std::fs::write(&file, "a\r\nb\r\n").unwrap();
    super::super::write::WriteTool::new()
        .execute(
            json!({ "file_path": file.to_string_lossy(), "content": "x\ny\n" }),
            eol_context(dir.path()),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), b"x\r\ny\r\n".to_vec());
}

#[tokio::test]
async fn write_gives_a_new_file_the_default_ending() {
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("new.rs");
    super::super::write::WriteTool::new()
        .execute(
            json!({ "file_path": fresh.to_string_lossy(), "content": "x\ny\n" }),
            eol_context(dir.path()),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&fresh).unwrap(), b"x\ny\n".to_vec());

    // A Windows script interpreter needs CRLF.
    let script = dir.path().join("run.bat");
    super::super::write::WriteTool::new()
        .execute(
            json!({ "file_path": script.to_string_lossy(), "content": "echo hi\n" }),
            eol_context(dir.path()),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&script).unwrap(), b"echo hi\r\n".to_vec());
}

#[tokio::test]
async fn a_windows_script_keeps_crlf_even_when_its_content_has_none() {
    let dir = tempfile::tempdir().unwrap();
    // A one-line .bat has no terminator to detect, but cmd.exe still needs CRLF.
    let script = dir.path().join("run.bat");
    std::fs::write(&script, "echo hi").unwrap();
    super::super::write::WriteTool::new()
        .execute(
            json!({ "file_path": script.to_string_lossy(), "content": "echo bye\n" }),
            eol_context(dir.path()),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(&script).unwrap(), b"echo bye\r\n".to_vec());
}

#[tokio::test]
async fn a_move_keeps_the_destinations_own_ending() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.rs");
    let destination = dir.path().join("destination.rs");
    // The source is CRLF, the destination is LF: the move must not impose the
    // source's endings on the destination.
    std::fs::write(&source, "fn a() {\r\n    old();\r\n}\r\n").unwrap();
    std::fs::write(&destination, "fn b() {\n    keep();\n}\n").unwrap();

    super::super::apply_patch::ApplyPatchTool::new()
        .execute(
            json!({
                "patch_text": format!(
                    "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-fn a() {{\n-    old();\n-}}\n+fn a() {{\n+    new();\n+}}\n*** End Patch",
                    source.to_string_lossy(),
                    destination.to_string_lossy(),
                ),
            }),
            eol_context(dir.path()),
        )
        .await
        .expect("the move must apply");

    assert!(!source.exists(), "the source must be gone after a move");
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"fn a() {\n    new();\n}\n".to_vec(),
        "the move must carry the source's content in the destination's own LF ending"
    );
}

#[tokio::test]
async fn a_move_into_a_crlf_destination_stays_crlf() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.rs");
    let destination = dir.path().join("destination.rs");
    // The source is LF, the destination is CRLF: the destination must stay CRLF.
    std::fs::write(&source, "fn a() {\n    old();\n}\n").unwrap();
    std::fs::write(&destination, "fn b() {\r\n    keep();\r\n}\r\n").unwrap();

    super::super::apply_patch::ApplyPatchTool::new()
        .execute(
            json!({
                "patch_text": format!(
                    "*** Begin Patch\n*** Update File: {}\n*** Move to: {}\n@@\n-fn a() {{\n-    old();\n-}}\n+fn a() {{\n+    new();\n+}}\n*** End Patch",
                    source.to_string_lossy(),
                    destination.to_string_lossy(),
                ),
            }),
            eol_context(dir.path()),
        )
        .await
        .expect("the move must apply");

    assert_eq!(
        std::fs::read(&destination).unwrap(),
        b"fn a() {\r\n    new();\r\n}\r\n".to_vec(),
        "the destination must keep its own CRLF ending"
    );
}
