// reference: no prior art, searched "rust config struct default impl in separate module" - the shape is upstream's own lib_agents_defaults.rs
use super::*;

impl Default for AgentsConfig {
    fn default() -> Self {
        Self {
            swarm_model: None,
            swarm_model_selection: false,
            swarm_model_allowlist: Vec::new(),
            swarm_effort: None,
            swarm_root_effort: None,
            swarm_spawn_mode: SwarmSpawnMode::default(),
            memory_provider: default_memory_provider(),
            memory_hindsight_url: None,
            memory_hindsight_bank: None,
            memory_hindsight_limit: default_memory_hindsight_limit(),
            memory_refresh_tool_calls: default_memory_refresh_tool_calls(),
            browser_jev_model: None,
            voice_jev_model: None,
            memory_model: None,
            memory_rerank_cadence: default_memory_rerank_cadence(),
            memory_rerank_votes: default_memory_rerank_votes(),
            memory_rerank_min_agree: default_memory_rerank_min_agree(),
            memory_embedding_backend: default_memory_embedding_backend(),
            memory_embedding_model: None,
            memory_embedding_base_url: None,
            memory_embedding_dim: None,
            memory_write_gate: MemoryWriteGate::default(),
            swarm_max_concurrent_agents: default_swarm_max_concurrent_agents(),
            swarm_stall_after_secs: default_swarm_stall_after_secs(),
            swarm_stall_nudge: false,
        }
    }
}

impl AgentsConfig {
    // reference: https://github.com/1jehuang/jcode/blob/main/crates/jcode-app-core/src/server/spawn_policy.rs
    // the sentinel vocabulary was consolidated from
    // crates/jcode-app-core/src/server/spawn_policy.rs:38 so the validator and
    // the spawn resolver cannot disagree about it.
    /// Whether a requested swarm model is the "inherit the coordinator's model"
    /// sentinel rather than a concrete route.
    ///
    /// The sentinel names the coordinator's own model, provider key and route
    /// (`resolve_swarm_spawn_selection`), so it is not a route override and must
    /// not be checked against `swarm_model_allowlist`.
    ///
    /// NOTE: a requested sentinel is NOT the same as omitting `model`. Omitting
    /// `model` falls back to the configured `agents.swarm_model` pin when one is
    /// set; a requested sentinel takes priority over that pin and resolves to
    /// the coordinator's model.
    ///
    /// This lives here, in the crate both the config layer and the server
    /// depend on, so the validator and the spawn resolver cannot disagree
    /// about which strings are sentinels.
    // reference: crates/jcode-app-core/src/server/comm_session.rs
    // The resolver that gives a requested sentinel priority over a configured
    // pin lives there; the doc above describes that existing behaviour.
    pub fn is_swarm_inherit_sentinel(model: &str) -> bool {
        let trimmed = model.trim();
        trimmed.eq_ignore_ascii_case("inherit") || trimmed.eq_ignore_ascii_case("coordinator")
    }

    /// Validate an agent-supplied override before resolving any provider route.
    /// Matching is exact: a bare model never authorizes another provider's route.
    ///
    /// The error names the requested model and every allowed route, so a
    /// coordinator that guessed a route string learns the exact set it may use
    /// instead of hitting a dead end.
    pub fn validate_swarm_model_request(&self, model: Option<&str>) -> Result<(), String> {
        let Some(model) = model else {
            return Ok(());
        };
        // The inherit sentinel names the coordinator's model, so it needs
        // neither an allowlist entry nor the agent-selection opt-in. It is
        // checked FIRST because it is not a route override. Note that it
        // overrides a configured `agents.swarm_model` pin, unlike omitting
        // `model` - see `is_swarm_inherit_sentinel`.
        // reference: crates/jcode-app-core/src/server/comm_session.rs:368
        // `resolve_swarm_spawn_selection` is the resolver whose precedence
        // rules this gate mirrors.
        if Self::is_swarm_inherit_sentinel(model) {
            return Ok(());
        }
        if !self.swarm_model_selection {
            return Err(
                "Agent-selected swarm models are disabled. Omit model to use the user-configured default."
                    .to_string(),
            );
        }
        if model.trim().is_empty()
            || !self
                .swarm_model_allowlist
                .iter()
                .any(|allowed| allowed == model)
        {
            if self.swarm_model_allowlist.is_empty() {
                return Err(format!(
                    "Requested swarm model '{model}' is not permitted: agents.swarm_model_allowlist is empty. \
                     Omit model to use the user-configured default."
                ));
            }
            let allowed = format!(
                "Allowed swarm models: {}.",
                self.swarm_model_allowlist
                    .iter()
                    .map(|route| route.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return Err(format!(
                "Requested swarm model '{model}' is not in agents.swarm_model_allowlist. {allowed} \
                 Use one of those routes exactly, or omit model to use the user-configured default."
            ));
        }
        Ok(())
    }

    /// Resolve swarm mode's root effort without allowing the orchestration
    /// sentinel to recurse into itself. Unknown values use the low-effort
    /// default without invalidating other settings.
    pub fn root_effort_for_swarm(&self) -> &'static str {
        let value = self.swarm_root_effort.as_deref().unwrap_or("low").trim();
        ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .find(|level| level.eq_ignore_ascii_case(value))
            .unwrap_or("low")
    }
}
