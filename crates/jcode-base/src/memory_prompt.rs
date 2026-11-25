// reference: vectorize-io/hindsight-coding-agents, dist/dcode-hook.js
//   :2189 boundRecallQuery and :2214 injectRecall - upstream sends the USER'S
//   PROMPT (bounded) as the recall query, not the transcript. The local
//   builder below now matches that shape; see the MEASURED note on
//   format_focused_query_for_relevance.
const MEMORY_CONTEXT_MAX_CHARS: usize = 8_000;
const MEMORY_CONTEXT_MAX_MESSAGES: usize = 12;
const MEMORY_CONTEXT_MAX_BLOCK_CHARS: usize = 1_200;
const EXTRACTION_CONTEXT_MAX_MESSAGES: usize = 40;
const EXTRACTION_CONTEXT_MAX_CHARS: usize = 24_000;

fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    value.chars().take(max_chars).collect()
}

/// Truncate a text block for the relevance context, keeping its reminder
/// structure intact.
///
/// MEASURED 2026-11-05: the MCP tool catalogue arrives as one text block of
/// ~48,000 chars. Plain truncation cut off the closing `</system-reminder>`,
/// so `focus_query_text` treated the whole rest of the transcript as
/// reminder content and dropped the user's question. With nothing left to
/// keep it fell back to the raw context, where the 1,200-char catalogue
/// excerpt dominated the recall query and the recall answered about MCP
/// tools instead of the question.
///
/// Closing the reminder at the truncation point keeps the block's structure
/// honest: the excerpt ends where the block was cut, so the skip ends there
/// too.
fn truncate_block_for_relevance(text: &str) -> String {
    let cut = truncate_chars(text, MEMORY_CONTEXT_MAX_BLOCK_CHARS);
    if cut.chars().count() == text.chars().count() {
        return cut;
    }
    let opens = cut.matches("<system-reminder>").count();
    let closes = cut.matches("</system-reminder>").count();
    if opens <= closes {
        return cut;
    }
    let mut out = cut;
    for _ in 0..(opens - closes) {
        out.push_str("\n</system-reminder>");
    }
    out
}

fn format_content_block_for_relevance(block: &crate::message::ContentBlock) -> Option<String> {
    match block {
        crate::message::ContentBlock::Text { text, .. } => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_block_for_relevance(trimmed))
            }
        }
        crate::message::ContentBlock::ToolUse { name, .. } => Some(format!("[Tool: {}]", name)),
        crate::message::ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            if is_error.unwrap_or(false) {
                // Keep the serialized block on one physical line. The focused-query
                // filter drops tool blocks by their leading marker, so preserving
                // payload newlines would let every line after the first escape as
                // apparent conversation prose.
                let content = truncate_chars(content.trim(), MEMORY_CONTEXT_MAX_BLOCK_CHARS / 4);
                let content = content.split_whitespace().collect::<Vec<_>>().join(" ");
                Some(format!("[Tool error: {}]", content))
            } else {
                None
            }
        }
        crate::message::ContentBlock::Reasoning { .. }
        | crate::message::ContentBlock::ReasoningTrace { .. }
        | crate::message::ContentBlock::AnthropicThinking { .. }
        | crate::message::ContentBlock::OpenAIReasoning { .. }
        | crate::message::ContentBlock::ToolReference { .. }
        | crate::message::ContentBlock::ProviderNative { .. } => None,
        crate::message::ContentBlock::Image { .. } => Some("[Image]".to_string()),
        crate::message::ContentBlock::OpenAICompaction { .. } => {
            Some("[OpenAI native compaction]".to_string())
        }
    }
}

fn format_content_block_for_extraction(block: &crate::message::ContentBlock) -> Option<String> {
    match block {
        crate::message::ContentBlock::Text { text, .. } => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(truncate_chars(trimmed, MEMORY_CONTEXT_MAX_BLOCK_CHARS))
            }
        }
        crate::message::ContentBlock::ToolUse { name, input, .. } => {
            let input_str =
                serde_json::to_string(input).unwrap_or_else(|_| "<invalid json>".into());
            let input_str = truncate_chars(&input_str, MEMORY_CONTEXT_MAX_BLOCK_CHARS / 2);
            Some(format!("[Tool: {} input: {}]", name, input_str))
        }
        crate::message::ContentBlock::ToolResult {
            content, is_error, ..
        } => {
            let label = if is_error.unwrap_or(false) {
                "Tool error"
            } else {
                "Tool result"
            };
            let content = truncate_chars(content, MEMORY_CONTEXT_MAX_BLOCK_CHARS / 2);
            Some(format!("[{}: {}]", label, content))
        }
        crate::message::ContentBlock::Reasoning { .. }
        | crate::message::ContentBlock::ReasoningTrace { .. }
        | crate::message::ContentBlock::AnthropicThinking { .. }
        | crate::message::ContentBlock::OpenAIReasoning { .. }
        | crate::message::ContentBlock::ToolReference { .. }
        | crate::message::ContentBlock::ProviderNative { .. } => None,
        crate::message::ContentBlock::Image { .. } => Some("[Image]".to_string()),
        crate::message::ContentBlock::OpenAICompaction { .. } => {
            Some("[OpenAI native compaction]".to_string())
        }
    }
}

