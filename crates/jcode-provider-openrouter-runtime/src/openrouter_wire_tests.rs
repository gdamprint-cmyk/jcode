//! Wire-level request tests: per-host headers, reasoning effort, Grok Build.

use super::*;

/// Issue #1167: OpenCode Go/Zen require a stable per-conversation
/// `x-opencode-session` header; other OpenAI-compatible hosts must not get it.
#[test]
fn opencode_session_header_only_for_opencode_hosts() {
    assert!(is_opencode_api_base("https://opencode.ai/zen/go/v1"));
    assert!(is_opencode_api_base("https://opencode.ai/zen/v1"));
    assert!(is_opencode_api_base("https://api.opencode.ai/v1"));
    assert!(!is_opencode_api_base("https://openrouter.ai/api/v1"));
    assert!(!is_opencode_api_base("https://api.deepseek.com/v1"));
    assert!(!is_opencode_api_base("not a url"));

    let client = reqwest::Client::new();
    let req = apply_opencode_session_header(
        client.post("https://opencode.ai/zen/go/v1/chat/completions"),
        "https://opencode.ai/zen/go/v1",
        "conv-123",
    )
    .build()
    .unwrap();
    assert_eq!(
        req.headers()
            .get(OPENCODE_SESSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("conv-123")
    );

    let req = apply_opencode_session_header(
        client.post("https://openrouter.ai/api/v1/chat/completions"),
        "https://openrouter.ai/api/v1",
        "conv-123",
    )
    .build()
    .unwrap();
    assert!(req.headers().get(OPENCODE_SESSION_HEADER).is_none());
}

#[test]
fn opencode_session_ids_are_uuids_and_unique() {
    let a = new_conversation_id();
    let b = new_conversation_id();
    assert_ne!(a, b);
    assert!(uuid::Uuid::parse_str(&a).is_ok());
}

/// Wire-level check for issue #1167: a real `chat/completions` request whose
/// api_base host is `opencode.ai` carries `x-opencode-session`, and a
/// request to another host does not. The DNS override points the hostname at
/// a local listener, so the full stream path (including retries) is exercised.
fn spawn_header_capturing_server() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        loop {
            let (mut stream, _) = listener.accept().expect("accept");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let mut buf = vec![0u8; 65536];
            let n = stream.read(&mut buf).unwrap_or(0);
            // Chat preflight may fetch model/pricing metadata. Return the actual
            // Grok catalog shape rather than treating GET /models as an SSE chat.
            if String::from_utf8_lossy(&buf[..n]).starts_with("GET /v1/models ") {
                let body = r#"{"data":[{"model":"grok-4.6","context_window":500000}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                continue;
            }
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes());
            break;
        }
    });
    (addr, rx)
}

fn captured_request_for_host(host: &str, conversation_id: &str) -> String {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let (addr, rx) = spawn_header_capturing_server();
        let client = reqwest::Client::builder()
            .resolve(host, addr)
            .build()
            .expect("client");
        let api_base = format!("http://{host}:{}/zen/go/v1", addr.port());
        let (tx, mut events) = tokio::sync::mpsc::channel::<anyhow::Result<StreamEvent>>(64);
        super::openrouter_sse_stream::run_stream_with_retries(
            client,
            api_base,
            ProviderAuth::None {
                label: "test".to_string(),
            },
            false,
            conversation_id.to_string(),
            serde_json::json!({"model": "m", "messages": [], "stream": true}),
            false,
            tx,
            Arc::new(Mutex::new(None)),
            "m".to_string(),
            false,
        )
        .await;
        while events.recv().await.is_some() {}
        rx.recv_timeout(Duration::from_secs(5))
            .expect("server captured request")
    })
}

#[test]
fn opencode_session_header_is_sent_on_the_wire_only_to_opencode_hosts() {
    let raw = captured_request_for_host("opencode.ai", "conv-wire-1167").to_ascii_lowercase();
    assert!(
        raw.contains("x-opencode-session: conv-wire-1167"),
        "opencode.ai request lacked the header:\n{raw}"
    );

    let raw = captured_request_for_host("example.test", "conv-wire-1167").to_ascii_lowercase();
    assert!(
        !raw.contains("x-opencode-session"),
        "non-opencode host received the header:\n{raw}"
    );
}

