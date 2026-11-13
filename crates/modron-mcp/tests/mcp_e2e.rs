//! MCP end-to-end: the real echo-server binary spawned over stdio.

use modron_core::Tool as _;
use modron_mcp::{McpServerHandle, McpTool};
use serde_json::json;

#[tokio::test]
async fn echo_server_round_trips() {
    let bin = env!("CARGO_BIN_EXE_modron-mcp-echo");
    let (handle, tools) = McpServerHandle::spawn("echo", bin, &[], &[]).await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert!(
        tools[0].schema_tokens > 0,
        "schema cost computed at connect"
    );

    let mut handle = handle.lock().await;
    let result = handle
        .call_tool("echo", json!({"message": "hello modron"}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, "hello modron");
}

#[tokio::test]
async fn mcp_tool_bridge_maps_names_and_errors() {
    let bin = env!("CARGO_BIN_EXE_modron-mcp-echo");
    let (handle, tools) = McpServerHandle::spawn("srv", bin, &[], &[]).await.unwrap();
    let tool = McpTool::new(handle.clone(), "srv", tools.into_iter().next().unwrap());
    assert_eq!(tool.qualified_name(), "mcp__srv__echo");
    // The spec the model sees uses the qualified name.
    assert_eq!(tool.spec().name, "mcp__srv__echo");

    let ok = tool
        .execute(json!({"message": "hi"}), &modron_core::ToolCtx::new("."))
        .await
        .unwrap();
    assert!(!ok.is_error);
    assert_eq!(ok.content, "hi");

    // Unknown tool at the protocol level → server JSON-RPC error.
    let mut handle = handle.lock().await;
    let err = handle
        .call_tool("nope", json!({}))
        .await
        .err()
        .expect("unknown tool must error");
    assert!(err.to_string().contains("unknown tool"), "{err}");
}

#[tokio::test]
async fn failing_command_is_an_actionable_error() {
    let err = McpServerHandle::spawn("broken", "/nonexistent/mcp-server-binary", &[], &[])
        .await
        .err()
        .expect("must fail");
    assert!(err.to_string().contains("failed to start"), "{err}");
}

#[tokio::test]
async fn resources_and_prompts_round_trip() {
    let bin = env!("CARGO_BIN_EXE_modron-mcp-echo");
    let (handle, _tools) = McpServerHandle::spawn("echo", bin, &[], &[]).await.unwrap();
    let mut handle = handle.lock().await;

    let resources = handle.list_resources().await.unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].uri, "memo://echo/notes");

    let body = handle.read_resource("memo://echo/notes").await.unwrap();
    assert_eq!(body, "notes body");

    let prompts = handle.list_prompts().await.unwrap();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].name, "greet");
}

/// A multibyte character straddling the reader's internal buffer boundary
/// must survive (cycle-2 L2 fix: per-chunk from_utf8_lossy turned it into
/// U+FFFDs). An 8 KiB run of ASCII followed by multibyte text forces the
/// boundary into the wide characters.
#[tokio::test]
async fn multibyte_output_survives_buffer_boundaries() {
    let bin = env!("CARGO_BIN_EXE_modron-mcp-echo");
    let (handle, _) = McpServerHandle::spawn("echo", bin, &[], &[]).await.unwrap();
    let mut handle = handle.lock().await;
    // The echo tool repeats the message: 8190 ASCII chars (lands the
    // boundary mid-run), then a wide char + trailing text.
    let payload = format!("{}\u{1F40D}end", "a".repeat(8190));
    let result = handle
        .call_tool("echo", json!({"message": payload}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content, payload, "no U+FFFD corruption");
    assert!(!result.content.contains('\u{FFFD}'));
}
