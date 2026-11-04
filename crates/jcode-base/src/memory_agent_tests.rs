// reference: no prior art, searched the jcode repo for an existing tool-call
// cadence / forced-refresh mechanism; the pending-memory TTL and injected-set
// bookkeeping in crates/jcode-base/src/memory/pending.rs are the closest
// prior art and are what this file's tests exercise.
use super::*;
use crate::memory::{MemoryCategory, MemoryEntry};

#[test]
fn extraction_transcript_omits_internal_system_reminders() {
    let messages = vec![
        crate::message::Message::user(
            "<system-reminder>\n# Session Context\nHardware: private\n</system-reminder>",
        ),
        crate::message::Message::user("Remember that tests use a temporary database."),
        crate::message::Message::assistant_text("Understood."),
    ];
    let transcript = build_transcript_for_extraction(&messages);
    assert!(!transcript.contains("Hardware: private"));
    assert!(transcript.contains("tests use a temporary database"));
    assert!(transcript.contains("Understood"));
}

struct TestEnv(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl TestEnv {
    fn set(values: &[(&'static str, &str)]) -> Self {
        let old = values
            .iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                crate::env::set_var(key, value);
                (*key, old)
            })
            .collect();
        Self(old)
    }
}
impl Drop for TestEnv {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => crate::env::set_var(key, value),
                None => crate::env::remove_var(key),
            }
        }
    }
}

#[tokio::test]
async fn missing_jev_access_clears_pending_without_fallback() {
    let _lock = crate::storage::lock_test_env();
    // Pin the provider: with `hindsight` selected this test would make a
    // live HTTP call instead of exercising the Jev fallback path.
    let _env = TestEnv::set(&[
        ("JCODE_MEMORY_JEV_PROVIDER", "disabled-for-test"),
        ("JCODE_MEMORY_PROVIDER", "jev"),
    ]);
    let sid = "jev-agent-no-fallback";
    memory::set_pending_memory(sid, "stale result".into(), 1);
    let (_, rx) = mpsc::channel(1);
    let mut agent = MemoryAgent::new(rx);
    agent
        .process_context(sid, &[crate::message::Message::user("current query")], false)
        .await
        .unwrap();
    assert!(!memory::has_pending_memory(sid));
}

/// An externally recalled payload (Hindsight) must survive the project
/// binding check AND the local-graph re-validation.
///
/// MEASURED 2026-11-04: both checks rejected the payload before this fix.
/// The binding check compared `binding.project_dir` against the session's
/// directory and found no binding at all on an unscoped publication, and the
/// re-validation asked the LOCAL memory graph to confirm documents that live
/// in the Hindsight bank - it cannot know about them, so it reported them as
/// absent. The symptom was silent: nothing was ever injected.
#[test]
fn external_payload_survives_project_binding_and_local_graph_check() {
    let _lock = crate::storage::lock_test_env();
    let sid = "external-memory-binding-test";
    let project = crate::storage::jcode_dir().expect("jcode dir").join("external-bind-test");
    let project_dir = project.to_string_lossy().to_string();

    // A Hindsight document id: it does NOT exist in the local memory graph.
    memory::set_pending_memory_external(
        sid,
        "# Memory\n\n- the exact value is 42".into(),
        1,
        vec!["hindsight-doc-abc".into()],
        None,
        Some(&project_dir),
    );

    let pending = memory::take_pending_memory_for_project(sid, Some(&project_dir))
        .expect("external payload must be taken for its own project");
    assert!(pending.prompt.contains("the exact value is 42"));
    assert_eq!(pending.memory_ids, vec!["hindsight-doc-abc".to_string()]);
}

/// An external payload is still BOUND to its project: a session that moved
/// to another directory must not pick it up.
#[test]
fn external_payload_is_not_taken_by_a_different_project() {
    let _lock = crate::storage::lock_test_env();
    let sid = "external-memory-wrong-project-test";
    let other = crate::storage::jcode_dir().expect("jcode dir").join("external-bind-other");
    let other_dir = other.to_string_lossy().to_string();

    memory::set_pending_memory_external(
        sid,
        "# Memory\n\n- a fact".into(),
        1,
        vec!["hindsight-doc-xyz".into()],
        None,
        Some(&other_dir),
    );

    assert!(
        memory::take_pending_memory_for_project(sid, Some("C:\\somewhere\\else")).is_none(),
        "a payload bound to another project must not be taken"
    );
}

