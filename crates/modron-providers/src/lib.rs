//! modron-providers — `ModelProvider` implementations for Modron.
//!
//! LEDGER items: P1 (Anthropic Messages), P2 (OpenAI-compatible chat
//! completions), P3 (factory from env/config), P4 (retry/backoff with an
//! error taxonomy mapped onto [`modron_core::Error`]), S1 (SSE streaming as
//! inherent `turn_streaming` methods on the concrete providers — no trait
//! change; see [`stream`]).
//!
//! Tests never touch the network: request shapes are asserted through the
//! pure `build_body` functions and responses through recorded-JSON fixtures.
//! Streaming (S1) additionally runs against a scripted local HTTP server
//! serving SSE fixtures (see `tests/streaming_fixtures.rs`).

pub mod anthropic;
pub mod factory;
pub mod openai_compat;
pub mod retry;
pub mod routers;
pub use routers::{KevRouter, LlmJudgeRouter};
pub mod stream;

pub use anthropic::AnthropicProvider;
pub use factory::{provider_from_config, provider_from_env};
pub use openai_compat::OpenAiCompatProvider;
pub use retry::{with_retry, DelayFn, RetryConfig, RetryingProvider};
pub use stream::{parse_sse_stream, StreamDelta};

use modron_core::Error;

/// Classify a reqwest transport failure into the message taxonomy understood
/// by [`retry::is_retryable`]: `timeout: …`, `connection error: …`,
/// `request failed: …`. Never includes credentials.
pub(crate) fn transport_error(provider: &str, err: reqwest::Error) -> Error {
    let message = if err.is_timeout() {
        format!("timeout: {err}")
    } else if err.is_connect() {
        format!("connection error: {err}")
    } else {
        format!("request failed: {err}")
    };
    Error::Provider {
        provider: provider.to_string(),
        message,
    }
}

/// Build the canonical non-success HTTP error: `HTTP <status>: <body snippet>`.
/// [`retry::is_retryable`] parses this shape to decide on retries.
pub(crate) fn http_error(provider: &str, status: reqwest::StatusCode, body: &str) -> Error {
    Error::Provider {
        provider: provider.to_string(),
        message: format!("HTTP {status}: {}", snippet(body, 300)),
    }
}

/// First `max_chars` characters of `s` on a single line, with an ellipsis
/// when truncated. Used to keep upstream error bodies out of logs/summaries.
pub(crate) fn snippet(s: &str, max_chars: usize) -> String {
    let mut out: String = s
        .chars()
        .take(max_chars)
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_is_single_line_and_truncated() {
        assert_eq!(snippet("a\nb", 10), "a b");
        assert_eq!(snippet("abcdef", 3), "abc…");
        assert_eq!(snippet("abc", 10), "abc");
    }
}
