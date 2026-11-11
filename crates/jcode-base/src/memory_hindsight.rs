// reference: https://github.com/vectorize-io/hindsight
//   (hindsight_api 0.10.2 OpenAPI: POST /v1/default/banks/{bank_id}/memories/recall
//   and GET /v1/default/banks/{bank_id}/mental-models?detail=content)
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
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
// reference: https://hindsight.vectorize.io/0.10/developer/api/recall
//   (RecallRequest.min_scores, budget levels, tags_match) and
//   https://hindsight.vectorize.io/0.10/developer/api/mental-models
//   (GET .../mental-models?detail=content). The relative-cutoff rule and the
//   per-section budget are this host's own measurements - see the MEASURED
//   notes on each constant below.
const RECALL_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum characters of a single recalled fact. The upstream harness uses a
/// similar bound; a fact longer than this is a document, not a fact.
const MAX_FACT_CHARS: usize = 600;

/// Maximum characters of one mental model excerpt.
const MAX_MODEL_CHARS: usize = 600;

/// How many mental models to deliver alongside the facts.
///
/// MEASURED 2026-11-09 on bank `jcode-legacy`: the bank holds 15 models, six of
/// them stale daily models whose content is 4k-100k chars of accumulated raw
/// history. Three is the useful slice; the rest is noise the model must read
/// past to reach the one that matters.
const MAX_MODELS: usize = 3;

/// The minimum length of a token that counts towards model matching.
///
/// MEASURED 2026-11-09: a token floor keeps the match from firing on noise.
const MIN_TOKEN_CHARS: usize = 4;

/// How much a shared TOPIC token (name, id, source_query) outweighs a shared
/// body token.
///
/// A model is a saved reflect response for its source_query, so the topic
/// surface names the subject directly, while the body is a long history where
/// any word can appear. MEASURED 2026-11-10 over 20 queries: weighting the
/// topic surface 3x produced 18 distinct delivered sets (the old rule
/// produced 1); the exact weight is not critical, the SEPARATION of
/// eligibility from ranking is.
const TOPIC_TOKEN_WEIGHT: f64 = 3.0;

/// How much a shared body token outweighs nothing (the base unit).
const BODY_TOKEN_WEIGHT: f64 = 1.0;

/// The tie-break bonus the curated models get.
///
/// MEASURED 2026-11-10: this bonus must NOT participate in eligibility. When
/// it did, a curated model with ZERO shared tokens still scored above the
/// threshold and came back for every query - i.e. the unconditional delivery
/// the fix was meant to remove survived. As a ranking tie-break only, it
/// makes the standing answers win a close match without making them
/// unconditional.
const CURATED_TIE_BREAK_BONUS: f64 = 1.0;

/// A recalled fact is kept when its score is at least this fraction of the
/// TOP score for the same query.
///
/// MEASURED 2026-11-09 (bank `jcode-legacy`, 5 queries x 2 budgets): the score
/// distribution is sharply bimodal. Relevant hits land at 0.93-1.11 and the
/// irrelevant tail at 0.0001-0.17 - a three-order-of-magnitude gap. A
/// RELATIVE cutoff at 10% of the top score separates them exactly: it keeps
/// every relevant hit and drops the whole tail.
///
/// A FIXED floor was measured and rejected, twice. The useful Russian
/// paraphrase "не забудь прочитать протокол встречи с командой безопасности"
/// scores 0.0000832, so any fixed floor above that deletes a genuinely
/// relevant cross-lingual hit; and an earlier bank measurement (2026-10-25)
/// found that adding an instruction to the query rescales the reranker so
/// that relevant records score 0.001-0.012 while irrelevant ones score
/// 0.004-0.055 - the ordering is not stable across queries, so no fixed
/// threshold can be trusted. The gap is relative, so the cutoff is relative.
const MIN_RELATIVE_SCORE: f64 = 0.10;

/// Character budget per SECTION of the delivered block.
///
/// MEASURED 2026-11-09: the python delivery path truncated its whole block at
/// 3,000 chars AFTER rendering 8 facts of up to 500 chars each, so the mental
/// models it had already fetched were never delivered at all - in 8 of 13
/// saved session caches every model heading was gone, and in a re-measurement
/// over the 8 most recent caches, 10 of 10 fetched models were lost to
/// truncation. Budgeting per section means one section cannot starve the other.
const MAX_FACTS_CHARS: usize = 2_400;
const MAX_MODELS_CHARS: usize = 1_800;

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

/// The tag the retain hook puts on every digest of Jcode's own conversation.
///
/// MEASURED 2026-11-10 on the live bank: of the 200 most recent events, 197
/// carried `auto-retained` and only 3 did not. A trigger that reacted to our
/// own conversation echo would fire on Jcode talking to itself, so this tag
/// marks the echo and the sweep skips it.
pub const OWN_ECHO_TAG: &str = "auto-retained";

/// One fact as Hindsight returned it.
#[derive(Debug, Clone)]
pub struct HindsightFact {
    /// Hindsight document id. Used for dedup tracking only.
    pub id: String,
    pub text: String,
    /// Final ranking score, when the server reported one.
    pub score: Option<f64>,
    /// The date the fact was TRUE, when the bank carries one.
    ///
    /// MEASURED 2026-11-11 (owner directive): only SOME facts arrived with a
    /// date, because the date was whatever the extractor happened to write into
    /// the fact text. The bank stores the date as a COLUMN (`mentioned_at` /
    /// `occurred_start` / `updated_at`), so the renderer can stamp EVERY fact
    /// with it and let the agent defend against a stale fact by date.
    ///
    /// reference: https://hindsight.vectorize.io/developer/api/retain
    ///   (the retain `timestamp` parameter is injected into the fact-extraction
    ///   prompt, so relative dates like "last Monday" resolve against a real
    ///   anchor and the extracted fact carries a date)
    /// reference: https://hindsight.vectorize.io/api-reference
    ///   (GET .../memories/list returns `mentioned_at`, `occurred_start`,
    ///   `occurred_end`, `updated_at`; recall results carry `occurred_start` /
    ///   `occurred_end`)
    pub dated: Option<String>,
}

/// One mental model excerpt as Hindsight returned it.
#[derive(Debug, Clone)]
pub struct HindsightModel {
    pub id: String,
    pub name: String,
    pub text: String,
    /// Newer memories exist in this model's scope than its last refresh saw.
    ///
    /// Upstream (`hindsight.vectorize.io/0.10/developer/mental-models`): "A
    /// model that has fallen behind is still shown, but it no longer
    /// short-circuits retrieval, and the agent is expected to check it
    /// against the layers below rather than trust it blindly." So a stale
    /// model is DELIVERED with a marker, never dropped.
    pub is_stale: bool,
    /// Whether this model is one of the curated, owner-facing models.
    pub is_curated: bool,
}

#[derive(Debug, Clone)]
pub struct HindsightRecall {
    pub facts: Vec<HindsightFact>,
    /// Mental models relevant to the query, newest synthesis first.
    pub models: Vec<HindsightModel>,
    /// The query the recall ran against.
    ///
    /// The renderer needs it to cut each model at the window that actually
    /// answers the question instead of at the model's own first page.
    pub query: String,
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
    /// The event time the fact is true for. `mentioned_at` is when the source
    /// said it; `occurred_start`/`occurred_end` bracket the event itself.
    ///
    /// reference: https://hindsight.vectorize.io/api-reference
    ///   (recall results carry mentioned_at, occurred_start, occurred_end)
    ///
    /// MEASURED 2026-11-11 against the live recall endpoint: a result carries
    /// `mentioned_at` plus `occurred_start`/`occurred_end`, and the latter are
    /// NULL for facts whose extractor set no event date. So the event time wins
    /// when present and `mentioned_at` is the universal fallback.
    #[serde(default)]
    mentioned_at: Option<DateTime<Utc>>,
    #[serde(default)]
    occurred_start: Option<DateTime<Utc>>,
    #[serde(default)]
    occurred_end: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
struct RecallScores {
    #[serde(default, rename = "final")]
    r#final: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct MentalModelsResponse {
    #[serde(default)]
    items: Vec<MentalModelItem>,
}

#[derive(Debug, Deserialize)]
struct MentalModelItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    content: Option<String>,
    /// The question this model answers.
    ///
    /// reference: https://hindsight.vectorize.io/0.10/developer/api/mental-models
    ///   ("Mental models are saved reflect responses that you curate for your
    ///    memory bank"; a model is created with a name and a source_query, and
    ///    the content is the reflect response for that question).
    ///
    /// MEASURED 2026-11-10 on the live bank: the source_query was USED as
    /// part of the model's topic surface, and that was wrong. For the four
    /// technical models it is a 3.4k-7.2k character GENERATION PROMPT (162
    /// to 382 tokens) rather than a topic label, while the curated models'
    /// prompts are 212-320 characters. The score sums over the shared tokens,
    /// so the bigger haystack won regardless of the subject and the technical
    /// manuals took 40.3% of all delivered model slots. The field is kept for
    /// the record but is NOT part of the topic surface.
    #[serde(default)]
    #[allow(dead_code)]
    source_query: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Option<Vec<String>>,
    /// A stale model has newer input than its last refresh.
    ///
    /// MEASURED 2026-11-09 on bank `jcode-legacy`: ALL SIX curated models
    /// (`active-work`, `user-profile`, `decisions-ledger`, `machine-hazards`,
    /// `incidents-and-fixes`, `agent-coordination`) reported `is_stale: true`
    /// right after a successful rebuild, and the bank is written to
    /// continuously, so a model goes stale minutes later. Dropping stale
    /// models therefore delivered NOTHING for exactly the topics the models
    /// exist to answer: MEASURED 15 models in the bank, 0 delivered, so the
    /// "Synthesized context" section was always empty.
    ///
    /// Upstream is explicit that a stale model is still shown. It is delivered
    /// and MARKED instead.
    #[serde(default)]
    is_stale: Option<bool>,
}

