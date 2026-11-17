//! `web_search` tool - search the public web and return numbered results.
//!
//! Backends are chosen by which API key is configured, in this order:
//! `BRAVE_API_KEY`, `TAVILY_API_KEY`, `EXA_API_KEY`. With no key the tool
//! refuses; it never scrapes a search page without a key.
//!
//! Keys are read from the process environment first and then from the
//! portal credential overlay (the same order the providers use). Keys are
//! sent only in request headers and never echoed into tool output.
//!
//! The endpoints are fixed, operator-chosen API hosts (not model-controlled),
//! so this tool does not pin addresses like `webfetch` does. It follows the
//! usual proxy environment, refuses redirects, caps the response body and the
//! output, and marks results as untrusted web content.

use crate::common::{arg_str, opt_u64, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use crate::webfetch::{html_to_text, push_capped, wrap_untrusted};
use async_trait::async_trait;
use modron_core::{
    credential_overlay, Result, SandboxPolicy, Tool, ToolCtx, ToolOutcome, ToolSpec,
};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde_json::{json, Value};
use std::time::Duration;

/// Results returned when `max_results` is omitted.
pub const DEFAULT_RESULTS: usize = 5;
/// Hard ceiling for `max_results`.
pub const MAX_RESULTS: usize = 10;
/// Byte cap for the formatted result list.
pub const OUTPUT_BYTES: usize = 8_000;
/// Byte cap for one backend response body.
const RESPONSE_BYTES: usize = 512 * 1024;
/// Per-result character caps before formatting.
const TITLE_CHARS: usize = 200;
const SNIPPET_CHARS: usize = 400;
/// Total request budget, covering connect, headers and body.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Byte cap for error messages.
const ERROR_BYTES: usize = 4096;
const USER_AGENT: &str = "modron-web-search";
const NAME: &str = "web_search";

const BRAVE_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const TAVILY_URL: &str = "https://api.tavily.com/search";
const EXA_URL: &str = "https://api.exa.ai/search";

/// The three backends, in selection order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Brave,
    Tavily,
    Exa,
}

impl Backend {
    /// Selection order: the first backend whose key is set wins.
    pub const ORDER: [Backend; 3] = [Backend::Brave, Backend::Tavily, Backend::Exa];

    /// The environment variable that holds this backend's key.
    pub const fn env_var(self) -> &'static str {
        match self {
            Backend::Brave => "BRAVE_API_KEY",
            Backend::Tavily => "TAVILY_API_KEY",
            Backend::Exa => "EXA_API_KEY",
        }
    }

    /// Display name used in output and errors.
    pub const fn label(self) -> &'static str {
        match self {
            Backend::Brave => "Brave",
            Backend::Tavily => "Tavily",
            Backend::Exa => "Exa",
        }
    }
}

/// Full endpoint URLs per backend. Tests point these at a local mock server.
#[derive(Clone)]
pub struct Endpoints {
    pub brave: String,
    pub tavily: String,
    pub exa: String,
}

impl Endpoints {
    fn url(&self, backend: Backend) -> &str {
        match backend {
            Backend::Brave => &self.brave,
            Backend::Tavily => &self.tavily,
            Backend::Exa => &self.exa,
        }
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            brave: BRAVE_URL.to_string(),
            tavily: TAVILY_URL.to_string(),
            exa: EXA_URL.to_string(),
        }
    }
}

/// The three optional keys. Never printed: no `Debug` impl.
#[derive(Clone, Default)]
pub struct Keys {
    pub brave: Option<String>,
    pub tavily: Option<String>,
    pub exa: Option<String>,
}

impl Keys {
    /// Reads each key from the environment, then the portal overlay. Blank
    /// values count as unset.
    fn from_env() -> Self {
        let get = |backend: Backend| {
            non_blank(std::env::var(backend.env_var()).ok())
                .or_else(|| non_blank(credential_overlay::shared().get(backend.env_var())))
        };
        Self {
            brave: get(Backend::Brave),
            tavily: get(Backend::Tavily),
            exa: get(Backend::Exa),
        }
    }

