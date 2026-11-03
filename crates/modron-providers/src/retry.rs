//! Retry/backoff — LEDGER item P4.
//!
//! Wraps fallible provider operations with bounded retries on transient
//! failures: HTTP 429, HTTP 5xx, and timeouts. Because the frozen
//! [`modron_core::Error`] carries no status field, the taxonomy is encoded
//! in `Error::Provider` message prefixes produced by this crate's providers
//! (`HTTP <status>: …`, `timeout: …`, `connection error: …`,
//! `request failed: …`) and decoded by [`is_retryable`].
//!
//! Backoff is exponential with factor 4: 200ms after the first failure,
//! 800ms after the second (max 3 attempts by default). The delay is
//! injectable so tests run instantly.

use async_trait::async_trait;
use modron_core::{Error, ModelProvider, Result, TurnRequest, TurnResponse};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

/// Injectable sleep: a future completing after `Duration`. The default is
/// [`tokio_sleep`]; tests substitute an immediate no-op.
pub type DelayFn = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Default delay implementation backed by `tokio::time::sleep`.
pub fn tokio_sleep() -> DelayFn {
    Arc::new(|delay| Box::pin(tokio::time::sleep(delay)))
}

/// Retry policy: at most [`RetryConfig::max_attempts`] attempts (minimum one),
/// waiting [`RetryConfig::backoff_for`] between attempts — 200ms then 800ms
/// with the defaults.
#[derive(Clone, Debug)]
pub struct RetryConfig {
    /// Total attempts including the first (default 3).
    pub max_attempts: u32,
    /// Backoff base for the exponential schedule (default 200ms).
    pub base_backoff: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff: Duration::from_millis(200),
        }
    }
}

impl RetryConfig {
    /// Backoff before the retry following `failed_attempt` (0-based):
    /// `base_backoff * 4^failed_attempt` → 200ms, 800ms, …
    pub fn backoff_for(&self, failed_attempt: u32) -> Duration {
        self.base_backoff
            .saturating_mul(4u32.saturating_pow(failed_attempt))
    }
}

/// Whether an error is transient and worth retrying. Matches the message
/// taxonomy emitted by this crate: `HTTP 429 …`, `HTTP 5xx …`, `timeout: …`.
/// Everything else (4xx, malformed responses, tool/session/budget errors)
/// fails fast.
pub fn is_retryable(err: &Error) -> bool {
    match err {
        Error::Provider { message, .. } => is_retryable_message(message),
        _ => false,
    }
}

fn is_retryable_message(message: &str) -> bool {
    if message.starts_with("timeout:") {
        return true;
    }
    if let Some(rest) = message.strip_prefix("HTTP ") {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(status) = digits.parse::<u16>() {
            return status == 429 || (500..600).contains(&status);
        }
    }
    false
}

/// Run `op` with bounded retries. Always makes at least one attempt; retries
/// only while the error is [`is_retryable`] and attempts remain, sleeping
/// `cfg.backoff_for(attempt)` between them.
pub async fn with_retry<T, F, Fut>(mut op: F, cfg: &RetryConfig, delay: &DelayFn) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                attempt += 1;
                if attempt >= cfg.max_attempts.max(1) || !is_retryable(&err) {
                    return Err(err);
                }
                let wait = cfg.backoff_for(attempt - 1);
                tracing::debug!(
                    attempt,
                    backoff_ms = wait.as_millis() as u64,
                    error = %err,
                    "retryable provider error; backing off"
                );
                (delay)(wait).await;
            }
        }
    }
}

/// [`ModelProvider`] wrapper adding retry/backoff to any inner provider.
/// `id` delegates to the inner provider.
pub struct RetryingProvider {
    inner: Arc<dyn ModelProvider>,
    cfg: RetryConfig,
    delay: DelayFn,
}

impl RetryingProvider {
    /// Wrap `inner` with the default policy (3 attempts, 200ms base backoff).
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        Self {
            inner,
            cfg: RetryConfig::default(),
            delay: tokio_sleep(),
        }
    }

    /// Wrap `inner` with an explicit policy.
    pub fn with_config(inner: Arc<dyn ModelProvider>, cfg: RetryConfig) -> Self {
        Self {
            inner,
            cfg,
            delay: tokio_sleep(),
        }
    }

    /// Override the sleep (tests use an immediate no-op).
    pub fn with_delay_fn(mut self, delay: DelayFn) -> Self {
        self.delay = delay;
        self
    }
}