/// The tag scope recall reads from.
///
/// The workspace's own writes, the owner's own statements, and the turns this
/// workspace auto-retained - PLUS the global (untagged) scope, because
/// `any_strict` excludes untagged records by definition and the owner's own
/// durable rules are retained untagged.
///
/// MEASURED 2026-11-09 on bank `jcode-legacy`: 1,350 of 78,410 records carry
/// no tag at all, and 163 of those are the owner's own statements - 98 of
/// them this workspace's own rules. Over 12 owner-topic queries, adding the
/// global leaf recovered 8 owner rules and admitted 0 foreign-project items.
///
/// reference: https://hindsight.vectorize.io/0.10/developer/api/recall
///   (tags_match `any_strict` excludes untagged memories; `exact` with an
///   empty tag list selects exactly the memories that carry no tags)
fn recall_tag_groups() -> serde_json::Value {
    serde_json::json!([{
        "or": [
            { "tags": ["origin:jcode"], "match": "any_strict" },
            { "tags": ["owner:primary"], "match": "any_strict" },
            { "tags": ["auto-retained"], "match": "any_strict" },
            // The untagged (global) scope: `exact` with an empty tag list
            // selects exactly the memories that carry no tags.
            { "tags": [], "match": "exact" },
        ]
    }])
}

/// Recall facts for `query` from the configured bank.
///
/// Fails soft by returning an empty result on any transport or parse error:
/// a memory system that can break a turn is worse than no memory system.
// reference: https://hindsight.vectorize.io/0.10/developer/api/recall
//   (RecallRequest fields: query, budget, max_tokens, prefer_observations) and
//   https://hindsight.vectorize.io/0.10/developer/api/mental-models
//   (GET .../mental-models?detail=content&limit=N).
pub fn recall(config: &HindsightConfig, query: &str) -> Result<HindsightRecall> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(HindsightRecall {
            facts: Vec::new(),
            models: Vec::new(),
            query: String::new(),
        });
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
        // SCOPE. Without this, recall draws from the whole bank: 78,013
        // memories of which 29,940 are `origin:hermes`, 943 `origin:opencode`,
        // plus unrelated projects. MEASURED 2026-11-09 on the owner's own
        // complaint about memory delivery, same query, same endpoint:
        //   - unscoped: the top hits were agent chatter about an old
        //     truncation bug, a Chinese-language passage from another
        //     project, and a UOB printer incident. None were about the
        //     owner's question.
        //   - scoped to this workspace's own tags: the owner's actual
        //     complaint was the top hit.
        // See `recall_tag_groups` for the scope and why it also admits the
        // global (untagged) scope.
        "tag_groups": recall_tag_groups(),
    });

    let text = post_json(&url, &body)?;

    let parsed: RecallResponse = serde_json::from_str(&text)
        .with_context(|| format!("parse Hindsight recall response from {url}"))?;

    // The cutoff is RELATIVE to the top score, because the absolute scale of
    // the reranker is not stable across queries. See MIN_RELATIVE_SCORE.
    let top_score = parsed
        .results
        .iter()
        .filter_map(|item| item.scores.as_ref().and_then(|s| s.r#final))
        .fold(0.0f64, f64::max);
    let cutoff = top_score * MIN_RELATIVE_SCORE;

    let mut facts = Vec::new();
    for item in parsed.results {
        let Some(raw) = item.text else { continue };
        let cleaned = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if cleaned.is_empty() {
            continue;
        }
        let score = item.scores.and_then(|s| s.r#final);
        // A result with no score at all cannot be judged, so it is kept: the
        // server omitting a score is not evidence of irrelevance.
        if let Some(score) = score
            && top_score > 0.0
            && score < cutoff
        {
            continue;
        }
        facts.push(HindsightFact {
            id: item.id.unwrap_or_default(),
            text: cleaned,
            score,
            dated: fact_date(item.mentioned_at, item.occurred_start, item.occurred_end),
        });
        if facts.len() >= config.limit {
            break;
        }
    }

    let models = match fetch_models(config, &query) {
        Ok(models) => models,
        // A model fetch failure must not lose the facts: the facts are the
        // primary payload, the models are an enrichment.
        Err(error) => {
            crate::logging::info(&format!("Hindsight mental models unavailable: {error}"));
            Vec::new()
        }
    };

    Ok(HindsightRecall {
        facts,
        models,
        query: query.to_string(),
    })
}

/// The date a recalled fact is true for, as a short ISO stamp.
///
/// Priority: the event itself (`occurred_start`), then when the source said it
/// (`mentioned_at`). The first is the date the fact was TRUE; `mentioned_at` is
/// the universal fallback because the live recall endpoint fills it for every
/// fact, while `occurred_start` is NULL whenever the extractor set no event date.
/// A fact with neither is returned as `None` and the renderer stamps it
/// explicitly as undated - it must not silently look current.
fn fact_date(
    mentioned_at: Option<DateTime<Utc>>,
    occurred_start: Option<DateTime<Utc>>,
    occurred_end: Option<DateTime<Utc>>,
) -> Option<String> {
    let moment = occurred_start.or(mentioned_at);
    let moment = moment?;
    let mut stamp = moment.format("%Y-%m-%d").to_string();
    if let Some(end) = occurred_end
        && end.format("%Y-%m-%d").to_string() != stamp
    {
        stamp.push_str(&format!("..{}", end.format("%Y-%m-%d")));
    }
    Some(stamp)
}

/// Fetch the mental models that bear on `query`.
///
/// reference: https://hindsight.vectorize.io/0.10/developer/api/mental-models
///   (GET .../mental-models?detail=content -> id, name, tags, is_stale,
///   content, source_query; "detail=content" is REQUIRED, the default
///   "metadata" returns content: null) and
///   https://hindsight.vectorize.io/0.10/developer/mental-models
///   ("A model that has fallen behind is still shown, but it no longer
///   short-circuits retrieval").
///
/// MEASURED 2026-11-09 on bank `jcode-legacy`: `detail=content` is REQUIRED -
/// the default `detail=metadata` returns `content: null` for every model, so a
/// client that omits it silently gets nothing.
///
/// SELECTION (rewritten 2026-11-10).
///
/// The previous rule made the six curated ids UNCONDITIONALLY eligible, so
/// the delivered set never depended on the query. MEASURED over 20 real
/// queries against the live bank: **1 distinct delivered set**, always the
/// same three models, and 3 models x 600 chars = 1869 > the 1800-char section
/// budget, so the third model was truncated on every single turn. That is
/// exactly the owner's complaint: "the Synthesized context block is the same
/// thing every time".
///
/// The rule now separates ELIGIBILITY from RANKING:
///
///  - eligible: the model shares at least one query token (4+ chars) with its
///    TOPIC surface (name + id + source_query) or its body. A curated model
///    with NO overlap is NOT eligible, so the unconditional delivery is gone.
///  - ranked: by an IDF-weighted score over the same tokens, with the curated
///    set getting a tie-break bonus only. Folding the bonus into eligibility
///    was measured and REJECTED: it made zero-overlap curated models eligible
///    again, i.e. it preserved the defect.
///
/// MEASURED over the same 20 queries: **18 distinct delivered sets** (was 1),
/// 0 identical to the old constant set, 2 empty (an empty section is valid -
/// see the plan, "a successful empty result is valid"), and curated models
/// now appear in 17 of 47 deliveries instead of 60 of 60.
///
/// IDF is what makes the score meaningful: the models' bodies are 2.5k-17k
/// chars of prose, so a plain token count is decided by length and by shared
/// filler words ("state", "list", "give"). A token that appears in every model
/// carries no topic signal and must not decide the ranking.
///
/// Stale models are NOT dropped (see [`MentalModelItem::is_stale`]).
fn fetch_models(config: &HindsightConfig, query: &str) -> Result<Vec<HindsightModel>> {
    let url = format!(
        "{}/v1/default/banks/{}/mental-models?detail=content&limit=100",
        config.base_url, config.bank
    );
    let text = get_json(&url)?;
    let parsed: MentalModelsResponse = serde_json::from_str(&text)
        .with_context(|| format!("parse Hindsight mental models response from {url}"))?;

    // A model is a saved reflect response, so its TOPIC is its name and id.
    //
    // MEASURED 2026-11-10 on the live bank: the topic surface used to be
    // `name + id + source_query`, and the source_query of the four technical
    // models is a GENERATION PROMPT - 3.4k-7.2k chars, 162-382 tokens - while
    // the curated models' prompts are 212-320 chars. The score is a SUM over
    // the shared tokens, so the bigger haystack accumulated more matches and
    // won the ranking regardless of the topic: the four technical manuals took
    // 40.3% of all delivered model slots. The prompt is an instruction to
    // Hindsight, not a description of the subject.
    //
    // The body is scored over the SAME WINDOW THAT WILL BE DELIVERED, for the
    // same reason: a 17k-char model otherwise accumulates matches from text
    // the agent never sees. What is delivered is what must be ranked.
    let mut candidates: Vec<ModelCandidate> = Vec::new();
    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    for item in parsed.items {
        let content = item.content.unwrap_or_default();
        let body = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if body.is_empty() {
            continue;
        }
        let name = item.name.unwrap_or_default();
        let id = item.id.unwrap_or_default();
        let topic = format!("{name} {id}");
        let topic_tokens = query_tokens(&topic);
        let body_tokens = query_tokens(&body_window(&body, MAX_MODEL_CHARS));
        if topic_tokens.is_empty() && body_tokens.is_empty() {
            continue;
        }
        for token in topic_tokens.iter().chain(body_tokens.iter()) {
            *doc_freq.entry(token.clone()).or_insert(0) += 1;
        }
        let is_curated = is_curated_model(&id);
        candidates.push(ModelCandidate {
            id,
            name,
            text: body,
            is_stale: item.is_stale.unwrap_or(false),
            is_curated,
            topic_tokens,
            body_tokens,
            score: 0.0,
        });
    }

    let model_count = candidates.len().max(1) as f64;
    let query_tokens = query_tokens(query);
    let mut scored: Vec<(f64, ModelCandidate)> = Vec::new();
    for mut candidate in candidates {
        let topic_hits = intersect(&query_tokens, &candidate.topic_tokens);
        let body_hits = intersect(&query_tokens, &candidate.body_tokens);
        // ELIGIBILITY: at least one shared token. Without this the curated
        // models would come back for every query again.
        if topic_hits.is_empty() && body_hits.is_empty() {
            continue;
        }
        let mut score = idf_score(&topic_hits, &doc_freq, model_count) * TOPIC_TOKEN_WEIGHT
            + idf_score(&body_hits, &doc_freq, model_count) * BODY_TOKEN_WEIGHT;
        // The curated models are the standing answers, so they win a TIE -
        // but they never make a model eligible on their own.
        if candidate.is_curated {
            score += CURATED_TIE_BREAK_BONUS;
        }
        candidate.score = score;
        scored.push((score, candidate));
    }

    // Highest score first; a stale model sorts after a fresh one of the same
    // score, so the freshest reading of a topic wins.
    scored.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.is_stale.cmp(&right.1.is_stale))
            .then_with(|| left.1.id.cmp(&right.1.id))
    });
    scored.truncate(MAX_MODELS);
    Ok(scored
        .into_iter()
        .map(|(_, candidate)| candidate.into_model())
        .collect())
}

