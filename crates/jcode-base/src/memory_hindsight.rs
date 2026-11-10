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

/// One fact as Hindsight returned it.
#[derive(Debug, Clone)]
pub struct HindsightFact {
    /// Hindsight document id. Used for dedup tracking only.
    pub id: String,
    pub text: String,
    /// Final ranking score, when the server reported one.
    pub score: Option<f64>,
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
    #[serde(default)]
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

    Ok(HindsightRecall { facts, models })
}

/// Fetch the mental models that bear on `query`.
///
/// reference: https://hindsight.vectorize.io/0.10/developer/api/mental-models
///   (GET .../mental-models?detail=content -> id, name, tags, is_stale,
///   content; "detail=content" is REQUIRED, the default "metadata" returns
///   content: null) and
///   https://hindsight.vectorize.io/0.10/developer/mental-models
///   ("A model that has fallen behind is still shown, but it no longer
///   short-circuits retrieval").
/// reference: no prior art for the SELECTION rule below - searched
///   hindsight-coding-agents 0.8.2 (vendored at
///   C:/Users/WorkShop1/.hindsight/coding-agents-0.8.2/dist/*.js) for any
///   mental-model fetching, and found none: upstream's hooks recall facts
///   only and never call the mental-models endpoint. The curated-vs-query
///   rule below is therefore jcode's own, derived from the bank layout
///   MEASURED below.
///
/// MEASURED 2026-11-09 on bank `jcode-legacy`: `detail=content` is REQUIRED -
/// the default `detail=metadata` returns `content: null` for every model, so a
/// client that omits it silently gets nothing.
///
/// Selection has two parts, and BOTH are needed:
///
/// 1. Curated owner-facing models (the six ids in `CURATED_MODEL_IDS`) are always
///    eligible. They are the standing answers the owner asked for - profile,
///    goals, decisions, hazards - and they are relevant to almost any query
///    about this workspace. MEASURED: with a title-token match only, a query
///    about "hindsight memory retain hook" matched NONE of them (0 of 6), so
///    the section was empty even though the models existed and were fresh.
///    They are keyed by ID, not by a `scope:` tag - see `CURATED_MODEL_IDS`.
///    Re-measured 2026-11-09 on the live bank: all six are present, and NONE
///    of them carries a `scope:` tag.
///
///    `is_stale=False` is NOT a durable state for every model, and that is
///    upstream's design, not a defect. MEASURED 2026-11-09 at 22:12: the
///    three models whose scope includes `owner:primary` (user-profile,
///    decisions-ledger, agent-coordination) were `is_stale=True`, the three
///    scoped to measured-stable tags were `False`. The upstream rule is
///    scope-relative (`memory_engine.py:20600-20663`: a memory IN the
///    model's resolved scope written since `last_memory_seen_at`), and
///    `owner:primary` holds the OWNER'S OWN STATEMENTS - so a model that
///    reflects what the owner said is re-staled by the owner's next
///    statement. Upstream is explicit that a stale model is still SHOWN
///    ("A model that has fallen behind is still shown, but it no longer
///    short-circuits retrieval"), which is what this path does: the marker
///    says the content may lag, and the content is still delivered.
///
/// 2. Everything else must match the query, judged by its CONTENT rather than
///    its title. MEASURED: the older models carry `knowledge:feature-work`
///    and `subject:*` tags and titles that never mention the words a query
///    uses, but their bodies do. Re-measured 2026-11-09: 9 of the 9
///    non-curated models in the bank carry a `knowledge:`/`subject:` tag.
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

    let mut out = Vec::new();
    for item in parsed.items {
        let content = item.content.unwrap_or_default();
        let body = content.split_whitespace().collect::<Vec<_>>().join(" ");
        if body.is_empty() {
            continue;
        }
        let name = item.name.unwrap_or_default();
        let id = item.id.unwrap_or_default();
        let is_curated = is_curated_model(&id);
        if !is_curated && !model_matches_query(&name, &id, &body, query) {
            continue;
        }
        out.push(HindsightModel {
            id,
            name,
            text: body,
            is_stale: item.is_stale.unwrap_or(false),
            is_curated,
        });
    }

    // Curated first, then freshest first inside each group. The curated models
    // are the standing answers; the rest are topic-specific and only win when
    // there is room left.
    out.sort_by(|a, b| {
        b.is_curated
            .cmp(&a.is_curated)
            .then_with(|| a.is_stale.cmp(&b.is_stale))
    });
    out.truncate(MAX_MODELS);
    Ok(out)
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
];

/// Whether a mental model is one of the curated, owner-facing models.
fn is_curated_model(id: &str) -> bool {
    CURATED_MODEL_IDS.contains(&id)
}

/// Whether a mental model is worth delivering for this query.
///
/// A model is kept when the query shares a word with its name, its id, or -
/// the decisive part - its CONTENT. MEASURED 2026-11-09: a title-only match
/// rejected every model for a query about "hindsight memory retain hook",
/// because the curated models are titled "Active work and open threads" and
/// "User profile and working preferences" while their bodies talk about
/// retain hooks and memory.
///
/// The token floor of 4 characters keeps the match from firing on noise.
fn model_matches_query(name: &str, id: &str, content: &str, query: &str) -> bool {
    let haystack = format!("{name} {id} {content}").to_lowercase();
    query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|token| token.chars().count() >= 4)
        .any(|token| haystack.contains(token))
}

