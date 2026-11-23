#[test]
fn abnormal_incomplete_stop_excludes_natural_completion() {
    for reason in [None, Some("end_turn"), Some("stop")] {
        assert!(super::incomplete_turn_stop(reason).is_none());
    }
    for reason in ["max_tokens", "length", "tool_use"] {
        assert!(
            matches!(super::incomplete_turn_stop(Some(reason)), Some(crate::protocol::ServerEvent::TurnStopped {
            reason: crate::protocol::TurnStopReason::LimitReached,
            provider_stop_reason: Some(raw), ..
        }) if raw == reason)
        );
    }
}

use super::*;
use serde_json::json;

fn tool_call(name: &str, input: serde_json::Value) -> ToolCall {
    ToolCall {
        id: "toolu_test".to_string(),
        name: name.to_string(),
        input,
        intent: None,
        thought_signature: None,
    }
}

#[test]
fn reload_interrupted_bg_wait_is_non_error_and_resumable() {
    let tc = tool_call(
        "bg",
        json!({"action": "wait", "task_id": "bg-123", "max_wait_seconds": 300}),
    );

    let (message, is_error) = reload_interrupted_tool_result(&tc, 1.2);

    assert!(!is_error);
    assert!(message.contains("Resume the wait"));
    assert!(message.contains("\"task_id\":\"bg-123\""));
}

use crate::provider::EventStream;
use async_trait::async_trait;
use tokio::sync::mpsc as tokio_mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// Provider that fails the first request with a transient stream error and
/// completes normally on the second one.
#[derive(Clone, Default)]
struct TransientStreamErrorProvider {
    calls: Arc<StdMutex<usize>>,
}

#[async_trait]
impl Provider for TransientStreamErrorProvider {
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut guard = self.calls.lock().unwrap();
            *guard += 1;
            *guard
        };
        let (tx, rx) = tokio_mpsc::channel::<Result<StreamEvent>>(8);
        tokio::spawn(async move {
            if call == 1 {
                // The exact wording observed in the wild: the provider
                // classified the fault as an in-band stream error.
                let _ = tx
                    .send(Ok(StreamEvent::Error {
                        message: "Provider returned an empty response".to_string(),
                        retry_after_secs: None,
                    }))
                    .await;
            } else {
                let _ = tx
                    .send(Ok(StreamEvent::TextDelta("all done".to_string())))
                    .await;
                let _ = tx
                    .send(Ok(StreamEvent::MessageEnd {
                        stop_reason: Some("end_turn".to_string()),
                    }))
                    .await;
            }
        });
        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn name(&self) -> &str {
        "transient-stream-error-test"
    }

    fn model(&self) -> String {
        "test-model".into()
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// A provider stream that fails mid-turn must not end the turn: the agent has
/// to re-issue the request, bounded, and deliver the model's real completion.
///
/// Before the fix the `StreamEvent::Error` arm returned `Err(StreamError)`
/// straight out of the turn loop, so the turn died and the session had to be
/// restarted by the user.
#[tokio::test]
async fn transient_stream_error_recovers_the_turn_instead_of_ending_it() {
    let _guard = crate::storage::lock_test_env();
    let provider = TransientStreamErrorProvider::default();
    let calls = provider.calls.clone();
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let registry = Registry::new(provider.clone()).await;
    let mut agent = Agent::new(provider, registry);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent
        .run_once_streaming_mpsc("do the task", Vec::new(), None, tx)
        .await
        .expect("a transient stream error must not end the turn");

    let mut text = String::new();
    while let Ok(event) = rx.try_recv() {
        if let ServerEvent::TextDelta { text: delta } = event {
            text.push_str(&delta);
        }
    }

    assert_eq!(
        *calls.lock().unwrap(),
        2,
        "the turn must re-issue the request exactly once after a transient stream error"
    );
    assert!(
        text.contains("all done"),
        "the recovered turn must deliver the model's real completion, got {text:?}"
    );
}

#[test]
fn reload_interrupted_non_wait_tool_remains_error() {
    let tc = tool_call("bash", json!({"command": "sleep 10"}));

    let (message, is_error) = reload_interrupted_tool_result(&tc, 1.2);

    assert!(is_error);
    assert!(message.contains("interrupted by server reload"));
}
