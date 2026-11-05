// reference: vectorize-io/hindsight-coding-agents, dist/dcode-hook.js
//   buildSystemInjection (:1947) and formatRecallFallback (:1942), vendored at
//   C:/Users/WorkShop1/.hindsight/coding-agents-0.8.2/dist/dcode-hook.js; and
//   hindsight_api 0.10.2 OpenAPI POST /v1/default/banks/{bank_id}/memories/recall
//   (RecallRequest: query, budget, max_tokens, prefer_observations).
//! Hindsight-backed retrieval for jcode's native memory pipeline.
//!
//! WHY THIS EXISTS
//! ---------------
//! jcode's native memory pipeline injects into the conversation as a trailing
//! USER MESSAGE before the model reasons (crates/jcode-app-core/src/agent/
//! turn_streaming_mpsc.rs), which is the correct injection point. Its
//! retrieval backend, however, is the remote JEV model: expensive, and
//! `get_relevant_parallel` currently returns an empty result for every caller,
//! so nothing is recalled at all.
//!
//! Hindsight (self-hosted, http://127.0.0.1:8888) already provides the
//! retrieval half: semantic (pgvector) + BM25 + graph + temporal arms fused
//! by RRF, then a local cross-encoder reranker. This module is the ADAPTER
//! that lets the native pipeline use it, so the injection machinery, the
//! dedup/TTL bookkeeping and the pending-memory store stay untouched.
//!
//! WHAT IT DOES NOT DO
//! -------------------
//! It does not replace the native store, and it does not write to Hindsight.
//! Retention stays where it is; this is recall only. The memories recalled
//! here are addressed by their Hindsight document ids, which is why they are
//! published through the UNSCOPED pending-memory path (see
//! `set_pending_memory_with_ids_and_display`): the scoped path validates a
//! selection against the LOCAL memory graph, and a Hindsight document is not
//! in that graph.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// How long a recall may take before it is abandoned. Measured on this host
/// (2026-11-04): 8.4-12.3 s with the cross-encoder reranker enabled, ~1-3 s
/// without. The budget is generous enough for the reranked path and short
/// enough that a hung server cannot stall the agent.
///
/// MEASURED 2026-11-05: 45 s was NOT enough. With the historical corpus
/// imported (37,918 facts) and Hindsight consolidating in the background
/// (pending_consolidation 1,947), a live recall took 25.1 s in the calm case
/// and exceeded 45 s under load, failing with "error sending request for url".
/// The recall runs on a dedicated thread inside the background memory agent,
/// so a longer timeout delays a memory update, never the model's turn.
/// 120 s leaves room for a 4x slow-down and still bounds a hung server.
const RECALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum characters of a single recalled fact. The upstream harness uses a
/// similar bound; a fact longer than this is a document, not a fact.
const MAX_FACT_CHARS: usize = 600;

#[derive(Debug, Clone)]
pub struct HindsightConfig {
    pub base_url: String,
    pub bank: String,
    /// Recall budget: "low" | "mid" | "high".
    pub budget: String,
    /// How many facts to inject.
    pub limit: usize,
}

impl Default for HindsightConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8888".to_string(),
            bank: "jcode-legacy".to_string(),
            budget: "low".to_string(),
            limit: 8,
        }
    }
}

impl HindsightConfig {
    /// Read the endpoint from the environment. `HINDSIGHT_API_URL` is the
    /// same variable the MCP harness and the hooks use, so one change moves
    /// all three.
    pub fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(url) = std::env::var("HINDSIGHT_API_URL")
            && !url.trim().is_empty()
        {
            config.base_url = url.trim().trim_end_matches('/').to_string();
        }
        if let Ok(bank) = std::env::var("HINDSIGHT_BANK")
            && !bank.trim().is_empty()
        {
            config.bank = bank.trim().to_string();
        }
        config
    }
}

