use super::*;

fn named(name: &str) -> Value {
    json!({"name": name, "description": format!("Work on {name}"), "status": "pending", "priority": "high"})
}

fn hierarchy(items: Vec<Value>) -> Value {
    json!({"phases": [{"phase": 1, "lanes": [{"lane": "work", "items": items}]}]})
}

fn create(input: Value) -> Value {
    apply_todo_patch(input, || json!([])).unwrap()
}

fn parsed(input: Value) -> Vec<TodoItem> {
    parse_todo_input(input).unwrap().todos.unwrap()
}

fn patch(input: Value, stored: &Value) -> Value {
    apply_todo_patch(input, || stored["todos"].clone()).unwrap()
}

#[test]
fn names_create_internal_ids_and_map_descriptions_to_storage() {
    let result = create(hierarchy(vec![named("Implement"), named("Test")]));
    let todos = parsed(result);
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].content, "Implement");
    assert_eq!(todos[0].details.as_deref(), Some("Work on Implement"));
    assert!(!todos[0].id.trim().is_empty());
    assert!(!todos[1].id.trim().is_empty());
    assert_ne!(todos[0].id, todos[1].id);
    assert_eq!(todos[0].stage, Some(1));
    assert_eq!(todos[0].group.as_deref(), Some("work"));
}

#[test]
fn names_select_updates_and_removals_without_changing_identity() {
    let initial = create(hierarchy(vec![named("Implement"), named("Test")]));
    let original = parsed(initial.clone());
    let updated = patch(
        json!({"update": [{"name": "Implement", "status": "in_progress", "description": "New context"}]}),
        &initial,
    );
    let todos = parsed(updated.clone());
    assert_eq!(todos[0].id, original[0].id);
    assert_eq!(todos[0].content, "Implement");
    assert_eq!(todos[0].status, "in_progress");
    assert_eq!(todos[0].details.as_deref(), Some("New context"));
    let remaining = parsed(patch(json!({"remove": ["Test"]}), &updated));
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].id, original[0].id);
}

#[test]
fn explicit_rename_preserves_id_and_future_name_lookup() {
    let initial = create(hierarchy(vec![named("Before")]));
    let id = parsed(initial.clone())[0].id.clone();
    let renamed = patch(
        json!({"update": [{"name": "Before", "rename_to": "After"}]}),
        &initial,
    );
    let todos = parsed(renamed.clone());
    assert_eq!(todos[0].id, id);
    assert_eq!(todos[0].content, "After");
    let updated = parsed(patch(
        json!({"update": [{"name": "After", "status": "completed"}]}),
        &renamed,
    ));
    assert_eq!(updated[0].id, id);
    assert_eq!(updated[0].status, "completed");
    assert!(
        apply_todo_patch(
            json!({"update": [{"name": "Before", "status": "pending"}]}),
            || renamed["todos"].clone()
        )
        .is_err()
    );
}

#[test]
fn replacement_preserves_ids_by_name_not_position() {
    let initial = create(hierarchy(vec![named("Implement"), named("Test")]));
    let before = parsed(initial.clone());
    let replaced = apply_todo_patch(
        hierarchy(vec![named("Test"), named("Implement"), named("Ship")]),
        || initial["todos"].clone(),
    )
    .unwrap();
    let after = parsed(replaced);
    assert_eq!(after[0].id, before[1].id);
    assert_eq!(after[1].id, before[0].id);
    assert!(before.iter().all(|todo| todo.id != after[2].id));
}

#[test]
fn add_after_name_inherits_membership_and_inserts_after_anchor() {
    let initial = create(hierarchy(vec![named("Implement"), named("Ship")]));
    let mut addition = named("Test");
    addition["after"] = json!("Implement");
    let todos = parsed(patch(json!({"add": [addition]}), &initial));
    assert_eq!(
        todos
            .iter()
            .map(|todo| todo.content.as_str())
            .collect::<Vec<_>>(),
        ["Implement", "Test", "Ship"]
    );
    assert_eq!(todos[1].stage, todos[0].stage);
    assert_eq!(todos[1].group, todos[0].group);
    assert_ne!(todos[1].id, todos[0].id);
}

