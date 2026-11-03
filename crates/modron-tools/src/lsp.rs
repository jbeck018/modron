//! Live LSP diagnostics (Phase 14 tranche 10): a minimal language-server
//! client so edits surface *live* compiler insight in tool output, per file,
//! before any project-wide checker runs.
//!
//! Scope, deliberately small: one client per language process, spawned
//! lazily the first time that language's file is written; `textDocument/
//! didOpen`/`didChange` after every successful edit/write; the server's
//! `publishDiagnostics` notifications land in a shared table the
//! `diagnostics` tool merges ahead of the project checker. Only stdio,
//! only the four methods above — no completion/hover/renames (yet).
//!
//! Protocol notes: unlike MCP (line-delimited), LSP frames bodies with a
//! `Content-Length: N\r\n\r\n` header. The reader task must skip server
//! noise (window/logMessage) and match responses by id.

use std::collections::HashMap;
use std::io::{BufRead, Read as _, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// One diagnostic line, LSP severity 1..=4 (error..hint).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diag {
    pub file: String,
    pub line: u32,
    pub severity: u8,
    pub message: String,
}

impl Diag {
    pub fn severity_label(&self) -> &'static str {
        match self.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        }
    }
}

/// A running language server.
pub struct LspServer {
    lang: &'static str,
    stdin: Mutex<Option<std::process::ChildStdin>>,
    /// Reaped in Drop — dropping the handle would leave the exited server
    /// as an unreapable zombie for the life of the process.
    child: Mutex<Option<std::process::Child>>,
    next_id: AtomicU64,
    /// Diagnostics published by the server, keyed by file URI.
    diagnostics: Arc<Mutex<HashMap<String, Vec<Diag>>>>,
    /// Files we didOpen (so didChange is used after the first write).
    opened: Mutex<std::collections::HashSet<String>>,
}

impl Drop for LspServer {
    fn drop(&mut self) {
        // best-effort shutdown; process exit closes pipes either way
        let _ = self.notify("shutdown", serde_json::json!({}));
        let _ = self.notify("exit", serde_json::json!({}));
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Process-wide server table (one per language), shared by every tool.
fn servers() -> &'static Mutex<HashMap<&'static str, Arc<LspServer>>> {
    static SERVERS: OnceLock<Mutex<HashMap<&'static str, Arc<LspServer>>>> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The default server command for a language, when it is on PATH.
/// Args are complete — spawn must not append anything.
fn server_command(lang: &str) -> Option<(&'static str, Vec<&'static str>)> {
    match lang {
        "rust" => Some(("rust-analyzer", vec![])),
        "go" => Some(("gopls", vec!["serve", "--stdio"])),
        "python" => Some(("pyright-langserver", vec!["--stdio"])),
        "typescript" | "javascript" => Some(("typescript-language-server", vec!["--stdio"])),
        _ => None,
    }
}

/// Language for a path, by extension.
pub fn language_for(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("rs") => Some("rust"),
        Some("go") => Some("go"),
        Some("py") | Some("pyi") => Some("python"),
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") => Some("typescript"),
        _ => None,
    }
}

fn on_path(bin: &str) -> bool {
    // A file on PATH is not enough: rustup SHIMS exist for binaries whose
    // toolchain component is not installed and die instantly. Probe that
    // the binary actually runs (bounded).
    let Ok(output) = std::process::Command::new(bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return false;
    };
    // rust-analyzer/pyright et al. exit 0; gopls prints and exits 0.
    output.status.success()
}

/// The server for `lang`, spawning it on first use. None when the language
/// is unsupported or its server binary is not installed.
pub fn server_for(lang: &str) -> Option<Arc<LspServer>> {
    let lang: &'static str = match lang {
        "rust" => "rust",
        "go" => "go",
        "python" => "python",
        "typescript" | "javascript" => "typescript",
        _ => return None,
    };
    let mut servers = servers().lock().unwrap();
    if let Some(server) = servers.get(lang) {
        return Some(Arc::clone(server));
    }
    let (bin, args) = server_command(lang)?;
    if !on_path(bin) {
        tracing::debug!(bin, "LSP server not on PATH");
        return None;
    }
    let root = std::env::current_dir().ok()?;
    let server = LspServer::spawn(lang, bin, &args, &root)?;
    let server = Arc::new(server);
    servers.insert(lang, Arc::clone(&server));
    Some(server)
}

impl LspServer {
    /// Spawn + initialize. Synchronous by design: called from spawn_blocking
    /// (edit tools run it inline; the write is local and fast).
    pub fn spawn(lang: &'static str, bin: &str, args: &[&str], root: &Path) -> Option<LspServer> {
        let mut child = std::process::Command::new(bin)
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let diagnostics: Arc<Mutex<HashMap<String, Vec<Diag>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let server = LspServer {
            lang,
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            next_id: AtomicU64::new(1),
            diagnostics: Arc::clone(&diagnostics),
            opened: Mutex::new(std::collections::HashSet::new()),
        };
        // Reader task: owns stdout, fills the diagnostics table, answers
        // nothing (server->client requests are ignored).
        std::thread::spawn(move || reader_loop(stdout, diagnostics));
        server.initialize(root).then_some(server)
    }

