//! Phase 1a end-to-end acceptance (LEDGER V6). Fully offline.
//!
//! 1. [`memory_persists_across_runs_and_is_inspectable`] — a scripted
//!    `MockProvider` drives the REAL engine with the REAL `remember` tool
//!    (memory root injected, PLAN.md §4.9 user tier): the entry file exists,
//!    `modron memory list` (real binary) shows it, a SECOND engine run's
//!    recorded request carries the memory in its system prompt, and
//!    `modron memory forget` removes it.
//!
//! 2. [`cli_compacts_under_pressure_and_reports_it`] — the REAL binary with
//!    `--provider openai-compat` pointed at a local scripted HTTP server
//!    (keyless `OPENAI_BASE_URL` path), a tiny `--max-input-tokens` and
//!    `--compact-threshold 0.5`: autocompaction fires, stderr carries the
//!    `compacted: N events → summary (est. saved ~X tok)` progress line, and
//!    the run completes with exactly one Compaction event persisted.

use modron_core::memory::MemoryStore;
use modron_core::testing::{MockProvider, StaticSelector};
use modron_core::{Engine, EngineConfig, ModelProvider, SessionEvent, SessionStore};
use modron_store::JsonlSessionStore;
use modron_tools::default_registry_with_memory;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MEMORY_TITLE: &str = "deploy target";
const MEMORY_BODY: &str = "staging runs on fly.io; production runs on bare metal in fra1";

fn mock_engine(
    provider: Arc<dyn ModelProvider>,
    store: Arc<dyn SessionStore>,
    working_dir: &Path,
    memory_root: &Path,
) -> Engine {
    let mut providers = HashMap::new();
    providers.insert("mock".to_string(), provider);
    Engine {
        selector: Arc::new(StaticSelector::new("mock-model", "mock")),
        providers,
        tools: Arc::new(default_registry_with_memory(memory_root)),
        store,
        config: EngineConfig {
            working_dir: working_dir.to_path_buf(),
            memory_root: Some(memory_root.to_path_buf()),
            ..EngineConfig::default()
        },
    }
}

