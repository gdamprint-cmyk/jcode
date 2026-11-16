// reference: crates/jcode-app-core/src/tool/plan_declaration_gate_tests.rs - the
// sibling test module for a per-session tool gate, same wiring via
// `#[path = ...] mod ...` plus `#[cfg(test)]`.
// reference: ~/.jcode/decisions/reference_first.py - `CASES`, the decision
// matrix this Rust matrix mirrors.
//
// The gate reads `crate::config::config()`, which is process-global. Tests that
// need a non-default config reload it into the global cache explicitly; the
// config cache checks `cfg!(test)` and reloads every call, so no lock is held
// between assertions.

use super::*;
use serde_json::json;

/// The `[research_gate]` section parses with `enabled = false`.
#[test]
fn research_gate_section_parses_from_toml() {
    let config: crate::config::Config =
        toml::from_str("[research_gate]\nenabled = false\nmin_bytes = 10\nmin_lines = 3\n")
            .expect("section parses");
    assert!(!config.research_gate.enabled);
    assert_eq!(config.research_gate.min_bytes, 10);
    assert_eq!(config.research_gate.min_lines, 3);
    assert_eq!(
        config.research_gate.required_tools,
        vec!["websearch", "webfetch", "web_search", "web_extract"],
        "unspecified required_tools keeps the default list"
    );
}

/// A config with no `[research_gate]` section gets the on-by-default gate.
#[test]
fn research_gate_defaults_on_when_absent() {
    let config: crate::config::Config = toml::from_str("").expect("empty config parses");
    assert!(config.research_gate.enabled);
    assert_eq!(config.research_gate.min_bytes, 2000);
    assert_eq!(config.research_gate.min_lines, 60);
}

/// 90 non-empty lines, well over both floors (2000 bytes and 60 lines).
fn substantial_content() -> String {
    (0..90)
        .map(|i| format!("    fn helper_{i}(value: u32) -> u32 {{ value.wrapping_add({i}) }}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn reset(session_id: &str) {
    research_gate::reset_research_state(session_id);
}

/// (a) substantial source write with no lookup -> blocked.
#[test]
fn substantial_source_write_without_lookup_is_blocked() {
    let session = "research-gate-a";
    reset(session);
    let payload = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "content": substantial_content(),
    });
    let reason = research_gate::should_require_research("write", &payload, session)
        .expect("a substantial source write with no lookup must be blocked");
    assert!(reason.contains("RESEARCH-FIRST GATE"), "reason: {reason}");
    assert!(reason.contains("websearch"), "reason must name the escape: {reason}");
    assert!(reason.contains("webfetch"), "reason must name the escape: {reason}");
    assert!(
        reason.len() < 1200,
        "reason is {} chars, over the ~1200 budget",
        reason.len()
    );
}

/// (b) the same write after a `websearch` -> allowed.
#[test]
fn substantial_source_write_after_websearch_is_allowed() {
    let session = "research-gate-b";
    reset(session);
    let payload = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "content": substantial_content(),
    });
    assert!(research_gate::should_require_research("write", &payload, session).is_some());
    research_gate::note_research_lookup(session, "websearch");
    assert!(
        research_gate::should_require_research("write", &payload, session).is_none(),
        "a completed websearch must clear the gate for this turn"
    );
    assert!(research_gate::research_lookup_occurred(session));
}

/// A `webfetch` and the Hermes aliases count too.
#[test]
fn webfetch_and_aliases_clear_the_gate() {
    for tool in ["webfetch", "web_search", "web_extract"] {
        let session = format!("research-gate-alias-{tool}");
        reset(&session);
        let payload = json!({
            "file_path": "C:\\repo\\src\\widget.py",
            "content": substantial_content(),
        });
        assert!(research_gate::should_require_research("write", &payload, &session).is_some());
        research_gate::note_research_lookup(&session, tool);
        assert!(
            research_gate::should_require_research("write", &payload, &session).is_none(),
            "{tool} must count as a qualifying lookup"
        );
    }
}