#[test]
fn configured_swarm_root_effort_covers_all_wire_formats() {
    let unified = make_provider();
    let deepseek = OpenRouterProvider {
        profile_id: Some("deepseek".into()),
        ..make_custom_compatible_provider()
    };
    let openai = OpenRouterProvider {
        profile_id: Some("zai".into()),
        ..make_custom_compatible_provider()
    };
    {
        let mode = "swarm";
        for (provider, strict, field, max) in [
            (&unified, false, "reasoning", "xhigh"),
            (&deepseek, false, "reasoning_effort", "max"),
            (&openai, false, "reasoning_effort", "max"),
            (&openai, true, "reasoning_effort", "xhigh"),
        ] {
            provider.set_reasoning_effort(mode).unwrap();
            for (resolved, expected) in [("low", "low"), ("medium", "medium"), ("max", max)] {
                let mut request = serde_json::json!({});
                assert!(provider.apply_resolved_reasoning_effort(&mut request, resolved, strict));
                let wire = if field == "reasoning" {
                    &request[field]["effort"]
                } else {
                    &request[field]
                };
                assert_eq!(wire, expected);
                assert_eq!(provider.reasoning_effort().as_deref(), Some(mode));
            }
            let mut request = serde_json::json!({});
            assert_eq!(
                provider.apply_resolved_reasoning_effort(&mut request, "none", strict),
                field == "reasoning"
            );
            if field == "reasoning" {
                assert_eq!(request[field]["effort"], "none");
            } else {
                assert!(request.get(field).is_none());
            }
        }
    }
    for (effort, expected) in [("minimal", "low"), ("xhigh", "high")] {
        let mut request = serde_json::json!({});
        assert!(deepseek.apply_resolved_reasoning_effort(&mut request, effort, false));
        assert_eq!(request["reasoning_effort"], expected);
    }
}

/// The MiniMax profile must put a DOCUMENTED effort level on the wire.
///
/// MiniMax accepts `low | medium | high | xhigh | max` and answers HTTP 400
/// for anything else (`none` is "requires adaptive thinking"), so the runtime
/// clamps before sending instead of forwarding a value the API refuses.
#[test]
fn minimax_reasoning_effort_reaches_the_wire_as_a_documented_level() {
    let minimax = OpenRouterProvider {
        profile_id: Some("minimax".into()),
        ..make_custom_compatible_provider()
    };
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();

    for effort in ["low", "medium", "high", "xhigh", "max"] {
        let mut request = serde_json::json!({});
        assert!(
            minimax.apply_resolved_reasoning_effort(&mut request, effort, false),
            "MiniMax must accept the documented level '{effort}'"
        );
        assert_eq!(
            request["reasoning_effort"], effort,
            "MiniMax wire effort must be the documented level itself"
        );
    }

    // Values the API rejects must never reach the wire. `none` is a real
    // OpenAI/DeepSeek rung, `minimal` is an OpenAI rung, and `swarm` is
    // Jcode's UI sentinel.
    for rejected in ["none", "minimal", "swarm"] {
        let mut request = serde_json::json!({});
        assert!(
            minimax.apply_resolved_reasoning_effort(&mut request, rejected, false),
            "MiniMax must still report that an effort was applied"
        );
        let wire = request["reasoning_effort"].as_str().unwrap_or_default();
        assert!(
            matches!(wire, "low" | "medium" | "high" | "xhigh" | "max"),
            "MiniMax wire effort '{wire}' (from '{rejected}') is not a documented level"
        );
        assert_ne!(wire, "none");
        assert_ne!(wire, "minimal");
        assert_ne!(wire, "swarm");
    }
}

