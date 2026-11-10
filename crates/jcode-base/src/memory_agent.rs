//! Nonblocking Jev recall coordinator.
//!
//! reference: https://hindsight.vectorize.io/0.10/developer/api/recall
//!   (the upstream recall contract this coordinator publishes against; the
//!   mental-models half is documented at
//!   https://hindsight.vectorize.io/developer/mental-models)
//!
//! Recall sends scoped local memories directly to Jev's typed Decisions API.
//! It never loads an embedder or calls a text-generating sidecar. Optional
//! periodic/final extraction is independent and can be disabled separately.
use crate::memory::{self, MemoryManager};
use crate::memory_types::{MemoryEventKind, MemoryState};
use anyhow::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const CONTEXT_CHANNEL_CAPACITY: usize = 16;
const PERIODIC_EXTRACTION_INTERVAL: usize = 12;
const REPEAT_SUPPRESSION: Duration = Duration::from_secs(30);
const FAILURE_BACKOFF: Duration = Duration::from_secs(60);
static MEMORY_AGENT: tokio::sync::OnceCell<MemoryAgentHandle> = tokio::sync::OnceCell::const_new();

/// Retain the public diagnostics shape for older clients. Jev does not need
/// embedding-cluster maintenance; those counters remain zero.
#[derive(Debug, Clone, Default)]
pub struct MemoryAgentStats {
    pub turns_processed: usize,
    pub maintenance_runs: usize,
    pub last_maintenance_ms: Option<u64>,
}
static MEMORY_AGENT_STATS: Mutex<MemoryAgentStats> = Mutex::new(MemoryAgentStats {
    turns_processed: 0,
    maintenance_runs: 0,
    last_maintenance_ms: None,
});

pub fn build_transcript_for_extraction(messages: &[crate::message::Message]) -> String {
    let mut transcript = String::new();
    for msg in messages {
        let role = match msg.role {
            crate::message::Role::User => "User",
            crate::message::Role::Assistant => "Assistant",
        };
        transcript.push_str(&format!("**{}:**\n", role));
        for block in &msg.content {
            match block {
                crate::message::ContentBlock::Text { text, .. } => {
                    if text.trim_start().starts_with("<system-reminder>") {
                        continue;
                    }
                    transcript.push_str(text);
                    transcript.push('\n');
                }
                crate::message::ContentBlock::ToolUse { name, .. } => {
                    transcript.push_str(&format!("[Used tool: {}]\n", name));
                }
                crate::message::ContentBlock::ToolResult { content, .. } => {
                    let preview = if content.len() > 200 {
                        format!("{}...", crate::util::truncate_str(content, 200))
                    } else {
                        content.clone()
                    };
                    transcript.push_str(&format!("[Result: {}]\n", preview));
                }
                crate::message::ContentBlock::Reasoning { .. }
                | crate::message::ContentBlock::ReasoningTrace { .. }
                | crate::message::ContentBlock::AnthropicThinking { .. }
                | crate::message::ContentBlock::OpenAIReasoning { .. }
                | crate::message::ContentBlock::ToolReference { .. }
                | crate::message::ContentBlock::ProviderNative { .. } => {}
                crate::message::ContentBlock::Image { .. } => {
                    transcript.push_str("[Image]\n");
                }
                crate::message::ContentBlock::OpenAICompaction { .. } => {
                    transcript.push_str("[OpenAI native compaction]\n");
                }
            }
        }
        transcript.push('\n');
    }
    transcript
}

fn manager_for_working_dir(working_dir: Option<&str>) -> MemoryManager {
    match working_dir {
        Some(dir) if !dir.trim().is_empty() => MemoryManager::new().with_project_dir(dir),
        _ => MemoryManager::new(),
    }
}

async fn run_final_extraction(transcript: String, session_id: String, working_dir: Option<String>) {
    // Extraction is optional. Its model/credentials never gate recall.
    if !memory::memory_llm_judge_available() {
        return;
    }
    let manager = manager_for_working_dir(working_dir.as_deref());
    match manager
        .extract_from_transcript(&transcript, &session_id)
        .await
    {
        Ok(ids) => {
            memory::mark_memories_known(&session_id, &ids, "extracted from this session");
            memory::add_event(MemoryEventKind::ExtractionComplete { count: ids.len() });
        }
        Err(_) => crate::logging::info("Optional memory extraction failed"),
    }
}

