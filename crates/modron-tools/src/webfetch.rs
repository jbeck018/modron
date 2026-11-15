//! `webfetch` tool - fetch one public http(s) URL and return its text.
//!
//! The URL is model-controlled, so this tool is an SSRF surface:
//! - every host is vetted against loopback, private, link-local, CGNAT and
//!   other non-public ranges (IPv4, IPv6 and IPv4-mapped/compatible forms);
//! - the vetted address is pinned on the client (resolve override), so a
//!   second DNS answer cannot change the destination;
//! - the connection never uses a proxy (a proxy would resolve the host
//!   itself and bypass the pin);
//! - redirects are never followed: a 3xx is reported with its `Location`
//!   so the model must issue a new, re-vetted call;
//! - the body is read through a streaming cap, never buffered whole;
//! - the tool refuses to run while the sandbox policy is not `Off`.

use crate::common::{arg_str, internal_tool_error, opt_u64, tool_err};
use crate::reduce::reduce_output;
use crate::validate::validate_args;
use async_trait::async_trait;
use modron_core::{Result, SandboxPolicy, Tool, ToolCtx, ToolOutcome, ToolSpec};
use reqwest::header::{CONTENT_TYPE, LOCATION};
use reqwest::{Client, Response, Url};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// Default body cap in bytes.
pub const DEFAULT_MAX_BYTES: usize = 40_000;
/// Hard ceiling for `max_bytes`.
pub const HARD_MAX_BYTES: usize = 200_000;
/// Total request budget, covering connect, headers and body.
const TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
/// Fixed, generic User-Agent; no cookies are stored.
const USER_AGENT: &str = "modron-webfetch";
/// Byte cap for error messages.
const ERROR_BYTES: usize = 4096;
const UNTRUSTED_OPEN: &str = "--- untrusted web content ---";
const UNTRUSTED_CLOSE: &str = "--- end untrusted web content ---";

/// Fetches one public `http(s)` URL. On-demand tool (reached via
/// `tool_search`); its output is marked as untrusted web content.
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "webfetch".into(),
            description: "Fetch a public http(s) URL and return its text (HTML is converted). \
                          No redirects are followed; private and local addresses are refused."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http:// or https:// URL to fetch."},
                    "max_bytes": {"type": "integer", "description": "Max body bytes to read (default 40000, max 200000)."}
                },
                "required": ["url"]
            }),
        }
    }

    async fn execute(&self, args: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        const NAME: &str = "webfetch";
        let spec = self.spec();
        if let Err(message) = validate_args(&spec, &args) {
            return Ok(tool_err(
                NAME,
                format!("invalid arguments: {message}"),
                ERROR_BYTES,
            ));
        }
        let Some(raw_url) = arg_str(&args, "url") else {
            return Ok(tool_err(NAME, "missing `url`", ERROR_BYTES));
        };
        let max_bytes = opt_u64(&args, "max_bytes")
            .unwrap_or(DEFAULT_MAX_BYTES as u64)
            .clamp(1, HARD_MAX_BYTES as u64) as usize;
        if !matches!(ctx.sandbox, SandboxPolicy::Off) {
            return Ok(tool_err(
                NAME,
                "network is disabled by the sandbox",
                ERROR_BYTES,
            ));
        }
        let url = match validate_url(raw_url) {
            Ok(url) => url,
            Err(message) => return Ok(tool_err(NAME, message, ERROR_BYTES)),
        };
        let pin = match vet_host(&url).await {
            Ok(pin) => pin,
            Err(message) => return Ok(tool_err(NAME, message, ERROR_BYTES)),
        };
        let client = build_client(pin)
            .map_err(|e| internal_tool_error(NAME, format!("failed to build HTTP client: {e}")))?;
        let mut response = match client.get(url.clone()).send().await {
            Ok(response) => response,
            Err(e) => return Ok(tool_err(NAME, format!("request failed: {e}"), ERROR_BYTES)),
        };

        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let ct_label = content_type
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        let media = media_type(content_type.as_deref().unwrap_or(""));
        let final_url = url.as_str();

        if response.status().is_redirection() {
            let location = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("(none)")
                .to_owned();
            let line = status_line(status, final_url, &ct_label, "0", false);
            let note = wrap_untrusted(&format!("redirect not followed; Location: {location}"));
            return Ok(ToolOutcome::ok(format!("{line}\n\n{note}")));
        }

        if !is_textual(&media) {
            let length = response
                .content_length()
                .map_or_else(|| "unknown".to_string(), |n| n.to_string());
            let line = status_line(status, final_url, &ct_label, &length, false);
            return Ok(ToolOutcome::ok(format!(
                "{line}\n\nbinary content not shown"
            )));
        }

        let (body, truncated) = match read_body(&mut response, max_bytes).await {
            Ok(read) => read,
            Err(e) => {
                return Ok(tool_err(
                    NAME,
                    format!("reading body failed: {e}"),
                    ERROR_BYTES,
                ))
            }
        };
        let text = String::from_utf8_lossy(&body);
        let converted = if is_html(&media) {
            html_to_text(&text)
        } else {
            text.into_owned()
        };
        let body_text = reduce_output(converted, max_bytes);
        let line = status_line(
            status,
            final_url,
            &ct_label,
            &body.len().to_string(),
            truncated,
        );
        Ok(ToolOutcome::ok(format!(
            "{line}\n\n{}",
            wrap_untrusted(&body_text)
        )))
    }
}

