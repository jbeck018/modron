//! X1 — ACP end-to-end: the real `modron acp` binary driven as a fake Zed
//! client over stdio JSON-RPC: initialize → session/new → session/prompt
//! (streamed updates) → end_turn; then a second prompt resuming the session.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

struct AcpClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl AcpClient {
    fn spawn(sessions_dir: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_modron"))
            .args([
                "acp",
                "--provider",
                "mock",
                "--approve-all",
                "--sessions-dir",
                sessions_dir.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn modron acp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> u64 {
        self.next_id += 1;
        let message =
            json!({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params});
        self.stdin
            .write_all(message.to_string().as_bytes())
            .unwrap();
        self.stdin.write_all(b"\n").unwrap();
        self.stdin.flush().unwrap();
        self.next_id
    }

    /// Read lines until the response with `id` arrives; returns
    /// (response, update notifications seen).
    fn read_until_response(&mut self, id: u64) -> (Value, Vec<Value>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut updates = Vec::new();
        loop {
            assert!(
                deadline > std::time::Instant::now(),
                "timed out waiting for response {id}"
            );
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read");
            assert!(n > 0, "server closed stdout");
            let value: Value = serde_json::from_str(line.trim()).expect("valid JSON line");
            if value.get("id").and_then(|v| v.as_u64()) == Some(id) && value.get("method").is_none()
            {
                return (value, updates);
            }
            if value.get("method").and_then(|v| v.as_str()) == Some("session/update") {
                updates.push(value);
            }
        }
    }
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn acp_initialize_session_prompt_and_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = AcpClient::spawn(&dir.path().join("sessions"));

    // initialize
    let id = client.request("initialize", json!({"protocolVersion": 1}));
    let (response, _) = client.read_until_response(id);
    assert_eq!(response["result"]["protocolVersion"], 1);

    // session/new — the editor's cwd is honored (tools run there).
    let project = tempfile::tempdir().unwrap();
    let id = client.request(
        "session/new",
        json!({"cwd": project.path().to_str().unwrap(), "mcpServers": []}),
    );
    let (response, _) = client.read_until_response(id);
    let acp_session = response["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    // session/prompt — streamed answer + end_turn result.
    let id = client.request(
        "session/prompt",
        json!({"sessionId": acp_session, "prompt": [{"type": "text", "text": "demo"}]}),
    );
    let (response, updates) = client.read_until_response(id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    let chunks: Vec<&Value> = updates
        .iter()
        .filter(|u| {
            u.pointer("/params/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("agent_message_chunk")
        })
        .collect();
    assert!(
        chunks.iter().any(|u| u
            .pointer("/params/update/content/text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .len()
            > 10),
        "streamed chunks carry the answer: {updates:?}"
    );
    let kind = |u: &&Value| {
        u.pointer("/params/update/sessionUpdate")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let starts: Vec<String> = updates
        .iter()
        .filter(|u| kind(u) == "tool_call")
        .filter_map(|u| {
            u.pointer("/params/update/toolCallId")?
                .as_str()
                .map(String::from)
        })
        .collect();
    let finishes: Vec<String> = updates
        .iter()
        .filter(|u| kind(u) == "tool_call_update")
        .filter_map(|u| {
            u.pointer("/params/update/toolCallId")?
                .as_str()
                .map(String::from)
        })
        .collect();
    assert!(!starts.is_empty(), "a tool call started: {updates:?}");
    assert!(
        starts.iter().zip(finishes.iter()).all(|(s, f)| s == f) && !finishes.is_empty(),
        "each completion reuses its start id: starts={starts:?} finishes={finishes:?}"
    );
    // The session cwd drove every tool: the mock's scripted write landed
    // in the editor's project dir (not the process launch dir).
    assert!(
        project.path().join("demo-output.txt").exists(),
        "demo-output.txt must land in the session cwd"
    );

    // Second prompt: same ACP session resumes the same Modron session.
    let id = client.request(
        "session/prompt",
        json!({"sessionId": acp_session, "prompt": [{"type": "text", "text": "again"}]}),
    );
    let (response, updates) = client.read_until_response(id);
    assert_eq!(response["result"]["stopReason"], "end_turn");
    assert!(
        updates.iter().any(|u| u
            .pointer("/params/update/content/text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .len()
            > 10),
        "resume streams a fresh answer: {updates:?}"
    );
}

#[test]
fn acp_unknown_method_is_json_rpc_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut client = AcpClient::spawn(&dir.path().join("sessions"));
    let id = client.request("agents/list", json!({}));
    let (response, _) = client.read_until_response(id);
    assert_eq!(response["error"]["code"], -32601);
}
