//! Measurement harness for the layer-2 compaction.
//!
//! reference: no prior art, searched "public_todos_compact" and "confidence
//! history" in crates/jcode-app-core/src (the tool's own helpers only). It
//! renders the same board through the same code path, once with the history
//! arrays (the pre-layer-2 rendering) and once without, and compares the two
//! byte counts.
//!
//! Measured on this host (2026-11-08): a 100-card board with a 5-entry
//! confidence history per card and a 5-entry plan intent history renders at
//! 77695 bytes before layer 2 and 64578 bytes after - 13117 bytes (16.9%)
//! saved. The histories keep growing while the board does not, so the saving
//! grows with the number of observations.

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
fn the_history_arrays_cost_a_sixth_of_the_board() {
    let todos = board();
    let plan = TodoPlan {
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

    // BEFORE: layer-1 compaction only (truncated descriptions, histories kept).
    let mut before_list = names::public_todos(&todos).expect("public");
    super::truncate_completed_descriptions(&mut before_list);
    let mut before = serde_json::to_string_pretty(&before_list).expect("serialize");
    before.push_str("\n\nPlan:\n");
    before.push_str(&serde_json::to_string_pretty(&plan).expect("serialize"));

    // AFTER: layer-2 compaction (histories dropped too).
    let after = super::public_todos_compact(&todos).expect("compact");
    let mut after_text = serde_json::to_string_pretty(&after).expect("serialize");
    after_text.push_str("\n\nPlan:\n");
    after_text.push_str(&super::model_facing_json(&plan).expect("plan"));

    eprintln!("MEASURE_BEFORE_BYTES={}", before.len());
    eprintln!("MEASURE_AFTER_BYTES={}", after_text.len());
    assert_eq!(before.len(), 77_695, "measured layer-1 rendering size");
    assert_eq!(after_text.len(), 64_578, "measured layer-2 rendering size");
    assert!(
        after_text.len() * 10 < before.len() * 9,
        "the history arrays must cost a real share of the board: {} vs {} bytes",
        after_text.len(),
        before.len()
    );
}