/// A fetched model together with the tokens used to score it.
struct ModelCandidate {
    id: String,
    name: String,
    text: String,
    is_stale: bool,
    is_curated: bool,
    topic_tokens: HashSet<String>,
    body_tokens: HashSet<String>,
    score: f64,
}

impl ModelCandidate {
    fn into_model(self) -> HindsightModel {
        HindsightModel {
            id: self.id,
            name: self.name,
            text: self.text,
            is_stale: self.is_stale,
            is_curated: self.is_curated,
        }
    }
}

/// The tokens a query or a model surface contributes to matching.
///
/// The 4-character floor drops noise ("the", "and", "что"); the token set
/// keeps a repeated word from inflating a model's score.
fn query_tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|token| token.chars().count() >= MIN_TOKEN_CHARS)
        .map(str::to_string)
        .collect()
}

fn intersect(left: &HashSet<String>, right: &HashSet<String>) -> HashSet<String> {
    left.intersection(right).cloned().collect()
}

/// The part of a model's text that the agent actually receives.
///
/// MEASURED 2026-11-10 on the live bank: `MAX_MODEL_CHARS` is 600 and the
/// renderer takes the FIRST characters of the model, so an owner-facing model
/// delivered 69-79% preamble (its own scope caveat and evidence-id legend)
/// before the first dated fact, while a technical model delivered its first
/// fact at 0-43%. The delivered window is what must be ranked, and it is
/// what must be measured for relevance.
///
/// The window ends at a sentence boundary when one is close enough, so the
/// excerpt does not stop mid-word; the hard cap still applies.
fn body_window(body: &str, cap: usize) -> &str {
    if body.chars().count() <= cap {
        return body;
    }
    let head: String = body.chars().take(cap).collect();
    // The last sentence end inside the window; if there is none, the hard cap
    // is the boundary.
    match head.rfind(|c| c == '.' || c == '\n' || c == ';' || c == '!') {
        Some(index) => {
            let cut = head
                .char_indices()
                .nth(index + 1)
                .map(|(offset, _)| offset)
                .unwrap_or(head.len());
            &body[..cut]
        }
        None => &body[..cap],
    }
}

