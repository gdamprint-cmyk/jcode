//! Live model pricing catalog backed by <https://models.dev>.
//!
//! jcode's static pricing tables (`jcode_provider_core::pricing`) only cover
//! first-party Anthropic/OpenAI models and go stale whenever a provider ships
//! new models or changes prices. models.dev publishes a free, no-auth JSON
//! catalog (`https://models.dev/api.json`) with per-model `input`/`output`/
//! `cache_read`/`cache_write` USD prices per million tokens across 140+
//! providers, including every OpenAI-compatible profile jcode ships.
//!
//! This module mirrors the OpenRouter catalog pattern:
//!   - a 24h disk cache under `~/.jcode/cache/models_dev_pricing.json`,
//!   - synchronous lookups that never block on the network,
//!   - a background refresh scheduled on cache miss/staleness.
//!
//! Lookup order for callers is curated static table first (exact, reviewed),
//! then this catalog, then provider-specific sources (OpenRouter endpoints),
//! and only then a generic fallback.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const API_URL: &str = "https://models.dev/api.json";
const CACHE_FILE: &str = "models_dev_pricing.json";
const CACHE_TTL_SECS: u64 = 24 * 60 * 60;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-model USD prices per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelCost {
    pub input_usd_per_mtok: f64,
    pub output_usd_per_mtok: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_per_mtok: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_per_mtok: Option<f64>,
    /// Published total context window in tokens from models.dev
    /// (`limit.context`). Absent in caches written before this field existed;
    /// `#[serde(default)]` keeps those older cache files loadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PricingCache {
    cached_at_unix_secs: u64,
    /// provider id -> model id -> cost. Provider ids are models.dev ids
    /// (e.g. `anthropic`, `openai`, `deepseek`, `moonshotai`).
    providers: HashMap<String, HashMap<String, ModelCost>>,
}

/// In-memory pricing cache keyed by the on-disk cache path it was loaded
/// from, so changing `JCODE_HOME` (tests, multi-home setups) never serves
/// pricing that belongs to a different home directory.
///
/// Held behind an `Arc` so per-route pricing lookups share one parsed catalog
/// instead of deep-cloning the multi-thousand-model map on every call (that
/// clone dominated server CPU during client connect bursts).
static MEMORY_CACHE: Mutex<Option<(PathBuf, Arc<PricingCache>)>> = Mutex::new(None);
static REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn cache_path() -> PathBuf {
    crate::storage::jcode_dir()
        .unwrap_or_else(|_| PathBuf::from(".").join(".jcode"))
        .join("cache")
        .join(CACHE_FILE)
}

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cache() -> Option<Arc<PricingCache>> {
    let path = cache_path();
    {
        let memory = MEMORY_CACHE.lock().ok()?;
        if let Some((cached_path, cache)) = memory.as_ref()
            && cached_path == &path
        {
            return Some(Arc::clone(cache));
        }
    }
    let cache: PricingCache = crate::storage::read_json(&path).ok()?;
    let cache = Arc::new(cache);
    if let Ok(mut memory) = MEMORY_CACHE.lock() {
        *memory = Some((path, Arc::clone(&cache)));
    }
    Some(cache)
}

fn save_cache(cache: &PricingCache) {
    let path = cache_path();
    if let Ok(mut memory) = MEMORY_CACHE.lock() {
        *memory = Some((path.clone(), Arc::new(cache.clone())));
    }
    let _ = crate::storage::write_json(&path, cache);
}

