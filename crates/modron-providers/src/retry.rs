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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Injectable sleep: a future completing after `Duration`. The default is
/// [`tokio_sleep`]; tests substitute an immediate no-op.
pub type DelayFn = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Injectable jitter source returning a factor in `[-1.0, 1.0]`; the
/// backoff is scaled by `1 + 0.2 * factor`. Tests inject a constant.
pub type JitterFn = Arc<dyn Fn() -> f64 + Send + Sync>;

/// Longest server-requested `Retry-After` honored, in seconds.
pub const RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

/// Marker appended to `HTTP <status>: …` errors that carried a
/// `Retry-After` header: ` [retry-after=<seconds>s]`. Appending at the end
/// keeps the status prefix that [`is_retryable`] keys on unchanged.
pub(crate) const RETRY_AFTER_TAG: &str = " [retry-after=";

/// The suffix [`crate::http`] appends to a non-success error for a
/// `Retry-After: <seconds>` header.
pub(crate) fn retry_after_tag(seconds: u64) -> String {
    format!("{RETRY_AFTER_TAG}{seconds}s]")
}

/// The `Retry-After` wait carried by an error, if any (see
/// [`retry_after_tag`]).
pub(crate) fn retry_after(err: &Error) -> Option<Duration> {
    let Error::Provider { message, .. } = err else {
        return None;
    };
    let start = message.rfind(RETRY_AFTER_TAG)? + RETRY_AFTER_TAG.len();
    let digits = message[start..].strip_suffix("s]")?;
    digits.parse::<u64>().ok().map(Duration::from_secs)
}