/// (c) a small file -> allowed.
#[test]
fn small_source_file_is_allowed() {
    let session = "research-gate-c";
    reset(session);
    let payload = json!({
        "file_path": "C:\\repo\\src\\helper.rs",
        "content": "pub fn add(a: u32, b: u32) -> u32 { a + b }\n",
    });
    assert!(
        research_gate::should_require_research("write", &payload, session).is_none(),
        "a two-line file is not a synthesised technique"
    );
}

/// (d) prose `.md` -> allowed, however large.
#[test]
fn prose_markdown_is_allowed() {
    let session = "research-gate-d";
    reset(session);
    let payload = json!({
        "file_path": "C:\\repo\\NOTES.md",
        "content": substantial_content(),
    });
    assert!(research_gate::should_require_research("write", &payload, session).is_none());
}

/// (e) a `.ps1` -> allowed.
#[test]
fn powershell_is_allowed() {
    let session = "research-gate-e";
    reset(session);
    let payload = json!({
        "file_path": "C:\\repo\\build.ps1",
        "content": substantial_content(),
    });
    assert!(research_gate::should_require_research("write", &payload, session).is_none());
}

/// (f) malformed payloads fail open.
#[test]
fn malformed_payloads_fail_open() {
    let session = "research-gate-f";
    reset(session);
    let cases = vec![
        json!(["not", "an", "object"]),
        json!("a string"),
        json!(null),
        json!({}),
        json!({"file_path": "C:\\repo\\src\\widget.rs"}),
        json!({"file_path": "C:\\repo\\src\\widget.rs", "content": {"nested": true}}),
        json!({"file_path": "C:\\repo\\src\\widget.rs", "content": [1, 2, 3]}),
        json!({"file_path": ["C:\\repo\\a.rs"], "content": substantial_content()}),
        json!({"file_path": "C:\\repo\\src\\widget.rs", "edits": "not a list"}),
    ];
    for payload in cases {
        assert!(
            research_gate::should_require_research("write", &payload, session).is_none(),
            "malformed payload must fail open: {payload}"
        );
    }
}

/// (g) research state is per session.
#[test]
fn research_state_is_per_session() {
    let session_a = "research-gate-g-a";
    let session_b = "research-gate-g-b";
    reset(session_a);
    reset(session_b);
    let payload = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "content": substantial_content(),
    });
    research_gate::note_research_lookup(session_a, "websearch");
    assert!(research_gate::research_lookup_occurred(session_a));
    assert!(
        !research_gate::research_lookup_occurred(session_b),
        "a lookup in session A must not unblock session B"
    );
    assert!(research_gate::should_require_research("write", &payload, session_a).is_none());
    assert!(research_gate::should_require_research("write", &payload, session_b).is_some());
}

/// A reset clears the turn's state, so a new turn starts gated again.
#[test]
fn reset_rearms_the_gate() {
    let session = "research-gate-reset";
    reset(session);
    research_gate::note_research_lookup(session, "websearch");
    assert!(research_gate::research_lookup_occurred(session));
    reset(session);
    assert!(
        !research_gate::research_lookup_occurred(session),
        "reset must clear the turn's lookup"
    );
    let payload = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "content": substantial_content(),
    });
    assert!(research_gate::should_require_research("write", &payload, session).is_some());
}

/// A non-research tool never credits the session.
#[test]
fn non_research_tools_do_not_clear_the_gate() {
    let session = "research-gate-non-research";
    reset(session);
    research_gate::note_research_lookup(session, "read");
    research_gate::note_research_lookup(session, "agentgrep");
    assert!(!research_gate::research_lookup_occurred(session));
    let payload = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "content": substantial_content(),
    });
    assert!(research_gate::should_require_research("write", &payload, session).is_some());
}