/// Publish the "reopen a good idea at the right time" notice, if any.
///
/// The trigger half runs on the same turn as the recall, but it is INDEPENDENT
/// of it: a session that recalls no facts may still have a deferred goal whose
/// condition just became true. The notice arrives as its own memory-block
/// section, so the shipped renderer gives it its own tile.
///
/// A failure here is never fatal: the plan treats a failed decision as
/// "stay quiet", and a broken trigger must not break the recall.
///
/// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
///   ("If the free tier only changed its name, do nothing."; "Retain the
///   verified outcome or rejection and link it to the same goal.").
fn publish_reopen_notice(session_id: &str, working_dir: Option<&str>) {
    let notice = match crate::goal_review::run_trigger(working_dir.map(std::path::Path::new)) {
        Ok(Some(notice)) => notice,
        Ok(None) => return,
        Err(error) => {
            crate::logging::info(&format!("reopen trigger failed: {error}"));
            return;
        }
    };
    // The notice is external content: it describes what the bank reported and
    // what `reflect` decided about it. It is published on its OWN section so
    // it cannot be blended into the recall facts, and it is MERGED into the
    // pending payload rather than replacing it: the session's pending slot
    // holds one payload, so a plain replace made the notice displace the
    // recall facts published moments earlier in the same turn.
    //
    // reference: crates/jcode-base/src/memory/pending.rs
    //   (`append_pending_memory_external` - the merging publisher).
    memory::append_pending_memory_external(
        session_id,
        notice.clone(),
        1,
        vec![format!("reopen:{}", crate::goal_review::REOPEN_SECTION)],
        None,
        working_dir,
    );
}

/// Handle to communicate with the memory agent
#[derive(Clone)]
pub struct MemoryAgentHandle {
    /// Send messages to the agent
    tx: mpsc::Sender<AgentMessage>,
}

impl MemoryAgentHandle {
    /// Send a context update to the memory agent (async)
    pub async fn update_context(
        &self,
        session_id: &str,
        messages: Arc<[crate::message::Message]>,
        working_dir: Option<String>,
    ) {
        self.update_context_sync_with_dir(session_id, messages, working_dir);
    }

    pub fn update_context_sync(&self, session_id: &str, messages: Arc<[crate::message::Message]>) {
        self.update_context_sync_with_dir(session_id, messages, None);
    }

    pub fn update_context_sync_with_dir(
        &self,
        session_id: &str,
        messages: Arc<[crate::message::Message]>,
        working_dir: Option<String>,
    ) {
        self.update_context_sync_with_dir_forced(session_id, messages, working_dir, false);
    }

    /// Send a context update that bypasses the same-query suppression.
    ///
    /// MEASURED 2026-11-04: during long autonomous work the focused query
    /// often does not change (the tool output is stripped from it), so the
    /// 30 s repeat suppression blocked every scheduled refresh and the agent
    /// ran for hours with the memory it started with.
    pub fn update_context_sync_with_dir_forced(
        &self,
        session_id: &str,
        messages: Arc<[crate::message::Message]>,
        working_dir: Option<String>,
        force: bool,
    ) {
        let msg = AgentMessage::Context {
            session_id: session_id.to_string(),
            messages,
            working_dir,
            timestamp: Instant::now(),
            force,
        };
        let _ = self.tx.try_send(msg);
    }

    /// Reset all memory agent state (call on new session)
    pub fn reset(&self) {
        let _ = self.tx.try_send(AgentMessage::Reset);
    }
}

/// Messages sent to the memory agent
enum AgentMessage {
    Context {
        session_id: String,
        messages: Arc<[crate::message::Message]>,
        working_dir: Option<String>,
        timestamp: Instant,
        /// Bypass the same-query repeat suppression.
        force: bool,
    },
    Reset,
}

#[derive(Default)]
struct SessionState {
    working_dir: Option<String>,
    last_query: Option<String>,
    last_check: Option<Instant>,
    failed_at: Option<Instant>,
    turns: usize,
}

pub struct MemoryAgent {
    rx: mpsc::Receiver<AgentMessage>,
    sessions: HashMap<String, SessionState>,
}

impl MemoryAgent {
    fn new(rx: mpsc::Receiver<AgentMessage>) -> Self {
        Self {
            rx,
            sessions: HashMap::new(),
        }
    }

    fn reset(&mut self) {
        self.sessions.clear();
        memory::clear_all_pending_memory();
        memory::clear_all_injected_memories();
        if let Ok(mut stats) = MEMORY_AGENT_STATS.lock() {
            *stats = MemoryAgentStats::default();
        }
    }

