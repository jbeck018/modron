//! Streamable HTTP client tests against an in-process HTTP/1.1 responder.

use modron_mcp::McpServerHandle;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// One request as the fake server saw it. Header names are lowercased.
struct Req {
    headers: Vec<(String, String)>,
    body: Value,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn method(&self) -> &str {
        self.body
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
    }

    fn id(&self) -> Value {
        self.body.get("id").cloned().unwrap_or(Value::Null)
    }
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Resp {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn text(status: u16, content_type: &str, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn json(value: Value) -> Self {
        Self::text(200, "application/json", &value.to_string())
    }

    fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// Serve `handler` on 127.0.0.1:0; returns the MCP endpoint URL.
async fn serve(handler: impl Fn(&Req) -> Resp + Send + Sync + 'static) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handler = Arc::new(handler);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let handler = Arc::clone(&handler);
            tokio::spawn(async move {
                let _ = answer(socket, handler.as_ref()).await;
            });
        }
    });
    format!("http://{addr}/mcp")
}

async fn answer(
    socket: TcpStream,
    handler: &(dyn Fn(&Req) -> Resp + Send + Sync),
) -> std::io::Result<()> {
    let (read, mut write) = socket.into_split();
    let mut reader = BufReader::new(read);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.push((name, value));
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).await?;
    let req = Req {
        headers,
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    };
    let resp = handler(&req);
    let mut head = format!(
        "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
        resp.status,
        resp.body.len()
    );
    for (name, value) in &resp.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    write.write_all(head.as_bytes()).await?;
    write.write_all(&resp.body).await?;
    write.shutdown().await
}

/// Server-side state for the fake MCP server.
#[derive(Default)]
struct State {
    session: String,
    inits: u32,
    /// Session-less or wrong-session requests are answered 404 when set.
    broken: bool,
    protocol_versions: Vec<String>,
    calls: u32,
    authorization: Vec<String>,
}

/// A fake MCP server. With `sse`, replies come as event streams with an
/// unrelated notification first and a multi-line, CRLF-terminated response.
fn fake_server(sse: bool, state: Arc<Mutex<State>>) -> impl Fn(&Req) -> Resp + Send + Sync {
    move |req| {
        let mut st = state.lock().unwrap();
        if let Some(auth) = req.header("authorization") {
            st.authorization.push(auth.to_string());
        }
        if req.method() == "initialize" {
            st.inits += 1;
            st.session = format!("sess-{}", st.inits);
            let result = json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "0"}
            });
            let session = st.session.clone();
            return reply(req, result, sse).with_header("Mcp-Session-Id", &session);
        }
        if st.broken || req.header("mcp-session-id") != Some(st.session.as_str()) {
            return Resp::empty(404);
        }
        st.protocol_versions
            .push(req.header("mcp-protocol-version").unwrap_or("").to_string());
        match req.method() {
            "notifications/initialized" => Resp::empty(202),
            "tools/list" => reply(
                req,
                json!({"tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]}),
                sse,
            ),
            "tools/call" => {
                st.calls += 1;
                let message = req.body["params"]["arguments"]["message"].clone();
                reply(
                    req,
                    json!({"content": [{"type": "text", "text": message}]}),
                    sse,
                )
            }
            other => Resp::json(json!({
                "jsonrpc": "2.0",
                "id": req.id(),
                "error": {"code": -32601, "message": format!("no method {other}")}
            })),
        }
    }
}

fn reply(req: &Req, result: Value, sse: bool) -> Resp {
    let envelope = json!({"jsonrpc": "2.0", "id": req.id(), "result": result});
    if !sse {
        return Resp::json(envelope);
    }
    // Split the JSON at a comma so the two `data:` lines rejoin into valid JSON.
    let full = envelope.to_string();
    let cut = full.find(",\"result\"").unwrap() + 1;
    let body = format!(
        ": keep-alive\r\n\r\nevent: message\r\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{}}}}\r\n\r\n\
         data: {}\r\ndata: {}\r\n\r\n",
        &full[..cut],
        &full[cut..]
    );
    Resp::text(200, "text/event-stream", &body)
}

fn new_state() -> Arc<Mutex<State>> {
    Arc::new(Mutex::new(State::default()))
}

async fn echo_once(handle: &Arc<tokio::sync::Mutex<McpServerHandle>>, message: &str) -> String {
    let mut handle = handle.lock().await;
    let result = handle
        .call_tool("echo", json!({"message": message}))
        .await
        .expect("tools/call succeeds");
    assert!(!result.is_error);
    result.content
}

#[tokio::test]
async fn http_json_replies_list_and_call_tools() {
    let url = serve(fake_server(false, new_state())).await;
    let (handle, tools) = McpServerHandle::connect_http("web", &url, &[])
        .await
        .expect("connect");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    assert!(tools[0].schema_tokens > 0);
    assert_eq!(echo_once(&handle, "hello http").await, "hello http");
}