/// Translate a jcode provider key (runtime key, activity source key, or
/// compatible-profile id) to the models.dev provider id.
pub fn models_dev_provider_id(jcode_provider: &str) -> Option<&'static str> {
    let key = jcode_provider
        .trim()
        .strip_prefix("openai-compatible:")
        .unwrap_or_else(|| jcode_provider.trim());
    Some(match key {
        "anthropic" | "claude" | "claude:api-key" | "anthropic-api" => "anthropic",
        "openai" | "openai:api-key" | "openai-api" => "openai",
        "openrouter" => "openrouter",
        "opencode" => "opencode",
        "opencode-go" => "opencode-go",
        "deepseek" => "deepseek",
        "moonshotai" => "moonshotai",
        "kimi" => "kimi-for-coding",
        "zai" => "zai",
        "cerebras" => "cerebras",
        "groq" => "groq",
        "mistral" => "mistral",
        "xai" => "xai",
        "minimax" => "minimax",
        "togetherai" => "togetherai",
        "fireworks" => "fireworks-ai",
        "deepinfra" => "deepinfra",
        "perplexity" => "perplexity",
        "nebius" => "nebius",
        "scaleway" => "scaleway",
        "stackit" => "stackit",
        "huggingface" => "huggingface",
        "baseten" => "baseten",
        "chutes" => "chutes",
        "nvidia-nim" => "nvidia",
        "302ai" => "302ai",
        "cortecs" => "cortecs",
        "alibaba-coding-plan" => "alibaba",
        "bedrock" => "amazon-bedrock",
        "azure-openai" | "azure" => "azure",
        "gemini" | "gemini-api" => "google",
        _ => return None,
    })
}

/// Strip jcode-local suffixes/prefixes a model id may carry before catalog
/// lookup (`[1m]` long-context alias, `provider/` prefixes for OpenRouter ids).
fn normalize_model_id(model: &str) -> &str {
    let model = jcode_provider_core::model_id::strip_long_context_suffix(model).trim();
    model
        .rsplit_once('@')
        .map_or(model, |(bare, _)| bare.trim())
}

/// Look up live pricing for `model` under a jcode provider key. Returns `None`
/// when the catalog has no entry; never blocks on the network. Schedules a
/// background refresh when the disk cache is missing or stale.
pub fn lookup(jcode_provider: &str, model: &str) -> Option<ModelCost> {
    let provider_id = models_dev_provider_id(jcode_provider)?;
    let cache = ensure_cache_fresh()?;
    let models = cache.providers.get(provider_id)?;
    let model = normalize_model_id(model);
    if let Some(cost) = models.get(model) {
        return Some(*cost);
    }
    // OpenRouter-style ids (`anthropic/claude-...`) may reach here with the
    // provider prefix still attached; retry on the bare model name.
    if let Some((_, bare)) = model.rsplit_once('/') {
        return models.get(bare).copied();
    }
    None
}

/// Return the freshest cache available, scheduling a refresh if needed.
fn ensure_cache_fresh() -> Option<Arc<PricingCache>> {
    let cache = load_cache();
    let stale = cache
        .as_ref()
        .map(|c| now_unix_secs().saturating_sub(c.cached_at_unix_secs) >= CACHE_TTL_SECS)
        .unwrap_or(true);
    if stale {
        schedule_refresh();
    }
    cache
}

/// Spawn one background refresh at a time. Safe to call from sync contexts;
/// uses a thread + ad-hoc runtime when no Tokio runtime is active.
pub fn schedule_refresh() {
    // Keep tests hermetic: never hit the network from test builds (the
    // `test-support` feature also covers downstream crates' test targets via
    // feature unification), and let users opt out entirely.
    // JCODE_FORCE_PRICING_REFRESH=1 re-enables the fetch for manual e2e checks
    // (e.g. `cargo run --example pricing_e2e_check`, which builds with
    // test-support unified in).
    let forced = std::env::var_os("JCODE_FORCE_PRICING_REFRESH").is_some();
    if !forced
        && (cfg!(any(test, feature = "test-support"))
            || std::env::var_os("JCODE_DISABLE_PRICING_REFRESH").is_some())
    {
        return;
    }
    if REFRESH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    let work = || async {
        if let Err(e) = refresh_now().await {
            crate::logging::warn(&format!("models.dev pricing refresh failed: {e:#}"));
        }
        REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);
    };
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(work());
    } else {
        std::thread::spawn(move || {
            if let Ok(runtime) = tokio::runtime::Runtime::new() {
                runtime.block_on(work());
            } else {
                REFRESH_IN_FLIGHT.store(false, Ordering::SeqCst);
            }
        });
    }
}