    async fn run(mut self) {
        while let Some(message) = self.rx.recv().await {
            match message {
                AgentMessage::Reset => self.reset(),
                AgentMessage::Context {
                    session_id,
                    messages,
                    working_dir,
                    timestamp,
                    force,
                } => {
                    // A bounded queue must not recall for a context that aged out
                    // while another session's remote provider was unavailable.
                    if timestamp.elapsed() > Duration::from_secs(120) {
                        continue;
                    }
                    let ss = self.sessions.entry(session_id.clone()).or_default();
                    // None is meaningful: never retain another project's scope.
                    if ss.working_dir != working_dir {
                        *ss = SessionState {
                            working_dir,
                            ..Default::default()
                        };
                        // The session moved to another project: its delivered
                        // set belongs to the old scope and must not survive.
                        memory::clear_pending_memory(&session_id);
                    }
                    ss.turns += 1;
                    if let Ok(mut stats) = MEMORY_AGENT_STATS.lock() {
                        stats.turns_processed += 1;
                    }
                    if let Err(error) = self.process_context(&session_id, &messages, force).await {
                        memory::clear_pending_memory(&session_id);
                        self.sessions.entry(session_id).or_default().failed_at =
                            Some(Instant::now());
                        memory::add_event(MemoryEventKind::Error {
                            message: error.to_string(),
                        });
                        memory::set_state(MemoryState::Idle);
                    }
                }
            }
        }
    }

