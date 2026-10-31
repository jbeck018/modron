//! Async decision-model routers (Phase-3b M1/M2): implementations of
//! `modron_core::AsyncRouter`.
//!
//! - [`LlmJudgeRouter`] — the generic "existing model as routing agent":
//!   any OpenAI-compatible provider (Ollama + a 0.5B model locally, or a
//!   cheap hosted model) classifies the task in one tiny turn. Router-not-
//!   judge: it only classifies complexity.
//! - [`KevRouter`] — the [Kev](https://github.com/jaredpalmer/kev) /
//!   TypeSafe Jev SystemOne client (`POST /v1/systemone`, wire-compatible
//!   with both; Apache-2.0, self-hostable). Calibrated probabilities, no
//!   text generation.

use async_trait::async_trait;
use modron_core::{ActionKind, AsyncRouter, RouteVerdict};
use serde_json::json;

use modron_core::ModelProvider;

/// LLM judge over any provider (Phase-3b M1).
pub struct LlmJudgeRouter {
    provider: std::sync::Arc<dyn ModelProvider>,
    model: String,
}

impl LlmJudgeRouter {
    pub fn new(provider: std::sync::Arc<dyn ModelProvider>, model: impl Into<String>) -> Self {
        Self {
            provider,
            model: model.into(),
        }
    }
}

#[async_trait]
impl AsyncRouter for LlmJudgeRouter {
    async fn route(&self, task: &str, _action: ActionKind) -> Option<RouteVerdict> {
        let request = modron_core::TurnRequest {
            model: self.model.clone(),
            system: modron_core::judge::ROUTE_JUDGE_SYSTEM.to_string(),
            messages: vec![modron_core::Message::user(task)],
            tools: Vec::new(),
            max_tokens: 8,
        };
        let response = match self.provider.turn(request).await {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(error = %e, "llm judge call failed — keeping sync choice");
                return None;
            }
        };
        modron_core::judge::parse_route_verdict(&response.text)
    }
}

/// Kev / Jev SystemOne client (Phase-3b M2). One `choice` question with two
/// criteria; the answer's `choice` field is the tier, `confidence` gates it.
pub struct KevRouter {
    http: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    /// Answers below this confidence keep the sync selector's choice.
    min_confidence: f64,
}

impl KevRouter {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
    ) -> Self {
        let _ = model; // reserved: forward the model id once servers require it
        Self {
            http: reqwest::Client::new(),
            endpoint: endpoint.into(),
            api_key,
            min_confidence: 0.6,
        }
    }

    /// Build the SystemOne request body for a routing decision.
    fn build_body(state: &str) -> serde_json::Value {
        json!({
            "state": state,
            "model": "kev-latest",
            "questions": {
                "tier": {
                    "type": "choice",
                    "instructions": "Which tier of model should handle this coding task?",
                    "criteria": {
                        "mechanical": "Search, read, list, summarize, look up, explain — no creation, no multi-step work.",
                        "judgment": "Implement, fix, refactor, plan, design, review, debug — creation or multi-step reasoning."
                    }
                }
            }
        })
    }

    /// Parse a SystemOne response into a verdict. Low confidence → None.
    fn parse_response_with(min_confidence: f64, body: &serde_json::Value) -> Option<RouteVerdict> {
        let answer = body.get("answers")?.get("tier")?;
        let choice = answer.get("choice")?.as_str()?;
        let confidence = answer.get("confidence")?.as_f64()?;
        if confidence < min_confidence {
            return None;
        }
        match choice {
            "mechanical" => Some(RouteVerdict::Mechanical),
            "judgment" => Some(RouteVerdict::Judgment),
            _ => None,
        }
    }
}

#[async_trait]
impl AsyncRouter for KevRouter {
    async fn route(&self, task: &str, _action: ActionKind) -> Option<RouteVerdict> {
        let url = format!("{}/v1/systemone", self.endpoint.trim_end_matches('/'));
        let mut request = self.http.post(&url).json(&Self::build_body(task));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!(error = %e, "kev call failed — keeping sync choice");
                return None;
            }
        };
        if !response.status().is_success() {
            tracing::warn!(status = %response.status(), "kev returned an error — keeping sync choice");
            return None;
        }
        let body: serde_json::Value = match response.json().await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(error = %e, "kev response was not JSON — keeping sync choice");
                return None;
            }
        };
        Self::parse_response_with(self.min_confidence, &body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::testing::MockProvider;

    #[tokio::test]
    async fn llm_judge_parses_the_verdict() {
        let mock = MockProvider::new().then_text("MECHANICAL");
        let router = LlmJudgeRouter::new(std::sync::Arc::new(mock), "tiny-model");
        assert_eq!(
            router.route("list the files", ActionKind::Chat).await,
            Some(RouteVerdict::Mechanical)
        );
    }

    #[tokio::test]
    async fn llm_judge_transport_failure_keeps_sync_choice() {
        // Empty script → turn errors → route returns None.
        let mock = MockProvider::new();
        let router = LlmJudgeRouter::new(std::sync::Arc::new(mock), "tiny-model");
        assert_eq!(router.route("anything", ActionKind::Chat).await, None);
    }

    #[test]
    fn kev_request_body_is_a_single_choice_question() {
        let body = KevRouter::build_body("fix the flaky test");
        assert_eq!(body["state"], "fix the flaky test");
        assert_eq!(body["questions"]["tier"]["type"], "choice");
        assert!(body["questions"]["tier"]["criteria"]["mechanical"].is_string());
        assert!(body["questions"]["tier"]["criteria"]["judgment"].is_string());
    }

    #[test]
    fn kev_response_parses_with_confidence_gate() {
        let answer = |choice: &str, confidence: f64| json!({"answers": {"tier": {"type": "choice", "choice": choice, "confidence": confidence}}});
        assert_eq!(
            KevRouter::parse_response_with(0.6, &answer("judgment", 0.91)),
            Some(RouteVerdict::Judgment)
        );
        assert_eq!(
            KevRouter::parse_response_with(0.6, &answer("mechanical", 0.88)),
            Some(RouteVerdict::Mechanical)
        );
        assert_eq!(
            KevRouter::parse_response_with(0.6, &answer("judgment", 0.55)),
            None,
            "low confidence falls through"
        );
        assert_eq!(
            KevRouter::parse_response_with(0.6, &answer("nonsense", 0.99)),
            None
        );
        assert_eq!(KevRouter::parse_response_with(0.6, &json!({})), None);
    }

    #[tokio::test]
    async fn kev_round_trip_against_a_scripted_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        // One-shot HTTP server returning a canned SystemOne answer.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 65536];
            let n = socket.read(&mut buf).await.unwrap();
            let request_text = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = r#"{"answers": {"tier": {"type": "choice", "choice": "mechanical", "confidence": 0.93}}, "usage": {"input_tokens": 40, "output_tokens": 6}, "latency_ms": 42}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            request_text
        });

        let router = KevRouter::new(format!("http://{addr}"), None, "kev-latest");
        let verdict = router.route("list the sessions", ActionKind::Chat).await;
        assert_eq!(verdict, Some(RouteVerdict::Mechanical));

        let request_text = server.await.unwrap();
        assert!(
            request_text.contains("POST /v1/systemone"),
            "endpoint: {request_text}"
        );
        assert!(
            request_text.contains("\"state\":\"list the sessions\""),
            "state in body"
        );
    }
}
