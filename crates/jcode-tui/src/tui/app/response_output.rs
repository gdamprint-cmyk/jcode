/// Output of the current provider response, independent of the render buffer.
/// Compaction may commit that buffer before the turn's Done event arrives.
// reference: the stop-reason vocabulary comes from the provider APIs themselves -
// Anthropic's Messages API (`stop_reason`: "end_turn"/"tool_use"/"max_tokens"),
// OpenAI Chat Completions (`choices[].finish_reason`: "stop"/"length"/"tool_calls"),
// and the OpenAI Responses API (`response.status` + `incomplete_details.reason`),
// as implemented in this repo at crates/jcode-provider-anthropic-runtime,
// crates/jcode-provider-openrouter/src/stream.rs and
// crates/jcode-provider-openai/src/stream.rs (extract_stop_reason_from_response).
#[derive(Clone, Debug, Default)]
pub(super) struct ResponseOutput {
    has_text: bool,
    has_tools: bool,
    ended_normally: bool,
}

impl ResponseOutput {
    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn text(&mut self, text: &str) {
        self.has_text |= !text.trim().is_empty();
    }

    pub(super) fn replace_text(&mut self, text: &str) {
        self.has_text = !text.trim().is_empty();
    }

    pub(super) fn tool(&mut self) {
        self.has_tools = true;
    }

    pub(super) fn finish(&mut self, stop_reason: Option<&str>) {
        // A stop reason is a provider vocabulary, not a fixed enum. Anthropic
        // says "end_turn", OpenAI Chat Completions says "stop", the OpenAI
        // Responses API says "commentary" for an ordinary completion, Gemini
        // says "STOP", and OpenRouter forwards whatever the upstream model
        // reported. Treating the allowlist as exhaustive made every completion
        // whose reason was not in the list look like a truncated answer, so the
        // final-response handoff fired on top of a finished answer (measured:
        // 142 of 151 handoffs followed a complete text-only response).
        //
        // The rule is therefore inverted: a completion is normal unless the
        // provider says it was cut short, refused, or asked for a tool.
        self.ended_normally = !matches!(
            stop_reason.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
            Some(
                // Truncated by a token/length budget.
                "length" | "max_tokens" | "max_output_tokens" | "max_new_tokens" | "token_limit"
                // Refused or filtered.
                | "refusal" | "content_filter" | "safety" | "blocklist" | "prohibited_content"
                | "spii" | "recitation"
                // The model asked for a tool, so the answer is not final yet.
                | "tool_use" | "tool_calls" | "function_call"
                // The provider stopped the turn without completing it.
                | "incomplete" | "pause_turn" | "cancelled" | "canceled" | "aborted"
                | "error" | "failed" | "guardrail" | "unknown"
            )
        );
    }

    pub(super) fn has_final_response(&self) -> bool {
        self.has_text && !self.has_tools && self.ended_normally
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn final_response_requires_text_without_tools_and_a_normal_end() {
        for reason in [None, Some("end_turn"), Some("stop"), Some("completed")] {
            let mut output = ResponseOutput::default();
            output.text(" \n");
            output.finish(reason);
            assert!(!output.has_final_response());
            output.text("Done.");
            assert!(output.has_final_response());
            output.tool();
            assert!(!output.has_final_response());
            output.text("Commentary after a tool call");
            assert!(!output.has_final_response());
            output.reset();
            assert!(!output.has_final_response());
        }
        for reason in [
            "max_tokens",
            "length",
            "refusal",
            "tool_use",
            "pause_turn",
            "unknown",
        ] {
            let mut output = ResponseOutput::default();
            output.text("Partial answer");
            output.finish(Some(reason));
            assert!(!output.has_final_response(), "{reason}");
        }
    }

    /// A provider's stop reason is a vocabulary, not a fixed enum. Every reason
    /// that means "the answer is complete" must count as a delivered final
    /// response, whatever the provider calls it.
    ///
    /// MEASURED 2026-11-05: with the previous exhaustive allowlist, 142 of 151
    /// final-response handoffs fired on top of a COMPLETE text-only answer,
    /// because the providers in use reported reasons outside the list
    /// (openrouter/openai-oauth/openai-compatible forward the API value
    /// verbatim; the OpenAI Responses API reports "commentary" for an ordinary
    /// completion). The handoff then told the model to stop working on an answer
    /// the user had already received.
    // reference: Anthropic Messages API "stop_reason", OpenAI Chat Completions
    // "choices[].finish_reason", OpenAI Responses API "response.status" - the
    // vocabularies this test enumerates, as implemented in this repo at
    // crates/jcode-provider-openrouter/src/stream.rs (finish_reason capture) and
    // crates/jcode-provider-openai/src/stream.rs (extract_stop_reason_from_response).
    #[test]
    fn provider_stop_reasons_that_mean_completed_are_treated_as_final() {
        for reason in [
            // Anthropic
            "end_turn",
            // OpenAI Chat Completions and the OpenRouter forwarding of it
            "stop",
            // OpenAI Responses API, ordinary completion
            "commentary",
            // Gemini, lower-cased by the provider runtime
            "stop_sequence",
            "completed",
            "complete",
            "done",
            "eos",
            "finished",
            // Case and padding must not matter.
            "END_TURN",
            "  Stop  ",
        ] {
            let mut output = ResponseOutput::default();
            output.text("The answer is complete.");
            output.finish(Some(reason));
            assert!(output.has_final_response(), "{reason}");
        }
    }

    /// The inverted rule must still refuse to treat a cut-short, refused or
    /// tool-requesting turn as a delivered answer.
    #[test]
    fn truncated_refused_and_tool_turns_are_not_final_responses() {
        for reason in [
            "length",
            "max_tokens",
            "max_output_tokens",
            "max_new_tokens",
            "token_limit",
            "refusal",
            "content_filter",
            "safety",
            "blocklist",
            "prohibited_content",
            "spii",
            "recitation",
            "tool_use",
            "tool_calls",
            "function_call",
            "incomplete",
            "pause_turn",
            "cancelled",
            "canceled",
            "aborted",
            "error",
            "failed",
            "guardrail",
            "unknown",
        ] {
            let mut output = ResponseOutput::default();
            output.text("Partial answer");
            output.finish(Some(reason));
            assert!(!output.has_final_response(), "{reason}");
        }
    }

    /// A provider that reports nothing at all still ends normally: an absent
    /// stop reason is not evidence of truncation.
    #[test]
    fn a_missing_stop_reason_is_a_normal_end() {
        let mut output = ResponseOutput::default();
        output.text("The answer is complete.");
        output.finish(None);
        assert!(output.has_final_response());
    }

    #[test]
    fn replacement_can_remove_an_answer() {
        let mut output = ResponseOutput::default();
        output.text("Tool markup, not an answer");
        output.finish(None);
        output.replace_text("");
        assert!(!output.has_final_response());
    }
}