/// Parses and screens a model-supplied URL without touching the network:
/// scheme, embedded credentials, local host names and literal IP addresses.
/// DNS-based checks happen in [`vet_host`].
pub(crate) fn validate_url(raw: &str) -> std::result::Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(format!(
                "unsupported scheme `{other}`: only http:// and https:// URLs are allowed"
            ))
        }
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URLs with embedded credentials (user:pass@) are not allowed".to_string());
    }
    if let Some(ip) = literal_ip(&url) {
        if is_blocked_ip(ip) {
            return Err(format!("refused: {ip} is a private or local address"));
        }
        return Ok(url);
    }
    check_host_name(url.host_str().unwrap_or_default())?;
    Ok(url)
}

/// The IP address a URL host names literally, if it is not a domain name.
/// The url crate normalizes IPv4 spellings such as `0x7f.1` first.
fn literal_ip(url: &Url) -> Option<IpAddr> {
    let host = url.host_str()?;
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok()
}

/// Refuses `localhost`, `*.localhost` and `*.internal` names, and empty hosts.
fn check_host_name(host: &str) -> std::result::Result<(), String> {
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() {
        return Err("URL has no host".to_string());
    }
    if name == "localhost" || name.ends_with(".localhost") || name.ends_with(".internal") {
        return Err(format!("refused: local host name `{host}`"));
    }
    Ok(())
}

/// Resolves a domain host and refuses it if ANY answer is non-public.
/// Returns the host and the first vetted address to pin, or `None` for a
/// literal IP (nothing to resolve; [`validate_url`] already vetted it).
async fn vet_host(url: &Url) -> std::result::Result<Option<(String, SocketAddr)>, String> {
    if let Some(ip) = literal_ip(url) {
        return if is_blocked_ip(ip) {
            Err(format!("refused: {ip} is a private or local address"))
        } else {
            Ok(None)
        };
    }
    let host = url.host_str().unwrap_or_default().to_string();
    check_host_name(&host)?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| format!("DNS lookup failed for `{host}`: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no address found for `{host}`"));
    }
    if let Some(bad) = addrs.iter().find(|addr| is_blocked_ip(addr.ip())) {
        return Err(format!(
            "refused: `{host}` resolves to a private or local address ({})",
            bad.ip()
        ));
    }
    Ok(Some((host, addrs[0])))
}

/// Builds the one-shot client: no proxy, no redirects, no cookies, the
/// total timeout, and the vetted address pinned for the domain.
fn build_client(pin: Option<(String, SocketAddr)>) -> reqwest::Result<Client> {
    let mut builder = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TOTAL_TIMEOUT)
        .user_agent(USER_AGENT);
    if let Some((host, addr)) = pin {
        builder = builder.resolve(&host, addr);
    }
    builder.build()
}

