//! The MCP row-name guarantee, checked through the REAL app draw path.
//!
//! `render_tool_message` tests prove the row builder is right; they do not prove
//! the app paints that row. This drives `ServerEvent::ToolStart`/`ToolDone`
//! through `App::handle_server_event` (the same path a live client takes), draws
//! the whole app into a cell buffer, and reads the painted text back.
//!
//! The negative control is part of the fixture: a non-MCP tool whose input also
//! carries `server`/`tool` must keep its own name on the same screen.

use super::*;

fn draw_screen(app: &mut crate::tui::app::App, width: u16, height: u16) -> String {
    let _render_lock = crate::tui::ui::render_state_test_lock();
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    // A first-run app opens the changelog overlay, which replaces the whole
    // frame. Close it so the transcript under test is what gets painted.
    app.changelog_scroll = None;
    terminal
        .draw(|frame| crate::tui::ui::draw(frame, app))
        .expect("draw failed");
    let buffer = terminal.backend().buffer().clone();
    let area = buffer.area;
    let mut rows: Vec<String> = Vec::with_capacity(area.height as usize);
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buffer[(area.x + x, area.y + y)].symbol());
        }
        rows.push(row.trim_end().to_string());
    }
    rows.join("\n")
}

fn mcp_tool_call_event(id: &str, name: &str) -> crate::protocol::ServerEvent {
    crate::protocol::ServerEvent::ToolStart {
        id: id.to_string(),
        name: name.to_string(),
    }
}

#[test]
fn mcp_tool_row_reaches_the_painted_screen_with_its_tool_name() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();

    app.handle_server_event(
        mcp_tool_call_event("live_mcp", "mcp"),
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolInput {
            id: Some("live_mcp".to_string()),
            delta: r#"{"action":"call","server":"cgrep","tool":"cgrep_agent_locate","arguments":{"query":"mcp label"},"intent":"find the label"}"#.to_string(),
        },
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolDone {
            id: "live_mcp".to_string(),
            name: "mcp".to_string(),
            output: "ok".to_string(),
            error: None,
        },
        &mut remote,
    );

    let message = app
        .display_messages()
        .iter()
        .rev()
        .find(|message| message.role == "tool")
        .expect("tool row exists");
    assert_eq!(
        message.tool_data.as_ref().expect("tool_data").name,
        "mcp",
        "the live path must carry the call metadata"
    );

    let screen = draw_screen(&mut app, 136, 44);
    let painted: Vec<String> = screen
        .split('\n')
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    let painted = painted.join("\n");
    assert!(
        screen.contains("cgrep / cgrep_agent_locate"),
        "painted screen must name the concrete tool; painted lines:\n{painted}"
    );
    assert!(!screen.contains("call cgrep"), "stale label leaked");
}

#[test]
fn non_mcp_row_on_the_same_screen_keeps_its_own_name() {
    let mut app = create_test_app();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let mut remote = crate::tui::backend::RemoteConnection::dummy();

    app.handle_server_event(
        mcp_tool_call_event("live_mcp2", "mcp"),
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolInput {
            id: Some("live_mcp2".to_string()),
            delta: r#"{"action":"call","server":"cgrep","tool":"cgrep_agent_locate","arguments":{"query":"mcp label"}}"#.to_string(),
        },
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolDone {
            id: "live_mcp2".to_string(),
            name: "mcp".to_string(),
            output: "ok".to_string(),
            error: None,
        },
        &mut remote,
    );

    // Same fields, different tool: `integration_tools` uses `tool` for a product.
    app.handle_server_event(
        mcp_tool_call_event("live_discover", "integration_tools"),
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolInput {
            id: Some("live_discover".to_string()),
            delta: r#"{"tool":"Stripe","query":"payments"}"#.to_string(),
        },
        &mut remote,
    );
    app.handle_server_event(
        crate::protocol::ServerEvent::ToolDone {
            id: "live_discover".to_string(),
            name: "integration_tools".to_string(),
            output: "ok".to_string(),
            error: None,
        },
        &mut remote,
    );

    let screen = draw_screen(&mut app, 136, 60);
    assert!(
        !screen.contains("integration_tools: Stripe"),
        "a non-MCP row must not gain an MCP suffix: {screen}"
    );
}