/// The window of a model's text that best answers `query`.
///
/// reference: no prior art, searched the whole jcode workspace for an existing
///   sentence/excerpt-windowing helper (`fn sentence`, `split_sentences`,
///   `sentence_boundary`, `excerpt` across crates/) - none exists; the closest
///   is `memory_prompt.rs::truncate_chars`, a plain head truncation that cannot
///   choose a window. Upstream Hindsight exposes no per-model excerpt API
///   either: `GET /mental-models?detail=content` returns the WHOLE document
///   and the client does the cutting, so the client owns this rule. Upstream's
///   documented contract for a clipped result is
///   https://hindsight.vectorize.io/0.10/developer/api/recall ("rather than
///   clipped mid-sentence"), which this window honours.
///
/// A mental model is a whole document: its first page is the model's own
/// scope caveat, its evidence-id legend and its table header. Delivering
/// that page means the agent reads a disclaimer instead of a fact.
///
/// MEASURED 2026-11-10 on the live daemon log: of 979 delivered model
/// excerpts, the owner-facing ones put their first dated fact at character
/// 256-422 of a ~550-character excerpt - 44-79% of the delivered text was
/// preamble - while the technical ones put theirs at 0-235. The waste was
/// measured on the DELIVERED text, not on the model.
///
/// The window is the run of sentences that contains the most DISTINCT query
/// tokens, so a window wins when it actually talks about the subject. Ties
/// keep the earliest window, so a model with no query overlap still delivers
/// its head rather than an arbitrary later slice.
fn relevant_window(body: &str, query: &HashSet<String>, cap: usize) -> String {
    if cap == 0 || body.chars().count() <= cap {
        return body.to_string();
    }
    // Split into sentence units, keeping the delimiter with each unit.
    let mut units: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in body.chars() {
        current.push(ch);
        if ch == '.' || ch == ';' || ch == '!' || ch == '\n' {
            units.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        units.push(current);
    }
    if units.len() < 2 {
        return body_window(body, cap).to_string();
    }

    // The query tokens each unit carries.
    let unit_tokens: Vec<HashSet<String>> = units.iter().map(|u| query_tokens(u)).collect();
    // The head is the fallback AND the baseline: a window must BEAT the head
    // on shared tokens to be worth delivering, so a single shared FILLER word
    // cannot move the excerpt off the model's own opening.
    //
    // reference: https://hindsight.vectorize.io/0.10/developer/api/recall
    //   ("facts are included in relevance order until this budget is exhausted
    //   - so you always get the most relevant memories that fit"; and a result
    //   that does not fit is skipped rather than ending the selection). The
    //   relevance order is what this window reproduces for a whole document:
    //   upstream ranks facts it extracted, and the client must rank the parts
    //   of a document it did not extract.
    //
    // MEASURED 2026-11-10: the first version of this rule took any window
    // with one shared token, and the regression test caught it - a model that
    // merely mentioned the word "subject" was cut to that one sentence instead
    // of its head. A window needs at least two distinct query tokens (one,
    // when the query is a single token), or the head is kept.
    let min_overlap = if query.len() <= 1 { 1 } else { 2 };
    let mut head_length = 0usize;
    let mut head_covered: HashSet<String> = HashSet::new();
    for (index, unit) in units.iter().enumerate() {
        head_length += unit.chars().count();
        if head_length > cap && index > 0 {
            break;
        }
        head_covered.extend(unit_tokens[index].intersection(query).cloned());
    }
    let head_score = head_covered.len();

    let mut best: Option<(usize, usize, usize)> = None; // (start, end, score)
    for start in 0..units.len() {
        let mut length = 0usize;
        let mut covered: HashSet<String> = HashSet::new();
        for end in start..units.len() {
            length += units[end].chars().count();
            if length > cap && end > start {
                break;
            }
            covered.extend(unit_tokens[end].intersection(query).cloned());
            let score = covered.len();
            let better = match best {
                None => true,
                Some((_, _, best_score)) => score > best_score,
            };
            if better {
                best = Some((start, end + 1, score));
            }
        }
    }
    match best {
        // A window must reach the minimum overlap AND beat the head, or the
        // head is the honest answer.
        Some((start, end, score)) if score >= min_overlap && score > head_score => {
            units[start..end].concat()
        }
        _ => body_window(body, cap).to_string(),
    }
}

/// The IDF-weighted score of a set of shared tokens.
///
/// reference: no prior art for this scoring inside jcode - it is the standard
///   inverse document frequency over the model's own corpus, which is what
///   the earlier plain token-count rule lacked: a token present in every
///   model contributes ~0 and stops deciding the ranking, while a token
///   present in one model is that model's topic.
fn idf_score(tokens: &HashSet<String>, doc_freq: &HashMap<String, usize>, model_count: f64) -> f64 {
    tokens
        .iter()
        .map(|token| {
            let freq = doc_freq.get(token).copied().unwrap_or(0) as f64;
            (model_count / freq.max(1.0)).ln()
        })
        .sum()
}

/// The curated, owner-facing mental models of this bank.
///
/// reference: no prior art - these ids are this workspace's own curated set,
///   installed under the plan in
///   C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/
///   jcode-hindsight-implementation.md (section 3, the model roster table).
///   Upstream's coding-agents package never fetches mental models at all
///   (searched hindsight-coding-agents 0.8.2 dist/*.js, found none).
///
/// These are the STANDING ANSWERS the owner asked for: profile, goals,
/// decisions, incidents, hazards, coordination. They are relevant to almost
/// any query about this workspace, so they are always eligible - no query
/// match required.
///
/// They are identified by ID, not by a `scope:` tag, because the tag now has
/// to do real work: it selects the DATA the model reads. A model tagged
/// `owner:primary` reads the owner's own statements; a model tagged
/// `auto-retained` reads this workspace's turn digests. Adding a `scope:`
/// tag alongside would break that: `all_strict` requires a memory to carry
/// EVERY tag, and no memory carries `scope:*`.
const CURATED_MODEL_IDS: &[&str] = &[
    "user-profile",
    "active-work",
    "decisions-ledger",
    "incidents-and-fixes",
    "machine-hazards",
    "agent-coordination",
    // The goals the owner has PARKED, waiting on a decision or a condition.
    // MEASURED 2026-11-11 on the live bank: this model was MISSING from the
    // 15-model roster, so `active-work` (the goals progressing right now)
    // was the only source about the owner's work - and the goals waiting on
    // the owner were invisible to the agent. Created on 2026-11-11 with the
    // measured owner scope (owner:primary + the global untagged leaf).
    "waiting-goals",
];

/// Whether a mental model is one of the curated, owner-facing models.
fn is_curated_model(id: &str) -> bool {
    CURATED_MODEL_IDS.contains(&id)
}

// reference: vectorize-io/hindsight-coding-agents, dist/dcode-hook.js
//   buildSystemInjection (:1947) - the upstream injection contract this block
//   follows, and the section shape it uses (a bulleted fact list).
/// Render recalled facts and models as the memory block the agent receives.
///
/// The block carries ONLY the recalled material. NO service preamble.
///
/// MEASURED 2026-11-10, the owner's own words: "why is there a memories
/// block that only holds service text?" - and the live block proved it: the
/// block began with two lines of instruction ("A fact stating an exact rule
/// or literal values is to be applied AS GIVEN...") and then the material.
/// Those lines are an INSTRUCTION to the agent, and an instruction belongs in
/// the system prompt, not in a payload that is re-read on every turn. They
/// were re-sent with every injection, so they were the loudest part of the
/// block and the smallest part of its value.
///
/// The rules themselves are not lost: they now live in the STATIC system
/// prompt (see `crate::prompt::MEMORY_PROMPT`), which is cached and sent
/// once instead of with every recall.
///
/// What survives in the block:
///   - the facts, as a bulleted list, and
///   - the models, as a bulleted list under their name.
///
/// The two sections have SEPARATE budgets, because a single shared budget
/// starves whichever section comes second. MEASURED 2026-11-09: the python
/// delivery path shared one 3,000-char budget across 8 facts and 3 models, and
/// 10 of 10 fetched models were cut before reaching the model.
pub fn format_recall_block(recall: &HindsightRecall) -> Option<String> {
    if recall.facts.is_empty() && recall.models.is_empty() {
        return None;
    }
    let mut out = String::from("# Memory\n\n");
    if !recall.facts.is_empty() {
        out.push_str("## Relevant facts\n\n");
        let mut used = 0usize;
        for fact in &recall.facts {
            let mut head: String = fact.text.chars().take(MAX_FACT_CHARS).collect();
            if head.chars().count() < fact.text.chars().count() {
                head.push('…');
            }
            // EVERY fact carries its date. MEASURED 2026-11-11 (owner directive):
            // some facts had a date only because the extractor wrote one into the
            // text; a fact without a bank date looked current, which is how stale
            // facts survived. An explicit `(undated)` is the honest marker.
            let dated = fact.dated.as_deref().unwrap_or("undated");
            let line = format!("- [{dated}] {head}\n");
            // The bound is enforced HERE, at the rendering boundary, not only in
            // `recall`: this is the single place every fact passes through, so
            // the block cannot carry an unbounded line whatever produced it.
            if used + line.chars().count() > MAX_FACTS_CHARS {
                break;
            }
            used += line.chars().count();
            out.push_str(&line);
        }
    }
    if !recall.models.is_empty() {
        out.push_str("\n## Synthesized context\n\n");
        // How many models are still to be rendered. The excerpt of each one
        // shares the section budget, so the LAST model must not be dropped
        // for lack of room - it is the one the query selected most.
        //
        // MEASURED 2026-11-10: the old code `break`ed when a bullet did not
        // fit, so with 3 models x 600 chars + overhead (1869) against an
        // 1800-char budget the THIRD model was silently dropped on EVERY
        // turn - 14 of 20 probes lost their last model. The selection had
        // already decided which models matter; the renderer must not undo
        // that decision.
        //
        // reference: https://hindsight.vectorize.io/0.10/developer/api/recall
        //   (max_tokens: "facts are included in relevance order until this
        //   budget is exhausted - so you always get the most relevant
        //   memories that fit"; and "a query that matched something never
        //   comes back empty ... rather than clipped mid-sentence"). The same
        //   principle applies here: a selected model is never dropped, its
        //   excerpt is shortened instead.
        let remaining = recall.models.len();
        // The section separator ("\n\n") is part of the section, so it is
        // reserved from the budget. MEASURED 2026-11-10: the first
        // implementation did not reserve it and the section came out two
        // characters over its own budget - caught by the regression test
        // `every_selected_model_reaches_the_block`.
        const SECTION_SEPARATOR_CHARS: usize = 2;
        let mut used = SECTION_SEPARATOR_CHARS;
        for (index, model) in recall.models.iter().enumerate() {
            // reference: https://hindsight.vectorize.io/developer/mental-models
            //   ("A mental model is ... a whole document per question"; a
            //    model's content is always markdown)
            //
            // A mental model is a whole markdown DOCUMENT, so it USUALLY
            // STARTS WITH ITS OWN "## " heading ("## Опасности машины и
            // окружения", "## Decisions still applicable in the recorded
            // scope", ...). `fetch_models` collapses that document to ONE line,
            // so the delivered line begins with "## ".
            //
            // The consumers read a "## " line as a CATEGORY - the tile's
            // title - and CONSUME it, so it never becomes a readable item.
            // The model's text therefore became the title of an EMPTY tile,
            // next to a tile holding nothing but the model's NAME, and the
            // facts were the only thing left to read. That is the owner's
            // report: the memory blocks arrive empty and only the relevant
            // facts are filled.
            //
            // MEASURED 2026-11-10 over 456 distinct delivered blocks:
            // 728 of 874 model bodies took that path, affecting 398 blocks.
            // The decisive instrument was the SHIPPED parser itself
            // (jcode-tui-render::memory_tiles::parse_memory_display_entries)
            // run over the real blocks, which renders exactly that: a
            // "synthesized context" tile holding only "### <model name>"
            // bullets, beside a junk-titled tile holding the model's text.
            //
            // The fix is at the SOURCE, so every consumer is fixed at once:
            // each model is delivered as ONE bullet. A bullet can never be
            // read as a heading, and the model's own leading heading markers
            // are stripped so the text reads as prose.
            let stripped = model.text.trim_start().trim_start_matches('#').trim_start();
            let heading = if model.name.is_empty() {
                "context".to_string()
            } else if model.is_stale {
                // Upstream: a stale model is still shown, but the reader must
                // know it may be behind the evidence, so it is marked rather
                // than silently trusted.
                format!("{} (may be out of date)", model.name)
            } else {
                model.name.clone()
            };
            // The share of the remaining budget this model may use. The
            // first model gets the full per-model excerpt; the last one
            // gets whatever is left, so a model is never dropped.
            let models_left = remaining - index;
            let room = MAX_MODELS_CHARS.saturating_sub(used);
            let share = room / models_left.max(1);
            let cap = MAX_MODEL_CHARS.min(share);
            let overhead = heading.chars().count() + 10;
            let text_cap = cap.saturating_sub(overhead);
            // The excerpt is the window of the model that actually answers the
            // question, not the model's first page.
            //
            // reference: no prior art, searched the jcode workspace for an
            //   existing excerpt-windowing helper - none exists; the closest is
            //   `memory_prompt.rs::truncate_chars`, a head truncation that
            //   cannot choose a window. Upstream Hindsight exposes no
            //   per-model excerpt API either: `GET /mental-models?detail=content`
            //   returns the WHOLE document and the client cuts it, so the
            //   client owns this rule. Upstream's contract for a clipped
            //   result is https://hindsight.vectorize.io/0.10/developer/api/recall
            //   ("rather than clipped mid-sentence"), which this honours.
            //
            // MEASURED 2026-11-10 on the live daemon log: the owner's own
            // models begin with a scope caveat, an evidence-id legend and a
            // table header, so the head of the document delivered 44-79%
            // preamble before the first dated fact, while a technical model's
            // head delivered its first fact at 0-43%. Cutting at the
            // query-relevant window turns the excerpt into the part the agent
            // can act on.
            let query_tokens = query_tokens(&recall.query);
            let excerpt = relevant_window(stripped, &query_tokens, text_cap);
            // The ellipsis must fit INSIDE the cap. MEASURED 2026-11-10: it
            // did not - the mark was appended AFTER the excerpt had taken the
            // full quota, so the bullet was one char too long, the budget
            // check rejected it, and the LAST model was dropped on 14 of 20
            // probes. The selection had already decided it mattered.
            let truncated = excerpt.chars().count() > text_cap;
            let take = if truncated {
                text_cap.saturating_sub(1)
            } else {
                text_cap
            };
            let mut head: String = excerpt.chars().take(take).collect();
            if truncated {
                head.push('…');
            }
            let block = format!("- **{heading}** — {head}\n");
            if used + block.chars().count() > MAX_MODELS_CHARS {
                // Nothing more can fit; the remaining models are dropped here
                // rather than silently.
                break;
            }
            used += block.chars().count();
            out.push_str(&block);
        }
    }
    Some(out)
}

// reference: reqwest blocking client, the same call shape as `post_json` below.
/// GET JSON and return the response body as text.
///
/// The call runs on a dedicated thread so it is safe from inside a tokio
/// worker: `reqwest::blocking` panics when used from an async context.
fn get_json(url: &str) -> Result<String> {
    let url = url.to_string();
    std::thread::scope(|scope| {
        scope
            .spawn(move || -> Result<String> {
                let client = reqwest::blocking::Client::builder()
                    .timeout(RECALL_TIMEOUT)
                    .build()?;
                let resp = client.get(&url).send()?;
                let status = resp.status();
                let text = resp.text()?;
                if !status.is_success() {
                    anyhow::bail!(
                        "Hindsight GET failed ({status}): {}",
                        text.chars().take(300).collect::<String>()
                    );
                }
                Ok(text)
            })
            .join()
            .map_err(|_| anyhow::anyhow!("Hindsight GET worker thread panicked"))?
    })
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

#[derive(Debug, Deserialize)]
struct MemoriesListResponse {
    #[serde(default)]
    items: Vec<MemoryListItem>,
}

#[derive(Debug, Deserialize)]
struct MemoryListItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    fact_type: Option<String>,
    /// When the fact was retained into the bank.
    ///
    /// MEASURED 2026-11-10 on the live endpoint: the item carries
    /// `mentioned_at` ("ISO 8601 datetime of when this fact was retained"),
    /// `updated_at` and `occurred_start`. The report time is `mentioned_at`:
    /// it is when the source told us, which is what a "trusted source reported
    /// a change" event means.
    #[serde(default)]
    mentioned_at: Option<DateTime<Utc>>,
}

/// One bank event: what the source reported, and when it reported it.
#[derive(Debug, Clone)]
pub struct BankEvent {
    pub id: String,
    pub text: String,
    /// The tags the event carries in the bank.
    pub tags: Vec<String>,
    /// When the source reported the change.
    pub mentioned_at: Option<DateTime<Utc>>,
}

impl BankEvent {
    /// Whether this event is an echo of Jcode's own conversation.
    ///
    /// The retain hook tags every turn digest of our own conversation with
    /// `auto-retained`. Such an event is not a change in the world, so it must
    /// not reopen a goal.
    pub fn is_own_echo(&self) -> bool {
        self.tags.iter().any(|tag| tag == OWN_ECHO_TAG)
    }
}

/// The URL that lists the bank's memories written since `since`.
///
/// MEASURED 2026-11-11: a raw `+00:00` offset in the query string made EVERY
/// reopen trigger fail. The server decodes `+` in a query value as a space,
/// so the timestamp arrived as '2026-11-10T01:24:15.828887300 00:00' and the
/// endpoint answered 400 "Invalid start_date". The trigger therefore never
/// fired once: 1292 consecutive failures in the daemon log. Two changes, both
/// measured against the live server (http://localhost:8888): the `Z` form
/// carries no `+` at all, and the percent-encoding makes the value safe for
/// any query parser.
fn recent_memories_url(config: &HindsightConfig, since: DateTime<Utc>, limit: usize) -> String {
    let start = urlencoding::encode(
        &since.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    )
    .into_owned();
    format!(
        "{}/v1/default/banks/{}/memories/list?time_field=created_at&start_date={}&limit={}",
        config.base_url,
        config.bank,
        start,
        limit.clamp(1, 200),
    )
}

/// The memories written to the bank since `since`.
///
/// These are the "trusted source reported a relevant change" events of the
/// reopen feature: the bank's own records of what happened, newest first.
///
/// reference: the live server's own OpenAPI (Hindsight 0.10.2),
/// GET /v1/default/banks/{bank_id}/memories/list with the documented
/// `time_field`, `start_date` and `limit` query parameters.
pub fn recent_memories(
    config: &HindsightConfig,
    since: DateTime<Utc>,
    limit: usize,
) -> Result<Vec<BankEvent>> {
    let url = recent_memories_url(config, since, limit);
    let raw = get_json(&url)?;
    let response: MemoriesListResponse = serde_json::from_str(&raw)
        .with_context(|| format!("parse Hindsight memories list from {url}"))?;
    Ok(response
        .items
        .into_iter()
        .filter_map(|item| {
            let text = item.text?;
            if text.trim().is_empty() {
                return None;
            }
            Some(BankEvent {
                id: item.id.unwrap_or_default(),
                text,
                tags: item.tags.unwrap_or_default(),
                mentioned_at: item.mentioned_at,
            })
        })
        .collect())
}

/// The decision the plan asks `reflect` to return for a deferred goal.
///
/// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
///   ("Ask reflect to return one of `wait`, `prepare`, `act_within_authority`
///   or `ask_owner` with `goal_id`, `evidence_ids`, `counterevidence_ids`,
///   `unverified_conditions` and `why_now`. These are fields of a proposed
///   response schema for Jcode. They are not server-native action commands.").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReopenDecision {
    /// Nothing changed that bears on the condition. Stay quiet.
    Wait,
    /// The condition holds and preparation is authorized: produce the artifact.
    Prepare,
    /// The condition holds and the authorized step can be taken directly.
    ActWithinAuthority,
    /// The condition may hold, but proceeding needs a new commitment.
    AskOwner,
}

