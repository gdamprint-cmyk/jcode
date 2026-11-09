//! The model-facing todo list must stay small enough to keep re-reading cheap.
//!
//! reference: no prior art, searched "public_todos_compact" and "description
//! truncate" in crates/jcode-app-core/src (0 hits before this change); the
//! bounding helper it uses is `crate::util::truncate_str`, the repo's existing
//! tool-output truncation helper.

use crate::todo::{ConfidenceState, IntentUnderstanding, TodoItem, TodoPlan};
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

// reference: no prior art for history-stripping tests, searched
// "confidence_history" and "understands_user_intent_history" in
// crates/jcode-app-core/src/tool (only the tool's own merge helpers and the
// policy gates); the production code under test is
// `crate::tool::todo::strip_tool_maintained_history` and
// `crate::tool::todo::build_todo_output`.
#[test]
fn a_todos_confidence_history_is_dropped_but_the_current_state_stays() {
    // The history is tool-maintained and unbounded; the model reads the
    // current state, not the trail behind it.
    let mut todo = item("open", "in_progress", "note");
    todo.confidence = Some(ConfidenceState::Validated);
    todo.completion_confidence = Some(ConfidenceState::Verified);
    todo.confidence_history = vec![
        ConfidenceState::Speculative,
        ConfidenceState::Plausible,
        ConfidenceState::Validated,
    ];
    let public = super::public_todos_compact(&[todo]).expect("public todos");
    assert!(
        public[0].get("confidence_history").is_none(),
        "the tool-maintained history must not reach the model: {public:?}"
    );
    assert_eq!(public[0]["confidence"].as_str(), Some("validated"));
    assert_eq!(
        public[0]["completion_confidence"].as_str(),
        Some("verified"),
        "the recorded completion state is cheap and informative, so it stays"
    );
}

#[test]
fn the_plan_keeps_its_intent_state_and_drops_the_history() {
    let plan = TodoPlan {
        user_intention: Some("ship the thing".to_string()),
        understands_user_intent: Some(IntentUnderstanding::Clear),
        understands_user_intent_history: vec![
            IntentUnderstanding::Uncertain,
            IntentUnderstanding::Partial,
            IntentUnderstanding::Clear,
        ],
    };
    let text = super::model_facing_json(&plan).expect("plan text");
    assert!(
        !text.contains("understands_user_intent_history"),
        "the tool-maintained intent history must not reach the model: {text}"
    );
    assert!(text.contains("user_intention"));
    assert!(text.contains("clear"));
}

#[test]
fn the_tool_metadata_still_carries_both_histories() {
    // The gates and the UI read the histories from the stored state and the
    // metadata, so compaction of the model-facing text must not touch them.
    let mut todo = item("done", "completed", "note");
    todo.confidence = Some(ConfidenceState::Validated);
    todo.confidence_history = vec![
        ConfidenceState::Speculative,
        ConfidenceState::Plausible,
        ConfidenceState::Validated,
    ];
    let plan = TodoPlan {
        user_intention: Some("ship the thing".to_string()),
        understands_user_intent: Some(IntentUnderstanding::Clear),
        understands_user_intent_history: vec![
            IntentUnderstanding::Uncertain,
            IntentUnderstanding::Clear,
        ],
    };
    let output = super::build_todo_output(vec![todo], plan, Vec::new(), None, None, Vec::new())
        .expect("todo output");
    assert!(
        !output.output.contains("confidence_history"),
        "the model-facing text must not carry the history: {}",
        output.output
    );
    assert!(!output.output.contains("understands_user_intent_history"));
    let metadata = output.metadata.expect("metadata");
    assert_eq!(
        metadata["todos"][0]["confidence_history"]
            .as_array()
            .expect("confidence history")
            .len(),
        3,
        "the metadata must keep every confidence history entry"
    );
    assert_eq!(
        metadata["plan"]["understands_user_intent_history"]
            .as_array()
            .expect("intent history")
            .len(),
        2,
        "the metadata must keep every intent history entry"
    );
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