/// Whether the Hindsight backend is selected for automatic recall.
///
/// The provider name is read from the agents config, so switching backends is
/// a config change and not a rebuild.
pub fn provider_enabled() -> bool {
    crate::config::config()
        .agents
        .memory_provider
        .eq_ignore_ascii_case("hindsight")
}

/// Build the recall configuration from the agents config, falling back to the
/// environment for the endpoint and bank id.
pub fn recall_configured() -> Result<HindsightConfig> {
    let agents = &crate::config::config().agents;
    let mut config = HindsightConfig::from_env();
    if let Some(url) = agents.memory_hindsight_url.as_ref()
        && !url.trim().is_empty()
    {
        config.base_url = url.trim().trim_end_matches('/').to_string();
    }
    if let Some(bank) = agents.memory_hindsight_bank.as_ref()
        && !bank.trim().is_empty()
    {
        config.bank = bank.trim().to_string();
    }
    config.limit = agents.memory_hindsight_limit;
    Ok(config)
}

/// One fact as Hindsight returned it.
#[derive(Debug, Clone)]
pub struct HindsightFact {
    /// Hindsight document id. Used for dedup tracking only.
    pub id: String,
    pub text: String,
    /// Final ranking score, when the server reported one.
    pub score: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct HindsightRecall {
    pub facts: Vec<HindsightFact>,
}

#[derive(Debug, Deserialize)]
struct RecallResponse {
    #[serde(default)]
    results: Vec<RecallResult>,
}

#[derive(Debug, Deserialize)]
struct RecallResult {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    scores: Option<RecallScores>,
}

#[derive(Debug, Deserialize)]
struct RecallScores {
    #[serde(default, rename = "final")]
    r#final: Option<f64>,
}

/// Recall facts for `query` from the configured bank.
///
/// Fails soft by returning an empty result on any transport or parse error:
/// a memory system that can break a turn is worse than no memory system.
pub fn recall(config: &HindsightConfig, query: &str) -> Result<HindsightRecall> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(HindsightRecall { facts: Vec::new() });
    }
    // The server truncates long queries itself; keep the request bounded so a
    // pathological transcript cannot produce a multi-megabyte request body.
    let query: String = query.chars().take(1200).collect();

    let url = format!(
        "{}/v1/default/banks/{}/memories/recall",
        config.base_url, config.bank
    );
    let body = serde_json::json!({
        "query": query,
        "budget": config.budget,
        "max_tokens": 1200,
        // Observations supersede the raw facts they were consolidated from,
        // so the agent sees one consolidated fact instead of its ingredients.
        "prefer_observations": true,
    });

    let text = post_json(&url, &body)?;

    let parsed: RecallResponse = serde_json::from_str(&text)
        .with_context(|| format!("parse Hindsight recall response from {url}"))?;

    let mut facts = Vec::new();
    for item in parsed.results {
        let Some(raw) = item.text else { continue };
        let cleaned = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if cleaned.is_empty() {
            continue;
        }
        facts.push(HindsightFact {
            id: item.id.unwrap_or_default(),
            text: cleaned,
            score: item.scores.and_then(|s| s.r#final),
        });
        if facts.len() >= config.limit {
            break;
        }
    }
    Ok(HindsightRecall { facts })
}