/// Fetch the catalog and persist the parsed cache.
async fn refresh_now() -> anyhow::Result<()> {
    let client = crate::provider::shared_http_client();
    let response = client
        .get(API_URL)
        .header("Accept", "application/json")
        .timeout(HTTP_TIMEOUT)
        .send()
        .await?;
    if !response.status().is_success() {
        anyhow::bail!("HTTP {}", response.status());
    }
    let body = response.text().await?;
    let cache = parse_api_response(&body)?;
    save_cache(&cache);
    crate::logging::info(&format!(
        "models.dev pricing refreshed: {} providers, {} priced models",
        cache.providers.len(),
        cache.providers.values().map(HashMap::len).sum::<usize>()
    ));
    Ok(())
}

fn parse_api_response(body: &str) -> anyhow::Result<PricingCache> {
    let json: serde_json::Value = serde_json::from_str(body)?;
    let top = json
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("expected top-level provider object"))?;

    let mut providers: HashMap<String, HashMap<String, ModelCost>> = HashMap::new();
    for (provider_id, provider) in top {
        let Some(models) = provider.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        let mut parsed_models = HashMap::new();
        for (model_id, model) in models {
            let Some(cost) = model.get("cost") else {
                continue;
            };
            let (Some(input), Some(output)) = (
                cost.get("input").and_then(|v| v.as_f64()),
                cost.get("output").and_then(|v| v.as_f64()),
            ) else {
                continue;
            };
            parsed_models.insert(
                model_id.clone(),
                ModelCost {
                    input_usd_per_mtok: input,
                    output_usd_per_mtok: output,
                    cache_read_usd_per_mtok: cost.get("cache_read").and_then(|v| v.as_f64()),
                    cache_write_usd_per_mtok: cost.get("cache_write").and_then(|v| v.as_f64()),
                    context_window: model
                        .get("limit")
                        .and_then(|limit| limit.get("context"))
                        .and_then(|v| v.as_u64()),
                },
            );
        }
        if !parsed_models.is_empty() {
            providers.insert(provider_id.clone(), parsed_models);
        }
    }

    if providers.is_empty() {
        anyhow::bail!("no priced models in models.dev response");
    }
    Ok(PricingCache {
        cached_at_unix_secs: now_unix_secs(),
        providers,
    })
}

/// Infer the models.dev provider namespace for an arbitrary model id, for
/// lookups where the active jcode provider has no direct models.dev mapping
/// (e.g. generic `openai-compatible` gateways serving open-weight models).
///
/// Only id shapes with strong first-party identity are matched, so an
/// ambiguous id resolves to no namespace rather than to a wrong window.
fn infer_provider_for_model(model: &str) -> Option<&'static str> {
    let m = model.trim().to_ascii_lowercase();
    let m = m.rsplit_once('/').map_or(m.as_str(), |(_, bare)| bare);
    if m.starts_with("glm-") {
        Some("zai")
    } else if m.starts_with("kimi-") {
        Some("moonshotai")
    } else if m.starts_with("deepseek-") {
        Some("deepseek")
    } else if m.starts_with("minimax-") {
        Some("minimax")
    } else if m.starts_with("mimo-") {
        Some("xiaomi")
    } else {
        None
    }
}

