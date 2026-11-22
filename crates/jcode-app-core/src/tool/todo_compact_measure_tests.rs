//! Measurement harness for the model-facing compaction.
//!
//! reference: no prior art, searched "public_todos_compact" and "confidence
//! history" in crates/jcode-app-core/src (the tool's own helpers only). It
//! renders the same board through the same code path, once with every card and
//! the history arrays kept, and once compacted, and compares byte counts.
//!
//! MEASURED 2026-11-22 on this host: a live `todo` result was 105,325 B / 183
//! cards, of which 179 completed (88,399 B) and 2 cancelled; the open cards
//! were 5,504 B. Finished work is the dominant term, so it is what leaves.

use crate::todo::{ConfidenceState, IntentUnderstanding, TodoItem, TodoPlan};
use crate::tool::todo::names;

fn board() -> Vec<TodoItem> {
    (0..100)
        .map(|index| {
            let mut todo = TodoItem {
                content: format!("card-{index}"),
                status: "completed".to_string(),
                priority: "medium".to_string(),
                id: format!("todo_{index}"),
                details: Some("d".repeat(2_000)),
                ..Default::default()
            };
            todo.confidence = Some(ConfidenceState::Validated);
            todo.completion_confidence = Some(ConfidenceState::Verified);
            todo.confidence_history = vec![
                ConfidenceState::Speculative,
                ConfidenceState::Plausible,
                ConfidenceState::Validated,
                ConfidenceState::Verified,
                ConfidenceState::Validated,
            ];
            todo
        })
        .collect()
}

#[test]
fn finished_cards_leave_the_model_facing_board() {
    let todos = board();
    let plan = TodoPlan {
        title: None,
        user_intention: Some("ship the thing".to_string()),
        understands_user_intent: Some(IntentUnderstanding::Clear),
        understands_user_intent_history: vec![
            IntentUnderstanding::Uncertain,
            IntentUnderstanding::Partial,
            IntentUnderstanding::Clear,
            IntentUnderstanding::Complete,
            IntentUnderstanding::Clear,
        ],
    };

    // BEFORE: every card rendered with its description and its history arrays.
    let before_list = names::public_todos(&todos).expect("public");
    let mut before = serde_json::to_string_pretty(&before_list).expect("serialize");
    before.push_str("\n\nPlan:\n");
    before.push_str(&serde_json::to_string_pretty(&plan).expect("serialize"));

    // AFTER: the current model-facing rendering.
    let output = super::build_todo_output(todos.clone(), plan, Vec::new(), None, None, Vec::new())
        .expect("todo output");

    eprintln!("MEASURE_BEFORE_BYTES={}", before.len());
    eprintln!("MEASURE_AFTER_BYTES={}", output.output.len());
    assert_eq!(before.len(), 231_095, "measured full rendering size");
    assert_eq!(output.output.len(), 118, "measured compacted rendering size");
    assert!(
        output.output.len() * 10 < before.len(),
        "finished cards must be gone from the board: {} vs {} bytes",
        output.output.len(),
        before.len()
    );
    assert!(
        output.output.contains("[100 finished cards omitted]"),
        "the omission count must be reported as text: {}",
        output.output
    );
}