/// The MiniMax selectable ladder is exactly the documented vocabulary plus
/// Jcode's `swarm` sentinel, and `/effort` must not reject the sentinel.
#[test]
fn minimax_ladder_offers_documented_levels_and_keeps_the_swarm_sentinel() {
    let minimax = OpenRouterProvider {
        profile_id: Some("minimax".into()),
        ..make_custom_compatible_provider()
    };
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();

    assert_eq!(
        minimax.available_efforts(),
        jcode_provider_core::MINIMAX_SELECTABLE_EFFORTS.to_vec()
    );
    assert!(minimax.available_efforts().contains(&"max"));
    assert!(minimax.available_efforts().contains(&"swarm"));
    assert!(!minimax.available_efforts().contains(&"none"));
    assert!(!minimax.available_efforts().contains(&"minimal"));

    // `none` is not selectable for MiniMax.
    assert!(minimax.set_reasoning_effort("none").is_err());
    assert!(minimax.set_reasoning_effort("minimal").is_err());

    // The swarm sentinel stays selectable and is preserved in session state.
    minimax.set_reasoning_effort("swarm").unwrap();
    assert_eq!(minimax.reasoning_effort().as_deref(), Some("swarm"));
}

/// `/effort` must not bail out for the MiniMax profile: the generic
/// `supports_any_reasoning_effort` gate used to reject it before the ladder
/// was ever consulted.
#[test]
fn minimax_profile_reports_reasoning_effort_support() {
    let minimax = OpenRouterProvider {
        profile_id: Some("minimax".into()),
        ..make_custom_compatible_provider()
    };
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();
    assert!(minimax.supports_any_reasoning_effort());
    minimax.set_reasoning_effort("high").unwrap();
    assert_eq!(minimax.reasoning_effort().as_deref(), Some("high"));
}