    async fn process_context(
        &mut self,
        session_id: &str,
        messages: &[crate::message::Message],
        force: bool,
    ) -> Result<()> {
        let query = memory::format_focused_query_for_relevance(messages);
        if query.trim().is_empty() {
            return Ok(());
        }
        let ss = self.sessions.entry(session_id.to_string()).or_default();
        // The working directory is copied OUT of the session entry before the
        // recall path borrows `self.sessions` again: the entry borrow is
        // mutable and lives to the end of this function, so a second borrow
        // of the map inside it does not compile.
        let session_working_dir = ss.working_dir.clone();
        // Optional writing is fire-and-forget and never delays a recall request.
        if ss.turns > 0
            && ss.turns.is_multiple_of(PERIODIC_EXTRACTION_INTERVAL)
            && memory::memory_llm_judge_available()
        {
            trigger_final_extraction_with_dir(
                memory::format_context_for_extraction(messages),
                session_id.to_string(),
                ss.working_dir.clone(),
            );
        }
        if ss
            .failed_at
            .is_some_and(|at| at.elapsed() < FAILURE_BACKOFF)
        {
            return Ok(());
        }
        if !force
            && ss.last_query.as_deref() == Some(query.as_str())
            && ss
                .last_check
                .is_some_and(|at| at.elapsed() < REPEAT_SUPPRESSION)
        {
            return Ok(());
        }
        ss.last_query = Some(query.clone());
        ss.last_check = Some(Instant::now());
        let manager = manager_for_working_dir(ss.working_dir.as_deref());
        // Only the QUEUED payload is dropped here. The bookkeeping of what
        // this session already received MUST survive, otherwise the same
        // facts and models are delivered again on the next turn.
        //
        // MEASURED 2026-11-10 on the live daemon: this call used to be
        // `clear_pending_memory`, which also cleared the injected-id set and
        // the last-injected-set map. Both dedup lines were dead, and one
        // session received the identical payload (6 facts + 2 models) twice
        // within 26 seconds.
        memory::clear_pending_payload(session_id);
        // The Hindsight backend replaces the remote Jev model for RECALL.
        // It is selected here rather than inside MemoryManager because the
        // two backends produce different things: Jev selects entries from the
        // LOCAL memory graph, Hindsight returns its own documents, which are
        // not in that graph. Both end up in the same pending-memory store, so
        // the injection, dedup and TTL machinery below is unchanged.
        if crate::memory_hindsight::provider_enabled() {
            // The trigger half of "reopen a good idea at the right time" runs on its
    // OWN payload, and it is INDEPENDENT of the recall: a session that recalls
    // no facts may still have a deferred goal whose condition just became
    // true.
    //
    // MEASURED 2026-11-11 on the source: the notice used to be published
    // INSIDE the `Ok(recall)` arm, so a session whose recall failed, whose
    // recall config was invalid, or whose recall was skipped by the
    // repeat-suppression guard received NO reopen notice at all. The
    // comment claimed the opposite, and the truth was the reverse: the
    // session that recalls LEAST was the one that lost the notice. The
    // trigger reads the goal store and the bank, not the recall result, so
    // it must not be gated on it.
    //
    // It is published AFTER the recall, because the recall publishes with a
    // REPLACING setter: a notice published first would be displaced by the
    // recall block. The notice itself publishes with the MERGING setter, so
    // it lands ON TOP of the recall facts instead of replacing them.
    //
    // reference: crates/jcode-base/src/memory/pending.rs
    //   (`append_pending_memory_external` - the merging publisher).
    // reference: https://hindsight.vectorize.io/0.10/developer/api/reflect
    //   (the upstream contract behind the notice: the trigger asks `reflect`
    //   for a decision about a changed entity, and the decision - not the
    //   recall - is what makes a deferred goal worth reopening).
    // reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
    //   ("When a trusted source reports a relevant change, resolve the
    //   product or capability to its entity. Retrieve affected open goals,
    //   candidate evidence about the change, the original constraint and any
    //   later cancellation.").

    let recall = match crate::memory_hindsight::recall_configured() {
                Ok(config) => crate::memory_hindsight::recall(&config, query.as_str()),
                Err(error) => {
                    crate::memory_log::log_pending_discarded(
                        session_id,
                        &format!("hindsight recall config invalid: {error}"),
                    );
                    publish_reopen_notice(session_id, session_working_dir.as_deref());
                    return Ok(());
                }
            };
            match recall {
                Ok(mut recall) => {
                    // A mental model is a DOCUMENT, not a fact. Once the agent
                    // has read it, sending it again costs context and adds
                    // nothing: the model does not change between turns, while
                    // the facts that travel with it DO.
                    //
                    // MEASURED 2026-11-10 on the live daemon log: 1055 model
                    // excerpts were delivered across 92 sessions, and only 179
                    // (17.0%) were a model's FIRST delivery in its session;
                    // 772 (73.2%) were a repeat inside the 45-minute
                    // injected-id TTL, and 104 (9.9%) after it. The payload
                    // level dedup could not catch this, because it suppresses
                    // a WHOLE payload, and the facts keep changing.
                    //
                    // The model ids are tracked in the same injected-id set as
                    // the facts, prefixed with "model:" so they cannot collide
                    // with a Hindsight fact id.
                    let selected_models = recall.models.len();
                    recall.models.retain(|model| {
                        !memory::is_memory_injected(
                            session_id,
                            &format!("model:{}", model.id),
                        )
                    });
                    let suppressed_models = selected_models - recall.models.len();
                    if suppressed_models > 0 {
                        crate::logging::info(&format!(
                            "[{}] Suppressed {} already-read mental model(s)",
                            session_id, suppressed_models
                        ));
                    }
                    if let Some(prompt) = crate::memory_hindsight::format_recall_block(&recall) {
                        let count = recall.facts.len();
                        // The ids tracked for the injected-set bookkeeping
                        // cover the FACTS and the MODELS.
                        //
                        // MEASURED 2026-11-10: only the fact ids were
                        // tracked, so a model whose text was already in the
                        // transcript could be delivered again on the next
                        // turn - the "same thing every time" complaint had a
                        // second cause here. The model ids are prefixed so
                        // they cannot collide with Hindsight fact ids.
                        let mut ids: Vec<String> = recall
                            .facts
                            .iter()
                            .map(|fact| fact.id.clone())
                            .filter(|id| !id.is_empty())
                            .collect();
                        ids.extend(
                            recall
                                .models
                                .iter()
                                .map(|model| format!("model:{}", model.id))
                                .filter(|id| id.len() > "model:".len()),
                        );
                        // EXTERNAL publication: these memories live in the
                        // Hindsight bank, not in the local graph, so the
                        // project-scoped path would reject the selection.
                        memory::set_pending_memory_external(
                            session_id,
                            prompt,
                            count,
                            ids,
                            None,
                            session_working_dir.as_deref(),
                        );
                    }
                }
                Err(error) => {
                    crate::memory_log::log_pending_discarded(
                        session_id,
                        &format!("hindsight recall failed: {error}"),
                    );
                }
            }
            // Published after the recall, for BOTH outcomes of the match
            // above: the notice must reach the agent even when the recall
            // failed. It merges into the payload the recall queued.
            publish_reopen_notice(session_id, session_working_dir.as_deref());
            self.sessions
                .entry(session_id.to_string())
                .or_default()
                .failed_at = None;
            return Ok(());
        }
        if !memory::memory_runtime_active() {
            return Ok(());
        }
        let memory::MemoryRelevanceResult {
            prompt,
            display_prompt,
            selected_entries,
        } = manager
            .get_relevant_parallel(session_id, messages, None)
            .await?;
        if let Some(prompt) = prompt {
            let count = selected_entries.len();
            memory::set_pending_memory_for_project_with_selection(
                session_id,
                prompt,
                count,
                &selected_entries,
                display_prompt,
                self.sessions
                    .get(session_id)
                    .and_then(|state| state.working_dir.as_deref()),
            );
        }
        self.sessions
            .entry(session_id.to_string())
            .or_default()
            .failed_at = None;
        Ok(())
    }
}

