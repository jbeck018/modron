//! Shared fixtures for the modron-server integration tests: a real server
//! (WS on an ephemeral port) driven by a delayed mock provider and a
//! scripted `bash` tool, plus a minimal WebSocket frame client.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use modron_core::{
    ApprovalMode, ApprovalPolicy, Engine, EngineConfig, ModelProvider, Result, StopReason, Tool,
    ToolCtx, ToolOutcome, ToolRegistry, ToolSpec, TurnRequest, TurnResponse,
};
use modron_protocol::{Frame, Payload, PROTOCOL_VERSION};
use modron_server::{BoundServer, ServeOptions};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// A stand-in for the mutating `bash` tool: named "bash" so the engine's
/// approval gate treats it as mutating, but it never spawns a process.
pub struct TestBashTool;

#[async_trait]
impl Tool for TestBashTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "test bash".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        Ok(ToolOutcome::ok("ran"))
    }
}

/// A stand-in for the read-only `read` tool: never approval-gated, so
/// tests that want Tool frames without touching the approval flow use it.
pub struct TestReadTool;

#[async_trait]
impl Tool for TestReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read".into(),
            description: "test read".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCtx) -> Result<ToolOutcome> {
        Ok(ToolOutcome::ok("contents"))
    }
}

/// `MockProvider` with a per-turn sleep so tests can attach mid-run.
pub struct DelayedProvider {
    pub inner: modron_core::testing::MockProvider,
    pub delay: Duration,
}

#[async_trait]
impl ModelProvider for DelayedProvider {
    fn id(&self) -> &str {
        "mock"
    }
    async fn turn(&self, req: TurnRequest) -> Result<TurnResponse> {
        tokio::time::sleep(self.delay).await;
        self.inner.turn(req).await
    }
}

/// A scripted tool-call response (the approval/interrupt trigger).
pub fn bash_call(id: &str) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![modron_core::ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "echo hi"}),
        }],
        usage: modron_core::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::ToolUse,
    }
}

/// A read-only tool-call response (bypasses the approval gate).
pub fn read_call(id: &str) -> TurnResponse {
    TurnResponse {
        text: String::new(),
        tool_calls: vec![modron_core::ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "x"}),
        }],
        usage: modron_core::Usage {
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::ToolUse,
    }
}

/// A plain end-of-turn text response.
pub fn text_turn(text: &str) -> TurnResponse {
    TurnResponse {
        text: text.into(),
        tool_calls: Vec::new(),
        usage: modron_core::Usage {
            input_tokens: 20,
            output_tokens: 8,
            cached_input_tokens: 0,
        },
        stop_reason: StopReason::EndTurn,
    }
}

/// A running test server + its scratch directories.
pub struct TestServer {
    pub addr: std::net::SocketAddr,
    pub server: BoundServer,
    pub dir: TempDir,
    pub socket_dir: TempDir,
    pub clients_dir: TempDir,
}

impl TestServer {
    pub fn clients_dir_path(&self) -> std::path::PathBuf {
        self.clients_dir.path().to_path_buf()
    }
}

/// Spawn a real server on 127.0.0.1:0 with the scripted provider.
pub async fn spawn_server(script: Vec<TurnResponse>, delay_ms: u64) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let socket_dir = tempfile::tempdir().unwrap();
    let clients_dir = tempfile::tempdir().unwrap();

    let store = Arc::new(modron_store::JsonlSessionStore::new(
        dir.path().join("sessions"),
    ));
    let config = EngineConfig {
        working_dir: dir.path().to_path_buf(),
        default_model: "mock-model".into(),
        default_provider: "mock".into(),
        max_turns: 8,
        approvals: ApprovalPolicy {
            mode: ApprovalMode::Ask,
            non_interactive: false,
            allow: vec![],
            deny: vec![],
        },
        self_check_on_finish: false,
        ..EngineConfig::default()
    };
    let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
    providers.insert(
        "mock".to_string(),
        Arc::new(DelayedProvider {
            inner: modron_core::testing::MockProvider::with_script(script),
            delay: Duration::from_millis(delay_ms),
        }),
    );
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(TestBashTool));
    registry.register(Arc::new(TestReadTool));
    let engine = Engine {
        selector: Arc::new(modron_core::testing::StaticSelector::new(
            "mock-model",
            "mock",
        )),
        providers,
        tools: Arc::new(registry),
        store: store as Arc<dyn modron_core::SessionStore>,
        config,
    };

    let server = modron_server::bind_server(
        engine,
        &ServeOptions {
            listen: Some("127.0.0.1:0".into()),
            socket: Some(socket_dir.path().join("serve.sock")),
            clients_dir: Some(clients_dir.path().to_path_buf()),
            bridge_base: None,
        },
    )
    .await
    .unwrap();
    let addr = server.ws_addr.unwrap();
    TestServer {
        addr,
        server,
        dir,
        socket_dir,
        clients_dir,
    }
}

/// Minimal WebSocket frame client for tests.
pub struct WsClient {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl WsClient {
    /// Connect and complete the hello/welcome handshake.
    pub async fn connect(addr: std::net::SocketAddr) -> Self {
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
            .await
            .expect("connect");
        let mut client = Self { ws, next_id: 1 };
        client
            .send(Payload::Hello {
                client: "test-client".into(),
                protocol: PROTOCOL_VERSION,
                token: None,
            })
            .await;
        let welcome = client.recv().await;
        assert!(
            matches!(welcome.payload, Payload::Welcome { .. }),
            "expected welcome, got {:?}",
            welcome.payload
        );
        client
    }

    pub async fn send(&mut self, payload: Payload) {
        let frame = Frame::request(self.next_id, payload);
        self.next_id += 1;
        let json = serde_json::to_string(&frame).unwrap();
        self.ws.send(Message::text(json)).await.unwrap();
    }

    /// Raw send for malformed-frame tests (no correlation id).
    pub async fn send_raw_text(&mut self, text: &str) {
        self.ws.send(Message::text(text.to_string())).await.unwrap();
    }

    /// Next server frame (5s timeout).
    pub async fn recv(&mut self) -> Frame {
        let msg = tokio::time::timeout(Duration::from_secs(15), self.ws.next())
            .await
            .expect("frame within 15s")
            .expect("websocket open")
            .expect("websocket read");
        match msg {
            Message::Text(text) => serde_json::from_str(&text).expect("frame json"),
            other => panic!("expected text frame, got {other:?}"),
        }
    }

    /// Scan frames until the payload matches (skipping everything else).
    pub async fn recv_until(&mut self, pred: impl Fn(&Payload) -> bool) -> Frame {
        for _ in 0..200 {
            let frame = self.recv().await;
            if pred(&frame.payload) {
                return frame;
            }
        }
        panic!("predicate never matched within 200 frames");
    }

    pub async fn close(&mut self) {
        let _ = self.ws.close(None).await;
    }
}