#[test]
fn lane_dependency_names_resolve_to_ids_and_follow_renames() {
    let initial = create(json!({"phases": [
        {"phase": 1, "lanes": [{"lane": "base", "items": [named("Build")]}]},
        {"phase": 2, "lanes": [{"lane": "checks", "after": ["Build"], "items": [named("Test"), named("Lint")]}]}
    ]}));
    let todos = parsed(initial.clone());
    let prerequisite = Some(vec![todos[0].id.clone()]);
    assert_eq!(todos[1].lane_after, prerequisite);
    assert_eq!(todos[2].lane_after, prerequisite);
    crate::todo::stages::validate_lane_dependencies(&todos).unwrap();
    let renamed = patch(
        json!({"update": [{"name": "Build", "rename_to": "Compile"}]}),
        &initial,
    );
    assert_eq!(parsed(renamed.clone())[1].lane_after, prerequisite);
    let cleared = patch(
        json!({"update": [{"name": "Test", "lane_after": null}]}),
        &renamed,
    );
    assert!(
        parsed(cleared.clone())
            .iter()
            .all(|todo| todo.lane_after.is_none())
    );
    let restored = parsed(patch(
        json!({"update": [{"name": "Lint", "lane_after": ["Compile"]}]}),
        &cleared,
    ));
    assert_eq!(restored[1].lane_after, prerequisite);
    assert_eq!(restored[2].lane_after, prerequisite);
}

#[test]
fn names_must_be_nonblank_and_globally_unique() {
    for name in ["", " ", "\n\t"] {
        assert!(apply_todo_patch(hierarchy(vec![named(name)]), || json!([])).is_err());
    }
    assert!(
        apply_todo_patch(
            json!({"phases": [
                {"phase": 1, "lanes": [{"lane": "one", "items": [named("Same")]}]},
                {"phase": 2, "lanes": [{"lane": "two", "items": [named("Same")]}]}
            ]}),
            || json!([])
        )
        .is_err()
    );
    let initial = create(hierarchy(vec![named("One"), named("Two")]));
    for input in [
        json!({"add": [{"name": "One", "phase": 1, "lane": "work", "status": "pending", "priority": "high"}]}),
        json!({"update": [{"name": "One", "rename_to": "Two"}]}),
        json!({"update": [{"name": "One", "rename_to": " "}]}),
    ] {
        assert!(apply_todo_patch(input, || initial["todos"].clone()).is_err());
    }
}

#[test]
fn ambiguous_legacy_names_are_rejected_for_every_name_lookup() {
    let stored = json!([
        {"id": "first", "content": "Same", "status": "pending", "priority": "high", "stage": 1, "group": "work"},
        {"id": "second", "content": "Same", "status": "pending", "priority": "high", "stage": 1, "group": "work"},
        {"id": "third", "content": "Dependent", "status": "pending", "priority": "high", "stage": 2, "group": "next"}
    ]);
    for input in [
        json!({"update": [{"name": "Same", "status": "completed"}]}),
        json!({"remove": ["Same"]}),
        json!({"add": [{"name": "Other", "after": "Same", "status": "pending", "priority": "high"}]}),
        json!({"update": [{"name": "Dependent", "lane_after": ["Same"]}]}),
    ] {
        assert!(
            apply_todo_patch(input.clone(), || stored.clone()).is_err(),
            "accepted ambiguous name lookup: {input}"
        );
    }
}

#[test]
fn legacy_id_content_details_calls_remain_supported() {
    let initial = create(hierarchy(vec![
        json!({"id": "legacy", "content": "Legacy", "details": "Old context", "status": "pending", "priority": "high"}),
    ]));
    let updated = patch(
        json!({"update": [{"id": "legacy", "content": "Changed", "details": "Updated context"}]}),
        &initial,
    );
    let todos = parsed(updated.clone());
    assert_eq!(todos[0].id, "legacy");
    assert_eq!(todos[0].content, "Changed");
    assert_eq!(todos[0].details.as_deref(), Some("Updated context"));
    assert!(parsed(patch(json!({"remove": ["legacy"]}), &updated)).is_empty());
}

