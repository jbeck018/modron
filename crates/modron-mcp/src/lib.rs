//! modron-mcp — Model Context Protocol client (Phase 5a).
//!
//! Speaks the MCP stdio transport: newline-delimited JSON-RPC 2.0. Surface
//! implemented: `initialize` handshake, `tools/list`, `tools/call` — the
//! tool-serving subset Modron consumes. Resources/prompts/sampling land
//! later; migrating to the official `rmcp` SDK is the documented path if the
//! full spec surface is ever needed (kept out to keep the binary lean).
//!
//! Token discipline (PLAN §4.8): every server's tool-schema cost is computed
//! at connect time (`McpToolInfo::schema_tokens`) so opt-in servers pay their
//! way visibly — no silent prompt fattening.

use async_trait::async_trait;
use modron_core::{estimate_tokens, Tool, ToolCall, ToolCtx, ToolOutcome, ToolSpec};
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Metadata for one tool a server exposes.
#[derive(Clone, Debug)]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Estimated prompt cost of this tool's spec (name + description +
    /// schema), computed at connect time.
    pub schema_tokens: u64,
}

/// A live MCP server process speaking JSON-RPC over stdio.
pub struct McpServerHandle {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    /// How long to wait for one response.
    timeout: std::time::Duration,
}

impl McpServerHandle {
    /// Spawn `command`, run the initialize handshake, and list tools.
    pub async fn spawn(
        server_name: &str,
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> anyhow::Result<(Arc<Mutex<McpServerHandle>>, Vec<McpToolInfo>)> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!("MCP server `{server_name}` ({command}) failed to start: {e}")
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("MCP server `{server_name}` has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("MCP server `{server_name}` has no stdout"))?;
        let mut handle = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            timeout: CALL_TIMEOUT,
        };

        // Initialize handshake.
        let result = handle
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "modron", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await;
        if let Err(e) = result {
            let _ = handle.child.start_kill();
            return Err(anyhow::anyhow!(
                "MCP server `{server_name}` initialize failed: {e}"
            ));
        }
        handle.notify("notifications/initialized", json!({})).await;

        // List tools.
        let listing = handle.request("tools/list", json!({})).await.map_err(|e| {
            let _ = handle.child.start_kill();
            anyhow::anyhow!("MCP server `{server_name}` tools/list failed: {e}")
        })?;
        let mut tools = Vec::new();
        for tool in listing
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            let schema_tokens = estimate_tokens(&format!("{name}{description}{}", input_schema));
            tools.push(McpToolInfo {
                name,
                description,
                input_schema,
                schema_tokens,
            });
        }
        Ok((Arc::new(Mutex::new(handle)), tools))
    }

    /// One JSON-RPC request/response round trip (skips notifications).
    async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send(&message).await?;
        let deadline = tokio::time::Instant::now() + self.timeout;
        loop {
            let mut line = String::new();
            let read = tokio::time::timeout_at(deadline, self.stdout.read_line(&mut line))
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for `{method}` response"))?
                .map_err(|e| anyhow::anyhow!("read failed: {e}"))?;
            if read == 0 {
                return Err(anyhow::anyhow!("server closed its stdout"));
            }
            let value: Value = match serde_json::from_str(line.trim()) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(line = %line, error = %e, "non-JSON line from MCP server");
                    continue;
                }
            };
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(anyhow::anyhow!("server error: {error}"));
                }
                return value
                    .get("result")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("response missing result: {value}"));
            }
            // Notifications / other ids: logged, skipped.
            tracing::debug!(line = %line, "skipping non-matching MCP message");
        }
    }

    /// Fire-and-forget notification (no id, no response expected).
    async fn notify(&mut self, method: &str, params: Value) {
        let message = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Err(e) = self.send(&message).await {
            tracing::warn!(error = %e, "failed to send MCP notification");
        }
    }

    async fn send(&mut self, message: &Value) -> anyhow::Result<()> {
        let mut line = message.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// List resources the server exposes (MCP resources surface).
    pub async fn list_resources(&mut self) -> anyhow::Result<Vec<McpResourceInfo>> {
        let result = self.request("resources/list", json!({})).await?;
        let mut out = Vec::new();
        for resource in result
            .get("resources")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default()
        {
            out.push(McpResourceInfo {
                uri: resource
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: resource
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: resource
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(out)
    }

    /// Read one resource by URI (text contents only).
    pub async fn read_resource(&mut self, uri: &str) -> anyhow::Result<String> {
        let result = self.request("resources/read", json!({"uri": uri})).await?;
        let mut text = String::new();
        if let Some(contents) = result.get("contents").and_then(|c| c.as_array()) {
            for block in contents {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
        }
        Ok(text)
    }

    /// List prompt templates the server exposes.
    pub async fn list_prompts(&mut self) -> anyhow::Result<Vec<McpPromptInfo>> {
        let result = self.request("prompts/list", json!({})).await?;
        let mut out = Vec::new();
        for prompt in result
            .get("prompts")
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default()
        {
            out.push(McpPromptInfo {
                name: prompt
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: prompt
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            });
        }
        Ok(out)
    }

    /// Call a tool on this server.
    pub async fn call_tool(
        &mut self,
        tool: &str,
        arguments: Value,
    ) -> anyhow::Result<McpCallResult> {
        let result = tokio::time::timeout(
            self.timeout,
            self.request("tools/call", json!({"name": tool, "arguments": arguments})),
        )
        .await
        .map_err(|_| anyhow::anyhow!("tools/call timed out"))??;
        // MCP result shape: {content: [{type: "text", text}], isError?}.
        let mut text = String::new();
        let is_error = result
            .get("isError")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
            for block in content {
                if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(block.get("text").and_then(|v| v.as_str()).unwrap_or(""));
                }
            }
        }
        if is_error && text.is_empty() {
            text = "tool reported an error without content".to_string();
        }
        Ok(McpCallResult {
            content: text,
            is_error,
        })
    }
}

/// One resource a server exposes.
#[derive(Clone, Debug)]
pub struct McpResourceInfo {
    pub uri: String,
    pub name: String,
    pub description: String,
}

/// One prompt template a server exposes.
#[derive(Clone, Debug)]
pub struct McpPromptInfo {
    pub name: String,
    pub description: String,
}

/// Result of a tools/call.
pub struct McpCallResult {
    pub content: String,
    pub is_error: bool,
}

impl Drop for McpServerHandle {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

/// A `modron_core::Tool` backed by an MCP server tool. Registered as
/// `mcp__<server>__<tool>` (Claude Code's naming convention, so configs and
/// habits transfer).
pub struct McpTool {
    handle: Arc<Mutex<McpServerHandle>>,
    server: String,
    info: McpToolInfo,
}

impl McpTool {
    pub fn new(handle: Arc<Mutex<McpServerHandle>>, server: &str, info: McpToolInfo) -> Self {
        Self {
            handle,
            server: server.to_string(),
            info,
        }
    }

    pub fn qualified_name(&self) -> String {
        format!("mcp__{}__{}", self.server, self.info.name)
    }

    /// Connect-time schema cost for the startup accounting line.
    pub fn schema_tokens(&self) -> u64 {
        self.info.schema_tokens
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.qualified_name(),
            description: if self.info.description.is_empty() {
                format!("MCP tool {} (server {}).", self.info.name, self.server)
            } else {
                format!("MCP ({}): {}", self.server, self.info.description)
            },
            parameters: self.info.input_schema.clone(),
        }
    }

    async fn execute(&self, args: Value, _ctx: &ToolCtx) -> modron_core::Result<ToolOutcome> {
        // The model sees the plain tool name; strip our prefix back off.
        let bare = self.info.name.as_str();
        let mut handle = self.handle.lock().await;
        match handle.call_tool(bare, args).await {
            Ok(result) => Ok(ToolOutcome {
                content: result.content,
                is_error: result.is_error,
                usage: None,
            }),
            Err(e) => Err(modron_core::Error::Tool {
                tool: self.qualified_name(),
                message: e.to_string(),
            }),
        }
    }
}

// Silence the unused-variant lint path: ToolCall is re-exported by the
// prelude consumers; the type only appears in generated schemas.
#[allow(dead_code)]
fn _t(_: ToolCall) {}