pub async fn init() -> Result<MemoryAgentHandle> {
    let handle = MEMORY_AGENT
        .get_or_init(|| async {
            let (tx, rx) = mpsc::channel(CONTEXT_CHANNEL_CAPACITY);

            // Spawn the memory agent task
            let agent = MemoryAgent::new(rx);
            tokio::spawn(agent.run());

            MemoryAgentHandle { tx }
        })
        .await;

    Ok(handle.clone())
}

/// Get the global memory agent handle (if initialized)
pub fn get() -> Option<MemoryAgentHandle> {
    MEMORY_AGENT.get().cloned()
}

/// Send a context update to the memory agent (convenience function)
pub async fn update_context(
    session_id: &str,
    messages: Arc<[crate::message::Message]>,
    working_dir: Option<String>,
) {
    if let Some(handle) = get() {
        handle
            .update_context(session_id, messages, working_dir)
            .await;
    }
}

/// Send a context update synchronously (for use from non-async code)
/// This is non-blocking - it just sends to the channel
pub fn update_context_sync(session_id: &str, messages: Arc<[crate::message::Message]>) {
    update_context_sync_with_dir(session_id, messages, None);
}

pub fn update_context_sync_with_dir(
    session_id: &str,
    messages: Arc<[crate::message::Message]>,
    working_dir: Option<String>,
) {
    if let Some(handle) = get() {
        handle.update_context_sync_with_dir(session_id, messages, working_dir);
    } else {
        let sid = session_id.to_string();
        tokio::spawn(async move {
            if let Ok(handle) = init().await {
                handle.update_context_sync_with_dir(&sid, messages, working_dir);
            }
        });
    }
}

/// Force a context update, bypassing the same-query repeat suppression.
///
/// Used by the scheduled memory refresh: during long autonomous work the
/// focused query usually does not change, so the normal suppression window
/// would silently block every refresh.
pub fn update_context_sync_with_dir_forced(
    session_id: &str,
    messages: Arc<[crate::message::Message]>,
    working_dir: Option<String>,
) {
    if let Some(handle) = get() {
        handle.update_context_sync_with_dir_forced(session_id, messages, working_dir, true);
    } else {
        let sid = session_id.to_string();
        tokio::spawn(async move {
            if let Ok(handle) = init().await {
                handle.update_context_sync_with_dir_forced(&sid, messages, working_dir, true);
            }
        });
    }
}

/// Reset the memory agent state (call on new session)
/// This clears per-session recall state and pending injections.
pub fn reset() {
    if let Some(handle) = get() {
        handle.reset();
    }
}

/// Trigger a final memory extraction when a session ends.
///
/// This is fire-and-forget: spawns a tokio task that runs extraction
/// and logs the result. Does not block the caller.
pub fn trigger_final_extraction(transcript: String, session_id: String) {
    trigger_final_extraction_with_dir(transcript, session_id, None);
}

pub fn trigger_final_extraction_with_dir(
    transcript: String,
    session_id: String,
    working_dir: Option<String>,
) {
    if transcript.len() < 200 {
        return;
    }

    crate::memory_log::log_final_extraction(&session_id, transcript.len());

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(run_final_extraction(transcript, session_id, working_dir));
    } else {
        std::thread::spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => {
                    runtime.block_on(run_final_extraction(transcript, session_id, working_dir))
                }
                Err(err) => crate::logging::info(&format!(
                    "Final extraction runtime startup failed: {}",
                    err
                )),
            }
        });
    }
}

/// Check if the memory agent is currently processing (has been initialized)
pub fn is_active() -> bool {
    get().is_some()
}

/// Snapshot memory-agent runtime stats for UI/debug.
pub fn stats() -> MemoryAgentStats {
    MEMORY_AGENT_STATS
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "memory_agent_tests.rs"]
mod tests;