/// Non-writing tools are never gated, even with a source payload.
#[test]
fn non_writing_tools_are_never_gated() {
    let session = "research-gate-non-writing";
    reset(session);
    let payload = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "content": substantial_content(),
    });
    for tool in ["read", "ls", "agentgrep", "bash", "mcp_call"] {
        assert!(
            research_gate::should_require_research(tool, &payload, session).is_none(),
            "{tool} must not be gated"
        );
    }
}

/// `edit` / `apply_patch` / `replace` are gated; `edit`'s `edits` list is read.
#[test]
fn edit_family_is_gated_and_edits_list_is_read() {
    let session = "research-gate-edit";
    reset(session);
    let single = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "old_string": "a",
        "new_string": substantial_content(),
    });
    assert!(research_gate::should_require_research("edit", &single, session).is_some());

    let many = json!({
        "file_path": "C:\\repo\\src\\widget.rs",
        "edits": [
            {"old_string": "a", "new_string": substantial_content()},
            {"old_string": "b", "new_string": "c"},
        ],
    });
    assert!(research_gate::should_require_research("edit", &many, session).is_some());

    for tool in ["apply_patch", "replace", "create_file", "write_file"] {
        assert!(
            research_gate::should_require_research(tool, &single, session).is_some(),
            "{tool} must be gated"
        );
    }
}

/// A source extension the gate covers, and one it does not.
#[test]
fn source_extension_coverage() {
    let session = "research-gate-ext";
    reset(session);
    for ext in [
        "rs", "py", "ts", "tsx", "go", "java", "kt", "c", "h", "cpp", "cs", "php", "rb", "dart",
        "lua", "pl",
    ] {
        let payload = json!({
            "file_path": format!("C:\\repo\\src\\file.{ext}"),
            "content": substantial_content(),
        });
        assert!(
            research_gate::should_require_research("write", &payload, session).is_some(),
            ".{ext} must be treated as source"
        );
    }
    for ext in ["json", "toml", "yaml", "yml", "csv", "log", "txt", "ini", "xml"] {
        let payload = json!({
            "file_path": format!("C:\\repo\\data\\file.{ext}"),
            "content": substantial_content(),
        });
        assert!(
            research_gate::should_require_research("write", &payload, session).is_none(),
            ".{ext} must not be treated as source"
        );
    }
    // No extension at all is not source.
    let payload = json!({"file_path": "C:\\repo\\thing", "content": substantial_content()});
    assert!(research_gate::should_require_research("write", &payload, session).is_none());
}

/// 40 lines is under the default 60-line floor, so it is allowed.
#[test]
fn forty_lines_is_under_the_default_floor() {
    let session = "research-gate-floors";
    reset(session);
    let content = (0..40)
        .map(|i| format!("var x{i} = {i};"))
        .collect::<Vec<_>>()
        .join("\n");
    let payload = json!({"file_path": "C:\\repo\\src\\file.cs", "content": content});
    assert!(
        research_gate::should_require_research("write", &payload, session).is_none(),
        "40 lines is under the default 60-line floor"
    );
    reset(session);
}

/// `enabled = false` is the kill switch; the section defaults are the floors
/// the external python gate uses, so the two agree.
#[test]
fn disabled_gate_allows_everything() {
    let gate = crate::config::ResearchGateConfig {
        enabled: false,
        ..Default::default()
    };
    assert!(!gate.enabled);
    assert_eq!(gate.min_bytes, 2000);
    assert_eq!(gate.min_lines, 60);
    assert_eq!(
        gate.required_tools,
        vec!["websearch", "webfetch", "web_search", "web_extract"]
    );
}

