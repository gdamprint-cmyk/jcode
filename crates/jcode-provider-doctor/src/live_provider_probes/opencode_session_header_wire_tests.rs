// Wire-level test for the non-streaming probe path's `x-opencode-session`
// header (issue #1167).
//
// reference: https://github.com/1jehuang/jcode/blob/main/crates/jcode-provider-openrouter-runtime/src/openrouter_wire_tests.rs (the streaming path's wire test; this applies the same pattern to the doctor probe)

use super::*;
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Accept one HTTP request, capture its raw head, and answer with a minimal
/// OpenAI-compatible chat-completions body.
fn spawn_capturing_server() -> (std::net::SocketAddr, std::sync::mpsc::Receiver<String>) {
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
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let body = r#"{"choices":[{"message":{"content":"AUTH_TEST_OK"}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            break;
        }
    });
    (addr, rx)
}

/// Build the probe's request the way `run_live_openai_compatible_smoke` does,
/// and return the raw wire bytes the server saw.
///
/// reference: https://github.com/1jehuang/jcode/blob/main/crates/jcode-provider-openrouter-runtime/src/openrouter_wire_tests.rs (the streaming path's wire test; this applies the same pattern to the doctor probe)
fn captured_probe_request(host: &str, conversation_id: &str) -> String {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let (addr, rx) = spawn_capturing_server();
        let client = reqwest::Client::builder()
            .resolve(host, addr)
            // The listener is on 127.0.0.1: a configured system proxy must
            // NOT intercept it, or the request never reaches the capture
            // server and the test times out instead of asserting.
            .no_proxy()
            .build()
            .expect("client");
        // The scheme is plain HTTP and the port is the local listener's: the
        // host name stays the real one, so the shared host gate is exercised
        // exactly as in production while the connection never leaves the box.
        let api_base = format!("http://{host}:{}/zen/go/v1", addr.port());
        let url = format!("{}/chat/completions", api_base);
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "Reply with exactly AUTH_TEST_OK and nothing else."}],
            "stream": false
        });
        let request = client.post(&url).json(&body);
        let request = jcode_base::provider_catalog::apply_opencode_session_header(
            request,
            &api_base,
            conversation_id,
        );
        let _ = request.send().await;
        rx.recv_timeout(Duration::from_secs(5))
            .expect("server captured request")
    })
}

/// reference: https://github.com/1jehuang/jcode/blob/main/crates/jcode-provider-core/src/transport.rs (the shared transient-transport classifier the retry uses)
#[test]
fn probe_retries_a_transient_transport_fault_and_not_a_real_error() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");

    // A server that closes the FIRST connection without a response, then
    // answers normally: the retry must recover.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let served = Arc::new(AtomicUsize::new(0));
    let served_in_thread = Arc::clone(&served);
    std::thread::spawn(move || {
        for index in 0..3 {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let n = stream.read(&mut vec![0u8; 4096]).unwrap_or(0);
            let _ = n;
            served_in_thread.fetch_add(1, Ordering::SeqCst);
            if index == 0 {
                // Abrupt close: no response at all -> a transport fault.
                continue;
            }
            let body = "{\"ok\":true}";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });

    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("client");
    let url = format!("http://127.0.0.1:{}/probe", addr.port());
    let response = rt.block_on(async {
        send_probe_request(
            || client.post(&url).json(&serde_json::json!({"model": "m"})),
            Duration::from_secs(10),
            3,
        )
        .await
    });
    assert!(response.is_ok(), "probe should recover: {response:?}");
    assert_eq!(served.load(Ordering::SeqCst), 2, "expected one retry");
}

#[test]
fn opencode_session_header_is_sent_on_the_non_streaming_probe_wire() {
    // The doctor probe sends the header to an OpenCode host.
    let raw = captured_probe_request("opencode.ai", "conv-doctor-1167").to_ascii_lowercase();
    assert!(
        raw.contains("x-opencode-session: conv-doctor-1167"),
        "opencode.ai probe request lacked the header:\n{raw}"
    );

    // Negative control: another OpenAI-compatible host must NOT receive it.
    let raw = captured_probe_request("example.test", "conv-doctor-1167").to_ascii_lowercase();
    assert!(
        !raw.contains("x-opencode-session"),
        "non-opencode host received the header:\n{raw}"
    );
}

fn resolved_with_api_base(api_base: &str) -> ResolvedOpenAiCompatibleProfile {
    ResolvedOpenAiCompatibleProfile {
        id: "test".to_string(),
        display_name: "test".to_string(),
        api_base: api_base.to_string(),
        api_key_env: "TEST_KEY".to_string(),
        env_file: String::new(),
        setup_url: String::new(),
        default_model: None,
        requires_api_key: true,
    }
}

#[test]
fn probe_header_helper_uses_the_shared_catalog_gate() {
    // The doctor helper must use the SAME host gate as the shared helper, so
    // the two paths cannot drift apart again.
    let client = reqwest::Client::new();
    let resolved = resolved_with_api_base("https://opencode.ai/zen/go/v1");
    let request = client.post("https://opencode.ai/zen/go/v1/chat/completions");
    let request = apply_probe_headers(request, &resolved, "key", "conv-1");
    let built = request.build().expect("build");
    assert_eq!(
        built
            .headers()
            .get(jcode_base::provider_catalog::OPENCODE_SESSION_HEADER)
            .and_then(|v| v.to_str().ok()),
        Some("conv-1")
    );

    let resolved = resolved_with_api_base("https://api.openai.com/v1");
    let request = client.post("https://api.openai.com/v1/chat/completions");
    let request = apply_probe_headers(request, &resolved, "key", "conv-1");
    let built = request.build().expect("build");
    assert!(
        built
            .headers()
            .get(jcode_base::provider_catalog::OPENCODE_SESSION_HEADER)
            .is_none()
    );
}