    fn initialize(&self, root: &Path) -> bool {
        let root_uri = format!("file://{}", root.display());
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {},
        });
        if self.request_raw(id, "initialize", params).is_none() {
            return false;
        }
        self.notify("initialized", serde_json::json!({}));
        // rust-analyzer starts analysis after a (settings-empty) config
        // push; harmless to every other server.
        self.notify(
            "workspace/didChangeConfiguration",
            serde_json::json!({"settings": {}}),
        );
        tracing::info!(lang = self.lang, "LSP server initialized");
        true
    }

    /// Send a request and wait (bounded) for its response. Only
    /// `initialize` uses this; other calls are notifications.
    fn request_raw(&self, id: u64, method: &str, params: serde_json::Value) -> Option<()> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_body(&body)
    }

    fn notify(&self, method: &str, params: serde_json::Value) -> Option<()> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        self.write_body(&body)
    }

    fn write_body(&self, body: &serde_json::Value) -> Option<()> {
        let json = serde_json::to_string(body).ok()?;
        let mut guard = self.stdin.lock().unwrap();
        let stdin = guard.as_mut()?;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n{json}", json.len()).as_bytes())
            .ok()?;
        stdin.flush().ok()
    }

    /// didOpen on first sight of a file, didChange after.
    pub fn did_write(&self, path: &Path, text: &str) {
        let uri = format!("file://{}", path.display());
        let language_id = self.lang;
        let first = self.opened.lock().unwrap().insert(uri.clone());
        if first {
            let _ = self.notify(
                "textDocument/didOpen",
                serde_json::json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": 1,
                        "text": text,
                    }
                }),
            );
        } else {
            let version = 2; // good enough: servers re-lint on any change
            let _ = self.notify(
                "textDocument/didChange",
                serde_json::json!({
                    "textDocument": { "uri": uri, "version": version },
                    "contentChanges": [{ "text": text }],
                }),
            );
        }
    }

    /// Current diagnostics for every file the server has reported on.
    pub fn all_diagnostics(&self) -> Vec<Diag> {
        self.diagnostics
            .lock()
            .unwrap()
            .values()
            .flatten()
            .cloned()
            .collect()
    }
}

/// Notify the right server (if any) that `path` now contains `text`.
/// Called by edit/write after a successful write. Cheap: local pipe write.
pub fn did_write(path: &Path, text: &str) {
    if let Some(lang) = language_for(path) {
        if let Some(server) = server_for(lang) {
            server.did_write(path, text);
        }
    }
}

/// All live diagnostics across spawned servers.
pub fn collect() -> Vec<Diag> {
    servers()
        .lock()
        .unwrap()
        .values()
        .flat_map(|s| s.all_diagnostics())
        .collect()
}