fn format_message_context_with(
    message: &crate::message::Message,
    format_block: fn(&crate::message::ContentBlock) -> Option<String>,
) -> String {
    let role = match message.role {
        crate::message::Role::User => "User",
        crate::message::Role::Assistant => "Assistant",
    };

    let mut chunk = String::new();
    chunk.push_str(role);
    chunk.push_str(":\n");

    let mut has_content = false;
    for block in &message.content {
        if let Some(text) = format_block(block)
            && !text.is_empty()
        {
            has_content = true;
            chunk.push_str(&text);
            chunk.push('\n');
        }
    }

    if has_content { chunk } else { String::new() }
}

/// Format messages into a context string for relevance checking
pub fn format_context_for_relevance(messages: &[crate::message::Message]) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut total_chars = 0usize;

    for message in messages.iter().rev().take(MEMORY_CONTEXT_MAX_MESSAGES) {
        let chunk = format_message_context_with(message, format_content_block_for_relevance);
        if chunk.is_empty() {
            continue;
        }
        let chunk_len = chunk.chars().count();
        if total_chars + chunk_len > MEMORY_CONTEXT_MAX_CHARS {
            if total_chars == 0 {
                chunks.push(truncate_chars(&chunk, MEMORY_CONTEXT_MAX_CHARS));
            }
            break;
        }
        total_chars += chunk_len;
        chunks.push(chunk);
    }

    chunks.reverse();
    chunks.join("\n").trim().to_string()
}

/// Build a focused retrieval query from recent messages.
///
/// Unlike [`format_context_for_relevance`], which concatenates up to a dozen
/// role-prefixed messages (including tool output) into one blob, this produces a
/// tighter query centered on the user's *current intent*. Memory retrieval and
/// (especially) cross-encoder reranking degrade badly on long, noisy inputs:
/// embeddings get diluted and rerankers trained on short search queries fail to
/// score relevance. This builder:
///   - drops `<system-reminder>...</system-reminder>` blocks (session boilerplate),
///   - drops tool-call / tool-result lines,
///   - uses ONLY the most recent user message when there is one.
///
/// The last rule is the one that matters, and it is MEASURED, not assumed.
///
/// MEASURED 2026-11-09 on bank `jcode-legacy`, same scoped recall, same
/// 10%-of-top cutoff, narrow = the user's own words, wide = the user's words
/// plus the assistant prose that follows (what this builder used to send):
///   "блок который называется просто memory теперь лишний релевантность"
///     narrow: 3 of 11 kept, top = the owner's own complaint (ratio 1.000)
///     wide:   8 of 13 kept, top = the same complaint, but the tail fills with
///             the assistant's own past reasoning about the tooling
///   "не забудь всегда применять точное значение из памяти"
///     narrow: top = the PINEAPPLE-7429 verification (ratio 1.000)
///     wide:   top = "The Rust implementation sets MIN_RELATIVE_SCORE = 0.10"
///             - the agent's own source code, not the owner's instruction
///   "собери бэклог и приоритизируй"
///     narrow: top = "Приоритет - точность и подтверждённый реальный статус"
///             (ratio 1.000), the owner's stated priority
///     wide:   top = "POST /banks/{bank}/recall returns 404" - a diagnostics
///             detail from a past session, with the owner nowhere in the head
/// The wide query is WORSE in every case: it drags the agent's own
/// implementation chatter to the top and pushes the owner's statements down.
/// The user's words alone carry the intent; the transcript is not a query.
///
/// This also matches the upstream harness, which sends the user's prompt
/// (bounded) as the recall query - see the file header.
///
/// Falls back to the retained prose if there is no user message at all.
pub fn format_focused_query_for_relevance(messages: &[crate::message::Message]) -> String {
    let raw = format_context_for_relevance(messages);
    focus_query_text(&raw)
}