#[test]
fn azure_tool_turn_uses_responses_with_reasoning_and_replays_tool_history() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (requests_tx, requests_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..n]);
                if let Some(headers_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers =
                        String::from_utf8_lossy(&bytes[..headers_end]).to_ascii_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= headers_end + 4 + length {
                        break;
                    }
                }
            }
            requests_tx.send(String::from_utf8(bytes).unwrap()).unwrap();
            let body = if turn == 0 {
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"rs_1\",\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"encrypted-reasoning\"}}\n\ndata: {\"type\":\"response.output_item.added\",\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"bash\",\"arguments\":\"\"}}\n\ndata: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"fc_1\",\"arguments\":\"{\\\"cmd\\\":\\\"pwd\\\"}\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            } else {
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"done\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    let provider = OpenRouterProvider {
        api_base: format!("http://{addr}/openai/v1"),
        profile_id: Some("azure-openai".into()),
        builtin_azure: true,
        auth: Arc::new(|| {
            Ok(ProviderAuth::HeaderValue {
                header_name: reqwest::header::HeaderName::from_static("api-key"),
                value: "test-key".into(),
                label: "test".into(),
            })
        }),
        ..make_custom_compatible_provider()
    };
    provider.set_model("southindia-production").unwrap();
    provider.set_reasoning_effort("high").unwrap();
    let tools = vec![ToolDefinition {
        name: "bash".into(),
        description: "run".into(),
        input_schema: serde_json::json!({"type":"object","properties":{"cmd":{"type":"string"}}}),
        defer_loading: false,
    }];
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut first = provider
            .complete(&[Message::user("run pwd")], &tools, "sys", None)
            .await
            .unwrap();
        let mut called = false;
        let mut reasoning = None;
        while let Some(event) = first.next().await {
            match event.unwrap() {
                StreamEvent::ToolUseStart { .. } => called = true,
                StreamEvent::OpenAIReasoning {
                    id,
                    summary,
                    encrypted_content,
                    status,
                } => {
                    reasoning = Some(ContentBlock::OpenAIReasoning {
                        id,
                        summary,
                        encrypted_content,
                        status,
                    });
                }
                _ => {}
            }
        }
        assert!(called, "Responses function call must reach the agent");
        let mut assistant = Message::assistant_text("");
        assistant.content = vec![
            reasoning.expect("encrypted reasoning item must reach the agent"),
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd":"pwd"}),
                thought_signature: None,
            },
        ];
        let history = [
            Message::user("run pwd"),
            assistant,
            Message::tool_result("call_1", "/repo", false),
        ];
        let mut second = provider
            .complete(&history, &tools, "sys", None)
            .await
            .unwrap();
        let mut text = false;
        while let Some(event) = second.next().await {
            if matches!(event.unwrap(), StreamEvent::TextDelta(_)) {
                text = true;
            }
        }
        assert!(text, "Responses text must reach the agent");
    });
    let requests = [
        requests_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        requests_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
    ];
    for raw in &requests {
        assert!(raw.starts_with("POST /openai/v1/responses "), "{raw}");
        assert!(
            raw.to_ascii_lowercase().contains("api-key: test-key"),
            "{raw}"
        );
        let body: serde_json::Value =
            serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["model"], "southindia-production");
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["store"], false);
    }
    let followup: serde_json::Value =
        serde_json::from_str(requests[1].split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert!(
        followup["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == "call_1")
    );
    assert!(
        followup["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output" && item["call_id"] == "call_1")
    );
    assert!(
        followup["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning"
                && item["encrypted_content"] == "encrypted-reasoning")
    );
}

#[test]
fn azure_runtime_constructor_preserves_azure_route_identity() {
    let _lock = ENV_LOCK.lock();
    let _namespace = EnvVarGuard::set("JCODE_OPENROUTER_CACHE_NAMESPACE", "azure-openai");
    let _base = EnvVarGuard::set(
        "JCODE_OPENROUTER_API_BASE",
        "https://example.services.ai.azure.com/openai/v1",
    );
    let _features = EnvVarGuard::set("JCODE_OPENROUTER_PROVIDER_FEATURES", "0");
    let _catalog = EnvVarGuard::set("JCODE_OPENROUTER_MODEL_CATALOG", "0");
    let _auth = EnvVarGuard::set("JCODE_OPENROUTER_AUTH_HEADER", "api-key");
    let _key_name = EnvVarGuard::set(
        "JCODE_OPENROUTER_API_KEY_NAME",
        jcode_base::auth::azure::API_KEY_ENV,
    );
    let _env_file = EnvVarGuard::set(
        "JCODE_OPENROUTER_ENV_FILE",
        jcode_base::auth::azure::ENV_FILE,
    );
    let _key = EnvVarGuard::set(jcode_base::auth::azure::API_KEY_ENV, "test-key");
    let _model = EnvVarGuard::set("JCODE_OPENROUTER_MODEL", "southindia-production");
    let _azure_effort = EnvVarGuard::set("AZURE_OPENAI_REASONING_EFFORT", "high");
    let provider = OpenRouterProvider::new().unwrap();
    assert_eq!(provider.profile_id.as_deref(), Some("azure-openai"));
    assert!(!provider.is_user_named_profile());
    let (label, api_method, _) = provider.direct_openai_compatible_route_parts().unwrap();
    assert_eq!(provider.runtime_display_name(), "Azure OpenAI");
    assert_eq!(label, "Azure OpenAI");
    assert_eq!(api_method, "openrouter");
    assert!(provider.supports_openai_reasoning_effort());
    assert_eq!(provider.reasoning_effort().as_deref(), Some("high"));
    assert_eq!(provider.model(), "southindia-production");
    provider.set_model("another-deployment").unwrap();
    assert_eq!(provider.reasoning_effort().as_deref(), Some("high"));
    let fork = provider.fork();
    assert_eq!(fork.runtime_display_name(), "Azure OpenAI");
    assert!(
        fork.model_routes()
            .iter()
            .any(|route| route.api_method == "openrouter")
    );
}

#[test]
fn named_azure_openai_profile_keeps_chat_protocol_and_named_route() {
    let _lock = ENV_LOCK.lock();
    let _namespace = EnvVarGuard::remove("JCODE_OPENROUTER_CACHE_NAMESPACE");
    let (api_base, captured) = spawn_single_response_chat_server();
    let profile = jcode_base::config::NamedProviderConfig {
        base_url: api_base,
        api_key: Some("test-key".into()),
        default_model: Some("custom-chat-deployment".into()),
        ..Default::default()
    };
    let provider =
        OpenRouterProvider::new_named_openai_compatible("azure-openai", &profile).unwrap();
    assert!(provider.is_user_named_profile());
    assert_eq!(provider.runtime_display_name(), "azure-openai");
    let (label, method, _) = provider.direct_openai_compatible_route_parts().unwrap();
    assert_eq!(label, "azure-openai");
    assert_eq!(method, "openai-compatible:azure-openai");
    assert!(!provider.supports_openai_reasoning_effort());
    let fork = provider.fork();
    assert_eq!(fork.runtime_display_name(), "azure-openai");
    assert!(
        fork.model_routes()
            .iter()
            .any(|route| route.api_method == "openai-compatible:azure-openai")
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut stream = fork
            .complete(&[Message::user("hello")], &[], "test", None)
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    });
    let raw = captured.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(raw.starts_with("POST /v1/chat/completions "), "{raw}");
    let body: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["model"], "custom-chat-deployment");
    assert!(body.get("messages").is_some());
    assert!(body.get("input").is_none());
}