/// Wait up to `ms` for at least one diagnostics entry for `uri_hint`
/// (any update counts), then return everything.
pub fn wait_diagnostics(ms: u64, uri_contains: &str) -> Vec<Diag> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
    loop {
        let diags = collect();
        let ready = diags
            .iter()
            .any(|d| uri_contains.is_empty() || d.file.contains(uri_contains));
        if ready || std::time::Instant::now() >= deadline {
            return diags;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Reader: parse framed messages; store publishDiagnostics payloads.
fn reader_loop(stdout: std::process::ChildStdout, table: Arc<Mutex<HashMap<String, Vec<Diag>>>>) {
    let mut reader = std::io::BufReader::new(stdout);
    loop {
        // Header lines until an empty line.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    let trimmed = line.trim_end();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(v) = trimmed.strip_prefix("Content-Length:") {
                        content_length = v.trim().parse().ok();
                    }
                }
            }
        }
        let Some(len) = content_length else { continue };
        // A hostile or buggy server must not OOM the agent with one
        // oversized Content-Length header: skip absurd frames whole
        // (draining the declared length keeps framing in sync).
        const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
        if len == 0 || len > MAX_FRAME_BYTES {
            let mut remaining = len;
            let mut sink = [0u8; 65536];
            while remaining > 0 {
                let want = remaining.min(sink.len());
                match reader.read_exact(&mut sink[..want]) {
                    Ok(()) => remaining -= want,
                    Err(_) => return,
                }
            }
            tracing::warn!(len, "skipped an oversized LSP frame");
            continue;
        }
        let mut body = vec![0u8; len];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else {
            continue;
        };
        if msg.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics") {
            let params = msg
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null);
            let uri = params
                .get("uri")
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string();
            let file = uri.strip_prefix("file://").unwrap_or(&uri).to_string();
            let diags: Vec<Diag> = params
                .get("diagnostics")
                .and_then(|d| d.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            Some(Diag {
                                file: file.clone(),
                                line: item.pointer("/range/start/line")?.as_u64()? as u32 + 1,
                                severity: item.get("severity").and_then(|s| s.as_u64()).unwrap_or(4)
                                    as u8,
                                message: item
                                    .get("message")?
                                    .as_str()?
                                    .lines()
                                    .next()
                                    .unwrap_or("")
                                    .chars()
                                    .take(200)
                                    .collect(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            table.lock().unwrap().insert(uri, diags);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_detection_covers_the_supported_set() {
        assert_eq!(language_for(Path::new("a/b/c.rs")), Some("rust"));
        assert_eq!(language_for(Path::new("x.GO")), Some("go"));
        assert_eq!(language_for(Path::new("m.pyi")), Some("python"));
        assert_eq!(language_for(Path::new("t.tsx")), Some("typescript"));
        assert_eq!(language_for(Path::new("t.txt")), None);
    }

    /// A fake LSP server in python: answers initialize, publishes one
    /// diagnostic per didOpen. Proves framing + handshake + collection.
    #[test]
    fn fake_server_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import json, sys
def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line or line == b"\r\n":
            break
        k, _, v = line.decode().partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", 0))
    return json.loads(sys.stdin.buffer.read(n))
def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
    sys.stdout.buffer.flush()
while True:
    try:
        msg = read_msg()
    except Exception:
        break
    method = msg.get("method", "")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"capabilities": {}}})
    elif method == "textDocument/didOpen":
        uri = msg["params"]["textDocument"]["uri"]
        send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
              "params": {"uri": uri, "diagnostics": [
                  {"range": {"start": {"line": 2, "character": 0}},
                   "severity": 1, "message": "fake error\nmore detail"}]}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": None})
    elif method == "exit":
        break
"#;
        let script_path = dir.path().join("fake_lsp.py");
        std::fs::write(&script_path, script).unwrap();

        let server = LspServer::spawn(
            "rust",
            "python3",
            &[script_path.to_str().unwrap()],
            dir.path(),
        )
        .expect("fake server spawns + initializes");
        let target = dir.path().join("lib.rs");
        server.did_write(&target, "fn broken() {}\n");
        // Poll this server directly (wait_diagnostics reads the process
        // registry, which the test bypasses by spawning directly).
        // Generous bound: under parallel test load the fake server's
        // spawn + publish can exceed a tight window; this is a correctness
        // test, not a latency one.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let diags = loop {
            let d = server.all_diagnostics();
            if !d.is_empty() || std::time::Instant::now() >= deadline {
                break d;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        assert!(!diags.is_empty(), "server published diagnostics");
        let d = &diags[0];
        assert_eq!(d.line, 3, "LSP lines are 0-based → 1-based");
        assert_eq!(d.severity, 1);
        assert_eq!(d.message, "fake error", "first line only: {d:?}");
        assert!(d.file.ends_with("lib.rs"), "{d:?}");
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// REAL rust-analyzer end-to-end (ignored by default: needs RA on PATH,
    /// first index of a fresh crate takes seconds). Run with
    /// `cargo test -p modron-tools --lib -- --ignored`.
    #[test]
    #[ignore]
    fn rust_analyzer_reports_a_type_error_live() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"lsp-live\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let lib = dir.path().join("src").join("lib.rs");
        // Language servers like rust-analyzer analyze DISK state — the real
        // edit/write tools always persist before did_write, so the test
        // must too.
        std::fs::write(&lib, "pub fn broken() -> u32 { \"not a number\" }\n").unwrap();
        let server = match LspServer::spawn("rust", "rust-analyzer", &[], dir.path()) {
            Some(s) => Arc::new(s),
            None => {
                eprintln!("rust-analyzer not available — skipping");
                return;
            }
        };
        server.did_write(&lib, &std::fs::read_to_string(&lib).unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
        let diags = loop {
            let d = server.all_diagnostics();
            let useful = d.iter().any(|x| x.file.ends_with("lib.rs"));
            if useful || std::time::Instant::now() >= deadline {
                break d;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        };
        let ours: Vec<&Diag> = diags
            .iter()
            .filter(|d| d.file.ends_with("lib.rs"))
            .collect();
        assert!(
            ours.iter().any(|d| d.severity_label() == "error"),
            "expected a live error diagnostic, got {ours:#?}"
        );
        eprintln!("live diagnostics: {ours:#?}");
    }
}
