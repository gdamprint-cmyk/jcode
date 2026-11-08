//! The model-facing todo list must stay small enough to keep re-reading cheap.
//!
//! reference: no prior art, searched "public_todos_compact" and "description
//! truncate" in crates/jcode-app-core/src (0 hits before this change); the
//! bounding helper it uses is `crate::util::truncate_str`, the repo's existing
//! tool-output truncation helper.

use crate::todo::TodoItem;
use crate::tool::todo::names;

fn item(content: &str, status: &str, details: &str) -> TodoItem {
    TodoItem {
        content: content.to_string(),
        status: status.to_string(),
        priority: "medium".to_string(),
        id: format!("todo_{content}"),
        details: Some(details.to_string()),
        ..Default::default()
    }
}

#[test]
fn an_open_card_keeps_its_full_description() {
    // The model works FROM this brief, so it must not be cut.
    let long = "x".repeat(9_000);
    let todos = vec![item("open", "in_progress", &long)];
    let public = super::public_todos_compact(&todos).expect("public todos");
    let description = public[0]["description"].as_str().expect("description");
    assert_eq!(
        description, long,
        "an open card's description must reach the model unchanged"
    );
}

#[test]
fn a_completed_card_is_truncated_and_marked() {
    let long = "y".repeat(9_000);
    let todos = vec![item("done", "completed", &long)];
    let public = super::public_todos_compact(&todos).expect("public todos");
    let description = public[0]["description"].as_str().expect("description");
    assert!(
        description.len() < long.len(),
        "a completed card's description must be cut down"
    );
    assert!(
        description.contains("[truncated:"),
        "a truncated description must say so: {description}"
    );
    assert_eq!(
        public[0]["description_truncated"].as_bool(),
        Some(true),
        "the card must be marked as truncated"
    );
}

#[test]
fn a_short_completed_card_is_left_alone() {
    let todos = vec![item("done", "completed", "short note")];
    let public = super::public_todos_compact(&todos).expect("public todos");
    assert_eq!(public[0]["description"].as_str(), Some("short note"));
    assert!(public[0].get("description_truncated").is_none());
}

#[test]
fn the_plan_shape_survives_compaction() {
    // Compaction may shrink prose, but never the structure the model plans with.
    let long = "z".repeat(9_000);
    let mut todo = item("done", "completed", &long);
    todo.group = Some("lane-a".to_string());
    let todos = vec![todo];
    let public = super::public_todos_compact(&todos).expect("public todos");
    assert_eq!(public[0]["name"].as_str(), Some("done"));
    assert_eq!(public[0]["status"].as_str(), Some("completed"));
    assert_eq!(public[0]["priority"].as_str(), Some("medium"));
    assert_eq!(public[0]["group"].as_str(), Some("lane-a"));
}

#[test]
fn a_full_board_of_completed_cards_shrinks_to_a_fraction() {
    // The measured problem: 520 cards / 844 KB, of which 838 KB was `details`
    // on completed cards. The compacted list must be far smaller.
    let long = "w".repeat(9_000);
    let todos: Vec<TodoItem> = (0..100)
        .map(|index| item(&format!("card-{index}"), "completed", &long))
        .collect();
    let public = super::public_todos_compact(&todos).expect("public todos");
    let compacted = serde_json::to_string(&public).expect("serialize");
    let full = serde_json::to_string(&names::public_todos(&todos).expect("full")).expect("s");
    assert!(
        compacted.len() * 4 < full.len(),
        "compacted {} vs full {} bytes",
        compacted.len(),
        full.len()
    );
}