/// V6(a): memory end-to-end — remember via a mock engine run → file exists →
/// `modron memory list` shows it → a second engine run's request carries the
/// memory → `modron memory forget` removes it.
#[tokio::test]
async fn memory_persists_across_runs_and_is_inspectable() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let memory_root = dir.path().join("memory");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // Run 1: the agent calls the REAL remember tool.
    let provider = Arc::new(
        MockProvider::new()
            .then_tool_call(
                "call-1",
                "remember",
                json!({"title": MEMORY_TITLE, "body": MEMORY_BODY}),
            )
            .then_text("Saved the deploy target."),
    );
    let store: Arc<dyn SessionStore> = Arc::new(JsonlSessionStore::new(&sessions_dir));
    let engine = mock_engine(provider.clone(), Arc::clone(&store), &work, &memory_root);
    let summary = engine.run_task("remember where we deploy").await.unwrap();
    assert_eq!(summary.tool_calls, 1);

    // The entry is on disk through the store (one markdown file + index).
    let memory = MemoryStore::for_workspace(&memory_root, &work).unwrap();
    assert_eq!(
        memory.read_entry(MEMORY_TITLE).unwrap().unwrap().trim(),
        MEMORY_BODY
    );
    let entry_files = std::fs::read_dir(memory.root().join("entries"))
        .unwrap()
        .count();
    assert_eq!(entry_files, 1, "exactly one entry file on disk");

    // Run 1's prompt had no memory section yet (nothing was remembered then).
    assert!(
        !provider.requests()[0]
            .system
            .contains("## Persistent memory"),
        "fresh workspace must not grow a memory section"
    );

    let bin = env!("CARGO_BIN_EXE_modron");
    let memory_args = [
        "--sessions-dir",
        sessions_dir.to_str().unwrap(),
        "--working-dir",
        work.to_str().unwrap(),
    ];

    // `modron memory list` (real binary) shows the entry.
    let list = Command::new(bin)
        .arg("memory")
        .arg("list")
        .args(memory_args)
        .output()
        .unwrap();
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(
        list.status.success(),
        "list stderr: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(list_out.contains(MEMORY_TITLE), "list output:\n{list_out}");

    // Run 2: a fresh engine in the same workspace sees the memory in its
    // assembled system prompt (M3 wiring through EngineConfig.memory_root).
    let provider2 = Arc::new(MockProvider::new().then_text("Deploy target known."));
    let engine2 = mock_engine(provider2.clone(), Arc::clone(&store), &work, &memory_root);
    engine2.run_task("where do we deploy?").await.unwrap();
    let system = &provider2.requests()[0].system;
    assert!(
        system.contains("## Persistent memory"),
        "second run prompt:\n{system}"
    );
    assert!(system.contains(MEMORY_BODY), "second run prompt:\n{system}");
    assert!(
        system.contains(MEMORY_TITLE),
        "second run prompt:\n{system}"
    );

    // `modron memory show <title>` prints the body; unknown titles fail.
    let show = Command::new(bin)
        .arg("memory")
        .arg("show")
        .arg(MEMORY_TITLE)
        .args(memory_args)
        .output()
        .unwrap();
    assert!(
        show.status.success(),
        "show stderr: {}",
        String::from_utf8_lossy(&show.stderr)
    );
    assert!(String::from_utf8_lossy(&show.stdout).contains(MEMORY_BODY));

    // `modron memory forget <title>` removes index line + file; list is
    // empty afterwards; a second forget is an honest failure, not silence.
    let forget = Command::new(bin)
        .arg("memory")
        .arg("forget")
        .arg(MEMORY_TITLE)
        .args(memory_args)
        .output()
        .unwrap();
    assert!(
        forget.status.success(),
        "forget stderr: {}",
        String::from_utf8_lossy(&forget.stderr)
    );
    assert!(String::from_utf8_lossy(&forget.stdout).contains("forgot"));
    assert_eq!(
        memory.read_entry(MEMORY_TITLE).unwrap(),
        None,
        "entry removed from the store"
    );
    assert_eq!(memory.read_index(), "", "index empty after forget");

    let list2 = Command::new(bin)
        .arg("memory")
        .arg("list")
        .args(memory_args)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&list2.stdout).contains("no memory for this workspace yet"));

    let forget2 = Command::new(bin)
        .arg("memory")
        .arg("forget")
        .arg(MEMORY_TITLE)
        .args(memory_args)
        .output()
        .unwrap();
    assert!(
        !forget2.status.success(),
        "forgetting a missing entry must not be silent"
    );
    assert!(String::from_utf8_lossy(&forget2.stderr).contains("no memory entry titled"));
}

// ---------------------------------------------------------------------------
// V6(b): compaction through the real CLI binary against a local scripted
// OpenAI-compatible HTTP server (the keyless `OPENAI_BASE_URL` path).
// ---------------------------------------------------------------------------

/// Minimal HTTP server: one scripted JSON body per accepted connection, in
/// order. The CLI makes its turn requests strictly sequentially, so a
/// single-threaded accept loop suffices; `Connection: close` keeps reqwest
/// from reusing sockets.
fn spawn_openai_mock(responses: Vec<String>) -> (u16, Arc<Mutex<VecDeque<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let queue = Arc::new(Mutex::new(VecDeque::from(responses)));
    let worker_queue = Arc::clone(&queue);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(body) = worker_queue.lock().unwrap().pop_front() else {
                break;
            };
            if serve_one(&mut stream, &body).is_err() {
                break;
            }
        }
    });
    (port, queue)
}

/// Read one HTTP request (headers + Content-Length body), answer with
/// `body` (application/json, Connection: close). Errors are for the caller
/// to decide on (listener closed mid-test).
fn serve_one(stream: &mut std::net::TcpStream, body: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| pos + 4)
        .unwrap_or(buf.len());
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let content_length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let mut received = buf.len() - header_end;
    while received < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        received += n;
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

/// OpenAI chat-completions response: assistant text + one tool call.
fn oai_tool_response(
    id: &str,
    name: &str,
    arguments: serde_json::Value,
    text: &str,
    usage: (u64, u64),
) -> String {
    json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": if text.is_empty() { None } else { Some(text.to_string()) },
                "tool_calls": [{
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": arguments.to_string()},
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": usage.0, "completion_tokens": usage.1},
    })
    .to_string()
}

