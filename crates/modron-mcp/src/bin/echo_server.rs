//! `modron-mcp-echo` — a minimal MCP server for testing and as a copy-paste
//! example of the stdio transport: newline-delimited JSON-RPC, initialize →
//! tools/list → tools/call. Exposes one `echo` tool.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
            eprintln!("modron-mcp-echo: non-JSON line ignored");
            continue;
        };
        let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
            continue; // response to something we never sent — ignore
        };
        let response = match method {
            "initialize" => {
                let id = message.get("id").cloned().unwrap_or(json!(null));
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "modron-mcp-echo", "version": "0.1.0"}
                    }
                })
            }
            "tools/list" => {
                let id = message.get("id").cloned().unwrap_or(json!(null));
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"tools": [{
                        "name": "echo",
                        "description": "Echo the message back (test tool).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"message": {"type": "string"}},
                            "required": ["message"]
                        }
                    }]}
                })
            }
            "tools/call" => {
                let id = message.get("id").cloned().unwrap_or(json!(null));
                let name = message
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if name != "echo" {
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "error": {"code": -32602, "message": format!("unknown tool {name}")}
                    })
                } else {
                    let text = message
                        .pointer("/params/arguments/message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    json!({
                        "jsonrpc": "2.0", "id": id,
                        "result": {"content": [{"type": "text", "text": text}], "isError": false}
                    })
                }
            }
            _ => continue, // notifications (initialized) and unknown methods
        };
        let mut line = response.to_string();
        line.push('\n');
        let _ = stdout.write_all(line.as_bytes());
        let _ = stdout.flush();
    }
}