#[test]
fn manager_without_working_dir_does_not_infer_process_project() {
    let manager = manager_for_working_dir(None);
    assert!(manager.load_project_graph().unwrap().memories.is_empty());
}

/// Exercises the real credential resolver, subscription capability check, HTTP
/// Decisions adapter, local stores, selection, and pending-injection boundary.
/// No provider calls or credentials leave this loopback fixture.
#[tokio::test]
async fn retained_recall_pipeline_uses_jev_http_without_embeddings_or_sidecar() {
    use std::io::{Read, Write};
    let _lock = crate::storage::lock_test_env();
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let _env = TestEnv::set(&[
        ("JCODE_HOME", dir.path().to_str().unwrap()),
        ("JCODE_MEMORY_JEV_PROVIDER", "jcode"),
        ("JCODE_API_KEY", "jcode_test_only_never_a_real_key"),
        ("JCODE_API_BASE", &base),
    ]);
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut decisions_seen = false;
        while !decisions_seen && Instant::now() < deadline {
            let (mut stream, _) = match listener.accept() {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            // On BSD/macOS an accepted socket can inherit the listener's nonblocking flag.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            let (header_end, length) = loop {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
                if let Some(offset) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..offset]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (offset + 4, length);
                }
            };
            while bytes.len() < header_end + length {
                let mut chunk = [0; 4096];
                let n = stream.read(&mut chunk).unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&chunk[..n]);
            }
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let response = if headers.starts_with("GET /v1/me ") {
                serde_json::json!({"capabilities":{"memory_jev":true}})
            } else {
                assert!(headers.starts_with("POST /v1/decisions "));
                let body: serde_json::Value =
                    serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap();
                assert_eq!(body["model"], "typesafe/jev-1.13");
                let state: serde_json::Value =
                    serde_json::from_str(body["state"].as_str().unwrap()).unwrap();
                let mut answers = serde_json::Map::new();
                for (id, question) in body["questions"].as_object().unwrap() {
                    assert_eq!(question["type"], "noul");
                    assert!(state["candidates"][id].get("embedding").is_none());
                    let relevant = state["candidates"][id]["content"]
                        .as_str()
                        .unwrap()
                        .contains("cargo test");
                    answers.insert(
                        id.clone(),
                        serde_json::json!({"type":"noul","noul":if relevant {0.98} else {0.01}}),
                    );
                }
                decisions_seen = true;
                serde_json::json!({"answers":answers})
            };
            let body = serde_json::to_vec(&response).unwrap();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        }
        assert!(decisions_seen, "no Decisions request reached fixture");
    });
    let manager = MemoryManager::new().with_project_dir("/jev-project");
    let id = manager
        .remember_project(MemoryEntry::new(
            MemoryCategory::Fact,
            "Run cargo test to test this Rust project.",
        ))
        .unwrap();
    manager
        .remember_project(MemoryEntry::new(
            MemoryCategory::Fact,
            "The garden has red flowers.",
        ))
        .unwrap();
    assert!(
        manager
            .list_all()
            .unwrap()
            .iter()
            .all(|m| m.embedding.is_none())
    );
    let (_, rx) = mpsc::channel(1);
    let mut agent = MemoryAgent::new(rx);
    let sid = "jev-agent-http-acceptance";
    agent.sessions.insert(
        sid.into(),
        SessionState {
            working_dir: Some("/jev-project".into()),
            ..Default::default()
        },
    );
    let disabled = agent
        .process_context(
            sid,
            &[crate::message::Message::user(
                "How do I test this Rust project?",
            )],
            false,
        )
        .await;
    disabled.unwrap();
    assert!(memory::take_pending_memory_for_project(sid, Some("/jev-project")).is_none());
    let result = manager
        .get_relevant_parallel_enabled(
            sid,
            &[crate::message::Message::user(
                "How do I test this Rust project?",
            )],
            None,
        )
        .await
        .unwrap();
    server.join().unwrap();
    memory::set_pending_memory_for_project_with_selection(
        sid,
        result.prompt.unwrap(),
        result.selected_entries.len(),
        &result.selected_entries,
        result.display_prompt,
        Some("/jev-project"),
    );
    let pending = memory::take_pending_memory_for_project(sid, Some("/jev-project"))
        .expect("judged memory ready for next turn");
    assert_eq!(pending.memory_ids, vec![id]);
    assert!(pending.prompt.contains("cargo test"));
    assert!(!pending.prompt.contains("red flowers"));
    assert!(!dir.path().join("embeddings").exists());
    memory::clear_pending_memory(sid);
    memory::clear_injected_memories(sid);
}