#[test]
fn builtin_azure_does_not_send_global_reasoning_default_to_unknown_deployment() {
    let _lock = ENV_LOCK.lock();
    let home = tempfile::tempdir().unwrap();
    let _home = EnvVarGuard::set("JCODE_HOME", home.path().to_str().unwrap());
    let (api_base, captured) = spawn_single_response_chat_server();
    let _namespace = EnvVarGuard::set("JCODE_OPENROUTER_CACHE_NAMESPACE", "azure-openai");
    let _base = EnvVarGuard::set("JCODE_OPENROUTER_API_BASE", api_base);
    let _features = EnvVarGuard::set("JCODE_OPENROUTER_PROVIDER_FEATURES", "0");
    let _catalog = EnvVarGuard::set("JCODE_OPENROUTER_MODEL_CATALOG", "0");
    let _key_name = EnvVarGuard::set(
        "JCODE_OPENROUTER_API_KEY_NAME",
        jcode_base::auth::azure::API_KEY_ENV,
    );
    let _env_file = EnvVarGuard::set(
        "JCODE_OPENROUTER_ENV_FILE",
        jcode_base::auth::azure::ENV_FILE,
    );
    let _key = EnvVarGuard::set(jcode_base::auth::azure::API_KEY_ENV, "test-key");
    let _model = EnvVarGuard::set("JCODE_OPENROUTER_MODEL", "nonreasoning-deployment");
    let _azure_effort = EnvVarGuard::remove("AZURE_OPENAI_REASONING_EFFORT");
    let provider = OpenRouterProvider::new().unwrap();
    provider.set_model("nonreasoning-deployment").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut stream = provider
            .complete(&[Message::user("hello")], &[], "test", None)
            .await
            .unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    });
    let raw = captured.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(raw.starts_with("POST /v1/responses "), "{raw}");
    let body: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body["model"], "nonreasoning-deployment");
    assert!(
        body.get("reasoning").is_none(),
        "unexpected reasoning default: {body}"
    );
}

#[test]
fn azure_responses_http_error_names_the_actual_route() {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);
        let body = r#"{"error":{"message":"deployment missing"}}"#;
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
    });
    let provider = OpenRouterProvider {
        api_base: format!("http://{addr}/openai/v1"),
        profile_id: Some("azure-openai".into()),
        builtin_azure: true,
        ..make_custom_compatible_provider()
    };
    provider.set_model("custom-deployment").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = rt.block_on(async {
        let mut stream = provider
            .complete(&[Message::user("test")], &[], "", None)
            .await
            .unwrap();
        loop {
            match stream.next().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => continue,
                None => panic!("HTTP 400 must reach the caller"),
            }
        }
    });
    let error = error.to_string();
    assert!(
        error.contains("Azure OpenAI Responses request failed"),
        "{error}"
    );
    assert!(error.contains("/openai/v1/responses"), "{error}");
    assert!(error.contains("custom-deployment"), "{error}");
}