#[tokio::test]
async fn http_sse_replies_skip_unrelated_events() {
    let url = serve(fake_server(true, new_state())).await;
    let (handle, tools) = McpServerHandle::connect_http("sse", &url, &[])
        .await
        .expect("connect over SSE");
    assert_eq!(tools[0].name, "echo");
    assert_eq!(echo_once(&handle, "through sse").await, "through sse");
}

#[tokio::test]
async fn http_session_id_is_echoed_and_protocol_version_sent() {
    let state = new_state();
    let url = serve(fake_server(false, Arc::clone(&state))).await;
    let (handle, _) = McpServerHandle::connect_http("web", &url, &[])
        .await
        .expect("connect");
    // The fake server answers 404 unless the session id is echoed, so a
    // successful call proves the echo.
    assert_eq!(echo_once(&handle, "again").await, "again");
    let st = state.lock().unwrap();
    assert_eq!(st.inits, 1, "no re-initialize without expiry");
    assert!(
        st.protocol_versions.iter().all(|v| v == "2025-03-26"),
        "{:?}",
        st.protocol_versions
    );
}

#[tokio::test]
async fn http_404_reinitializes_once_and_retries_the_request() {
    let state = new_state();
    let url = serve(fake_server(false, Arc::clone(&state))).await;
    let (handle, _) = McpServerHandle::connect_http("web", &url, &[])
        .await
        .expect("connect");
    // Simulate the server dropping the session: the next request carries the
    // stale id, gets 404, and must re-initialize and retry.
    state.lock().unwrap().session = "expired".into();
    assert_eq!(echo_once(&handle, "after expiry").await, "after expiry");
    let st = state.lock().unwrap();
    assert_eq!(st.inits, 2, "exactly one re-initialize");
    assert_eq!(st.calls, 1);
}

#[tokio::test]
async fn http_404_after_reinitialize_is_an_error_not_a_loop() {
    let state = new_state();
    let url = serve(fake_server(false, Arc::clone(&state))).await;
    let (handle, _) = McpServerHandle::connect_http("web", &url, &[])
        .await
        .expect("connect");
    state.lock().unwrap().broken = true;
    let mut handle = handle.lock().await;
    let err = handle
        .call_tool("echo", json!({"message": "x"}))
        .await
        .err()
        .expect("persistent 404 must fail");
    assert!(err.to_string().contains("404"), "{err}");
    assert_eq!(
        state.lock().unwrap().inits,
        2,
        "one re-initialize, then give up"
    );
}

#[tokio::test]
async fn http_header_templates_expand_from_the_environment() {
    let state = new_state();
    let url = serve(fake_server(false, Arc::clone(&state))).await;
    std::env::set_var("MODRON_MCP_TEST_TOKEN_H9D", "secret-h9d");
    let headers = vec![(
        "Authorization".to_string(),
        "Bearer ${MODRON_MCP_TEST_TOKEN_H9D}".to_string(),
    )];
    let (handle, _) = McpServerHandle::connect_http("auth", &url, &headers)
        .await
        .expect("connect with expanded header");
    assert_eq!(echo_once(&handle, "authed").await, "authed");
    let st = state.lock().unwrap();
    assert!(st.authorization.iter().all(|a| a == "Bearer secret-h9d"));
}

#[tokio::test]
async fn http_unset_header_variable_is_a_startup_error_naming_it() {
    let url = serve(fake_server(false, new_state())).await;
    let headers = vec![(
        "Authorization".to_string(),
        "Bearer ${MODRON_MCP_TEST_UNSET_H9D}".to_string(),
    )];
    let err = McpServerHandle::connect_http("auth", &url, &headers)
        .await
        .err()
        .expect("unset variable must fail at connect");
    let text = err.to_string();
    assert!(text.contains("MODRON_MCP_TEST_UNSET_H9D"), "{text}");
    assert!(text.contains("auth"), "{text}");
}

#[tokio::test]
async fn http_oversized_body_is_rejected() {
    let big = vec![b' '; 16 * 1024 * 1024 + 1];
    let url = serve(move |_req| Resp {
        status: 200,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: big.clone(),
    })
    .await;
    let err = McpServerHandle::connect_http("big", &url, &[])
        .await
        .err()
        .expect("oversized reply must be refused");
    assert!(err.to_string().contains("exceeds"), "{err}");
}

#[tokio::test]
async fn http_error_status_reports_code_and_short_body() {
    let url = serve(|_req| Resp::text(500, "text/plain", &"x".repeat(400))).await;
    let err = McpServerHandle::connect_http("broken", &url, &[])
        .await
        .err()
        .expect("500 must fail");
    let text = err.to_string();
    assert!(text.contains("500"), "{text}");
    assert!(
        !text.contains(&"x".repeat(201)),
        "body capped at 200 chars: {text}"
    );
}

#[tokio::test]
async fn http_rejects_non_http_scheme_without_echoing_the_url() {
    let err = McpServerHandle::connect_http("ftp", "ftp://user:pw@example.test/mcp?token=abc", &[])
        .await
        .err()
        .expect("ftp must be refused");
    let text = err.to_string();
    assert!(text.contains("http or https"), "{text}");
    assert!(!text.contains("token=abc"), "url not echoed: {text}");
}