/// True when the address is not a public unicast address. IPv4-mapped and
/// IPv4-compatible IPv6 addresses are judged by their embedded IPv4 address.
pub(crate) fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4() {
            Some(v4) => is_blocked_v4(v4),
            None => is_blocked_v6(v6),
        },
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    ip.is_loopback()                    // 127/8
        || ip.is_private()              // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()           // 169.254/16 (includes the metadata address)
        || (a == 100 && (b & 0xc0) == 64) // CGNAT 100.64/10
        || a == 0                       // "this network", includes unspecified
        || a >= 224 // multicast 224/4, reserved 240/4, broadcast
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback()                    // ::1
        || ip.is_unspecified()          // ::
        || ip.is_multicast()            // ff00::/8
        || (first & 0xffc0) == 0xfe80   // link-local fe80::/10
        || (first & 0xfe00) == 0xfc00 // unique local fc00::/7
}

/// Reads the body through a streaming cap; stops at `max_bytes`.
/// Returns the bytes read and whether the body was cut.
async fn read_body(response: &mut Response, max_bytes: usize) -> reqwest::Result<(Vec<u8>, bool)> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if push_capped(&mut body, &chunk, max_bytes) {
            return Ok((body, true));
        }
    }
    Ok((body, false))
}

/// Appends as much of `chunk` as fits under `max_bytes`. Returns true when
/// bytes had to be dropped, i.e. the body is truncated.
pub(crate) fn push_capped(body: &mut Vec<u8>, chunk: &[u8], max_bytes: usize) -> bool {
    let room = max_bytes.saturating_sub(body.len());
    if chunk.len() > room {
        body.extend_from_slice(&chunk[..room]);
        return true;
    }
    body.extend_from_slice(chunk);
    false
}

/// `type/subtype` from a Content-Type value, lowercased, parameters dropped.
fn media_type(content_type: &str) -> String {
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Content types shown as text: `text/*`, JSON, XML (and `+json` / `+xml`).
pub(crate) fn is_textual(media: &str) -> bool {
    media.starts_with("text/")
        || media == "application/json"
        || media == "application/xml"
        || media.ends_with("+json")
        || media.ends_with("+xml")
}

fn is_html(media: &str) -> bool {
    media == "text/html" || media == "application/xhtml+xml"
}

pub(crate) fn status_line(
    status: u16,
    url: &str,
    content_type: &str,
    bytes: &str,
    truncated: bool,
) -> String {
    let note = if truncated { ", truncated" } else { "" };
    format!("HTTP {status} {url} ({content_type}, {bytes} bytes{note})")
}

pub(crate) fn wrap_untrusted(body: &str) -> String {
    format!("{UNTRUSTED_OPEN}\n{body}\n{UNTRUSTED_CLOSE}")
}

/// Small hand-written HTML-to-text pass. Drops script/style/noscript/svg/head
/// blocks and comments; turns `<br>` and block-level tags into newlines;
/// renders `<a href="X">t</a>` as `t (X)`; strips other tags; decodes
/// entities; collapses whitespace and blank-line runs.
pub(crate) fn html_to_text(html: &str) -> String {
    const SKIPPED: [&str; 5] = ["script", "style", "noscript", "svg", "head"];
    // ASCII lowercasing keeps byte offsets identical to `html`.
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut pending_href: Option<String> = None;
    let mut pos = 0usize;
    while pos < html.len() {
        let Some(rel) = html[pos..].find('<') else {
            push_text(&mut out, &html[pos..]);
            break;
        };
        let lt = pos + rel;
        push_text(&mut out, &html[pos..lt]);
        if lower[lt..].starts_with("<!--") {
            pos = match lower[lt + 4..].find("-->") {
                Some(end) => lt + 4 + end + 3,
                None => html.len(),
            };
            continue;
        }
        let Some(rel_gt) = html[lt..].find('>') else {
            // Unterminated tag: keep the remainder as text.
            push_text(&mut out, &html[lt..]);
            break;
        };
        let gt = lt + rel_gt;
        let tag = &lower[lt + 1..gt];
        let is_close = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        pos = gt + 1;

        if !is_close && SKIPPED.contains(&name.as_str()) {
            let close = format!("</{name}");
            pos = match lower[pos..].find(&close) {
                Some(found) => {
                    let close_at = pos + found;
                    match lower[close_at..].find('>') {
                        Some(end) => close_at + end + 1,
                        None => html.len(),
                    }
                }
                None => html.len(),
            };
            continue;
        }

        match name.as_str() {
            "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                push_newline(&mut out)
            }
            "a" if is_close => {
                if let Some(href) = pending_href.take() {
                    if !href.is_empty() && !href.starts_with('#') {
                        out.push_str(" (");
                        out.push_str(&href);
                        out.push(')');
                    }
                }
            }
            "a" => pending_href = attr_value(&html[lt + 1..gt], "href"),
            _ => {}
        }
    }
    tidy(&out)
}