    fn get(&self, backend: Backend) -> Option<&str> {
        match backend {
            Backend::Brave => self.brave.as_deref(),
            Backend::Tavily => self.tavily.as_deref(),
            Backend::Exa => self.exa.as_deref(),
        }
    }

    /// The backend chosen by [`Backend::ORDER`], with its key.
    fn select(&self) -> Option<(Backend, &str)> {
        Backend::ORDER
            .into_iter()
            .find_map(|b| self.get(b).map(|key| (b, key)))
    }
}

fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Searches the public web. On-demand tool (reached via `tool_search`).
#[derive(Default)]
pub struct WebSearchTool {
    endpoints: Endpoints,
    /// `None` (the default) reads keys at call time; tests inject a fixed set.
    fixed_keys: Option<Keys>,
}

impl WebSearchTool {
    /// Test constructor: custom endpoints and fixed keys.
    #[cfg(test)]
    pub(crate) fn with_endpoints_and_keys(endpoints: Endpoints, keys: Keys) -> Self {
        Self {
            endpoints,
            fixed_keys: Some(keys),
        }
    }
}

/// One search hit, already reduced to plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME.into(),
            description: "Search the public web and return numbered results (title, url, \
                          snippet). Needs BRAVE_API_KEY, TAVILY_API_KEY or EXA_API_KEY; \
                          without one it refuses."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query."},
                    "max_results": {"type": "integer", "description": "Results to return (default 5, max 10)."},
                    "allowed_domains": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Keep only results whose host is one of these domains (subdomains included)."
                    },
                    "blocked_domains": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Drop results whose host is one of these domains (subdomains included)."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                ERROR_BYTES,
            ));
        }
        let query = match arg_str(&args, "query").map(str::trim) {
            Some(q) if !q.is_empty() => q,
            _ => return Ok(tool_err(NAME, "missing `query`", ERROR_BYTES)),
        };
        let max_results = opt_u64(&args, "max_results")
            .unwrap_or(DEFAULT_RESULTS as u64)
            .clamp(1, MAX_RESULTS as u64) as usize;
        let allowed = match domain_list(&args, "allowed_domains") {
            Ok(list) => list,
            Err(message) => return Ok(tool_err(NAME, message, ERROR_BYTES)),
        };
        let blocked = match domain_list(&args, "blocked_domains") {
            Ok(list) => list,
            Err(message) => return Ok(tool_err(NAME, message, ERROR_BYTES)),
        };
        if !matches!(ctx.sandbox, SandboxPolicy::Off) {
            return Ok(tool_err(
                NAME,
                "network is disabled by the sandbox",
                ERROR_BYTES,
            ));
        }

        let keys = match &self.fixed_keys {
            Some(keys) => keys.clone(),
            None => Keys::from_env(),
        };
        let Some((backend, key)) = keys.select() else {
            return Ok(tool_err(NAME, NO_KEY_MESSAGE, ERROR_BYTES));
        };

        // Domain filters run client-side for every backend, so ask for the
        // maximum when filtering: otherwise the filter could leave nothing.
        let filtering = !allowed.is_empty() || !blocked.is_empty();
        let fetch_count = if filtering { MAX_RESULTS } else { max_results };
        let hits = match search(&self.endpoints, backend, key, query, fetch_count).await {
            Ok(hits) => hits,
            Err(message) => return Ok(tool_err(NAME, message, ERROR_BYTES)),
        };
        let hits: Vec<Hit> = hits
            .into_iter()
            .filter(|hit| domain_ok(&hit.url, &allowed, &blocked))
            .take(max_results)
            .collect();

        Ok(ToolOutcome::ok(format_results(backend, query, &hits)))
    }
}

const NO_KEY_MESSAGE: &str = "web_search needs a search API key: set BRAVE_API_KEY, \
     TAVILY_API_KEY or EXA_API_KEY (for example `modron auth set brave <key>`)";

