//! Live LSP diagnostics (Phase 14 tranche 10): a minimal language-server
//! client so edits surface *live* compiler insight in tool output, per file,
//! before any project-wide checker runs.
//!
//! Scope, deliberately small: one client per (language, workspace root),
//! spawned lazily the first time that language's file is written under that
//! root; `textDocument/didOpen`/`didChange` after every successful edit/write;
//! the server's `publishDiagnostics` notifications land in a shared table the
//! `diagnostics` tool merges ahead of the project checker. Only stdio,
//! only the four methods above — no completion/hover/renames (yet).
//!
//! Protocol notes: unlike MCP (line-delimited), LSP frames bodies with a
//! `Content-Length: N\r\n\r\n` header. The reader task must skip server
//! noise (window/logMessage) and match responses by id.

use std::collections::HashMap;
use std::io::{BufRead, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use modron_core::hooks::scrub_std_command;

/// How long the `initialize` response may take before the server counts as
/// failed to start.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a `<bin> --version` probe may run before the binary counts as
/// broken.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

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

/// Responses to requests we sent, keyed by request id; filled by the reader.
type Pending = Arc<Mutex<HashMap<u64, mpsc::Sender<serde_json::Value>>>>;

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
    /// Requests awaiting a response (the `initialize` handshake).
    pending: Pending,
    /// Last version sent per document URI. The first sighting is `didOpen`
    /// (version 1); every later change sends the next integer. The lock is
    /// held across the send so versions reach the server in order.
    versions: Mutex<HashMap<String, i64>>,
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

/// Registry key: one server per (language, workspace root).
type ServerKey = (&'static str, PathBuf);

/// Process-wide server table, shared by every tool. A `None` entry records a
/// failed spawn or handshake for that key, so it is not retried on every edit.
fn servers() -> &'static Mutex<HashMap<ServerKey, Option<Arc<LspServer>>>> {
    static SERVERS: OnceLock<Mutex<HashMap<ServerKey, Option<Arc<LspServer>>>>> = OnceLock::new();
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-binary probe results for the lifetime of the process.
fn probe_results() -> &'static Mutex<HashMap<String, bool>> {
    static PROBES: OnceLock<Mutex<HashMap<String, bool>>> = OnceLock::new();
    PROBES.get_or_init(|| Mutex::new(HashMap::new()))
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

/// `file://` URI for `path`. Every byte outside the unreserved set
/// (`A-Z a-z 0-9 - . _ ~ /`) is percent-encoded as UTF-8 `%XX`, so spaces,
/// `#`, `%`, `?` and non-ASCII characters yield a valid URI.
pub fn file_uri(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut s = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.into_owned()
    };
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    let mut uri = String::with_capacity("file://".len() + s.len());
    uri.push_str("file://");
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Local path for a URI reported by a server: strips `file://` and decodes
/// percent-escapes so diagnostics show real paths.
fn path_from_uri(uri: &str) -> String {
    percent_decode(uri.strip_prefix("file://").unwrap_or(uri))
}

fn percent_decode(s: &str) -> String {
    fn hex_digit(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Version bookkeeping: returns the next version for `uri`. The first call
/// yields 1 (the `didOpen` version); each later call increases by one.
fn bump_version(versions: &mut HashMap<String, i64>, uri: &str) -> i64 {
    let version = versions.entry(uri.to_string()).or_insert(0);
    *version += 1;
    *version
}

/// Runs `bin --version` and reports whether it exits 0 within `timeout`.
/// An overrunning probe is killed, so a hung binary cannot stall the caller.
fn probe_within(bin: &str, timeout: Duration) -> bool {
    // D9: provider secrets never reach a language server's environment.
    let mut probe = std::process::Command::new(bin);
    scrub_std_command(&mut probe);
    let Ok(mut child) = probe
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Whether `bin` runs. Probed once per process: a missing or broken binary
/// is remembered and never probed again. The cache lock is not held during
/// the probe.
fn bin_usable(bin: &str) -> bool {
    let cached = probe_results().lock().unwrap().get(bin).copied();
    if let Some(known) = cached {
        return known;
    }
    let usable = probe_within(bin, PROBE_TIMEOUT);
    probe_results()
        .lock()
        .unwrap()
        .insert(bin.to_string(), usable);
    usable
}

/// Returns the registry entry for `key`, building it with `make` on first
/// use. `make` runs without the registry lock held (spawning waits for the
/// handshake). Its failure (`None`) is cached like a success.
fn registry_get_or_make(
    key: ServerKey,
    make: impl FnOnce() -> Option<LspServer>,
) -> Option<Arc<LspServer>> {
    let cached = servers().lock().unwrap().get(&key).cloned();
    if let Some(entry) = cached {
        return entry;
    }
    let made = make().map(Arc::new);
    servers().lock().unwrap().entry(key).or_insert(made).clone()
}

/// The server for `lang` under workspace `root`, spawning it on first use.
/// None when the language is unsupported or its server binary is not usable.
pub fn server_for(lang: &str, root: &Path) -> Option<Arc<LspServer>> {
    let lang: &'static str = match lang {
        "rust" => "rust",
        "go" => "go",
        "python" => "python",
        "typescript" | "javascript" => "typescript",
        _ => return None,
    };
    let (bin, args) = server_command(lang)?;
    registry_get_or_make((lang, root.to_path_buf()), || {
        if !bin_usable(bin) {
            tracing::debug!(bin, "LSP server not usable (missing or broken)");
            return None;
        }
        LspServer::spawn(lang, bin, &args, root)
    })
}

impl LspServer {
    /// Spawn + initialize. Synchronous by design: called from spawn_blocking
    /// (edit tools run it inline; the write is local and fast).
    pub fn spawn(lang: &'static str, bin: &str, args: &[&str], root: &Path) -> Option<LspServer> {
        Self::spawn_with_timeout(lang, bin, args, root, INIT_TIMEOUT)
    }

    fn spawn_with_timeout(
        lang: &'static str,
        bin: &str,
        args: &[&str],
        root: &Path,
        init_timeout: Duration,
    ) -> Option<LspServer> {
        // D9: provider secrets never reach the language server's environment.
        let mut command = std::process::Command::new(bin);
        scrub_std_command(&mut command);
        let mut child = command
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
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let server = LspServer {
            lang,
            stdin: Mutex::new(Some(stdin)),
            child: Mutex::new(Some(child)),
            next_id: AtomicU64::new(1),
            diagnostics: Arc::clone(&diagnostics),
            pending: Arc::clone(&pending),
            versions: Mutex::new(HashMap::new()),
        };
        // Reader task: owns stdout, fills the diagnostics table and routes
        // responses to waiting requests. Server->client requests are ignored.
        std::thread::spawn(move || reader_loop(stdout, diagnostics, pending));
        server.initialize(root, init_timeout).then_some(server)
    }

    /// Sends `initialize`, waits (bounded) for its response, then sends
    /// `initialized`. A timeout, a closed pipe or an error response means
    /// the server failed to start.
    fn initialize(&self, root: &Path, timeout: Duration) -> bool {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": file_uri(root),
            "capabilities": {},
        });
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": params,
        });
        let sent = self.write_body(&body).is_some();
        let response = if sent {
            rx.recv_timeout(timeout).ok()
        } else {
            None
        };
        self.pending.lock().unwrap().remove(&id);
        let Some(response) = response else {
            tracing::warn!(lang = self.lang, "LSP server did not answer initialize");
            return false;
        };
        if response.get("error").is_some() {
            tracing::warn!(lang = self.lang, "LSP server rejected initialize");
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

    /// didOpen on first sight of a file, didChange with the next version after.
    pub fn did_write(&self, path: &Path, text: &str) {
        let uri = file_uri(path);
        let language_id = self.lang;
        let mut versions = self.versions.lock().unwrap();
        let version = bump_version(&mut versions, &uri);
        if version == 1 {
            let _ = self.notify(
                "textDocument/didOpen",
                serde_json::json!({
                    "textDocument": {
                        "uri": uri,
                        "languageId": language_id,
                        "version": version,
                        "text": text,
                    }
                }),
            );
        } else {
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

/// Notify the right server (if any) for workspace `root` that `path` now
/// contains `text`. Called by edit/write after a successful write. Cheap:
/// local pipe write.
pub fn did_write(path: &Path, text: &str, root: &Path) {
    if let Some(lang) = language_for(path) {
        if let Some(server) = server_for(lang, root) {
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
        .flatten()
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

/// Reader: parse framed messages; hand responses to waiting requests; store
/// publishDiagnostics payloads.
fn reader_loop(
    stdout: std::process::ChildStdout,
    table: Arc<Mutex<HashMap<String, Vec<Diag>>>>,
    pending: Pending,
) {
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
        // A response (id, no method) goes to whichever request awaits it.
        if msg.get("method").is_none() {
            if let Some(id) = msg.get("id").and_then(|v| v.as_u64()) {
                let waiter = pending.lock().unwrap().remove(&id);
                if let Some(tx) = waiter {
                    let _ = tx.send(msg);
                }
            }
            continue;
        }
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
            let file = path_from_uri(&uri);
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
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn language_detection_covers_the_supported_set() {
        assert_eq!(language_for(Path::new("a/b/c.rs")), Some("rust"));
        assert_eq!(language_for(Path::new("x.GO")), Some("go"));
        assert_eq!(language_for(Path::new("m.pyi")), Some("python"));
        assert_eq!(language_for(Path::new("t.tsx")), Some("typescript"));
        assert_eq!(language_for(Path::new("t.txt")), None);
    }

    #[test]
    fn file_uri_percent_encodes_unsafe_bytes() {
        assert_eq!(
            file_uri(Path::new("/a b/c#d%e?f/x.rs")),
            "file:///a%20b/c%23d%25e%3Ff/x.rs"
        );
        assert_eq!(
            file_uri(Path::new("/proj/caf\u{e9}/\u{65e5}.rs")),
            "file:///proj/caf%C3%A9/%E6%97%A5.rs"
        );
        // The unreserved set passes through untouched.
        assert_eq!(
            file_uri(Path::new("/tmp/x-y_z.~a/b.rs")),
            "file:///tmp/x-y_z.~a/b.rs"
        );
    }

    #[test]
    fn file_uri_decodes_back_to_the_same_path() {
        let p = "/a b/c#d%e?f/caf\u{e9}.rs";
        assert_eq!(path_from_uri(&file_uri(Path::new(p))), p);
    }

    #[test]
    fn document_versions_increase_per_uri() {
        let mut versions = HashMap::new();
        assert_eq!(bump_version(&mut versions, "file:///a.rs"), 1, "didOpen");
        assert_eq!(bump_version(&mut versions, "file:///a.rs"), 2);
        assert_eq!(bump_version(&mut versions, "file:///a.rs"), 3);
        assert_eq!(
            bump_version(&mut versions, "file:///b.rs"),
            1,
            "per document"
        );
        assert_eq!(bump_version(&mut versions, "file:///a.rs"), 4);
    }

    /// A shell script that is a fake binary: `body` runs as `sh`.
    #[cfg(unix)]
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn probe_accepts_a_binary_that_runs() {
        let dir = tempfile::tempdir().unwrap();
        let bin = write_script(dir.path(), "ok-lsp", "exit 0");
        assert!(probe_within(bin.to_str().unwrap(), Duration::from_secs(3)));
    }

    #[cfg(unix)]
    #[test]
    fn probe_is_bounded_and_kills_a_hung_binary() {
        let dir = tempfile::tempdir().unwrap();
        // `exec` so the kill lands on the sleeping process itself.
        let bin = write_script(dir.path(), "hung-lsp", "exec /bin/sleep 30");
        let started = Instant::now();
        assert!(!probe_within(
            bin.to_str().unwrap(),
            Duration::from_millis(400)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "probe overran its deadline: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn broken_binary_is_probed_once_then_cached() {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("probes.txt");
        let bin = write_script(
            dir.path(),
            "broken-lsp",
            &format!("echo probe >> '{}'\nexit 1", counter.display()),
        );
        let bin = bin.to_str().unwrap();
        assert!(!bin_usable(bin));
        assert!(!bin_usable(bin));
        assert!(!bin_usable(bin));
        let runs = std::fs::read_to_string(&counter).unwrap();
        assert_eq!(runs.lines().count(), 1, "probed exactly once: {runs:?}");
    }

    /// Python fake server: answers initialize, publishes one diagnostic per
    /// didOpen. Proves framing + handshake + collection.
    fn write_fake_server(dir: &Path) -> PathBuf {
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
        let script_path = dir.join("fake_lsp.py");
        std::fs::write(&script_path, script).unwrap();
        script_path
    }

    #[test]
    fn fake_server_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = write_fake_server(dir.path());

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

    #[cfg(unix)]
    #[test]
    fn server_that_never_answers_initialize_fails_to_start() {
        let dir = tempfile::tempdir().unwrap();
        // Runs forever, never reads stdin, never answers.
        let started = Instant::now();
        let server = LspServer::spawn_with_timeout(
            "rust",
            "/bin/sleep",
            &["30"],
            dir.path(),
            Duration::from_millis(300),
        );
        assert!(server.is_none(), "no initialize response means no server");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn registry_keys_servers_by_root_and_caches_failures() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let script = write_fake_server(a.path());
        let script = script.to_str().unwrap();
        let spawns = AtomicUsize::new(0);
        let spawn_fake = |root: &Path| {
            spawns.fetch_add(1, Ordering::SeqCst);
            LspServer::spawn("rust", "python3", &[script], root)
        };

        let first = registry_get_or_make(("rust", a.path().to_path_buf()), || spawn_fake(a.path()))
            .expect("spawns for root a");
        let again = registry_get_or_make(("rust", a.path().to_path_buf()), || spawn_fake(a.path()))
            .expect("cached for root a");
        assert!(Arc::ptr_eq(&first, &again), "same root shares one server");
        assert_eq!(spawns.load(Ordering::SeqCst), 1);

        let other = registry_get_or_make(("rust", b.path().to_path_buf()), || spawn_fake(b.path()))
            .expect("spawns for root b");
        assert!(
            !Arc::ptr_eq(&first, &other),
            "distinct roots get distinct servers"
        );
        assert_eq!(spawns.load(Ordering::SeqCst), 2);

        // A failed spawn is cached too: the builder is not run again.
        let missing = ("rust", PathBuf::from("/no/such/workspace/for/lsp-test"));
        assert!(registry_get_or_make(missing.clone(), || None).is_none());
        assert!(registry_get_or_make(missing, || panic!("failure must be cached")).is_none());
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
