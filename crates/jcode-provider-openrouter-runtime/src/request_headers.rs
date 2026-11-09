//! Per-host request headers for OpenAI-compatible coding endpoints (Kimi
//! coding agents, OpenCode Go/Zen session ids).

pub(crate) const KIMI_CODING_USER_AGENT: &str = "claude-cli/1.0.0";
pub(crate) const KIMI_CODING_X_APP: &str = "cli";

pub(crate) fn is_kimi_coding_api_base(api_base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(api_base) else {
        return false;
    };
    matches!(url.host_str(), Some("api.kimi.com"))
        && url.path().trim_end_matches('/').starts_with("/coding")
}

pub(crate) fn is_coding_agent_api_base(api_base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(api_base) else {
        return false;
    };
    let host = url.host_str().unwrap_or_default();
    let path = url.path().trim_end_matches('/');
    is_kimi_coding_api_base(api_base)
        || host == "coding.dashscope.aliyuncs.com"
        || host == "coding-intl.dashscope.aliyuncs.com"
        || (host == "api.z.ai" && path.starts_with("/api/coding/paas"))
}

pub(crate) fn is_kimi_model_name(model: &str) -> bool {
    model.to_ascii_lowercase().contains("kimi")
}

pub(crate) fn should_send_kimi_coding_agent_headers(api_base: &str, model: Option<&str>) -> bool {
    is_coding_agent_api_base(api_base) || model.map(is_kimi_model_name).unwrap_or(false)
}

pub(crate) fn apply_kimi_coding_agent_headers(
    req: reqwest::RequestBuilder,
    api_base: &str,
    model: Option<&str>,
) -> reqwest::RequestBuilder {
    if should_send_kimi_coding_agent_headers(api_base, model) {
        req.header("User-Agent", KIMI_CODING_USER_AGENT)
            .header("x-app", KIMI_CODING_X_APP)
    } else {
        req
    }
}

/// Hosts that require the `x-opencode-session` header (issue #1167).
///
/// The helper now lives in `jcode-base::provider_catalog` so the streaming
/// runtime and the non-streaming provider-doctor probes share ONE
/// implementation; a second copy here could drift and leave the doctor path
/// without the header again.
pub(crate) use jcode_base::provider_catalog::apply_opencode_session_header;

#[cfg(test)]
pub(crate) use jcode_base::provider_catalog::{is_opencode_api_base, OPENCODE_SESSION_HEADER};

pub(crate) fn new_conversation_id() -> String {
    jcode_base::provider_catalog::new_opencode_conversation_id()
}