/// OpenAI chat-completions response: plain final text, no tool calls.
fn oai_text_response(text: &str, usage: (u64, u64)) -> String {
    json!({
        "choices": [{
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": usage.0, "completion_tokens": usage.1},
    })
    .to_string()
}

/// ~8.1KB of chatty turn text (~2030 estimated tokens per turn): enough that
/// the turn-5 context estimate clears `0.5 × 12000` for any plausible
/// baseline, while turn 4's pre-flight stays under the 12000 budget cap.
fn long_text() -> String {
    "chatty analysis of the login module. ".repeat(220)
}

/// V6(b): the real binary, `--provider openai-compat` against a local mock
/// server, tiny `--max-input-tokens` + `--compact-threshold 0.5` →
/// autocompaction fires, the stderr progress line appears, the run completes.
#[test]
fn cli_compacts_under_pressure_and_reports_it() {
    let bin = env!("CARGO_BIN_EXE_modron");
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let long = long_text();
    let summary = "TASK: review login module auth edge cases. DECISIONS: fix token refresh \
                   races first. EDITS: none yet. OPEN ITEMS: write the two chosen fixes.";
    // Turn order: 4 accumulating turns (chatty text + tool calls), then the
    // compaction summarizer call, then the final answer turn. Turn 1 also
    // carries text so the summarized portion is meaty and the est.-saved
    // figure is meaningfully positive.
    let responses = vec![
        oai_tool_response(
            "c1",
            "bash",
            json!({"command": "echo modron-e2e-start"}),
            &"initial scan of the login module. ".repeat(30),
            (100, 20),
        ),
        oai_tool_response(
            "c2",
            "bash",
            json!({"command": "echo step-2"}),
            &long,
            (3000, 900),
        ),
        oai_tool_response(
            "c3",
            "bash",
            json!({"command": "echo step-3"}),
            &long,
            (3000, 900),
        ),
        oai_tool_response(
            "c4",
            "bash",
            json!({"command": "echo step-4"}),
            &long,
            (3000, 900),
        ),
        oai_text_response(summary, (400, 60)),
        oai_text_response("compaction e2e complete.", (50, 10)),
    ];
    let (port, _queue) = spawn_openai_mock(responses);

    let output = Command::new(bin)
        .arg("review the login module")
        .args(["--provider", "openai-compat"])
        .args(["--model", "modron-e2e-model"])
        .args(["--max-input-tokens", "12000"])
        .args(["--compact-threshold", "0.5"])
        .args(["--sessions-dir", sessions_dir.to_str().unwrap()])
        .args(["--working-dir", work.to_str().unwrap()])
        .env("OPENAI_BASE_URL", format!("http://127.0.0.1:{port}/v1"))
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {stdout}\nstderr: {stderr}",
        output.status.code()
    );

    // X3: the progress line, exactly in the mandated shape. The CLI line
    // starts with `compacted:` (engine tracing lines carry a timestamp
    // prefix, and the engine's own compaction log is debug-level).
    let line = stderr
        .lines()
        .find(|l| l.starts_with("compacted: "))
        .unwrap_or_else(|| panic!("no compaction line on stderr:\n{stderr}"));
    assert!(
        line.contains(" events → summary (est. saved ~"),
        "line: {line}"
    );
    assert!(line.trim_end().ends_with(" tok)"), "line: {line}");
    let saved: i64 = line
        .split("(est. saved ~")
        .nth(1)
        .and_then(|rest| rest.trim_end_matches(" tok)").parse().ok())
        .unwrap_or(-1);
    assert!(
        saved > 0,
        "compaction must estimate positive savings: {line}"
    );

    // The run completed with the final answer on stdout.
    assert!(
        stdout.contains("compaction e2e complete."),
        "stdout:\n{stdout}"
    );

    // Exactly one Compaction event was persisted for this run.
    let session_line = stdout
        .lines()
        .find(|l| l.starts_with("session: "))
        .expect("session line");
    let session_id = session_line
        .trim_start_matches("session: ")
        .split_whitespace()
        .next()
        .unwrap();
    let jsonl = std::fs::read_to_string(sessions_dir.join(format!("{session_id}.jsonl"))).unwrap();
    let compactions = jsonl
        .lines()
        .filter(|line| {
            serde_json::from_str::<SessionEvent>(line)
                .is_ok_and(|event| matches!(event, SessionEvent::Compaction { .. }))
        })
        .count();
    assert_eq!(
        compactions, 1,
        "exactly one compaction under the min-interval policy"
    );
}