#[test]
fn configured_swarm_root_effort_reads_real_config() {
    // Run this single test in a child process so changing config cannot race
    // other provider tests or reuse an already-initialized global config cache.
    if std::env::var_os("JCODE_TEST_SWARM_ROOT_CHILD").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                std::thread::current().name().unwrap(),
                "--nocapture",
            ])
            .env("JCODE_TEST_SWARM_ROOT_CHILD", "1")
            .env("JCODE_SWARM_ROOT_EFFORT", "low")
            .output()
            .expect("run isolated config test");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    {
        let (mode, expected) = ("swarm", "low");
        let (api_base, request_rx) = spawn_single_response_chat_server();
        let provider = OpenRouterProvider {
            api_base,
            supports_model_catalog: false,
            ..make_provider()
        };
        provider.set_reasoning_effort(mode).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut stream = provider.complete(&[], &[], "test", None).await.unwrap();
            while let Some(event) = stream.next().await {
                event.unwrap();
            }
        });
        let request = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            request.contains(&format!(r#""reasoning":{{"effort":"{expected}"}}"#)),
            "{request}"
        );
        assert_eq!(provider.reasoning_effort().as_deref(), Some(mode));
    }
}

/// `/effort swarm` must keep working on the MiniMax profile: the sentinel is
/// resolved to the configured swarm root effort and that RESOLVED level is
/// what reaches the wire, as a documented MiniMax level.
#[test]
fn minimax_swarm_sentinel_resolves_to_the_root_effort_on_the_wire() {
    let _lock = ENV_LOCK.lock();
    let (api_base, request_rx) = spawn_single_response_chat_server();
    let minimax = OpenRouterProvider {
        api_base,
        profile_id: Some("minimax".into()),
        supports_model_catalog: false,
        ..make_custom_compatible_provider()
    };
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();

    minimax.set_reasoning_effort("swarm").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut stream = minimax.complete(&[], &[], "test", None).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    });

    let request = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // The sentinel itself must never reach the wire.
    assert!(
        !request.contains(r#""reasoning_effort":"swarm""#),
        "the swarm sentinel must not be sent to MiniMax: {request}"
    );
    // Whatever level arrived must be one MiniMax documents.
    let wire = request
        .split(r#""reasoning_effort":""#)
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .unwrap_or_default();
    assert!(
        matches!(wire, "low" | "medium" | "high" | "xhigh" | "max"),
        "MiniMax wire effort '{wire}' is not a documented level: {request}"
    );
    // The stored mode stays the sentinel, so the UI keeps showing swarm.
    assert_eq!(minimax.reasoning_effort().as_deref(), Some("swarm"));
}

/// MiniMax documents `reasoning_effort` as effective for `MiniMax-M3.1-Flash`
/// only. A live probe on 2026-11-02 showed the older models ACCEPT the field
/// with HTTP 200 instead of rejecting it, so sending it is harmless - but on
/// those models it is silently ignored, so offering an effort ladder there
/// offers a dial that does nothing. The documented scope is M3.1.
#[test]
fn minimax_effort_ladder_is_scoped_to_the_documented_m31_models() {
    let _lock = ENV_LOCK.lock();
    let minimax = OpenRouterProvider {
        profile_id: Some("minimax".into()),
        ..make_custom_compatible_provider()
    };

    // M3.1-Flash: the documented target.
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();
    assert_eq!(
        minimax.available_efforts(),
        jcode_provider_core::MINIMAX_SELECTABLE_EFFORTS.to_vec()
    );
    let mut request = serde_json::json!({});
    assert!(minimax.apply_resolved_reasoning_effort(&mut request, "high", false));
    assert_eq!(request["reasoning_effort"], "high");

    // M3: accepts the field with HTTP 200 but does not honour it, so it is
    // outside the documented scope.
    minimax.set_model("MiniMax-M3").unwrap();
    assert_eq!(
        minimax.available_efforts(),
        vec!["swarm"],
        "MiniMax-M3 accepts reasoning_effort but does not honour it"
    );

    // M2.x: outside the scope, so no effort rungs and nothing on the wire.
    // The swarm sentinel survives, because it is a Jcode UI mode rather than
    // a wire-level effort: `/effort swarm` must keep working there.
    minimax.set_model("MiniMax-M2.7").unwrap();
    assert_eq!(
        minimax.available_efforts(),
        vec!["swarm"],
        "MiniMax-M2.7 must not offer an effort ladder the API ignores"
    );
    minimax.set_reasoning_effort("swarm").unwrap();
    assert_eq!(minimax.reasoning_effort().as_deref(), Some("swarm"));
    // An effort rung that the model ignores must be rejected outright, so the
    // UI cannot offer a dial that does nothing.
    assert!(minimax.set_reasoning_effort("high").is_err());
    let mut request = serde_json::json!({});
    assert!(!minimax.apply_resolved_reasoning_effort(&mut request, "high", false));
    assert!(
        request.get("reasoning_effort").is_none(),
        "an out-of-scope MiniMax model must not receive reasoning_effort"
    );
}

/// The MiniMax profile owns its vocabulary even when a profile-level
/// `supports_reasoning_effort = true` override is set. That override makes the
/// generic DeepSeek-style check true, and the DeepSeek ladder contains
/// `none`, which the MiniMax API answers with HTTP 400.
#[test]
fn minimax_profile_keeps_its_vocabulary_under_a_reasoning_effort_override() {
    let _lock = ENV_LOCK.lock();
    let minimax = OpenRouterProvider {
        profile_id: Some("minimax".into()),
        reasoning_effort_support: Some(true),
        ..make_custom_compatible_provider()
    };
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();

    // The MiniMax ladder, not the DeepSeek one: no `none`.
    assert_eq!(
        minimax.available_efforts(),
        jcode_provider_core::MINIMAX_SELECTABLE_EFFORTS.to_vec()
    );
    assert!(!minimax.available_efforts().contains(&"none"));

    // `none` is not selectable on MiniMax.
    assert!(minimax.set_reasoning_effort("none").is_err());

    // Even if `none` reached the wire path, it must be clamped to `low` and
    // never sent verbatim: MiniMax answers a literal `none` with HTTP 400.
    let mut request = serde_json::json!({});
    assert!(minimax.apply_resolved_reasoning_effort(&mut request, "none", false));
    assert_eq!(request["reasoning_effort"], "low");

    // A documented level still reaches the wire unchanged.
    let mut request = serde_json::json!({});
    assert!(minimax.apply_resolved_reasoning_effort(&mut request, "high", false));
    assert_eq!(request["reasoning_effort"], "high");

    // The other direction: a profile that explicitly disables reasoning effort
    // gets no ladder and nothing on the wire, like every other profile.
    let disabled = OpenRouterProvider {
        profile_id: Some("minimax".into()),
        reasoning_effort_support: Some(false),
        ..make_custom_compatible_provider()
    };
    disabled.set_model("MiniMax-M3.1-Flash").unwrap();
    assert!(disabled.available_efforts().is_empty());
    let mut request = serde_json::json!({});
    assert!(!disabled.apply_resolved_reasoning_effort(&mut request, "high", false));
    assert!(request.get("reasoning_effort").is_none(), "{request}");
}

/// Grok Build: a real chat/completions request built by the Grok Build
/// subscription runtime carries the OIDC bearer from `$GROK_HOME/auth.json`
/// and the Grok CLI identity headers the chat proxy requires, plus Jcode's
/// tools in OpenAI format (Jcode owns tool execution).
#[test]
fn grok_build_subscription_request_spoofs_grok_cli_and_uses_oidc_bearer() {
    let _lock = ENV_LOCK.lock();
    let grok_home = TempDir::new().expect("grok home");
    std::fs::write(
        grok_home.path().join("auth.json"),
        format!(
            r#"{{"https://auth.x.ai::{}": {{"key":"oidc-access","auth_mode":"oidc","expires_at":"2999-01-01T00:00:00Z"}}}}"#,
            jcode_base::auth::grok_build::OAUTH_CLIENT_ID
        ),
    )
    .expect("auth.json");
    let _home = EnvVarGuard::set("GROK_HOME", grok_home.path());
    let _deploy = EnvVarGuard::remove("GROK_DEPLOYMENT_KEY");
    let _version = EnvVarGuard::set("JCODE_GROK_CLI_VERSION", "9.8.7");
    let (addr, rx) = spawn_header_capturing_server();
    let _base = EnvVarGuard::set(
        "GROK_CLI_CHAT_PROXY_BASE_URL",
        format!("http://127.0.0.1:{}/v1", addr.port()),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let raw = rt.block_on(async {
        let provider = OpenRouterProvider::new_grok_build_subscription("grok-4.6");
        assert_eq!(provider.context_window(), 500_000);
        let tools = vec![ToolDefinition {
            name: "bash".to_string(),
            description: "run".to_string(),
            input_schema: serde_json::json!({"type":"object","properties":{"cmd":{"type":"string"}}}),
            defer_loading: false,
        }];
        let mut stream = provider
            .complete(&[Message::user("hello")], &tools, "sys", None)
            .await
            .expect("stream");
        while stream.next().await.is_some() {}
        rx.recv_timeout(Duration::from_secs(5))
            .expect("server captured request")
    });
    let lower = raw.to_ascii_lowercase();
    assert!(lower.starts_with("post /v1/chat/completions "), "{raw}");
    for expected in [
        "authorization: bearer oidc-access",
        "user-agent: grok-cli/9.8.7",
        "x-xai-token-auth: xai-grok-cli",
        "x-grok-client-version: 9.8.7",
        "x-grok-client-identifier: grok-shell",
        "x-grok-client-surface: cli",
        "x-grok-model-override: grok-4.6",
        "x-grok-conv-id: ",
        "x-grok-req-id: ",
    ] {
        assert!(lower.contains(expected), "missing `{expected}` in:\n{raw}");
    }
    assert!(!lower.contains("user-agent: jcode"), "{raw}");
    assert!(!lower.contains("http-referer"), "{raw}");
    let body: serde_json::Value =
        serde_json::from_str(raw.split("\r\n\r\n").nth(1).expect("body")).expect("json body");
    assert_eq!(body["model"], "grok-4.6");
    assert_eq!(body["stream"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "bash");
    assert_eq!(body["messages"][0]["role"], "system");
    assert!(body.get("reasoning_effort").is_none());
}

/// MiniMax only separates thinking into `reasoning_content` when
/// `reasoning_split` is true.
///
/// Without the field, MiniMax-M3 and the M2.x family return their thinking
/// INSIDE `content` wrapped in `<think>` tags. Jcode's stream parser reads only
/// `delta.reasoning_content`, so the reasoning text was rendered as ordinary
/// assistant text. MiniMax-M3.1-Flash goes further and REQUIRES
/// `reasoning_split: true` - `reasoning_split: false` is answered with HTTP 400
/// "requires reasoning_split=true".
///
/// Measured live on 2026-11-03 against api.minimax.io:
///   M3.1-Flash split=omitted -> reasoning_content=286, no think tags
///   M3         split=omitted -> reasoning_content=None, 1 think tag
///   M3         split=true    -> reasoning_content=233, no think tags
///   M2.7       split=true    -> reasoning_content=189, no think tags
#[test]
fn minimax_requests_split_reasoning_into_the_reasoning_content_field() {
    let _lock = ENV_LOCK.lock();
    let (api_base, request_rx) = spawn_single_response_chat_server();
    let minimax = OpenRouterProvider {
        api_base,
        profile_id: Some("minimax".into()),
        supports_model_catalog: false,
        ..make_custom_compatible_provider()
    };
    minimax.set_model("MiniMax-M3.1-Flash").unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut stream = minimax.complete(&[], &[], "test", None).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    });

    let request = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        request.contains(r#""reasoning_split":true"#),
        "the MiniMax profile must ask for split reasoning: {request}"
    );
}

/// The `reasoning_split` field belongs to MiniMax only. Another OpenAI-
/// compatible profile must not receive it, because strict OpenAI-schema
/// endpoints reject unknown top-level fields.
#[test]
fn reasoning_split_is_not_sent_to_other_openai_compatible_profiles() {
    let _lock = ENV_LOCK.lock();
    let (api_base, request_rx) = spawn_single_response_chat_server();
    let other = OpenRouterProvider {
        api_base,
        profile_id: Some("some-other-profile".into()),
        supports_model_catalog: false,
        ..make_custom_compatible_provider()
    };
    other.set_model("some-model").unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut stream = other.complete(&[], &[], "test", None).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    });

    let request = request_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        !request.contains("reasoning_split"),
        "only the MiniMax profile may send reasoning_split: {request}"
    );
}