/// Appends text, decoding entities and collapsing ASCII whitespace runs to
/// one space (never leading, never doubled).
fn push_text(out: &mut String, raw: &str) {
    for c in decode_entities(raw).chars() {
        if c.is_ascii_whitespace() {
            if !matches!(out.chars().last(), None | Some(' ') | Some('\n')) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
}

/// Ends the current line (trailing spaces dropped, no empty-line stacking here).
fn push_newline(out: &mut String) {
    while out.ends_with(' ') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

/// Trims each line and collapses blank-line runs to one blank line.
fn tidy(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() && lines.last().is_some_and(|last| last.is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

/// Value of attribute `attr` in a raw tag (`<a href="x">`), entity-decoded.
/// Matching is case-insensitive and requires a whitespace boundary before the
/// name, so `data-href` does not match.
fn attr_value(raw: &str, attr: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find(attr) {
        let start = from + rel;
        let after_name = start + attr.len();
        from = after_name;
        let boundary = start == 0 || lower.as_bytes()[start - 1].is_ascii_whitespace();
        if !boundary {
            continue;
        }
        let rest = &raw[after_name..];
        let Some(value_part) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value_part.trim_start();
        let raw_value = match value.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let inner = &value[1..];
                &inner[..inner.find(quote).unwrap_or(inner.len())]
            }
            _ => {
                &value[..value
                    .find(|c: char| c.is_ascii_whitespace())
                    .unwrap_or(value.len())]
            }
        };
        return Some(decode_entities(raw_value));
    }
    None
}

/// Decodes `&amp; &lt; &gt; &quot; &apos; &nbsp;` and numeric references in
/// one pass (so `&amp;lt;` yields `&lt;`, not `<`). Unknown references are
/// left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        match decode_one(after) {
            Some((ch, used)) => {
                out.push(ch);
                rest = &after[used..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decodes one reference body (the text after `&`), returning the character
/// and the number of bytes consumed including the `;`.
fn decode_one(s: &str) -> Option<(char, usize)> {
    // Bound the search: a reference name is short.
    let window_end = (0..=s.len().min(12))
        .rev()
        .find(|&k| s.is_char_boundary(k))?;
    let semi = s[..window_end].find(';')?;
    if semi == 0 {
        return None;
    }
    let name = &s[..semi];
    let ch = if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse::<u32>().ok()?,
        };
        char::from_u32(code)?
    } else {
        match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => ' ',
            _ => return None,
        }
    };
    Some((ch, semi + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ctx_with(dir: &std::path::Path, sandbox: SandboxPolicy) -> ToolCtx {
        ToolCtx {
            engine: None,
            sandbox,
            working_dir: dir.to_path_buf(),
            session_id: None,
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("test address")
    }

    #[test]
    fn classifier_blocks_non_public_ipv4() {
        for s in [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "224.0.0.1",
            "239.255.255.255",
            "240.0.0.1",
            "255.255.255.255",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} must be blocked");
        }
    }

    #[test]
    fn classifier_allows_public_ipv4_near_the_edges() {
        for s in [
            "8.8.8.8",
            "1.1.1.1",
            "11.0.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "169.253.0.1",
            "192.169.0.1",
            "223.255.255.255",
        ] {
            assert!(!is_blocked_ip(ip(s)), "{s} must be allowed");
        }
    }

    #[test]
    fn classifier_blocks_non_public_ipv6() {
        for s in [
            "::",
            "::1",
            "fe80::1",
            "febf::1",
            "fc00::1",
            "fd12:3456::1",
            "ff02::1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} must be blocked");
        }
    }

    #[test]
    fn classifier_allows_public_ipv6() {
        for s in ["2001:4860:4860::8888", "2606:4700::1111", "2a00:1450::1"] {
            assert!(!is_blocked_ip(ip(s)), "{s} must be allowed");
        }
    }

    #[test]
    fn classifier_judges_mapped_and_compatible_addresses_by_embedded_ipv4() {
        for s in [
            "::ffff:127.0.0.1",
            "::ffff:10.1.2.3",
            "::ffff:169.254.169.254",
            "::ffff:192.168.0.1",
            "::ffff:100.64.0.1",
            "::10.0.0.1",
            "::127.0.0.1",
        ] {
            assert!(is_blocked_ip(ip(s)), "{s} must be blocked");
        }
        for s in ["::ffff:8.8.8.8", "::8.8.8.8"] {
            assert!(!is_blocked_ip(ip(s)), "{s} must be allowed");
        }
    }

    #[test]
    fn validate_url_accepts_public_http_and_https() {
        for s in [
            "https://example.com/a?b=1",
            "http://example.org",
            "https://8.8.8.8/",
            "  https://example.com/trimmed  ",
        ] {
            assert!(validate_url(s).is_ok(), "{s} must be accepted");
        }
    }

    #[test]
    fn validate_url_rejects_other_schemes() {
        for s in [
            "ftp://example.com/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,hi",
        ] {
            let err = validate_url(s).expect_err(s);
            assert!(err.contains("unsupported scheme"), "{s}: {err}");
        }
    }

    #[test]
    fn validate_url_rejects_embedded_credentials() {
        for s in [
            "https://user:pw@example.com/",
            "https://user@example.com/",
            "https://:pw@example.com/",
        ] {
            let err = validate_url(s).expect_err(s);
            assert!(err.contains("credentials"), "{s}: {err}");
        }
    }

    #[test]
    fn validate_url_rejects_local_names_and_addresses() {
        for s in [
            "http://localhost/",
            "http://LOCALHOST:8080/",
            "http://localhost./",
            "http://api.localhost/",
            "https://metadata.internal/",
            "http://127.0.0.1/",
            "http://0x7f.0.0.1/",
            "http://[::1]/",
            "http://169.254.169.254/latest/meta-data",
            "http://10.0.0.1/",
            "http://[::ffff:10.0.0.1]/",
            "http://0.0.0.0/",
        ] {
            assert!(validate_url(s).is_err(), "{s} must be refused");
        }
    }

    #[test]
    fn status_line_format_with_and_without_truncation() {
        assert_eq!(
            status_line(
                200,
                "https://example.com/",
                "text/html; charset=utf-8",
                "1234",
                false
            ),
            "HTTP 200 https://example.com/ (text/html; charset=utf-8, 1234 bytes)"
        );
        assert_eq!(
            status_line(404, "https://example.com/x", "unknown", "0", true),
            "HTTP 404 https://example.com/x (unknown, 0 bytes, truncated)"
        );
    }

    #[test]
    fn untrusted_marker_wraps_body() {
        assert_eq!(
            wrap_untrusted("hi"),
            "--- untrusted web content ---\nhi\n--- end untrusted web content ---"
        );
    }

    #[test]
    fn content_type_classification() {
        assert!(is_textual(&media_type("text/html; charset=utf-8")));
        assert!(is_textual(&media_type("application/json")));
        assert!(is_textual(&media_type("application/xml")));
        assert!(is_textual(&media_type("application/vnd.api+json")));
        assert!(is_textual(&media_type("application/atom+xml")));
        assert!(!is_textual(&media_type("image/png")));
        assert!(!is_textual(&media_type("application/octet-stream")));
        assert!(!is_textual(&media_type("")));
        assert!(is_html(&media_type("TEXT/HTML")));
        assert!(!is_html(&media_type("application/json")));
    }

    #[test]
    fn html_to_text_drops_scripts_head_and_comments_and_keeps_links() {
        let html = "<html><head><title>T</title><style>p{}</style></head><body>\
                    <script>var x=\"<p>\";</script><h1>Title</h1>\
                    <p>Hello &amp; <a href=\"https://x.test/a\">link</a> &lt;b&gt; \
                    &quot;q&quot; &#39;s&#39; &#x41;&nbsp;z</p><!-- hidden -->\
                    <div>one</div>\n\n\n\n<br>two</body></html>";
        assert_eq!(
            html_to_text(html),
            "Title\nHello & link (https://x.test/a) <b> \"q\" 's' A z\none\ntwo"
        );
    }

    #[test]
    fn html_to_text_handles_unterminated_and_anchorless_markup() {
        assert_eq!(html_to_text("<a name=\"x\">plain</a>"), "plain");
        assert_eq!(html_to_text("<noscript>hidden"), "");
        assert_eq!(html_to_text("a < b"), "a < b");
        assert_eq!(
            html_to_text("<a href='#top'>up</a> and <a data-href=\"/no\">x</a>"),
            "up and x"
        );
    }

    #[test]
    fn entities_decode_once_and_unknown_refs_stay() {
        assert_eq!(decode_entities("&amp;lt;"), "&lt;");
        assert_eq!(
            decode_entities("AT&T &foo; &#128512;"),
            "AT&T &foo; \u{1f600}"
        );
        assert_eq!(decode_entities("&#xZZ; &"), "&#xZZ; &");
    }

    #[test]
    fn push_capped_stops_at_the_limit() {
        let mut body = Vec::new();
        assert!(!push_capped(&mut body, b"abcd", 10));
        assert!(!push_capped(&mut body, b"efghij", 10));
        assert_eq!(body.len(), 10);
        assert!(push_capped(&mut body, b"more", 10));
        assert_eq!(body, b"abcdefghij");
    }

    #[test]
    fn synthetic_stream_is_capped_not_buffered_whole() {
        // 1000 chunks of 1 KiB stand in for an unbounded response.
        let chunks: Vec<Vec<u8>> = (0..1000).map(|_| vec![b'a'; 1024]).collect();
        let mut body = Vec::new();
        let mut truncated = false;
        for chunk in &chunks {
            if push_capped(&mut body, chunk, DEFAULT_MAX_BYTES) {
                truncated = true;
                break;
            }
        }
        assert_eq!(body.len(), DEFAULT_MAX_BYTES);
        assert!(truncated);
    }

    #[tokio::test]
    async fn loopback_listener_is_refused_without_connecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let dir = tempdir().expect("tempdir");
        let ctx = ctx_with(dir.path(), SandboxPolicy::Off);
        let outcome = WebFetchTool
            .execute(json!({"url": format!("http://127.0.0.1:{port}/")}), &ctx)
            .await
            .expect("execute");
        assert!(outcome.is_error, "{outcome:?}");
        assert!(outcome.content.contains("refused"), "{outcome:?}");
        let accepted = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(accepted.is_err(), "no connection may reach the listener");
    }

    #[tokio::test]
    async fn sandboxed_runs_refuse_network() {
        let dir = tempdir().expect("tempdir");
        let ctx = ctx_with(dir.path(), SandboxPolicy::Bubblewrap);
        let outcome = WebFetchTool
            .execute(json!({"url": "https://example.com/"}), &ctx)
            .await
            .expect("execute");
        assert!(outcome.is_error, "{outcome:?}");
        assert!(
            outcome
                .content
                .contains("network is disabled by the sandbox"),
            "{outcome:?}"
        );
    }
}