/// A probe, a measurement, a test, or a one-off helper is EXEMPT from the
/// research requirement, with no lookup on record.
///
/// MEASURED 2026-11-16: this gate refused a one-off measurement probe written
/// by an agent, and it had no exemption at all, while the python
/// `research_required.py` gate did - so the two disagreed about the same file.
/// The rules now live in `decisions/triviality.py`, which both python gates
/// import, and this file mirrors them.
/// reference: C:\Users\WorkShop1\.jcode\decisions\triviality.py
#[test]
fn trivial_writes_are_exempt_without_a_lookup() {
    let session = "research-gate-trivial";
    reset(session);

    // A probe under a working directory.
    let probe = json!({
        "file_path": "C:\\Users\\WorkShop1\\.jcode\\scratch\\probe_state.py",
        "content": probe_content(),
    });
    assert!(
        research_gate::should_require_research("write", &probe, session).is_none(),
        "a probe under a working directory must be exempt"
    );

    // A probe NAMED as a probe, even under a product tree.
    let named = json!({
        "file_path": "C:\\repo\\crates\\x\\probe_thing.py",
        "content": probe_content(),
    });
    assert!(
        research_gate::should_require_research("write", &named, session).is_none(),
        "a probe named as a probe must be exempt"
    );

    // A TEST file: its provenance is the module under test.
    let test = json!({
        "file_path": "C:\\repo\\crates\\x\\mod_tests.rs",
        "content": substantial_content(),
    });
    assert!(
        research_gate::should_require_research("write", &test, session).is_none(),
        "a test file must be exempt"
    );

    // A test file under a tests/ directory.
    let under_tests = json!({
        "file_path": "C:\\repo\\tests\\thing.rs",
        "content": substantial_content(),
    });
    assert!(
        research_gate::should_require_research("write", &under_tests, session).is_none(),
        "a file under tests/ must be exempt"
    );

    // A PRODUCT module with the same shape is NOT exempt: the shape test must
    // not swallow real modules.
    let product = json!({
        "file_path": "C:\\repo\\crates\\x\\mod.rs",
        "content": product_content(),
    });
    assert!(
        research_gate::should_require_research("write", &product, session).is_some(),
        "a product module must still need a lookup"
    );

    // The test-name shapes the Python rule recognises, and the near-misses it
    // does NOT.
    //
    // MEASURED 2026-11-16: the first Rust version compared the whole stem, so
    // "mod_tests.rs" was not a test; a later version split on `_` and
    // over-matched, so "specs.rs" WAS a test in Rust and not in Python. The
    // two implementations are hand-mirrored, so they drift - and this is the
    // case list that makes the drift visible.
    for (name, is_test) in [
        ("mod_tests.rs", true),
        ("tests.rs", true),
        ("test_thing.rs", true),
        ("thing_spec.rs", true),
        ("mod_test.rs", true),
        ("conftest.rs", true),
        ("thing.test.rs", true),
        ("mytest.rs", false),
        ("specs.rs", false),
        ("contest.rs", false),
        ("manifest.rs", false),
    ] {
        let payload = json!({
            "file_path": format!("C:\\repo\\crates\\x\\{name}"),
            "content": substantial_content(),
        });
        let blocked = research_gate::should_require_research("write", &payload, session).is_some();
        assert_eq!(
            blocked, !is_test,
            "{name}: expected {}",
            if is_test { "exempt" } else { "gated" }
        );
    }

    reset(session);
}

/// A product module: a module docstring and typed signatures.
fn product_content() -> String {
    let mut out = String::from("//! The module docstring makes this a real module.\n");
    out.push_str("use std::sync::Arc;\n\n");
    out.push_str("/// A typed public surface.\n");
    out.push_str("pub struct Config {\n    pub name: String,\n}\n\n");
    for i in 0..90 {
        out.push_str(&format!("pub fn helper_{i}(value: u32) -> u32 {{ value + {i} }}\n"));
    }
    out
}

/// A measurement script: no module docstring, no typed signatures, mostly calls.
fn probe_content() -> String {
    let mut out = String::from("import os\nimport sys\n\n");
    for i in 0..90 {
        out.push_str(&format!("    print('probe line {i}')\n"));
    }
    out
}