/// A forced context update must bypass the same-query repeat suppression.
///
/// MEASURED 2026-11-04: during long autonomous work the focused query rarely
/// changes, because tool output is stripped from it. The 30 s suppression
/// therefore swallowed every scheduled refresh and the agent ran for hours
/// with the memory it started the turn with. This test pins the bypass: the
/// SAME query must produce a second recall when forced.
#[tokio::test]
async fn forced_update_bypasses_the_same_query_suppression() {
    let _lock = crate::storage::lock_test_env();
    let _env = TestEnv::set(&[
        ("JCODE_MEMORY_JEV_PROVIDER", "disabled-for-test"),
        ("JCODE_MEMORY_PROVIDER", "jev"),
    ]);
    let sid = "forced-update-bypasses-suppression";
    let (_, rx) = mpsc::channel(4);
    let mut agent = MemoryAgent::new(rx);
    let messages = [crate::message::Message::user("a stable question")];

    agent.process_context(sid, &messages, false).await.unwrap();
    let first_check = agent.sessions.get(sid).and_then(|s| s.last_check);
    assert!(first_check.is_some(), "the first recall must run");

    // The second call with the SAME query is suppressed, so it never reaches
    // the point where `last_check` is refreshed.
    std::thread::sleep(Duration::from_millis(5));
    agent.process_context(sid, &messages, false).await.unwrap();
    let after_suppressed = agent
        .sessions
        .get(sid)
        .and_then(|s| s.last_check)
        .expect("last_check");
    assert_eq!(
        first_check,
        Some(after_suppressed),
        "a repeated identical query must be suppressed"
    );

    // The forced call must go through the suppression and refresh the check.
    std::thread::sleep(Duration::from_millis(5));
    agent.process_context(sid, &messages, true).await.unwrap();
    let after_forced = agent
        .sessions
        .get(sid)
        .and_then(|s| s.last_check)
        .expect("last_check");
    assert!(
        after_forced > after_suppressed,
        "a forced update must bypass the suppression window: \
         {after_forced:?} must be later than {after_suppressed:?}"
    );
}

/// The tool-call counter must accumulate across calls and reset only when the
/// caller asks for it.
///
/// MEASURED 2026-11-04: a combined take-and-reset counter cleared itself on
/// every provider iteration, so a threshold of 30 could never be reached in a
/// normal agent loop and the refresh never fired.
#[test]
fn tool_call_counter_accumulates_and_resets_only_on_demand() {
    let sid = "tool-call-counter-test";
    memory::reset_tool_calls_since_refresh(sid);

    memory::note_tool_calls(sid, 10);
    memory::note_tool_calls(sid, 5);
    assert_eq!(
        memory::tool_calls_since_refresh(sid),
        15,
        "counter must accumulate across calls"
    );

    // Peeking must not reset: the refresh decision is evaluated on EVERY
    // provider iteration, so a peek-and-reset would clear the counter each
    // time and the threshold could never be reached.
    assert_eq!(memory::tool_calls_since_refresh(sid), 15);
    assert_eq!(memory::tool_calls_since_refresh(sid), 15);

    memory::reset_tool_calls_since_refresh(sid);
    assert_eq!(memory::tool_calls_since_refresh(sid), 0);
}