/// Look up the published context window (tokens) for `model` from the cached
/// models.dev catalog. Resolution order:
/// 1. first-party namespace inferred from the model id itself, because that
///    carries the model's published window (gateway-specific entries are
///    sometimes truncated by the serving deployment);
/// 2. `jcode_provider`'s own models.dev namespace, when one is mapped.
///
/// Never blocks on the network; schedules the same throttled background
/// refresh as pricing lookups when the cache is missing or stale.
pub fn lookup_context_limit(jcode_provider: &str, model: &str) -> Option<u64> {
    let cache = ensure_cache_fresh()?;
    let model = normalize_model_id(model);
    let mut candidates: [Option<&str>; 2] = [infer_provider_for_model(model), None];
    candidates[1] = models_dev_provider_id(jcode_provider);
    for provider_id in candidates.into_iter().flatten() {
        let Some(models) = cache.providers.get(provider_id) else {
            continue;
        };
        if let Some(cost) = models.get(model) {
            return cost.context_window;
        }
        if let Some((_, bare)) = model.rsplit_once('/')
            && let Some(cost) = models.get(bare)
        {
            return cost.context_window;
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn save_test_cache(entries: &[(&str, &str, ModelCost)]) {
    let mut providers: HashMap<String, HashMap<String, ModelCost>> = HashMap::new();
    for (provider, model, cost) in entries {
        providers
            .entry((*provider).to_string())
            .or_default()
            .insert((*model).to_string(), *cost);
    }
    save_cache(&PricingCache {
        cached_at_unix_secs: now_unix_secs(),
        providers,
    });
}

#[cfg(test)]
pub(crate) fn clear_memory_cache_for_tests() {
    if let Ok(mut memory) = MEMORY_CACHE.lock() {
        *memory = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_models_dev_shape() {
        let body = r#"{
            "deepseek": {
                "id": "deepseek",
                "models": {
                    "deepseek-v4-flash": {
                        "cost": {"input": 0.14, "output": 0.28, "cache_read": 0.0028}
                    },
                    "free-model": {"cost": {"input": 0, "output": 0}},
                    "no-cost-model": {}
                }
            },
            "anthropic": {
                "models": {
                    "claude-fable-5": {
                        "cost": {"input": 10, "output": 50, "cache_read": 1, "cache_write": 12.5}
                    }
                }
            }
        }"#;
        let cache = parse_api_response(body).expect("parsed");
        let deepseek = cache.providers.get("deepseek").expect("deepseek");
        assert_eq!(deepseek.len(), 2, "model without cost is skipped");
        let flash = deepseek.get("deepseek-v4-flash").expect("flash");
        assert!((flash.input_usd_per_mtok - 0.14).abs() < 1e-9);
        assert!((flash.output_usd_per_mtok - 0.28).abs() < 1e-9);
        assert_eq!(flash.cache_read_usd_per_mtok, Some(0.0028));
        assert_eq!(flash.cache_write_usd_per_mtok, None);

        let fable = cache
            .providers
            .get("anthropic")
            .and_then(|m| m.get("claude-fable-5"))
            .expect("fable");
        assert!((fable.input_usd_per_mtok - 10.0).abs() < 1e-9);
        assert_eq!(fable.cache_write_usd_per_mtok, Some(12.5));
    }

    #[test]
    fn rejects_empty_response() {
        assert!(parse_api_response("{}").is_err());
        assert!(parse_api_response("[]").is_err());
    }

    #[test]
    fn provider_key_mapping_covers_jcode_providers() {
        assert_eq!(models_dev_provider_id("claude:api-key"), Some("anthropic"));
        assert_eq!(models_dev_provider_id("openai:api-key"), Some("openai"));
        assert_eq!(
            models_dev_provider_id("openai-compatible:deepseek"),
            Some("deepseek")
        );
        assert_eq!(
            models_dev_provider_id("openai-compatible:nvidia-nim"),
            Some("nvidia")
        );
        assert_eq!(models_dev_provider_id("bedrock"), Some("amazon-bedrock"));
        assert_eq!(models_dev_provider_id("unknown-thing"), None);
    }

    #[test]
    fn lookup_normalizes_model_ids() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());
        clear_memory_cache_for_tests();

        save_test_cache(&[
            (
                "anthropic",
                "claude-opus-4-6",
                ModelCost {
                    input_usd_per_mtok: 5.0,
                    output_usd_per_mtok: 25.0,
                    cache_read_usd_per_mtok: Some(0.5),
                    cache_write_usd_per_mtok: Some(6.25),
                    context_window: None,
                },
            ),
            (
                "openrouter",
                "kimi-k2",
                ModelCost {
                    input_usd_per_mtok: 0.5,
                    output_usd_per_mtok: 2.0,
                    cache_read_usd_per_mtok: None,
                    cache_write_usd_per_mtok: None,
                    context_window: None,
                },
            ),
        ]);

        // [1m] suffix strips before lookup.
        let opus = lookup("claude:api-key", "claude-opus-4-6[1m]").expect("priced");
        assert!((opus.input_usd_per_mtok - 5.0).abs() < 1e-9);

        // provider/model ids fall back to the bare model name.
        let kimi = lookup("openrouter", "moonshotai/kimi-k2").expect("priced");
        assert!((kimi.output_usd_per_mtok - 2.0).abs() < 1e-9);
        let pinned = lookup("openrouter", "moonshotai/kimi-k2@Sail Research").expect("priced");
        assert!((pinned.output_usd_per_mtok - 2.0).abs() < 1e-9);

        assert!(lookup("claude:api-key", "claude-unknown").is_none());

        clear_memory_cache_for_tests();
        if let Some(prev) = prev_home {
            crate::env::set_var("JCODE_HOME", prev);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[test]
    fn parses_limit_context_into_context_window() {
        let body = r#"{
            "zai": {
                "models": {
                    "glm-5.3-flash": {
                        "cost": {"input": 0.0, "output": 0.0},
                        "limit": {"context": 1000000, "output": 131072}
                    },
                    "glm-legacy": {"cost": {"input": 0.1, "output": 0.2}}
                }
            }
        }"#;
        let cache = parse_api_response(body).expect("parsed");
        let zai = cache.providers.get("zai").expect("zai");
        assert_eq!(zai.get("glm-5.3-flash").and_then(|c| c.context_window), Some(1_000_000));
        assert_eq!(zai.get("glm-legacy").and_then(|c| c.context_window), None);
    }

    #[test]
    fn context_limit_lookup_prefers_inferred_namespace_then_provider() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let prev_home = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", temp.path());
        clear_memory_cache_for_tests();

        save_test_cache(&[
            (
                "zai",
                "glm-5.3-flash",
                ModelCost {
                    input_usd_per_mtok: 0.0,
                    output_usd_per_mtok: 0.0,
                    cache_read_usd_per_mtok: None,
                    cache_write_usd_per_mtok: None,
                    context_window: Some(1_000_000),
                },
            ),
            (
                "openrouter",
                "glm-5.3-flash",
                ModelCost {
                    input_usd_per_mtok: 0.0,
                    output_usd_per_mtok: 0.0,
                    cache_read_usd_per_mtok: None,
                    cache_write_usd_per_mtok: None,
                    context_window: Some(131_072),
                },
            ),
        ]);

        // First-party inference wins over the provider namespace entry.
        let inferred = lookup_context_limit("openrouter", "glm-5.3-flash");
        assert_eq!(inferred, Some(1_000_000));

        // Direct zai profile resolves through its own namespace.
        let direct = lookup_context_limit("zai", "glm-5.3-flash");
        assert_eq!(direct, Some(1_000_000));

        // Long-context alias and pinned suffixes are stripped before lookup.
        let aliased = lookup_context_limit("zai", "glm-5.3-flash[1m]");
        assert_eq!(aliased, Some(1_000_000));

        // Unknown model with a mappable provider resolves to nothing.
        assert!(lookup_context_limit("zai", "glm-unknown").is_none());
        // Unmapped gateway + unknown family id: no inference, no scan.
        assert!(lookup_context_limit("openai-compatible", "totally-unknown").is_none());

        clear_memory_cache_for_tests();
        if let Some(prev) = prev_home {
            crate::env::set_var("JCODE_HOME", prev);
        } else {
            crate::env::remove_var("JCODE_HOME");
        }
    }

    #[test]
    fn infer_provider_matches_first_party_id_shapes() {
        assert_eq!(infer_provider_for_model("glm-5.3-flash"), Some("zai"));
        assert_eq!(infer_provider_for_model("GLM-5.2"), Some("zai"));
        assert_eq!(infer_provider_for_model("moonshotai/kimi-k2.5"), Some("moonshotai"));
        assert_eq!(infer_provider_for_model("deepseek-v4-pro"), Some("deepseek"));
        assert_eq!(infer_provider_for_model("MiniMax-M2.7"), Some("minimax"));
        assert_eq!(infer_provider_for_model("mimo-v2.5-pro"), Some("xiaomi"));
        assert_eq!(infer_provider_for_model("gpt-5.4"), None);
        assert_eq!(infer_provider_for_model("claude-opus-4-6"), None);
        // Company-prefixed third-party ids must not claim a first-party slot.
        assert_eq!(infer_provider_for_model("somevendor/glm-5.2"), Some("zai"));
    }
}