/// Parses a `string` array argument into normalized domains. Absent means
/// empty; a non-string or blank entry is an error.
fn domain_list(args: &Value, key: &str) -> std::result::Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| format!("`{key}` must be an array of domain names"))?;
    items
        .iter()
        .map(|item| {
            let raw = item
                .as_str()
                .ok_or_else(|| format!("`{key}` must contain only strings"))?;
            let domain = normalize_domain(raw);
            if domain.is_empty() {
                return Err(format!("`{key}` contains an empty domain"));
            }
            Ok(domain)
        })
        .collect()
}

/// Lowercases and strips a leading `*.` or `.` and a trailing dot.
fn normalize_domain(raw: &str) -> String {
    raw.trim()
        .trim_start_matches("*.")
        .trim_start_matches('.')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// True when `host` is `domain` or a subdomain of it.
fn host_matches(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Applies the allow and block lists to one result URL. A URL without a
/// parseable host passes only when no filter is set.
fn domain_ok(url: &str, allowed: &[String], blocked: &[String]) -> bool {
    if allowed.is_empty() && blocked.is_empty() {
        return true;
    }
    let Some(host) = Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
    else {
        return false;
    };
    if blocked.iter().any(|d| host_matches(&host, d)) {
        return false;
    }
    allowed.is_empty() || allowed.iter().any(|d| host_matches(&host, d))
}

/// Runs one search on the chosen backend.
async fn search(
    endpoints: &Endpoints,
    backend: Backend,
    key: &str,
    query: &str,
    count: usize,
) -> std::result::Result<Vec<Hit>, String> {
    let url = endpoints.url(backend);
    let client = build_client(url).map_err(|e| format!("failed to build HTTP client: {e}"))?;
    let request = match backend {
        Backend::Brave => brave_request(&client, url, key, query, count),
        Backend::Tavily => tavily_request(&client, url, key, query, count),
        Backend::Exa => exa_request(&client, url, key, query, count),
    };
    let response = request
        .send()
        .await
        .map_err(|e| format!("{} search request failed: {e}", backend.label()))?;
    let status = response.status();
    let body = read_capped(response)
        .await
        .map_err(|e| format!("{} search response could not be read: {e}", backend.label()))?;
    if !status.is_success() {
        let detail = String::from_utf8_lossy(&body);
        let snippet: String = detail.chars().take(300).collect();
        return Err(format!(
            "{} search returned HTTP {}: {}",
            backend.label(),
            status.as_u16(),
            snippet.trim()
        ));
    }
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("{} search returned invalid JSON: {e}", backend.label()))?;
    Ok(match backend {
        Backend::Brave => parse_brave(&json),
        Backend::Tavily => parse_tavily(&json),
        Backend::Exa => parse_exa(&json),
    })
}

/// Brave: `GET {url}?q=&count=` with `X-Subscription-Token`.
fn brave_request(
    client: &Client,
    url: &str,
    key: &str,
    query: &str,
    count: usize,
) -> RequestBuilder {
    let count = count.to_string();
    client
        .get(url)
        .query(&[("q", query), ("count", count.as_str())])
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
}

/// Tavily: `POST {url}` with a JSON body and a bearer token.
fn tavily_request(
    client: &Client,
    url: &str,
    key: &str,
    query: &str,
    count: usize,
) -> RequestBuilder {
    client
        .post(url)
        .header("Authorization", format!("Bearer {key}"))
        .json(&json!({"query": query, "max_results": count}))
}

/// Exa: `POST {url}` with a JSON body and an `x-api-key` header.
fn exa_request(client: &Client, url: &str, key: &str, query: &str, count: usize) -> RequestBuilder {
    client.post(url).header("x-api-key", key).json(&json!({
        "query": query,
        "numResults": count,
        "contents": {"text": {"maxCharacters": SNIPPET_CHARS}}
    }))
}

/// Client for one call: no redirects, a total timeout, the fixed user agent.
/// A loopback endpoint (only a test mock) never goes through a proxy.
fn build_client(endpoint: &str) -> reqwest::Result<Client> {
    let mut builder = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TOTAL_TIMEOUT)
        .user_agent(USER_AGENT);
    if is_loopback(endpoint) {
        builder = builder.no_proxy();
    }
    builder.build()
}

