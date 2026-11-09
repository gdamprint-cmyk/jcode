use super::*;

pub(super) fn apply_swarm_model_policy(schema: &mut Value, agents: &crate::config::AgentsConfig) {
    let Some(properties) = schema["properties"].as_object_mut() else {
        crate::logging::error(
            "Cannot apply swarm model policy: schema properties are not an object",
        );
        return;
    };
    if !agents.swarm_model_selection || agents.swarm_model_allowlist.is_empty() {
        properties.remove("model");
    } else {
        // The inherit sentinel is always offered: it means "use the
        // user-configured default", which is a different thing from naming a
        // concrete route. Without it the schema would forbid the one value
        // that asks for the default.
        let mut routes = vec!["inherit".to_string()];
        routes.extend(agents.swarm_model_allowlist.iter().cloned());
        properties.insert("model".to_string(), json!({
            "type": "string",
            "enum": routes,
            "description": "Optional worker model. Only exact user-allowlisted routes are permitted, plus 'inherit' for the configured default. Omit to use the configured default."
        }));
    }
}