#[test]
fn public_schema_uses_names_and_descriptions_not_storage_fields() {
    let schema = TodoTool.parameters_schema();
    let phase_item = &schema["properties"]["phases"]["items"]["properties"]["lanes"]["items"]["properties"]
        ["items"]["items"];
    let add_item = &schema["properties"]["add"]["items"];
    let update_item = &schema["properties"]["update"]["items"];
    for item in [phase_item, add_item, update_item] {
        let properties = item["properties"].as_object().unwrap();
        assert!(properties.contains_key("name"));
        assert!(properties.contains_key("description"));
        for field in ["id", "content", "details"] {
            assert!(
                !properties.contains_key(field),
                "public schema exposes {field}"
            );
        }
        assert!(
            item["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "name")
        );
    }
    assert!(update_item["properties"]["rename_to"].is_object());
}

#[test]
fn named_edits_accept_null_phases_singletons_and_stringified_arguments() {
    let initial = create(hierarchy(vec![named("Implement")]));
    let updated = patch(
        json!({"phases": null, "update": {"name": "Implement", "status": "in_progress"}}),
        &initial,
    );
    assert_eq!(parsed(updated.clone())[0].status, "in_progress");
    let added = patch(
        json!({"add": json!({"name": "Test", "description": "Verify", "after": "Implement", "status": "pending", "priority": "high"}).to_string()}),
        &updated,
    );
    assert_eq!(parsed(added.clone())[1].content, "Test");
    let removed = patch(json!({"remove": json!(["Test"]).to_string()}), &added);
    assert_eq!(parsed(removed).len(), 1);
    assert!(apply_todo_patch(json!({"remove": ["Typo"]}), || initial["todos"].clone()).is_err());
}

#[test]
fn incremental_dependencies_can_reference_tasks_added_in_the_same_call() {
    let initial = create(json!({"phases": [
        {"phase": 1, "lanes": [{"lane": "base", "items": [named("Anchor")]}]},
        {"phase": 2, "lanes": [{"lane": "checks", "items": [named("Test")]}]}
    ]}));
    let todos = parsed(patch(
        json!({
            "add": [{"name": "Build", "after": "Anchor", "status": "pending", "priority": "high"}],
            "update": [{"name": "Test", "lane_after": ["Build"]}]
        }),
        &initial,
    ));
    assert_eq!(todos[2].lane_after, Some(vec![todos[1].id.clone()]));
    crate::todo::stages::validate_lane_dependencies(&todos).unwrap();
}

#[tokio::test]
async fn named_tool_execution_persists_ids_but_returns_public_names() {
    let _guard = crate::storage::lock_test_env();
    struct RestoreHome(Option<std::ffi::OsString>);
    impl Drop for RestoreHome {
        fn drop(&mut self) {
            if let Some(home) = &self.0 {
                crate::env::set_var("JCODE_HOME", home);
            } else {
                crate::env::remove_var("JCODE_HOME");
            }
        }
    }
    let _restore = RestoreHome(std::env::var_os("JCODE_HOME"));
    let dir = tempfile::TempDir::new().unwrap();
    crate::env::set_var("JCODE_HOME", dir.path());
    let context = || ToolContext {
        session_id: "named-tool-execution".into(),
        message_id: "test".into(),
        tool_call_id: "call".into(),
        working_dir: None,
        stdin_request_tx: None,
        graceful_shutdown_signal: None,
        execution_mode: crate::tool::ToolExecutionMode::Direct,
    };
    let tool = TodoTool::new();
    tool.execute(hierarchy(vec![named("Implement")]), context())
        .await
        .unwrap();
    let original = load_todos("named-tool-execution").unwrap();
    tool.execute(json!({"phases": null, "update": [{"name": "Implement", "status": "in_progress", "description": "Updated brief"}]}), context()).await.unwrap();
    let output = tool.execute(json!({}), context()).await.unwrap();
    let public: Value = serde_json::from_str(&output.output).unwrap();
    assert_eq!(public[0]["name"], "Implement");
    assert_eq!(public[0]["description"], "Updated brief");
    assert_eq!(public[0]["status"], "in_progress");
    for field in ["id", "content", "details"] {
        assert!(public[0].get(field).is_none());
    }
    let metadata = output.metadata.unwrap();
    assert_eq!(metadata["todos"][0]["id"], original[0].id);
    let before_error = load_todos("named-tool-execution").unwrap();
    assert!(
        tool.execute(
            json!({"update": [{"name": "Implement", "rename_to": " "}]}),
            context()
        )
        .await
        .is_err()
    );
    assert_eq!(
        load_todos("named-tool-execution").unwrap()[0].id,
        before_error[0].id
    );
    assert_eq!(
        load_todos("named-tool-execution").unwrap()[0].content,
        "Implement"
    );
}

// reference: no prior art, searched crates/jcode-app-core/src/tool/todo.rs and
//            crates/jcode-todo-policy/src/stages.rs for an existing
//            duplicate-name refusal on the add path. The merge-side
//            disambiguation (`stages.rs::disambiguate_names`) renames silently
//            and is merge-only; it cannot be reused here because a silent
//            rename would make the caller address the pre-existing card on
//            its next call. The refusal shape follows the tool's own precedent
//            for "this card exists" (`todo.rs`: "todo id {id:?} already
//            exists; use `update` to change it").

/// A coordinator add whose name a worker lane already holds is refused with an
/// actionable message, and the board stays usable afterwards.
#[test]
fn add_with_an_existing_name_is_refused_and_leaves_the_board_usable() {
    let stored = json!({
        "todos": [
            {"id": "w1", "content": "check-brief-consistency", "status": "in_progress", "priority": "high", "stage": 1, "group": "worker"},
            {"id": "c1", "content": "write-report", "status": "pending", "priority": "high", "stage": 1, "group": "coord"}
        ]
    });
    let error = apply_todo_patch(
        json!({"add": [{"name": "check-brief-consistency", "phase": 1, "lane": "coord", "status": "pending", "priority": "high"}]}),
        || stored["todos"].clone(),
    )
    .expect_err("a colliding add must be refused")
    .to_string();
    // Actionable: names the conflict and points at the alternative.
    assert!(error.contains("check-brief-consistency"), "{error}");
    assert!(error.contains("already exists"), "{error}");
    assert!(error.contains("update"), "{error}");

    // The board is still usable: a later named update on the clean board works.
    let updated = apply_todo_patch(
        json!({"update": [{"name": "write-report", "status": "in_progress"}]}),
        || stored["todos"].clone(),
    )
    .expect("the board must stay usable");
    let todos = parsed(updated);
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[1].status, "in_progress");
}