// reference: vectorize-io/hindsight-coding-agents, dist/dcode-hook.js
//   buildSystemInjection (:1947) - the upstream injection contract this block
//   follows, and the section shape it uses (a bulleted fact list).
/// Render recalled facts and models as the memory block the agent receives.
///
/// The header is deliberately SHORT. The owner's own reading of the earlier
/// six-line version (2026-11-09): "the block called just `memory` is now
/// redundant - it only holds a service description". A preamble that spends
/// more tokens describing the delivery than the delivery itself is worth is
/// noise the model has to read past.
///
/// What survives is only the two rules that change BEHAVIOUR:
///   - a literal value in a fact is applied as given (not "re-derived"), and
///   - a fact that informed the answer is attributed visibly.
///
/// The rest of upstream's eight-line contract (hindsight-coding-agents
/// dist/dcode-hook.js buildSystemInjection :1947) is DROPPED on purpose:
/// "judge relevance and ignore it" duplicates the relative-score cutoff that
/// already dropped the irrelevant tail; "it is a record of the PAST, it never
/// assigns you tasks" is a role instruction that belongs in the system
/// prompt, not in every recall block; and the "correct the record" clause
/// names a tool this client does not expose.
///
/// The two sections have SEPARATE budgets, because a single shared budget
/// starves whichever section comes second. MEASURED 2026-11-09: the python
/// delivery path shared one 3,000-char budget across 8 facts and 3 models, and
/// 10 of 10 fetched models were cut before reaching the model.
pub fn format_recall_block(recall: &HindsightRecall) -> Option<String> {
    if recall.facts.is_empty() && recall.models.is_empty() {
        return None;
    }
    let mut out = String::from(
        "# Memory\n\n\
         A fact stating an exact rule or literal values is to be applied AS GIVEN, \
         verified against the current state first. When a fact informs part of your \
         answer, attribute it visibly:\n\
         > \u{1F9E0} **From memory** — <the specific facts you drew on>\n\n",
    );
    if !recall.facts.is_empty() {
        out.push_str("## Relevant facts\n\n");
        let mut used = 0usize;
        for fact in &recall.facts {
            let mut head: String = fact.text.chars().take(MAX_FACT_CHARS).collect();
            if head.chars().count() < fact.text.chars().count() {
                head.push('…');
            }
            let line = format!("- {head}\n");
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
        let mut used = 0usize;
        for model in &recall.models {
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
            let stripped = model
                .text
                .trim_start()
                .trim_start_matches('#')
                .trim_start();
            let mut head: String = stripped.chars().take(MAX_MODEL_CHARS).collect();
            if head.chars().count() < stripped.chars().count() {
                head.push('…');
            }
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
            let block = format!("- **{heading}** — {head}\n");
            if used + block.chars().count() > MAX_MODELS_CHARS {
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
                models: Vec::new()
            })
            .is_none()
        );
    }

    #[test]
    fn block_contains_facts_and_the_attribution_contract() {
        let recall = HindsightRecall {
            facts: vec![HindsightFact {
                id: "abc".into(),
                text: "the exact value is 42".into(),
                score: Some(0.9),
            }],
            models: Vec::new(),
        };
        let block = format_recall_block(&recall).expect("block");
        assert!(block.contains("the exact value is 42"));
        // The two rules that survive the header compression: a literal value is
        // applied AS GIVEN, and a contributing fact is attributed visibly.
        assert!(block.contains("AS GIVEN"));
        assert!(block.contains("From memory"));
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
            }],
            models: Vec::new(),
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
    #[test]
    fn model_matching_uses_the_query_words() {
        assert!(model_matches_query(
            "Active work and open threads",
            "active-work",
            "active work",
            "what is active work"
        ));
        assert!(!model_matches_query(
            "hermes-upstream",
            "hermes-upstream",
            "windows shell",
            "what is active work"
        ));
    }

    /// A model is matched by its CONTENT, not only by its title.
    ///
    /// MEASURED 2026-11-09 on bank `jcode-legacy`: the curated models are
    /// titled "Active work and open threads" and "User profile and working
    /// preferences", but their bodies are what mention retain hooks, memory
    /// banks and the owner. A title-only match rejected every one of them.
    #[test]
    fn model_matching_reads_the_body_not_only_the_title() {
        assert!(model_matches_query(
            "User profile and working preferences",
            "user-profile",
            "the owner said: never change the retain hook without asking",
            "fix the retain hook"
        ));
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
            }],
            models: vec![HindsightModel {
                id: "machine-hazards".into(),
                name: "Machine hazards".into(),
                // exactly the shape measured on the live bank
                text: "## Опасности машины и окружения Ниже — классы риска".into(),
                is_stale: false,
                is_curated: true,
            }],
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