fn is_loopback(endpoint: &str) -> bool {
    Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "[::1]" || h == "::1")
}

/// Reads a response body through a streaming cap. Over-cap bodies are an
/// error: a search response is never legitimately this large.
async fn read_capped(mut response: Response) -> std::result::Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if push_capped(&mut body, &chunk, RESPONSE_BYTES) {
            return Err(format!("response exceeds {RESPONSE_BYTES} bytes"));
        }
    }
    Ok(body)
}

/// Brave: `web.results[] { title, url, description }`. Brave marks up
/// matches with HTML, so title and description go through the HTML pass.
pub fn parse_brave(json: &Value) -> Vec<Hit> {
    results_array(json, &["web", "results"])
        .iter()
        .filter_map(|item| {
            Some(Hit {
                title: html_to_text(item.get("title")?.as_str()?),
                url: item.get("url")?.as_str()?.to_string(),
                snippet: html_to_text(
                    item.get("description")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                ),
            })
        })
        .collect()
}

/// Tavily: `results[] { title, url, content }`.
pub fn parse_tavily(json: &Value) -> Vec<Hit> {
    results_array(json, &["results"])
        .iter()
        .filter_map(|item| {
            Some(Hit {
                title: item.get("title")?.as_str()?.to_string(),
                url: item.get("url")?.as_str()?.to_string(),
                snippet: item
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// Exa: `results[] { title, url, text }`.
pub fn parse_exa(json: &Value) -> Vec<Hit> {
    results_array(json, &["results"])
        .iter()
        .filter_map(|item| {
            Some(Hit {
                title: item
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                url: item.get("url")?.as_str()?.to_string(),
                snippet: item
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect()
}

/// The array at a key path, or an empty slice when any step is missing.
fn results_array<'a>(json: &'a Value, path: &[&str]) -> &'a [Value] {
    let mut node = json;
    for key in path {
        match node.get(key) {
            Some(next) => node = next,
            None => return &[],
        }
    }
    node.as_array().map_or(&[], Vec::as_slice)
}

/// Collapses whitespace and caps a field at `max_chars` characters.
fn clip(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut cut: String = flat.chars().take(max_chars).collect();
    cut.push_str("...");
    cut
}

/// Numbered list, one result per block, capped to [`OUTPUT_BYTES`] and
/// wrapped as untrusted web content.
pub fn format_results(backend: Backend, query: &str, hits: &[Hit]) -> String {
    let header = format!(
        "web_search via {}: {} result(s) for \"{}\"",
        backend.label(),
        hits.len(),
        clip(query, 200)
    );
    if hits.is_empty() {
        return format!("{header}\n\nno results");
    }
    let mut body = String::new();
    for (i, hit) in hits.iter().enumerate() {
        if i > 0 {
            body.push('\n');
        }
        body.push_str(&format!(
            "{}. {}\n   {}\n   {}\n",
            i + 1,
            clip(&hit.title, TITLE_CHARS),
            hit.url.trim(),
            clip(&hit.snippet, SNIPPET_CHARS)
        ));
    }
    let body = reduce_output(body.trim_end().to_string(), OUTPUT_BYTES);
    format!("{header}\n\n{}", wrap_untrusted(&body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    /// A request as the mock server saw it.
    struct Seen {
        request_line: String,
        /// Header lines exactly as sent (`Name: value`).
        headers: Vec<String>,
        body: String,
    }

    impl Seen {
        /// Value of a header, matched case-insensitively by name; the value
        /// keeps its original case.
        fn header(&self, name: &str) -> Option<String> {
            self.headers.iter().find_map(|h| {
                let (n, v) = h.split_once(':')?;
                n.trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| v.trim().to_string())
            })
        }
    }

    /// Serves exactly one request with a canned response; returns the base
    /// URL (`http://127.0.0.1:PORT/path`) and the captured request.
    async fn mock(path: &str, status: u16, body: &'static str) -> (String, JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let header_end = loop {
                let n = sock.read(&mut buf).await.expect("read");
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = find(&raw, b"\r\n\r\n") {
                    break pos;
                }
                assert!(n > 0, "client closed early");
            };
            let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or("").to_string();
            let headers: Vec<String> = lines.map(str::to_string).collect();
            let want: usize = headers
                .iter()
                .find_map(|h| {
                    let (n, v) = h.split_once(':')?;
                    n.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            let mut body_bytes = raw[header_end + 4..].to_vec();
            while body_bytes.len() < want {
                let n = sock.read(&mut buf).await.expect("read body");
                if n == 0 {
                    break;
                }
                body_bytes.extend_from_slice(&buf[..n]);
            }
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(response.as_bytes()).await.expect("write");
            Seen {
                request_line,
                headers,
                body: String::from_utf8_lossy(&body_bytes).to_string(),
            }
        });
        (format!("http://{addr}{path}"), handle)
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }

    /// An address nothing listens on: a backend that must not be chosen
    /// fails the test instead of silently answering.
    const DEAD: &str = "http://127.0.0.1:1/unused";

    fn dead_endpoints() -> (String, String) {
        (DEAD.to_string(), DEAD.to_string())
    }

    fn ctx(dir: &std::path::Path, sandbox: SandboxPolicy) -> ToolCtx {
        ToolCtx {
            engine: None,
            sandbox,
            working_dir: dir.to_path_buf(),
            session_id: None,
        }
    }

    fn keys(brave: Option<&str>, tavily: Option<&str>, exa: Option<&str>) -> Keys {
        Keys {
            brave: brave.map(str::to_string),
            tavily: tavily.map(str::to_string),
            exa: exa.map(str::to_string),
        }
    }

    const BRAVE_BODY: &str = r#"{"web":{"results":[
        {"title":"<strong>Rust</strong> language","url":"https://www.rust-lang.org/","description":"A <strong>fast</strong> &amp; safe language."},
        {"title":"Docs","url":"https://doc.rust-lang.org/std/","description":"Std docs."}
    ]}}"#;

    const TAVILY_BODY: &str = r#"{"results":[
        {"title":"Tavily hit","url":"https://example.com/a","content":"tavily body"}
    ]}"#;

    const EXA_BODY: &str = r#"{"results":[
        {"title":"Exa hit","url":"https://example.org/b","text":"exa text"}
    ]}"#;

    #[test]
    fn spec_declares_query_and_optional_filters() {
        let spec = WebSearchTool::default().spec();
        assert_eq!(spec.name, "web_search");
        let required = spec.parameters["required"].as_array().expect("required");
        assert_eq!(required, &vec![json!("query")]);
        for key in ["max_results", "allowed_domains", "blocked_domains"] {
            assert!(spec.parameters["properties"].get(key).is_some(), "{key}");
        }
    }

    #[test]
    fn domains_are_normalized_and_matched_with_subdomains() {
        assert_eq!(normalize_domain(" *.Example.COM. "), "example.com");
        assert_eq!(normalize_domain(".example.com"), "example.com");
        assert!(host_matches("docs.example.com", "example.com"));
        assert!(host_matches("example.com", "example.com"));
        assert!(!host_matches("notexample.com", "example.com"));
        assert!(!host_matches("example.com.evil.net", "example.com"));
    }

    #[test]
    fn domain_filters_keep_allowed_drop_blocked_and_reject_unparseable() {
        let allowed = vec!["example.com".to_string()];
        let blocked = vec!["bad.example.com".to_string()];
        assert!(domain_ok("https://example.com/x", &allowed, &blocked));
        assert!(domain_ok("https://a.example.com/x", &allowed, &blocked));
        assert!(!domain_ok("https://bad.example.com/x", &allowed, &blocked));
        assert!(!domain_ok("https://other.net/", &allowed, &blocked));
        assert!(!domain_ok("not a url", &allowed, &[]));
        assert!(domain_ok("https://other.net/", &[], &[]));
        assert!(!domain_ok("https://bad.example.com/", &[], &blocked));
    }

    #[test]
    fn domain_list_rejects_bad_shapes() {
        assert!(domain_list(&json!({}), "allowed_domains")
            .unwrap()
            .is_empty());
        assert!(domain_list(&json!({"allowed_domains": "x.com"}), "allowed_domains").is_err());
        assert!(domain_list(&json!({"allowed_domains": [1]}), "allowed_domains").is_err());
        assert!(domain_list(&json!({"allowed_domains": ["  "]}), "allowed_domains").is_err());
    }

    #[test]
    fn selection_follows_brave_tavily_exa_order() {
        assert_eq!(keys(None, None, None).select(), None);
        assert_eq!(
            keys(None, None, Some("e")).select(),
            Some((Backend::Exa, "e"))
        );
        assert_eq!(
            keys(None, Some("t"), Some("e")).select(),
            Some((Backend::Tavily, "t"))
        );
        assert_eq!(
            keys(Some("b"), Some("t"), Some("e")).select(),
            Some((Backend::Brave, "b"))
        );
    }

    #[test]
    fn parse_brave_strips_markup_and_reads_web_results() {
        let hits = parse_brave(&serde_json::from_str(BRAVE_BODY).unwrap());
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust language");
        assert_eq!(hits[0].snippet, "A fast & safe language.");
        assert_eq!(hits[1].url, "https://doc.rust-lang.org/std/");
        assert!(parse_brave(&json!({"web": {}})).is_empty());
    }

    #[test]
    fn parse_tavily_and_exa_read_their_shapes() {
        let t = parse_tavily(&serde_json::from_str(TAVILY_BODY).unwrap());
        assert_eq!(t[0].snippet, "tavily body");
        let e = parse_exa(&serde_json::from_str(EXA_BODY).unwrap());
        assert_eq!(e[0].title, "Exa hit");
        assert_eq!(e[0].snippet, "exa text");
        // A result without a URL is dropped, not shown half-formed.
        assert!(parse_exa(&json!({"results": [{"title": "x"}]})).is_empty());
    }

    #[test]
    fn format_numbers_results_and_marks_untrusted() {
        let hits = vec![Hit {
            title: "T".into(),
            url: "https://e.com/".into(),
            snippet: "S".into(),
        }];
        let text = format_results(Backend::Tavily, "rust", &hits);
        assert!(text.starts_with("web_search via Tavily: 1 result(s) for \"rust\""));
        assert!(text.contains("1. T\n   https://e.com/\n   S"));
        assert!(text.contains("--- untrusted web content ---"));
        assert!(format_results(Backend::Exa, "q", &[]).ends_with("no results"));
    }

    #[test]
    fn format_caps_total_output_and_snippet_length() {
        let long = "word ".repeat(400);
        let hits: Vec<Hit> = (0..MAX_RESULTS)
            .map(|i| Hit {
                title: long.clone(),
                url: format!("https://e.com/{i}"),
                snippet: long.clone(),
            })
            .collect();
        let text = format_results(Backend::Brave, "q", &hits);
        assert!(text.len() <= OUTPUT_BYTES + 400, "len {}", text.len());
        let one = format_results(Backend::Brave, "q", &hits[..1]);
        assert!(one.len() < 1000, "one result is compact: {}", one.len());
    }

    #[tokio::test]
    async fn no_key_names_all_three_variables_and_makes_no_request() {
        let dir = tempdir().unwrap();
        let (t, e) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: DEAD.into(),
                tavily: t,
                exa: e,
            },
            keys(None, None, None),
        );
        let out = tool
            .execute(
                json!({"query": "rust"}),
                &ctx(dir.path(), SandboxPolicy::Off),
            )
            .await
            .expect("execute");
        assert!(out.is_error, "{out:?}");
        for var in ["BRAVE_API_KEY", "TAVILY_API_KEY", "EXA_API_KEY"] {
            assert!(out.content.contains(var), "{var} named: {out:?}");
        }
    }

    #[tokio::test]
    async fn brave_is_chosen_when_all_keys_are_set() {
        let dir = tempdir().unwrap();
        let (url, seen) = mock("/res/v1/web/search", 200, BRAVE_BODY).await;
        let (t, e) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: url,
                tavily: t,
                exa: e,
            },
            keys(Some("brave-key"), Some("tavily-key"), Some("exa-key")),
        );
        let out = tool
            .execute(
                json!({"query": "rust lang"}),
                &ctx(dir.path(), SandboxPolicy::Off),
            )
            .await
            .expect("execute");
        assert!(!out.is_error, "{out:?}");
        assert!(
            out.content.contains("web_search via Brave: 2 result(s)"),
            "{out:?}"
        );
        assert!(out.content.contains("1. Rust language"), "{out:?}");
        assert!(
            out.content.contains("https://doc.rust-lang.org/std/"),
            "{out:?}"
        );
        let seen = seen.await.expect("mock");
        assert!(
            seen.request_line.starts_with("GET /res/v1/web/search?"),
            "{}",
            seen.request_line
        );
        assert!(
            seen.request_line.contains("q=rust"),
            "{}",
            seen.request_line
        );
        assert!(
            seen.request_line.contains("count=5"),
            "{}",
            seen.request_line
        );
        assert_eq!(
            seen.header("x-subscription-token").as_deref(),
            Some("brave-key")
        );
        assert!(!out.content.contains("brave-key"), "key never echoed");
    }

    #[tokio::test]
    async fn tavily_request_shape_and_parse() {
        let dir = tempdir().unwrap();
        let (url, seen) = mock("/search", 200, TAVILY_BODY).await;
        let (b, e) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: b,
                tavily: url,
                exa: e,
            },
            keys(None, Some("tv-key"), None),
        );
        let out = tool
            .execute(
                json!({"query": "q1", "max_results": 3}),
                &ctx(dir.path(), SandboxPolicy::Off),
            )
            .await
            .expect("execute");
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("via Tavily"), "{out:?}");
        assert!(out.content.contains("Tavily hit"), "{out:?}");
        let seen = seen.await.expect("mock");
        assert!(
            seen.request_line.starts_with("POST /search "),
            "{}",
            seen.request_line
        );
        assert_eq!(
            seen.header("authorization").as_deref(),
            Some("Bearer tv-key")
        );
        let body: Value = serde_json::from_str(&seen.body).expect("json body");
        assert_eq!(body["query"], "q1");
        assert_eq!(body["max_results"], 3);
        assert!(!out.content.contains("tv-key"));
    }

    #[tokio::test]
    async fn exa_request_shape_and_parse() {
        let dir = tempdir().unwrap();
        let (url, seen) = mock("/search", 200, EXA_BODY).await;
        let (b, t) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: b,
                tavily: t,
                exa: url,
            },
            keys(None, None, Some("ex-key")),
        );
        let out = tool
            .execute(json!({"query": "q2"}), &ctx(dir.path(), SandboxPolicy::Off))
            .await
            .expect("execute");
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("1. Exa hit"), "{out:?}");
        assert!(out.content.contains("exa text"), "{out:?}");
        let seen = seen.await.expect("mock");
        assert!(
            seen.request_line.starts_with("POST /search "),
            "{}",
            seen.request_line
        );
        assert_eq!(seen.header("x-api-key").as_deref(), Some("ex-key"));
        let body: Value = serde_json::from_str(&seen.body).expect("json body");
        assert_eq!(body["query"], "q2");
        assert_eq!(body["numResults"], 5);
    }

    #[tokio::test]
    async fn max_results_defaults_to_five_and_caps_at_ten() {
        let dir = tempdir().unwrap();
        let (url, seen) = mock("/res/v1/web/search", 200, r#"{"web":{"results":[]}}"#).await;
        let (t, e) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: url,
                tavily: t,
                exa: e,
            },
            keys(Some("k"), None, None),
        );
        let out = tool
            .execute(
                json!({"query": "x", "max_results": 99}),
                &ctx(dir.path(), SandboxPolicy::Off),
            )
            .await
            .expect("execute");
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("no results"), "{out:?}");
        let seen = seen.await.expect("mock");
        assert!(
            seen.request_line.contains("count=10"),
            "{}",
            seen.request_line
        );
    }

    #[tokio::test]
    async fn domain_filters_are_applied_and_request_the_maximum() {
        let dir = tempdir().unwrap();
        let body = r#"{"results":[
            {"title":"A","url":"https://docs.example.com/a","content":"a"},
            {"title":"B","url":"https://notexample.com/b","content":"b"},
            {"title":"C","url":"https://bad.example.com/c","content":"c"},
            {"title":"D","url":"https://example.com/d","content":"d"},
            {"title":"E","url":"https://example.com/e","content":"e"}
        ]}"#;
        let (url, seen) = mock("/search", 200, body).await;
        let (b, e) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: b,
                tavily: url,
                exa: e,
            },
            keys(None, Some("k"), None),
        );
        let out = tool
            .execute(
                json!({
                    "query": "x",
                    "max_results": 2,
                    "allowed_domains": ["example.com"],
                    "blocked_domains": ["bad.example.com"]
                }),
                &ctx(dir.path(), SandboxPolicy::Off),
            )
            .await
            .expect("execute");
        assert!(!out.is_error, "{out:?}");
        assert!(out.content.contains("2 result(s)"), "{out:?}");
        assert!(out.content.contains("docs.example.com"), "{out:?}");
        assert!(out.content.contains("https://example.com/d"), "{out:?}");
        assert!(!out.content.contains("notexample.com"), "{out:?}");
        assert!(!out.content.contains("bad.example.com"), "{out:?}");
        assert!(
            !out.content.contains("https://example.com/e"),
            "cap of 2 applied: {out:?}"
        );
        let seen = seen.await.expect("mock");
        let body: Value = serde_json::from_str(&seen.body).expect("json body");
        assert_eq!(body["max_results"], 10, "filters ask for the maximum");
    }

    #[tokio::test]
    async fn http_error_is_a_tool_error_without_the_key() {
        let dir = tempdir().unwrap();
        let (url, seen) = mock("/res/v1/web/search", 401, r#"{"error":"bad token"}"#).await;
        let (t, e) = dead_endpoints();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints {
                brave: url,
                tavily: t,
                exa: e,
            },
            keys(Some("secret-brave"), None, None),
        );
        let out = tool
            .execute(json!({"query": "x"}), &ctx(dir.path(), SandboxPolicy::Off))
            .await
            .expect("execute");
        assert!(out.is_error, "{out:?}");
        assert!(out.content.contains("HTTP 401"), "{out:?}");
        assert!(out.content.contains("bad token"), "{out:?}");
        assert!(!out.content.contains("secret-brave"), "{out:?}");
        seen.await.expect("mock");
    }

    #[tokio::test]
    async fn invalid_and_missing_arguments_are_refused() {
        let dir = tempdir().unwrap();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints::default(),
            keys(Some("k"), None, None),
        );
        let c = ctx(dir.path(), SandboxPolicy::Off);
        let missing = tool.execute(json!({}), &c).await.expect("execute");
        assert!(missing.is_error, "{missing:?}");
        let blank = tool
            .execute(json!({"query": "   "}), &c)
            .await
            .expect("execute");
        assert!(
            blank.is_error && blank.content.contains("query"),
            "{blank:?}"
        );
        let bad_domains = tool
            .execute(json!({"query": "x", "allowed_domains": "example.com"}), &c)
            .await
            .expect("execute");
        assert!(bad_domains.is_error, "{bad_domains:?}");
    }

    #[tokio::test]
    async fn sandboxed_runs_refuse_network() {
        let dir = tempdir().unwrap();
        let tool = WebSearchTool::with_endpoints_and_keys(
            Endpoints::default(),
            keys(Some("k"), None, None),
        );
        let out = tool
            .execute(
                json!({"query": "x"}),
                &ctx(dir.path(), SandboxPolicy::Bubblewrap),
            )
            .await
            .expect("execute");
        assert!(out.is_error, "{out:?}");
        assert!(
            out.content.contains("network is disabled by the sandbox"),
            "{out:?}"
        );
    }
}