/// The negative control for the refusal: a non-colliding add succeeds and no
/// existing card name is rewritten.
#[test]
fn add_with_a_distinct_name_leaves_existing_names_untouched() {
    let stored = json!({
        "todos": [
            {"id": "w1", "content": "check-brief-consistency", "status": "in_progress", "priority": "high", "stage": 1, "group": "worker"},
            {"id": "c1", "content": "write-report", "status": "pending", "priority": "high", "stage": 1, "group": "coord"}
        ]
    });
    let result = apply_todo_patch(
        json!({"add": [{"name": "ship-notes", "phase": 1, "lane": "coord", "status": "pending", "priority": "high"}]}),
        || stored["todos"].clone(),
    )
    .expect("a distinct name must be accepted");
    let todos = parsed(result);
    let names: Vec<&str> = todos.iter().map(|todo| todo.content.as_str()).collect();
    assert_eq!(
        names,
        ["check-brief-consistency", "write-report", "ship-notes"],
        "existing names must not be renamed: {names:?}"
    );
    assert!(
        todos.iter().all(|todo| !todo.content.contains('(')),
        "no spurious disambiguation suffix: {names:?}"
    );
}

/// Two adds in ONE call that share a name are refused too: the second one
/// collides with the first, not only with the stored board.
#[test]
fn add_with_a_duplicate_name_inside_one_call_is_refused() {
    let stored = json!({"todos": []});
    let error = apply_todo_patch(
        json!({"add": [
            {"name": "same-name", "phase": 1, "lane": "coord", "status": "pending", "priority": "high"},
            {"name": "same-name", "phase": 1, "lane": "coord", "status": "pending", "priority": "high"}
        ]}),
        || stored["todos"].clone(),
    )
    .expect_err("two adds sharing a name must be refused")
    .to_string();
    assert!(error.contains("already exists"), "{error}");
}