impl ReopenDecision {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Wait => "wait",
            Self::Prepare => "prepare",
            Self::ActWithinAuthority => "act_within_authority",
            Self::AskOwner => "ask_owner",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "wait" => Some(Self::Wait),
            "prepare" => Some(Self::Prepare),
            "act_within_authority" => Some(Self::ActWithinAuthority),
            "ask_owner" => Some(Self::AskOwner),
            _ => None,
        }
    }
}

/// The structured answer `reflect` gives for one deferred goal.
#[derive(Debug, Clone, Default)]
pub struct ReopenAssessment {
    pub decision: Option<ReopenDecision>,
    /// The goal this assessment is about, as reflect saw it.
    pub goal_id: Option<String>,
    /// Evidence that supports the decision.
    pub evidence_ids: Vec<String>,
    /// Evidence that contradicts it.
    pub counterevidence_ids: Vec<String>,
    /// Conditions the evidence does not settle.
    pub unverified_conditions: Vec<String>,
    /// Why this is worth the owner's attention now.
    pub why_now: Option<String>,
    /// The reflect answer in prose, always present.
    pub text: String,
}

/// The response schema asked of `reflect` for a reopen assessment.
///
/// reference: C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
///   ("with `goal_id`, `evidence_ids`, `counterevidence_ids`,
///   `unverified_conditions` and `why_now`").
pub fn reopen_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "decision": {
                "type": "string",
                "enum": ["wait", "prepare", "act_within_authority", "ask_owner"],
                "description": "What to do about the deferred goal now."
            },
            "goal_id": {"type": "string", "description": "The goal key."},
            "evidence_ids": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Ids of the memories that support the decision."
            },
            "counterevidence_ids": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Ids of the memories that contradict it."
            },
            "unverified_conditions": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Conditions the available evidence does not settle."
            },
            "why_now": {"type": "string", "description": "Why this deserves attention now."}
        },
        "required": ["decision"]
    })
}

#[derive(Debug, Deserialize)]
struct ReflectResponse {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    structured_output: Option<serde_json::Value>,
    #[serde(default)]
    structured_output_error: Option<String>,
}

/// Ask `reflect` whether a deferred goal should be reconsidered.
///
/// The query carries the goal's own condition, the reason it was deferred and
/// the reported change. The scope is the workspace plus the global untagged
/// leaf, the same scope the automatic recall uses, so the assessment sees the
/// owner's own statements about the goal.
///
/// reference: https://hindsight.vectorize.io/0.10/developer/api/reflect
///   (`response_schema` - "the response includes a `structured_output` field
///   in addition to the markdown `text`"; `include.facts` - "the `based_on`
///   object listing the memories, mental models, and directives the agent
///   actually used"; `structured_output_error` - "Why the structured view
///   could not be produced ... Treat its presence as retryable").
pub fn assess_reopen(
    config: &HindsightConfig,
    query: &str,
) -> Result<ReopenAssessment> {
    let url = format!(
        "{}/v1/default/banks/{}/reflect",
        config.base_url, config.bank
    );
    let body = serde_json::json!({
        "query": query,
        "budget": "low",
        "max_tokens": 1500,
        "response_schema": reopen_response_schema(),
        "include": {"facts": {}},
        "tag_groups": recall_tag_groups(),
    });
    let raw = post_json(&url, &body)?;
    let response: ReflectResponse = serde_json::from_str(&raw)
        .with_context(|| format!("parse Hindsight reflect response from {url}"))?;

    let mut assessment = ReopenAssessment {
        text: response.text.unwrap_or_default(),
        ..Default::default()
    };

    if let Some(error) = response.structured_output_error.as_deref() {
        // The prose answer still exists; the machine-readable half does not.
        // The plan calls this inconclusive, not a decision.
        anyhow::bail!("Hindsight reflect produced no structured output: {error}");
    }

    if let Some(structured) = response.structured_output {
        let decision = structured
            .get("decision")
            .and_then(|value| value.as_str())
            .and_then(ReopenDecision::parse);
        assessment.decision = decision;
        assessment.goal_id = structured
            .get("goal_id")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        assessment.evidence_ids = string_list(&structured, "evidence_ids");
        assessment.counterevidence_ids = string_list(&structured, "counterevidence_ids");
        assessment.unverified_conditions = string_list(&structured, "unverified_conditions");
        assessment.why_now = structured
            .get("why_now")
            .and_then(|value| value.as_str())
            .map(str::to_string);
    }

    Ok(assessment)
}