/// Render recalled facts as the memory block the agent receives.
///
/// The wording is deliberately close to the upstream Hindsight harness
/// (coding-agents dist/dcode-hook.js buildSystemInjection): the model applies
/// a fact AS GIVEN, and says so visibly when it does. A block that only
/// lists facts is a block the model treats as background noise.
pub fn format_recall_block(recall: &HindsightRecall) -> Option<String> {
    if recall.facts.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# Memory\n\n\
         Retrieved from this workspace's own memory (Hindsight bank). Real memory, but \
         retrieval is heuristic: it may or may not bear on the current task.\n\
         If a fact states an exact rule or literal values (specific strings, numbers, \
         set members, mappings), apply it AS GIVEN rather than substituting a plausible \
         alternative, and verify it against the current state before acting. When a fact \
         informs part of your answer, attribute it visibly, starting with:\n\
         > \u{1F9E0} **From memory** — <the specific facts you drew on>\n\
         Never attribute memory that did not contribute. If a fact turns out to be wrong \
         or outdated, correct the record instead of propagating it.\n\n",
    );
    for fact in &recall.facts {
        out.push_str("- ");
        // The bound is enforced HERE, at the rendering boundary, not only in
        // `recall`: this is the single place every fact passes through, so
        // the block cannot carry an unbounded line whatever produced it.
        if fact.text.chars().count() > MAX_FACT_CHARS {
            let mut head: String = fact.text.chars().take(MAX_FACT_CHARS).collect();
            head.push('…');
            out.push_str(&head);
        } else {
            out.push_str(&fact.text);
        }
        out.push('\n');
    }
    Some(out)
}

/// POST JSON and return the response body as text.
///
/// The call runs on a dedicated thread so it is safe from inside a tokio
/// worker: `reqwest::blocking` panics when used from an async context.
fn post_json(url: &str, body: &serde_json::Value) -> Result<String> {
    let url = url.to_string();
    let body = body.clone();
    std::thread::scope(|scope| {
        scope
            .spawn(move || -> Result<String> {
                let client = reqwest::blocking::Client::builder()
                    .timeout(RECALL_TIMEOUT)
                    .build()?;
                let resp = client
                    .post(&url)
                    .header("Content-Type", "application/json")
                    .json(&body)
                    .send()?;
                let status = resp.status();
                let text = resp.text()?;
                if !status.is_success() {
                    anyhow::bail!(
                        "Hindsight recall failed ({status}): {}",
                        text.chars().take(300).collect::<String>()
                    );
                }
                Ok(text)
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Hindsight recall worker thread panicked"))?
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_yields_no_facts() {
        let config = HindsightConfig::default();
        let recall = recall(&config, "   ").expect("empty query must not fail");
        assert!(recall.facts.is_empty());
    }

    #[test]
    fn block_is_none_without_facts() {
        assert!(format_recall_block(&HindsightRecall { facts: Vec::new() }).is_none());
    }

    #[test]
    fn block_contains_facts_and_the_attribution_contract() {
        let recall = HindsightRecall {
            facts: vec![HindsightFact {
                id: "abc".into(),
                text: "the exact value is 42".into(),
                score: Some(0.9),
            }],
        };
        let block = format_recall_block(&recall).expect("block");
        assert!(block.contains("the exact value is 42"));
        assert!(block.contains("apply it AS GIVEN"));
        assert!(block.contains("From memory"));
    }

    #[test]
    fn long_fact_is_truncated() {
        let recall = HindsightRecall {
            facts: vec![HindsightFact {
                id: "x".into(),
                text: "a".repeat(MAX_FACT_CHARS * 2),
                score: None,
            }],
        };
        let block = format_recall_block(&recall).expect("block");
        // The fact line itself must be bounded: "- " + MAX_FACT_CHARS chars
        // + the ellipsis marker.
        let fact_line = block
            .lines()
            .find(|line| line.starts_with("- "))
            .expect("a fact line");
        assert!(
            fact_line.chars().count() <= MAX_FACT_CHARS + 4,
            "the fact line must be truncated to MAX_FACT_CHARS, got {} chars",
            fact_line.chars().count()
        );
        assert!(!block.contains(&"a".repeat(MAX_FACT_CHARS + 1)));
    }

    #[test]
    fn recall_reports_an_unreachable_server_as_an_error() {
        // Port 1 is never a Hindsight server; the call must not silently
        // pretend it recalled nothing.
        let config = HindsightConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            ..HindsightConfig::default()
        };
        let result = recall(&config, "anything");
        assert!(result.is_err(), "an unreachable server is an error");
    }
}