#[async_trait]
impl ModelProvider for RetryingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn turn(&self, request: TurnRequest) -> Result<TurnResponse> {
        let inner = Arc::clone(&self.inner);
        with_retry(
            move || {
                // Own the Arc and the request clone inside the future: the
                // async-trait future borrows the provider, so it cannot borrow
                // the closure's captured variables.
                let inner = Arc::clone(&inner);
                let request = request.clone();
                async move { inner.turn(request).await }
            },
            &self.cfg,
            &self.delay,
        )
        .await
    }

    async fn turn_streaming(
        &self,
        req: TurnRequest,
        tx: tokio::sync::mpsc::UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<TurnResponse> {
        // Streaming delegates directly — a retry would replay a turn whose
        // deltas were already emitted to the sink. Without this override the
        // trait default silently degrades every streaming turn to one
        // blocking non-streaming call (single final delta), making the
        // providers' SSE paths unreachable through the wrapper.
        self.inner.turn_streaming(req, tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::{StopReason, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn noop_delay() -> DelayFn {
        Arc::new(|_| Box::pin(async {}))
    }

    fn provider_err(message: &str) -> Error {
        Error::Provider {
            provider: "stub".into(),
            message: message.into(),
        }
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result: Result<u32> = with_retry(
            move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(provider_err("HTTP 429 Too Many Requests: slow down"))
                    } else {
                        Ok(42)
                    }
                }
            },
            &RetryConfig::default(),
            &noop_delay(),
        )
        .await;
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_client_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result: Result<u32> = with_retry(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async move { Err::<u32, _>(provider_err("HTTP 400 Bad Request: bad args")) }
            },
            &RetryConfig::default(),
            &noop_delay(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "non-retryable fails fast");
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let result: Result<u32> = with_retry(
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async move { Err::<u32, _>(provider_err("HTTP 503 Service Unavailable")) }
            },
            &RetryConfig::default(),
            &noop_delay(),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn taxonomy_classification() {
        assert!(is_retryable(&provider_err("HTTP 429 Too Many Requests")));
        assert!(is_retryable(&provider_err(
            "HTTP 500 Internal Server Error"
        )));
        assert!(is_retryable(&provider_err("HTTP 502 Bad Gateway")));
        assert!(is_retryable(&provider_err(
            "timeout: request took too long"
        )));
        assert!(!is_retryable(&provider_err("HTTP 400 Bad Request")));
        assert!(!is_retryable(&provider_err("HTTP 401 Unauthorized")));
        assert!(!is_retryable(&provider_err("HTTP 404 Not Found")));
        assert!(!is_retryable(&provider_err(
            "connection error: dns failure"
        )));
        assert!(!is_retryable(&provider_err(
            "malformed response: unexpected token"
        )));
        assert!(!is_retryable(&Error::Budget("not a provider error".into())));
    }

    #[test]
    fn backoff_schedule_is_200_then_800() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.backoff_for(0), Duration::from_millis(200));
        assert_eq!(cfg.backoff_for(1), Duration::from_millis(800));
        assert_eq!(cfg.backoff_for(2), Duration::from_millis(3200));
    }

    /// Counting stub transport behind the [`RetryingProvider`] wrapper.
    struct FlakyProvider {
        calls: Arc<AtomicUsize>,
        failures: usize,
    }

    #[async_trait]
    impl ModelProvider for FlakyProvider {
        fn id(&self) -> &str {
            "flaky"
        }

        async fn turn(&self, _req: TurnRequest) -> Result<TurnResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.failures {
                Err(provider_err("HTTP 503 Service Unavailable: overloaded"))
            } else {
                Ok(TurnResponse {
                    text: "ok".into(),
                    tool_calls: Vec::new(),
                    usage: Usage::default(),
                    stop_reason: StopReason::EndTurn,
                })
            }
        }
    }

    #[tokio::test]
    async fn retrying_provider_wraps_inner_and_delegates_id() {
        let calls = Arc::new(AtomicUsize::new(0));
        let inner = Arc::new(FlakyProvider {
            calls: Arc::clone(&calls),
            failures: 2,
        });
        let provider = RetryingProvider::new(inner).with_delay_fn(noop_delay());

        let response = provider
            .turn(TurnRequest {
                model: "m".into(),
                system: String::new(),
                messages: vec![],
                tools: vec![],
                max_tokens: 16,
            })
            .await
            .unwrap();
        assert_eq!(response.text, "ok");
        assert_eq!(provider.id(), "flaky", "id delegates to the inner provider");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two retries after two failures"
        );
    }
}