/// Default jitter: a pseudo-random factor in `[-1.0, 1.0]` derived from the
/// sub-second clock. No RNG dependency; good enough to de-synchronize
/// concurrent retries.
pub fn system_jitter_factor() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (f64::from(nanos) / 999_999_999.0) * 2.0 - 1.0
}

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
        // Patient on transient bursts: provider-side overload (Z.ai 1305,
        // 429s) routinely outlasts a 1-second budget — terminal-bench
        // batch runs showed 2/8 trials dying to flakiness that passes on
        // any retry. 5 attempts x 500ms base = ~13s of backoff ceiling.
        Self {
            max_attempts: 5,
            base_backoff: Duration::from_millis(500),
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

    /// The actual wait before the retry following `failed_attempt`:
    /// `max(backoff_for, retry_after capped at 60s)`, then scaled by
    /// `1 + 0.2 * jitter` (jitter clamped to `[-1, 1]`).
    pub fn wait_before_retry(
        &self,
        failed_attempt: u32,
        retry_after: Option<Duration>,
        jitter: f64,
    ) -> Duration {
        let policy = self.backoff_for(failed_attempt);
        let base = match retry_after {
            Some(requested) => policy.max(requested.min(RETRY_AFTER_CAP)),
            None => policy,
        };
        let jitter = if jitter.is_nan() {
            0.0
        } else {
            jitter.clamp(-1.0, 1.0)
        };
        let factor = 1.0 + 0.2 * jitter;
        base.mul_f64(factor)
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
    match http_status(message) {
        Some(status) => status == 429 || (500..600).contains(&status),
        None => false,
    }
}

/// The status code of an `HTTP <status>: …` message.
fn http_status(message: &str) -> Option<u16> {
    let rest = message.strip_prefix("HTTP ")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u16>().ok()
}

/// Statuses replayed on a stream that has emitted nothing yet: rate limit,
/// the common 5xx, and Anthropic's 529 overload.
fn is_stream_replayable_status(err: &Error) -> bool {
    match err {
        Error::Provider { message, .. } => {
            matches!(http_status(message), Some(429 | 500 | 502 | 503 | 529))
        }
        _ => false,
    }
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
                let wait =
                    cfg.wait_before_retry(attempt - 1, retry_after(&err), system_jitter_factor());
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
    jitter: JitterFn,
}

impl RetryingProvider {
    /// Wrap `inner` with the default policy (5 attempts, 500ms base backoff).
    pub fn new(inner: Arc<dyn ModelProvider>) -> Self {
        let jitter: JitterFn = Arc::new(system_jitter_factor);
        Self {
            inner,
            cfg: RetryConfig::default(),
            delay: tokio_sleep(),
            jitter,
        }
    }

    /// Override the sleep (tests use an immediate no-op).
    pub fn with_delay_fn(mut self, delay: DelayFn) -> Self {
        self.delay = delay;
        self
    }

    /// Override the jitter source (tests pin it to a constant).
    pub fn with_jitter_fn(mut self, jitter: JitterFn) -> Self {
        self.jitter = jitter;
        self
    }
}

#[async_trait]
impl ModelProvider for RetryingProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    async fn turn(&self, request: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
        // The request is shared by Arc: every attempt (including the
        // first) re-sends it without a deep clone. Non-streaming turns
        // are idempotent to replay (nothing observable escapes).
        let attempts = self.cfg.max_attempts.max(1);
        for attempt in 1..=attempts {
            let is_last = attempt == attempts;
            match self.inner.turn(Arc::clone(&request)).await {
                Ok(response) => return Ok(response),
                Err(err) if !is_last && is_retryable(&err) => {
                    let wait =
                        self.cfg
                            .wait_before_retry(attempt - 1, retry_after(&err), (self.jitter)());
                    tracing::debug!(
                        attempt,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        "retryable provider error; backing off"
                    );
                    (self.delay)(wait).await;
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("the loop returns on every path")
    }

    async fn turn_streaming(
        &self,
        req: std::sync::Arc<TurnRequest>,
        tx: tokio::sync::mpsc::UnboundedSender<modron_core::StreamDelta>,
    ) -> Result<TurnResponse> {
        // Streaming replays a turn ONLY when nothing observable was
        // emitted: deltas already sent to the sink cannot be replayed
        // (duplication), but a transport death or a 429/500/502/503/529
        // BEFORE any output is safe and uses the same attempt budget +
        // backoff (Retry-After aware) as `turn` — the empty-completion
        // safety net providers like oh-my-pi ship.
        // (Without this override the trait default silently degrades
        // every streaming turn to one blocking non-streaming call.)
        use std::sync::atomic::{AtomicBool, Ordering};
        let attempts = self.cfg.max_attempts.max(1);
        for attempt in 1..=attempts {
            let is_last = attempt == attempts;
            // Shared by Arc: no attempt ever deep-clones the request.
            let call_req = Arc::clone(&req);
            let (fwd_tx, mut fwd_rx) =
                tokio::sync::mpsc::unbounded_channel::<modron_core::StreamDelta>();
            let emitted = std::sync::Arc::new(AtomicBool::new(false));
            let flag = std::sync::Arc::clone(&emitted);
            let sink = tx.clone();
            // Live forwarding keeps the caller's stream real-time; the
            // flag records whether anything observable was sent. Relies on
            // the provider dropping its tx when the call returns.
            let forwarder = tokio::spawn(async move {
                while let Some(delta) = fwd_rx.recv().await {
                    flag.store(true, Ordering::Relaxed);
                    let _ = sink.send(delta);
                }
            });
            let result = self.inner.turn_streaming(call_req, fwd_tx).await;
            // fwd_tx was consumed by the call; awaiting the forwarder
            // flushes any tail deltas before we decide on a replay.
            let _ = forwarder.await;
            let nothing_emitted = !emitted.load(Ordering::Relaxed);
            match result {
                Ok(response) => return Ok(response),
                Err(err)
                    if !is_last
                        && nothing_emitted
                        && (is_transport_death(&err) || is_stream_replayable_status(&err)) =>
                {
                    let wait =
                        self.cfg
                            .wait_before_retry(attempt - 1, retry_after(&err), (self.jitter)());
                    tracing::warn!(
                        attempt,
                        backoff_ms = wait.as_millis() as u64,
                        error = %err,
                        "provider failed before any output — replaying"
                    );
                    (self.delay)(wait).await;
                    continue;
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("the loop returns on its final iteration")
    }
}

/// A transport-class provider error ("connection error:", "timeout:",
/// "request failed:"): the request or stream died at the transport layer.
/// Used for the nothing-emitted streaming replay — semantic failures
/// (HTTP 4xx, malformed payloads) fail fast instead.
fn is_transport_death(err: &Error) -> bool {
    match err {
        Error::Provider { message, .. } => {
            message.starts_with("connection error:")
                || message.starts_with("timeout:")
                || message.starts_with("request failed:")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::{Message, StopReason, StreamDelta, Usage};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A provider whose stream dies (emitting nothing) on the first call
    /// and succeeds on the second — the empty-completion replay case.
    struct DeadThenAliveStreamProvider {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for DeadThenAliveStreamProvider {
        fn id(&self) -> &str {
            "dead-then-alive"
        }

        async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
            unreachable!("streaming path under test")
        }

        async fn turn_streaming(
            &self,
            _req: std::sync::Arc<TurnRequest>,
            tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
        ) -> Result<TurnResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                // Died before ANY delta: nothing reaches the sink.
                return Err(provider_err("connection error: stream ended before [DONE]"));
            }
            let _ = tx.send(StreamDelta::Text("replayed".into()));
            Ok(TurnResponse {
                text: "replayed".into(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    #[tokio::test]
    async fn empty_stream_death_is_replayed_once_without_duplicates() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = RetryingProvider::new(Arc::new(DeadThenAliveStreamProvider {
            calls: Arc::clone(&calls),
        }))
        .with_delay_fn(noop_delay());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let response = provider
            .turn_streaming(
                Arc::new(TurnRequest {
                    model: "m".into(),
                    system: String::new(),
                    messages: vec![Message::user("hi")],
                    tools: vec![],
                    max_tokens: 16,
                }),
                tx,
            )
            .await
            .unwrap();
        assert_eq!(response.text, "replayed");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "exactly one replay");
        let mut texts = Vec::new();
        while let Ok(delta) = rx.try_recv() {
            if let StreamDelta::Text(t) = delta {
                texts.push(t);
            }
        }
        assert_eq!(texts, vec!["replayed"], "no duplicated content");
    }

    #[tokio::test]
    async fn stream_death_after_output_is_not_replayed() {
        struct DiedMidStream;
        #[async_trait]
        impl ModelProvider for DiedMidStream {
            fn id(&self) -> &str {
                "died-mid-stream"
            }
            async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
                unreachable!()
            }
            async fn turn_streaming(
                &self,
                _req: std::sync::Arc<TurnRequest>,
                tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
            ) -> Result<TurnResponse> {
                let _ = tx.send(StreamDelta::Text("partial ".into()));
                Err(provider_err("connection error: stream ended before [DONE]"))
            }
        }
        let provider = RetryingProvider::new(Arc::new(DiedMidStream)).with_delay_fn(noop_delay());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let err = provider
            .turn_streaming(
                Arc::new(TurnRequest {
                    model: "m".into(),
                    system: String::new(),
                    messages: vec![Message::user("hi")],
                    tools: vec![],
                    max_tokens: 16,
                }),
                tx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("stream ended"), "{err}");
        let mut texts = Vec::new();
        while let Ok(delta) = rx.try_recv() {
            if let StreamDelta::Text(t) = delta {
                texts.push(t);
            }
        }
        assert_eq!(
            texts,
            vec!["partial "],
            "emitted content preserved, no replay"
        );
    }

    /// Streaming stub that fails with a scripted error before emitting
    /// anything on the first call, then streams `replayed` on later calls.
    struct ErrThenOkStream {
        calls: Arc<AtomicUsize>,
        first_error: String,
    }

    #[async_trait]
    impl ModelProvider for ErrThenOkStream {
        fn id(&self) -> &str {
            "err-then-ok"
        }

        async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
            unreachable!("streaming path under test")
        }

        async fn turn_streaming(
            &self,
            _req: std::sync::Arc<TurnRequest>,
            tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
        ) -> Result<TurnResponse> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                return Err(provider_err(&self.first_error));
            }
            let _ = tx.send(StreamDelta::Text("replayed".into()));
            Ok(TurnResponse {
                text: "replayed".into(),
                tool_calls: Vec::new(),
                usage: Usage::default(),
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    fn stream_request() -> Arc<TurnRequest> {
        Arc::new(TurnRequest {
            model: "m".into(),
            system: String::new(),
            messages: vec![Message::user("hi")],
            tools: vec![],
            max_tokens: 16,
        })
    }

    /// Delay stub that records every wait instead of sleeping.
    fn recording_delay() -> (DelayFn, Arc<std::sync::Mutex<Vec<Duration>>>) {
        let waits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&waits);
        let delay: DelayFn = Arc::new(move |d| {
            sink.lock().unwrap().push(d);
            Box::pin(async {})
        });
        (delay, waits)
    }

    #[tokio::test]
    async fn streamed_retryable_status_before_output_is_replayed() {
        for status in [
            "HTTP 429 Too Many Requests: slow down",
            "HTTP 500 Internal Server Error: boom",
            "HTTP 502 Bad Gateway: upstream",
            "HTTP 503 Service Unavailable: overloaded",
            "HTTP 529 Overloaded: busy",
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let provider = RetryingProvider::new(Arc::new(ErrThenOkStream {
                calls: Arc::clone(&calls),
                first_error: status.into(),
            }))
            .with_delay_fn(noop_delay());
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let response = provider
                .turn_streaming(stream_request(), tx)
                .await
                .unwrap_or_else(|e| panic!("{status} should replay: {e}"));
            assert_eq!(response.text, "replayed", "{status}");
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{status}: one replay");
        }
    }

    #[tokio::test]
    async fn streamed_client_error_before_output_is_not_replayed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = RetryingProvider::new(Arc::new(ErrThenOkStream {
            calls: Arc::clone(&calls),
            first_error: "HTTP 400 Bad Request: bad args".into(),
        }))
        .with_delay_fn(noop_delay());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(provider.turn_streaming(stream_request(), tx).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "4xx fails fast");
    }

    #[tokio::test]
    async fn streamed_429_after_output_is_not_replayed() {
        struct EmitThen429;
        #[async_trait]
        impl ModelProvider for EmitThen429 {
            fn id(&self) -> &str {
                "emit-then-429"
            }
            async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
                unreachable!()
            }
            async fn turn_streaming(
                &self,
                _req: std::sync::Arc<TurnRequest>,
                tx: tokio::sync::mpsc::UnboundedSender<StreamDelta>,
            ) -> Result<TurnResponse> {
                let _ = tx.send(StreamDelta::Text("partial ".into()));
                Err(provider_err("HTTP 429 Too Many Requests: slow down"))
            }
        }
        let provider = RetryingProvider::new(Arc::new(EmitThen429)).with_delay_fn(noop_delay());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let err = provider
            .turn_streaming(stream_request(), tx)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 429"), "{err}");
        let mut texts = Vec::new();
        while let Ok(StreamDelta::Text(t)) = rx.try_recv() {
            texts.push(t);
        }
        assert_eq!(texts, vec!["partial "], "no replay once output was sent");
    }

    #[tokio::test]
    async fn streamed_replay_waits_for_retry_after() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (delay, waits) = recording_delay();
        let provider = RetryingProvider::new(Arc::new(ErrThenOkStream {
            calls: Arc::clone(&calls),
            first_error: "HTTP 429 Too Many Requests: slow [retry-after=12s]".into(),
        }))
        .with_delay_fn(delay)
        .with_jitter_fn(Arc::new(|| 0.0));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        provider.turn_streaming(stream_request(), tx).await.unwrap();
        assert_eq!(*waits.lock().unwrap(), vec![Duration::from_secs(12)]);
    }

    #[tokio::test]
    async fn turn_retry_honors_retry_after_capped_at_60s() {
        struct OnceRateLimited {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait]
        impl ModelProvider for OnceRateLimited {
            fn id(&self) -> &str {
                "once-rate-limited"
            }
            async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
                if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(provider_err(
                        "HTTP 429 Too Many Requests [retry-after=3600s]",
                    ))
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
        let (delay, waits) = recording_delay();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = RetryingProvider::new(Arc::new(OnceRateLimited {
            calls: Arc::clone(&calls),
        }))
        .with_delay_fn(delay)
        .with_jitter_fn(Arc::new(|| 0.0));
        provider.turn(stream_request()).await.unwrap();
        assert_eq!(*waits.lock().unwrap(), vec![Duration::from_secs(60)]);
    }

    #[test]
    fn retry_after_is_parsed_from_the_error_text() {
        assert_eq!(
            retry_after(&provider_err(
                "HTTP 429 Too Many Requests: x [retry-after=12s]"
            )),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            retry_after(&provider_err("HTTP 429 Too Many Requests")),
            None
        );
        assert_eq!(
            retry_after(&provider_err("HTTP 429 x [retry-after=soon]")),
            None
        );
        assert_eq!(retry_after(&Error::Budget("[retry-after=3s]".into())), None);
    }

    #[test]
    fn wait_is_max_of_policy_and_capped_retry_after() {
        let cfg = RetryConfig::default();
        assert_eq!(
            cfg.wait_before_retry(0, None, 0.0),
            Duration::from_millis(500)
        );
        assert_eq!(
            cfg.wait_before_retry(0, Some(Duration::from_millis(100)), 0.0),
            Duration::from_millis(500),
            "policy wins when Retry-After is shorter"
        );
        assert_eq!(
            cfg.wait_before_retry(0, Some(Duration::from_secs(12)), 0.0),
            Duration::from_secs(12)
        );
        assert_eq!(
            cfg.wait_before_retry(0, Some(Duration::from_secs(3600)), 0.0),
            Duration::from_secs(60),
            "Retry-After is capped at 60s"
        );
    }

    #[test]
    fn jitter_is_plus_minus_twenty_percent() {
        let cfg = RetryConfig::default();
        assert_eq!(
            cfg.wait_before_retry(0, None, 1.0),
            Duration::from_millis(600)
        );
        assert_eq!(
            cfg.wait_before_retry(0, None, -1.0),
            Duration::from_millis(400)
        );
        assert_eq!(
            cfg.wait_before_retry(0, None, 0.0),
            Duration::from_millis(500)
        );
        assert_eq!(
            cfg.wait_before_retry(0, None, 9.0),
            Duration::from_millis(600),
            "out-of-range jitter is clamped"
        );
    }

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
        assert_eq!(calls.load(Ordering::SeqCst), 5, "new default: 5 attempts");
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
    fn backoff_schedule_is_500_then_2000() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.backoff_for(0), Duration::from_millis(500));
        assert_eq!(cfg.backoff_for(1), Duration::from_millis(2000));
        assert_eq!(cfg.backoff_for(2), Duration::from_millis(8000));
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

        async fn turn(&self, _req: std::sync::Arc<TurnRequest>) -> Result<TurnResponse> {
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
            .turn(Arc::new(TurnRequest {
                model: "m".into(),
                system: String::new(),
                messages: vec![],
                tools: vec![],
                max_tokens: 16,
            }))
            .await
            .unwrap();
        assert_eq!(response.text, "ok");
        assert_eq!(provider.id(), "flaky", "id delegates to the inner provider");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "two retries after two failures (within the 5-attempt budget)"
        );
    }
}