/// Pure text transform powering [`format_focused_query_for_relevance`]. Split out
/// so it can be unit-tested without constructing `Message` values.
pub fn focus_query_text(raw: &str) -> String {
    let mut kept: Vec<String> = Vec::new();
    let mut last_user: Option<String> = None;
    let mut in_reminder = false;
    let mut current_role_is_user = false;

    for line in raw.lines() {
        let trimmed = line.trim();

        // Skip system-reminder blocks (session bootstrap / memory injections).
        if trimmed.starts_with("<system-reminder>") {
            in_reminder = true;
            // A single-line reminder both opens and closes.
            if trimmed.ends_with("</system-reminder>") {
                in_reminder = false;
            }
            continue;
        }
        if in_reminder {
            if trimmed.ends_with("</system-reminder>") {
                in_reminder = false;
            }
            continue;
        }

        // Role markers emitted by format_message_context_with ("User:" / "Assistant:").
        if trimmed == "User:" {
            current_role_is_user = true;
            continue;
        }
        if trimmed == "Assistant:" {
            current_role_is_user = false;
            continue;
        }

        // Drop tool noise; keep human/assistant prose.
        if trimmed.is_empty()
            || trimmed.starts_with("[Tool:")
            || trimmed.starts_with("[Tool error:")
            || trimmed.starts_with("[Result:")
            || trimmed.starts_with("[Image]")
            || trimmed.starts_with("[OpenAI native compaction]")
        {
            continue;
        }

        // Drop lines with no word character. MEASURED 2026-11-05: on 20 of
        // 7301 live session files the focused query was "." or ".\n." and
        // Hindsight answered HTTP 422 "query must contain at least one word
        // character after normalization, input: .\n." - every one of those
        // recalls produced nothing. A punctuation-only line carries no
        // retrieval signal, so keeping it can only cost the whole recall.
        if !trimmed.chars().any(|c| c.is_alphanumeric()) {
            continue;
        }

        kept.push(trimmed.to_string());
        if current_role_is_user {
            last_user = Some(trimmed.to_string());
        }
    }

    let mut out = String::new();
    // The USER'S WORDS ARE THE QUERY. The retained prose is the fallback, not
    // the addition.
    //
    // MEASURED 2026-11-09 (see the note on
    // format_focused_query_for_relevance): appending the assistant's prose
    // after the user's message made the recall WORSE in all three measured
    // queries - it lifted the agent's own implementation chatter to the top
    // and pushed the owner's statements down. The cross-encoder scores
    // (query, candidate) PAIRS, so every extra word in the query is another
    // chance to match the wrong thing.
    if let Some(user) = last_user {
        out.push_str(&user);
    } else {
        // No user message in the window (an autonomous turn, or a session that
        // starts mid-stream). The retained prose is all the intent there is.
        out.push_str(&kept.join("\n"));
    }

    let out = out.trim();
    if !out.is_empty() {
        return out.to_string();
    }
    // Fall back to the raw relevance context, but only when it can produce a
    // usable query: Hindsight rejects a query with no word character, so an
    // empty query is better than a guaranteed 422. The callers skip the
    // recall entirely on an empty query.
    if raw.chars().any(|c| c.is_alphanumeric()) {
        raw.to_string()
    } else {
        String::new()
    }
}

/// Format messages into a wider context string for extraction.
/// Uses a larger window than relevance checking since extraction needs to
/// capture learnings from a broader portion of the conversation.
pub(crate) fn format_context_for_extraction(messages: &[crate::message::Message]) -> String {
    let mut chunks: Vec<String> = Vec::new();
    let mut total_chars = 0usize;

    for message in messages.iter().rev().take(EXTRACTION_CONTEXT_MAX_MESSAGES) {
        let chunk = format_message_context_with(message, format_content_block_for_extraction);
        if chunk.is_empty() {
            continue;
        }
        let chunk_len = chunk.chars().count();
        if total_chars + chunk_len > EXTRACTION_CONTEXT_MAX_CHARS {
            if total_chars == 0 {
                chunks.push(truncate_chars(&chunk, EXTRACTION_CONTEXT_MAX_CHARS));
            }
            break;
        }
        total_chars += chunk_len;
        chunks.push(chunk);
    }

    chunks.reverse();
    chunks.join("\n").trim().to_string()
}