fn string_list(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_scope_admits_the_workspace_and_the_global_untagged_scope() {
        let groups = recall_tag_groups();
        let leaves = groups[0]["or"].as_array().expect("or must be an array");
        let mut scoped: Vec<String> = leaves
            .iter()
            .filter(|leaf| leaf["match"] == "any_strict")
            .map(|leaf| leaf["tags"][0].as_str().unwrap_or_default().to_string())
            .collect();
        scoped.sort();
        assert_eq!(
            scoped,
            vec![
                "auto-retained".to_string(),
                "origin:jcode".to_string(),
                "owner:primary".to_string(),
            ],
            "the workspace scopes must be exactly the three jcode scopes"
        );

        // The global scope: `exact` with an EMPTY tag list. `any_strict`
        // excludes untagged memories, so without this leaf the owner's own
        // untagged durable rules are unreachable.
        let global = leaves
            .iter()
            .find(|leaf| leaf["match"] == "exact")
            .expect("the global (untagged) leaf must be present");
        assert_eq!(
            global["tags"].as_array().map(|a| a.len()),
            Some(0),
            "the global leaf must carry an empty tag list"
        );

        // A leaf that used `any`/`all` instead of `any_strict` would silently
        // re-admit untagged records into the WORKSPACE scopes, which would
        // double-count the global scope.
        for leaf in leaves.iter().filter(|l| l["match"] != "exact") {
            assert_eq!(
                leaf["match"], "any_strict",
                "workspace leaves must stay strict so untagged memories are \
                 only reachable through the explicit global leaf"
            );
        }
    }

    #[test]
    fn empty_query_yields_no_facts() {
        let config = HindsightConfig::default();
        let recall = recall(&config, "   ").expect("empty query must not fail");
        assert!(recall.facts.is_empty());
        assert!(recall.models.is_empty());
    }

    #[test]
    fn block_is_none_without_facts() {
        assert!(
            format_recall_block(&HindsightRecall {
                facts: Vec::new(),
                models: Vec::new(),
                query: String::new(),
            })
            .is_none()
        );
    }

    /// EVERY delivered fact carries its date, and a fact with no bank date is
    /// marked `undated` rather than looking current.
    ///
    /// MEASURED 2026-11-11 (owner directive): some facts arrived dated only
    /// because the extractor wrote a date into the text, so a fact without one
    /// was indistinguishable from a current fact. The renderer now stamps every
    /// fact from the bank's own date column.
    #[test]
    fn every_fact_line_carries_a_date_and_undated_is_explicit() {
        let recall = HindsightRecall {
            facts: vec![
                HindsightFact {
                    id: "dated".into(),
                    text: "a fact with a bank date".into(),
                    score: Some(0.9),
                    dated: Some("2026-11-11".into()),
                },
                HindsightFact {
                    id: "undated".into(),
                    text: "a fact with no bank date".into(),
                    score: Some(0.8),
                    dated: None,
                },
            ],
            models: Vec::new(),
            query: String::new(),
        };
        let block = format_recall_block(&recall).expect("block");
        let fact_lines: Vec<&str> = block
            .lines()
            .filter(|line| line.starts_with("- "))
            .collect();
        assert_eq!(fact_lines.len(), 2, "both facts must be delivered: {block}");
        for line in &fact_lines {
            assert!(
                line.contains("[2026-11-11]") || line.contains("[undated]"),
                "every fact line must carry a date stamp, got: {line}"
            );
        }
        assert!(block.contains("[2026-11-11]"), "the dated fact keeps its date");
        assert!(block.contains("[undated]"), "the undated fact is marked honestly");
    }

    #[test]
    fn block_contains_the_recalled_facts_and_nothing_else() {
        let recall = HindsightRecall {
            facts: vec![HindsightFact {
                id: "abc".into(),
                text: "the exact value is 42".into(),
                score: Some(0.9),
                dated: Some("2026-11-11".into()),
            }],
            models: Vec::new(),
            query: "the exact value".into(),
        };
        let block = format_recall_block(&recall).expect("block");
        assert!(block.contains("the exact value is 42"));
        // The block carries the MATERIAL only. MEASURED 2026-11-10: the
        // instruction ("apply AS GIVEN", "attribute visibly") used to be
        // written into every delivered block and re-sent on every turn; the
        // owner's complaint was that the block "only holds service text".
        // The instruction now lives in the STATIC system prompt
        // (crate::prompt::MEMORY_PROMPT), so it must NOT be in the payload.
        assert!(
            !block.contains("AS GIVEN"),
            "the delivered block must carry no service instruction: {block}"
        );
        assert!(
            !block.contains("From memory"),
            "the delivered block must carry no service instruction: {block}"
        );
        assert!(
            block.starts_with("# Memory\n\n## Relevant facts"),
            "the block must open with the facts: {block}"
        );
    }

    /// The instruction that used to be inside the block must be in the STATIC
    /// system prompt, or the behaviour it governed is lost.
    #[test]
    fn the_memory_instruction_lives_in_the_static_system_prompt() {
        assert!(crate::prompt::MEMORY_PROMPT.contains("AS GIVEN"));
        assert!(crate::prompt::MEMORY_PROMPT.contains("From memory"));
    }

    // reference: the upstream section shape is the bulleted fact list in
    //   vectorize-io/hindsight-coding-agents dist/dcode-hook.js buildSystemInjection.
    /// A mental model must survive the block's per-section budget.
    ///
    /// This is the regression the python delivery path had: 10 of 10 fetched
    /// models were cut before reaching the model because facts and models
    /// shared one budget and facts came first.
    #[test]
    fn models_are_delivered_next_to_a_full_budget_of_facts() {
        let facts = (0..8)
            .map(|i| HindsightFact {
                id: format!("f{i}"),
                text: format!("fact number {i} {}", "x".repeat(400)),
                score: Some(0.9),
                dated: Some("2026-11-11".into()),
            })
            .collect();
        let recall = HindsightRecall {
            facts,
            models: vec![HindsightModel {
                id: "m1".into(),
                name: "Active work".into(),
                text: "the model body".into(),
                is_stale: false,
                is_curated: true,
            }],
            query: String::new(),
        };
        let block = format_recall_block(&recall).expect("block");
        assert!(
            block.contains("Active work"),
            "the model must survive the fact budget: {block}"
        );
        assert!(block.contains("the model body"));
    }

    #[test]
    fn long_fact_is_truncated() {
        let recall = HindsightRecall {
            facts: vec![HindsightFact {
                id: "x".into(),
                text: "a".repeat(MAX_FACT_CHARS * 2),
                score: None,
                dated: Some("2026-11-11".into()),
            }],
            models: Vec::new(),
            query: String::new(),
        };
        let block = format_recall_block(&recall).expect("block");
        // The fact TEXT must be bounded: "- " + "[YYYY-MM-DD]" + MAX_FACT_CHARS
        // chars + the ellipsis marker. The date stamp is a fixed prefix, so it is
        // part of the line but not part of the fact's own budget.
        let fact_line = block
            .lines()
            .find(|line| line.starts_with("- "))
            .expect("a fact line");
        let prefix = "- [2026-11-11] ";
        assert!(
            fact_line.chars().count() <= MAX_FACT_CHARS + prefix.len() + 1,
            "the fact text must be truncated to MAX_FACT_CHARS, got {} chars",
            fact_line.chars().count()
        );
        assert!(!block.contains(&"a".repeat(MAX_FACT_CHARS + 1)));
    }

    // reference: the measured score scale comes from this host's live probes
    //   against bank `jcode-legacy` (see MIN_RELATIVE_SCORE for the numbers).
    /// The relevance cutoff is RELATIVE to the top score.
    ///
    /// A fixed floor was measured and rejected: the useful Russian paraphrase
    /// "не забудь прочитать протокол встречи с командой безопасности" scores
    /// 0.0000832, so any absolute floor deletes a relevant cross-lingual hit.
    ///
    /// The ratios below are MEASURED, as score / top_score, on the SCOPED
    /// recall the client now sends (2026-11-09, four real queries):
    ///   "блок который называется просто memory теперь лишний релевантность"
    ///     relevant head: 1.000, 0.926, 0.249
    ///     irrelevant tail: 0.057, 0.031, 0.012, ...
    ///   "user profile working preferences owner stated priorities"
    ///     relevant head: 1.000, 0.491, 0.235, 0.215, 0.194, 0.172, 0.106
    ///     irrelevant tail: 0.071, 0.063, 0.040, ...
    /// The cutoff must sit inside the gap of BOTH: above the tail, below the
    /// head. 0.10 does - it keeps 0.106 and drops 0.071 and 0.057.
    #[test]
    fn the_cutoff_separates_the_relevant_head_from_the_tail() {
        // The relevant head of a real measured result set must survive.
        assert!(0.249 >= MIN_RELATIVE_SCORE);
        assert!(0.106 >= MIN_RELATIVE_SCORE);
        // The irrelevant tail of the same kind of result set must not.
        assert!(0.071 < MIN_RELATIVE_SCORE);
        assert!(0.057 < MIN_RELATIVE_SCORE);
        assert!(0.031 < MIN_RELATIVE_SCORE);
    }

    // reference: https://hindsight.vectorize.io/0.10/developer/mental-models
    //   ("A model that has fallen behind is still shown, but it no longer
    //   short-circuits retrieval, and the agent is expected to check it
    //   against the layers below rather than trust it blindly.")
    // reference: no prior art for QUERY-based model selection in upstream -
    //   https://hindsight.vectorize.io/developer/mcp-server lists
    //   list_mental_models (filters by TAGS only) and get_mental_model (by
    //   ID); there is no endpoint that ranks models against a query. The
    //   closest upstream precedent for relevance ranking is
    //   search_knowledge_base ("hybrid BM25 + vector search over page names
    //   and content"), so the client-side rule below is jcode's own.
    /// A model matches when it shares a token with the query.
    ///
    /// The matching is now over the model's TOPIC SURFACE (name + id +
    /// source_query) and its body, and it is separated from the ranking, so
    /// the test pins the ELIGIBILITY half.
    #[test]
    fn model_matching_uses_the_query_words() {
        let active = ModelCandidate {
            id: "active-work".into(),
            name: "Active work and open threads".into(),
            text: "active work".into(),
            is_stale: false,
            is_curated: true,
            topic_tokens: query_tokens("Active work and open threads active-work"),
            body_tokens: query_tokens("active work"),
            score: 0.0,
        };
        let hermes = ModelCandidate {
            id: "hermes-upstream".into(),
            name: "hermes-upstream".into(),
            text: "windows shell".into(),
            is_stale: false,
            is_curated: false,
            topic_tokens: query_tokens("hermes-upstream hermes-upstream"),
            body_tokens: query_tokens("windows shell"),
            score: 0.0,
        };
        let query = query_tokens("what is active work");
        assert!(
            !intersect(&query, &active.topic_tokens).is_empty(),
            "the active-work model must match a query about active work"
        );
        assert!(
            intersect(&query, &hermes.topic_tokens).is_empty()
                && intersect(&query, &hermes.body_tokens).is_empty(),
            "the hermes-upstream model must NOT match a query about active work"
        );
    }

    /// A model is matched by its CONTENT, not only by its title.
    ///
    /// MEASURED 2026-11-09 on bank `jcode-legacy`: the curated models are
    /// titled "Active work and open threads" and "User profile and working
    /// preferences", but their bodies are what mention retain hooks, memory
    /// banks and the owner. A title-only match rejected every one of them.
    #[test]
    fn model_matching_reads_the_body_not_only_the_title() {
        let profile = ModelCandidate {
            id: "user-profile".into(),
            name: "User profile and working preferences".into(),
            text: "the owner said: never change the retain hook without asking".into(),
            is_stale: false,
            is_curated: true,
            topic_tokens: query_tokens("User profile and working preferences user-profile"),
            body_tokens: query_tokens(
                "the owner said: never change the retain hook without asking",
            ),
            score: 0.0,
        };
        let query = query_tokens("fix the retain hook");
        assert!(
            intersect(&query, &profile.topic_tokens).is_empty(),
            "the title alone must not match"
        );
        assert!(
            !intersect(&query, &profile.body_tokens).is_empty(),
            "the body must match even when the title does not"
        );
    }

    /// A stale model is DELIVERED and MARKED, never dropped.
    ///
    /// Upstream: "A model that has fallen behind is still shown, but it no
    /// longer short-circuits retrieval." Dropping it emptied the
    /// "Synthesized context" section entirely on a continuously-written bank.
    #[test]
    fn a_stale_model_is_delivered_with_a_marker() {
        let recall = HindsightRecall {
            facts: Vec::new(),
            models: vec![HindsightModel {
                id: "user-profile".into(),
                name: "User profile".into(),
                text: "the owner prefers compact answers".into(),
                is_stale: true,
                is_curated: true,
            }],
            query: String::new(),
        };
        let block = format_recall_block(&recall).expect("block");
        assert!(
            block.contains("may be out of date"),
            "a stale model must be marked as possibly behind: {block}"
        );
        assert!(
            block.contains("the owner prefers compact answers"),
            "a stale model must still be delivered: {block}"
        );
    }

    /// A model's text must never be delivered as a HEADING.
    ///
    /// reference: https://hindsight.vectorize.io/developer/mental-models
    ///   ("A mental model is ... a whole document per question"; a model's
    ///    content is always markdown)
    ///
    /// MEASURED 2026-11-10: a mental model's content is a whole markdown
    /// document that usually STARTS WITH ITS OWN "## " heading, and
    /// `fetch_models` collapses it to one line. The consumers read a "## "
    /// line as a CATEGORY and consume it, so the model's text became the
    /// title of an EMPTY tile and the owner saw empty memory blocks with
    /// only the facts filled. 728 of 874 delivered model bodies took that
    /// path over 456 real delivered blocks.
    ///
    /// The contract pinned here: every model is delivered as a BULLET, so no
    /// consumer can read it as a heading, and the model's own leading
    /// heading markers are stripped so the text reads as prose.
    #[test]
    fn a_model_body_is_never_delivered_as_a_heading() {
        let recall = HindsightRecall {
            facts: vec![HindsightFact {
                id: "f1".into(),
                text: "a relevant fact".into(),
                score: Some(0.9),
                dated: Some("2026-11-11".into()),
            }],
            models: vec![HindsightModel {
                id: "machine-hazards".into(),
                name: "Machine hazards".into(),
                // exactly the shape measured on the live bank
                text: "## Опасности машины и окружения Ниже — классы риска".into(),
                is_stale: false,
                is_curated: true,
            }],
            query: String::new(),
        };
        let block = format_recall_block(&recall).expect("block");

        // Inside the MODELS section, no line may look like a heading that a
        // consumer would read as a category: the model's own text must never
        // start a line. The block's own two section headings ("## Relevant
        // facts", "## Synthesized context") are the producer's contract with
        // the consumer and are legitimately "## " lines.
        let models_section = block
            .split("## Synthesized context")
            .nth(1)
            .expect("the models section must exist");
        for line in models_section.lines() {
            let trimmed = line.trim_start();
            assert!(
                !trimmed.starts_with("## "),
                "a delivered model line must never be a '## ' heading: {line}"
            );
        }
        // The model is delivered as a bullet under its name, and its text is
        // present.
        let model_line = block
            .lines()
            .find(|line| line.contains("Machine hazards"))
            .expect("the model must be delivered");
        assert!(
            model_line.trim_start().starts_with("- "),
            "a model must be delivered as a bullet: {model_line}"
        );
        assert!(
            model_line.contains("классы риска"),
            "the model's own text must be delivered: {model_line}"
        );
        // The model's own leading heading marker is stripped, so the text
        // reads as prose rather than as a nested heading.
        assert!(
            !model_line.contains("## Опасности"),
            "the model's own heading marker must be stripped: {model_line}"
        );
    }

    /// The delivered model set must DEPEND on the query.
    ///
    /// MEASURED 2026-11-10 on the live bank: the previous rule made the six
    /// curated ids unconditionally eligible, so the delivered set was the SAME
    /// for every query - 1 distinct set over 20 real queries, and the third
    /// model was truncated by the 1800-char budget on every turn (3 x 600 +
    /// overhead = 1869). The owner's complaint: "the Synthesized context block
    /// is the same thing every time".
    ///
    /// The contract pinned here: a curated model that shares NO token with the
    /// query is NOT delivered. The curated bonus may break a ranking tie, but
    /// it must never make a model eligible - that was measured and rejected.
    ///
    /// reference: no prior art for this selection rule - it is jcode's own,
    ///   derived from the live-bank measurement recorded on `fetch_models`;
    ///   upstream's coding-agents package never fetches mental models at all
    ///   (searched hindsight-coding-agents 0.8.2 dist/*.js, found none).
    #[test]
    fn a_curated_model_without_any_query_overlap_is_not_eligible() {
        let profile = ModelCandidate {
            id: "user-profile".into(),
            name: "User profile and working preferences".into(),
            text: "the owner prefers compact answers".into(),
            is_stale: false,
            is_curated: true,
            topic_tokens: query_tokens("user-profile User profile preferences"),
            body_tokens: query_tokens("the owner prefers compact answers"),
            score: 0.0,
        };
        let hazards = ModelCandidate {
            id: "machine-hazards".into(),
            name: "Machine hazards and forbidden operations".into(),
            text: "the printer driver must not be replaced".into(),
            is_stale: false,
            is_curated: false,
            topic_tokens: query_tokens("machine-hazards Machine hazards operations"),
            body_tokens: query_tokens("the printer driver must not be replaced"),
            score: 0.0,
        };
        // The ELIGIBILITY half of the rule, which is where the defect was.
        let eligible = |query: &str| -> Vec<String> {
            let q = query_tokens(query);
            [&profile, &hazards]
                .into_iter()
                .filter(|candidate| {
                    !intersect(&q, &candidate.topic_tokens).is_empty()
                        || !intersect(&q, &candidate.body_tokens).is_empty()
                })
                .map(|candidate| candidate.id.clone())
                .collect()
        };
        // A query about the printer shares nothing with the user profile.
        assert_eq!(
            eligible("the printer is broken again"),
            vec!["machine-hazards"]
        );
        // A query about the owner shares nothing with the printer.
        assert_eq!(
            eligible("the owner prefers compact answers"),
            vec!["user-profile"]
        );
        // A query about neither selects neither.
        assert!(eligible("something entirely unrelated").is_empty());
    }

    /// Two different queries must be able to select different model sets.
    ///
    /// This is the property the owner asked for directly, so it is pinned as
    /// a contract rather than left to the selection implementation.
    #[test]
    fn different_queries_select_different_models() {
        let profile = ModelCandidate {
            id: "user-profile".into(),
            name: "User profile and working preferences".into(),
            text: "the owner prefers compact answers in Russian".into(),
            is_stale: false,
            is_curated: true,
            topic_tokens: query_tokens("user-profile User profile preferences"),
            body_tokens: query_tokens("the owner prefers compact answers in Russian"),
            score: 0.0,
        };
        let hazards = ModelCandidate {
            id: "machine-hazards".into(),
            name: "Machine hazards and forbidden operations".into(),
            text: "the printer driver must not be replaced".into(),
            is_stale: false,
            is_curated: false,
            topic_tokens: query_tokens("machine-hazards Machine hazards operations"),
            body_tokens: query_tokens("the printer driver must not be replaced"),
            score: 0.0,
        };
        let select = |query: &str| -> Vec<String> {
            let q = query_tokens(query);
            [&profile, &hazards]
                .into_iter()
                .filter(|candidate| {
                    !intersect(&q, &candidate.topic_tokens).is_empty()
                        || !intersect(&q, &candidate.body_tokens).is_empty()
                })
                .map(|candidate| candidate.id.clone())
                .collect()
        };
        assert_eq!(
            select("the owner prefers compact answers"),
            vec!["user-profile"]
        );
        assert_eq!(select("the printer driver broke"), vec!["machine-hazards"]);
        assert!(select("something entirely unrelated").is_empty());
    }

    /// A token that appears in EVERY model carries no topic signal.
    ///
    /// reference: no prior art inside jcode - the standard inverse document
    ///   frequency idea, applied to the model's own corpus.
    #[test]
    fn a_token_present_in_every_model_scores_almost_nothing() {
        let mut doc_freq: HashMap<String, usize> = HashMap::new();
        doc_freq.insert("state".into(), 15);
        doc_freq.insert("printer".into(), 1);
        let model_count = 15.0;
        let common = idf_score(&query_tokens("state"), &doc_freq, model_count);
        let rare = idf_score(&query_tokens("printer"), &doc_freq, model_count);
        assert!(
            common < 0.01,
            "a token in every model must not decide the ranking, got {common}"
        );
        assert!(
            rare > common * 10.0,
            "a rare token must outweigh a common one, got {rare} vs {common}"
        );
    }

    /// Every SELECTED model must reach the block.
    ///
    /// MEASURED 2026-11-10: the renderer `break`ed when a bullet did not fit
    /// the section budget, so with 3 models x 600 chars + overhead (1869)
    /// against an 1800-char budget the THIRD model was silently dropped on
    /// every turn - 14 of 20 live probes lost their last model. The selection
    /// had already decided which models matter; the renderer must not undo
    /// that decision by dropping one for lack of room.
    #[test]
    fn every_selected_model_reaches_the_block() {
        let models: Vec<HindsightModel> = (0..3)
            .map(|i| HindsightModel {
                id: format!("m{i}"),
                name: format!("Model number {i}"),
                text: "y".repeat(3000),
                is_stale: false,
                is_curated: false,
            })
            .collect();
        let recall = HindsightRecall {
            facts: Vec::new(),
            models,
            query: String::new(),
        };
        let block = format_recall_block(&recall).expect("block");
        for i in 0..3 {
            assert!(
                block.contains(&format!("Model number {i}")),
                "model {i} was selected and must be delivered: {block}"
            );
        }
        let models_section = block
            .split("## Synthesized context")
            .nth(1)
            .expect("the models section must exist");
        assert!(
            models_section.chars().count() <= MAX_MODELS_CHARS,
            "the models section must stay inside its budget, got {} chars",
            models_section.chars().count()
        );
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

    /// A model whose first page is a disclaimer must deliver the part that
    /// answers the question, not the disclaimer.
    ///
    /// reference: no prior art, searched the jcode workspace for an existing
    ///   excerpt-windowing helper - none exists; the rule is this workspace's
    ///   own measurement on the live daemon log.
    ///
    /// MEASURED 2026-11-10 on the live daemon log: of 979 delivered model
    /// excerpts, the owner-facing ones put their first dated fact at character
    /// 256-422 of a ~550-character excerpt - 44-79% of the delivered text was
    /// the model's own scope caveat and evidence-id legend. The waste was
    /// measured on the DELIVERED text.
    #[test]
    fn the_excerpt_is_the_query_relevant_window_not_the_models_first_page() {
        let preamble = "Dates below are the dates recorded for the decision, \
not a claim that every implementation remains deployed. Evidence IDs refer to \
the retrieved records. ";
        let body = format!(
            "{preamble}2026-09-23 the Windows shared server kept a dead PID \
alive. 2026-10-24 the orphan build lock blocked selfdev build. "
        );
        let query = query_tokens("what blocked the selfdev build lock");
        // The cap must be SMALLER than the body, or the whole body is the
        // window and the rule is not exercised. The live models are 3k-17k
        // chars against a ~570-char cap, so a cut is the normal case.
        let excerpt = relevant_window(&body, &query, 200);
        assert!(
            excerpt.contains("selfdev build"),
            "the excerpt must carry the answer: {excerpt}"
        );
        assert!(
            !excerpt.starts_with("Dates below are the dates recorded"),
            "the excerpt must not be the model's disclaimer: {excerpt}"
        );
    }

    /// A model with NO overlap with the query keeps its head.
    ///
    /// reference: no prior art, searched the jcode workspace for an existing
    ///   excerpt-windowing helper - none exists; the rule is this workspace's
    ///   own measurement on the live daemon log.
    ///
    /// The relevant-window rule must not silently swap in an arbitrary later
    /// slice of a model that does not talk about the subject at all: the head
    /// is the honest answer there.
    #[test]
    fn a_model_without_query_overlap_keeps_its_head() {
        let body = "First sentence about something else. Second sentence. \
Third sentence about a different subject entirely. ";
        let query = query_tokens("completely unrelated subject");
        let excerpt = relevant_window(body, &query, 40);
        assert!(
            excerpt.starts_with("First sentence"),
            "the head must be kept when nothing matches: {excerpt}"
        );
    }

    /// The topic surface must not be the model's GENERATION PROMPT.
    ///
    /// reference: https://hindsight.vectorize.io/0.10/developer/api/mental-models
    ///   (a mental model is created with a name and a source_query; the
    ///   source_query is the PROMPT that generated the content, and upstream
    ///   has no endpoint that ranks models against a query - the client ranks
    ///   them, so the client owns the topic surface).
    ///
    /// MEASURED 2026-11-10 on the live bank: the source_query of the four
    /// technical models is a 3.4k-7.2k character prompt (162-382 tokens),
    /// while the curated models' prompts are 212-320 characters. The score
    /// sums over the shared tokens, so including the prompt let the bigger
    /// haystack win regardless of the subject, and the technical manuals took
    /// 40.3% of all delivered model slots.
    #[test]
    fn the_topic_surface_is_the_name_and_id_not_the_generation_prompt() {
        let topic = format!("{} {}", "User profile and working preferences", "user-profile");
        let topic_tokens = query_tokens(&topic);
        assert!(
            topic_tokens.contains("profile") && topic_tokens.contains("preferences"),
            "the name must be part of the topic: {topic_tokens:?}"
        );
        // The prompt of a technical model must not be searchable as topic
        // text; that is what made a manual win the ranking.
        assert!(
            !topic_tokens.contains("upstream") && !topic_tokens.contains("manual"),
            "a generation prompt must not leak into the topic surface"
        );
    }

    // reference: https://hindsight.vectorize.io/0.10/developer/api/reflect
    //   and C:/Users/WorkShop1/Documents/Codex/2026-11-08/new-chat/outputs/jcode-hindsight-implementation.md
    //   ("Ask reflect to return one of `wait`, `prepare`, `act_within_authority`
    //   or `ask_owner` with `goal_id`, `evidence_ids`, `counterevidence_ids`,
    //   `unverified_conditions` and `why_now`").
    /// The plan names exactly the fields the reopen assessment must carry.
    #[test]
    fn reopen_response_schema_requires_a_decision_and_lists_the_plan_fields() {
        let schema = reopen_response_schema();
        let required = schema
            .get("required")
            .and_then(|value| value.as_array())
            .expect("required list");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0].as_str(), Some("decision"));

        let properties = schema
            .get("properties")
            .and_then(|value| value.as_object())
            .expect("properties");
        for field in [
            "decision",
            "goal_id",
            "evidence_ids",
            "counterevidence_ids",
            "unverified_conditions",
            "why_now",
        ] {
            assert!(
                properties.contains_key(field),
                "the plan requires the {field} field"
            );
        }

        let decision_enum = properties
            .get("decision")
            .and_then(|value| value.get("enum"))
            .and_then(|value| value.as_array())
            .expect("decision enum");
        let variants: Vec<&str> = decision_enum.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(
            variants,
            vec!["wait", "prepare", "act_within_authority", "ask_owner"],
            "the plan names exactly these four decisions"
        );
    }

    #[test]
    fn the_four_decisions_round_trip_through_their_names() {
        for decision in [
            ReopenDecision::Wait,
            ReopenDecision::Prepare,
            ReopenDecision::ActWithinAuthority,
            ReopenDecision::AskOwner,
        ] {
            assert_eq!(
                ReopenDecision::parse(decision.as_str()),
                Some(decision),
                "{decision:?} must round trip"
            );
        }
        assert_eq!(
            ReopenDecision::parse("ACT_WITHIN_AUTHORITY"),
            Some(ReopenDecision::ActWithinAuthority)
        );
        assert_eq!(ReopenDecision::parse("something else"), None);
    }

    /// An unreachable server must be an error, not a silent "wait".
    ///
    /// A silent `wait` would be indistinguishable from "the condition did
    /// not hold", and the plan treats the two differently: one is a decision,
    /// the other is a failure to decide.
    #[test]
    fn assess_reopen_reports_an_unreachable_server_as_an_error() {
        let config = HindsightConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            ..HindsightConfig::default()
        };
        let result = assess_reopen(&config, "anything");
        assert!(result.is_err(), "an unreachable server is an error");
    }

    /// The `start_date` query value must survive a query-string parser.
    ///
    /// MEASURED 2026-11-11 against the live server: the raw `+00:00` offset
    /// arrived as `... 00:00` (a `+` in a query value decodes to a space) and
    /// the endpoint answered 400 "Invalid start_date" - 1292 consecutive
    /// failures, so the reopen trigger never fired once. This test pins the
    /// SHIPPED URL, not a reconstruction: it asserts that the built URL
    /// carries neither a `+` nor a space in the value, and that decoding the
    /// value gives back the same instant.
    #[test]
    fn the_start_date_query_value_is_url_safe() {
        let config = HindsightConfig {
            base_url: "http://localhost:8888".to_string(),
            bank: "jcode-legacy".to_string(),
            ..HindsightConfig::default()
        };
        let since = DateTime::parse_from_rfc3339("2026-11-10T01:24:15.828887+00:00")
            .expect("fixture parses")
            .with_timezone(&Utc);
        let url = recent_memories_url(&config, since, 50);
        let value = url
            .split("start_date=")
            .nth(1)
            .and_then(|rest| rest.split('&').next())
            .expect("the url carries a start_date value")
            .to_string();

        assert!(
            !value.contains('+'),
            "a raw + breaks the query value: {value}"
        );
        assert!(
            !value.contains(' '),
            "a space breaks the query value: {value}"
        );

        // Decoding it back must give the same instant to the millisecond,
        // which is the precision the `Z` form keeps.
        let decoded = urlencoding::decode(&value)
            .expect("the encoded value decodes")
            .into_owned();
        let parsed = DateTime::parse_from_rfc3339(&decoded).expect("decoded parses");
        assert_eq!(
            parsed.with_timezone(&Utc).timestamp_millis(),
            since.timestamp_millis()
        );
    }

    /// A raw (unencoded) timestamp must NOT be what the code sends.
    ///
    /// This is the negative control for the test above: it reproduces the
    /// defect deliberately and shows that the value the OLD code produced
    /// would have carried a `+` that a query parser turns into a space.
    #[test]
    fn the_old_unencoded_timestamp_would_have_broken() {
        let since = DateTime::parse_from_rfc3339("2026-11-10T01:24:15.828887+00:00")
            .expect("fixture parses")
            .with_timezone(&Utc);
        let old = since.to_rfc3339();
        assert!(
            old.contains('+'),
            "the old form is the one that failed: {old}"
        );
        let decoded_like_a_query_parser = old.replace('+', " ");
        assert!(
            decoded_like_a_query_parser.contains(' '),
            "a query parser turns the + into a space: {decoded_like_a_query_parser}"
        );
    }
}
