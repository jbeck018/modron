//! R1 — heuristic [`ModelSelector`]: maps the request's action kind, task
//! length, and context size onto a cheap/strong model tier.
//!
//! Routing is pure: no I/O, no clock, no network — a single match over the
//! action plus one catalog lookup for overflow escalation. The only lazy
//! state is the in-memory catalog (R2), parsed once per process.

use std::collections::HashMap;

use modron_core::{ActionKind, ModelChoice, ModelSelector, SelectRequest};

use crate::catalog::{lookup, Tier};

/// Remaining USD below which the selector downgrades to the cheap tier
/// (budget pressure, Phase-3a R2).
const BUDGET_PRESSURE_USD: f64 = 0.50;

/// Chat tasks shorter than this many chars stay on the cheap tier.
const SHORT_TASK_CHARS: usize = 240;
/// Chat turns with context below this many tokens stay on the cheap tier.
const SMALL_CONTEXT_TOKENS: u64 = 50_000;

/// Conservative size/keyword router: cheap models for mechanical work
/// (search, summarize, short chat), strong models for judgment work
/// (planning, edits, long or large-context turns). Escalates to the strong
/// tier when the chosen model's context window (from the R2 catalog) cannot
/// hold the request's context.
#[derive(Clone, Debug)]
pub struct HeuristicSelector {
    cheap: (String, String),
    strong: (String, String),
    /// Per-action model overrides (Phase-3a R1): `[router.actions]` in
    /// config, e.g. `plan = "glm-5.3"`. Win over tier rules; provider is
    /// looked up from the catalog.
    overrides: HashMap<ActionKind, String>,
}

impl HeuristicSelector {
    /// Build a selector with explicit (model, provider) pairs per tier.
    /// Models absent from the catalog simply skip overflow escalation.
    pub fn new(cheap: (String, String), strong: (String, String)) -> Self {
        Self {
            cheap,
            strong,
            overrides: HashMap::new(),
        }
    }

    /// Attach per-action model overrides (`[router.actions]`).
    pub fn with_overrides(mut self, overrides: HashMap<ActionKind, String>) -> Self {
        self.overrides = overrides;
        self
    }

    /// Force EVERY action onto one model (agent personas, competitive
    /// review A1): applied after per-action overrides, before tier rules.
    pub fn with_forced_model(mut self, model: Option<String>) -> Self {
        if let Some(model) = model {
            for action in [
                ActionKind::Chat,
                ActionKind::Search,
                ActionKind::Summarize,
                ActionKind::Plan,
                ActionKind::Edit,
            ] {
                self.overrides.insert(action, model.clone());
            }
        }
        self
    }
}

impl Default for HeuristicSelector {
    fn default() -> Self {
        Self {
            cheap: ("claude-haiku-5-5".to_string(), "anthropic".to_string()),
            strong: ("claude-sonnet-5-5".to_string(), "anthropic".to_string()),
            overrides: HashMap::new(),
        }
    }
}

impl HeuristicSelector {
    /// Budget pressure + per-action overrides: the checks that win over any
    /// tier decision. Shared by the direct heuristic path (`select`) and the
    /// learned router's tier path (`select_with_tier`) — persona pins and
    /// `[router.actions]` overrides must hold on both.
    fn pre_tier(&self, req: &SelectRequest) -> Option<ModelChoice> {
        // Budget pressure (Phase-3a R2): inside the last ~$0.50 of a capped
        // run, everything downgrades — finishing the task cheaply beats
        // failing mid-edit on an exhausted budget.
        if let Some(remaining) = req.budget_remaining_usd {
            if remaining < BUDGET_PRESSURE_USD {
                let (model, provider) = self.cheap.clone();
                let window = lookup(&model).map(|i| i.context_window);
                return Some(ModelChoice {
                    model,
                    provider,
                    reason: format!(
                        "budget pressure: ${remaining:.2} remaining of the cap → cheap tier"
                    ),
                    confident: true,
                    context_window: window,
                });
            }
        }

        // Per-action overrides win over tier rules. A model absent from the
        // catalog keeps the override model but rides the provider the tier
        // rules would have picked — an empty provider would make the engine
        // silently discard the override model.
        if let Some(model) = self.overrides.get(&req.action) {
            let fallback_provider = match route_tier(req).0 {
                Tier::Cheap => self.cheap.1.clone(),
                Tier::Strong => self.strong.1.clone(),
            };
            let provider = lookup(model)
                .map(|i| i.provider.clone())
                .unwrap_or(fallback_provider);
            return Some(ModelChoice {
                model: model.clone(),
                provider,
                reason: format!("config override: action={:?} → {model}", req.action),
                confident: true,
                context_window: lookup(model).map(|i| i.context_window),
            });
        }

        None
    }

    /// Resolve a request against an already-decided tier (used by the
    /// learned router, Phase-3a R3): same overflow escalation, caller's
    /// reason (or the tier's default when empty).
    pub(crate) fn select_with_tier(
        &self,
        req: &SelectRequest,
        tier: Tier,
        reason: String,
    ) -> ModelChoice {
        if let Some(choice) = self.pre_tier(req) {
            return choice;
        }
        let (tier, mut reason) = if reason.is_empty() {
            let (t, r) = route_tier(req);
            (t, r)
        } else {
            (tier, reason)
        };
        let (mut model, mut provider) = match tier {
            Tier::Cheap => self.cheap.clone(),
            Tier::Strong => self.strong.clone(),
        };

        // Overflow escalation: a model that cannot hold the context is the
        // wrong model, whatever the tier rules said.
        if let Some(info) = lookup(&model) {
            if req.context_tokens > info.context_window && tier == Tier::Cheap {
                reason = format!(
                    "{}; context_tokens={} overflows cheap model {} window ({}) → stepped up to strong tier",
                    reason, req.context_tokens, model, info.context_window
                );
                let strong = self.strong.clone();
                model = strong.0;
                provider = strong.1;
            }
        }

        ModelChoice {
            model,
            provider,
            reason,
            confident: true,
            context_window: None,
        }
    }
}

impl ModelSelector for HeuristicSelector {
    fn select(&self, req: &SelectRequest) -> ModelChoice {
        if let Some(choice) = self.pre_tier(req) {
            return choice;
        }

        let (tier, mut reason) = route_tier(req);
        let (mut model, mut provider) = match tier {
            Tier::Cheap => self.cheap.clone(),
            Tier::Strong => self.strong.clone(),
        };

        // Overflow escalation: a model that cannot hold the context is the
        // wrong model, whatever the tier rules said. The strong pair is the
        // ceiling — if even it overflows we keep it and say so.
        if let Some(info) = lookup(&model) {
            if req.context_tokens > info.context_window {
                if tier == Tier::Cheap {
                    reason = format!(
                        "heuristic: context_tokens={} overflows cheap model {} window ({}) → stepped up to strong tier",
                        req.context_tokens, model, info.context_window
                    );
                    let strong = self.strong.clone();
                    model = strong.0;
                    provider = strong.1;
                } else {
                    reason = format!(
                        "heuristic: context_tokens={} overflows strong model {} window ({}); strong is the ceiling",
                        req.context_tokens, model, info.context_window
                    );
                }
            }
        }

        ModelChoice {
            model,
            provider,
            reason,
            confident: true,
            context_window: None,
        }
    }
}

/// Tier mapping. Search/Summarize → cheap; Plan/Edit → strong; Chat → cheap
/// only when both task and context are small. `reason` is always populated
/// and machine-readable (routing decisions are auditable training data).
fn route_tier(req: &SelectRequest) -> (Tier, String) {
    match req.action {
        ActionKind::Search => (
            Tier::Cheap,
            "heuristic: action=Search → cheap tier".to_string(),
        ),
        ActionKind::Summarize => (
            Tier::Cheap,
            "heuristic: action=Summarize → cheap tier".to_string(),
        ),
        ActionKind::Plan => (
            Tier::Strong,
            "heuristic: action=Plan → strong tier".to_string(),
        ),
        ActionKind::Edit => (
            Tier::Strong,
            "heuristic: action=Edit → strong tier".to_string(),
        ),
        ActionKind::Chat => {
            // Short imperative tasks that name judgment work still need the
            // strong tier ("optimize session loading" is not mechanical just
            // because it is brief) — Phase-3a: learned-router fallback fix.
            if starts_with_strong_verb(&req.task) {
                (
                    Tier::Strong,
                    "heuristic: imperative strong verb → strong tier".to_string(),
                )
            } else {
                chat_size_rule(req)
            }
        }
    }
}

/// Verbs that mark judgment work even in a one-line task.
const STRONG_VERBS: [&str; 10] = [
    "implement",
    "refactor",
    "fix ",
    "optimize",
    "migrate",
    "design ",
    "review ",
    "harden",
    "rewrite",
    "debug ",
];

fn starts_with_strong_verb(task: &str) -> bool {
    let t = task.trim_start().to_lowercase();
    STRONG_VERBS.iter().any(|v| t.starts_with(v))
}

fn chat_size_rule(req: &SelectRequest) -> (Tier, String) {
    let chars = req.task.chars().count();
    if chars >= SHORT_TASK_CHARS {
        (
            Tier::Strong,
            format!("heuristic: long task ({} chars) → strong tier", chars),
        )
    } else if req.context_tokens >= SMALL_CONTEXT_TOKENS {
        (
            Tier::Strong,
            format!(
                "heuristic: large context ({} tokens) → strong tier",
                req.context_tokens
            ),
        )
    } else {
        (
            Tier::Cheap,
            format!(
                "heuristic: action=Chat, short task ({} chars) → cheap tier",
                chars
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_core::ActionKind;

    const CHEAP: &str = "claude-haiku-5-5";
    const STRONG: &str = "claude-sonnet-5-5";

    fn req(action: ActionKind, task: &str, context_tokens: u64) -> SelectRequest {
        SelectRequest {
            task: task.to_string(),
            action,
            context_tokens,
            budget_remaining_usd: None,
        }
    }

    fn req_with_budget(action: ActionKind, task: &str, remaining: f64) -> SelectRequest {
        SelectRequest {
            task: task.to_string(),
            action,
            context_tokens: 1_000,
            budget_remaining_usd: Some(remaining),
        }
    }

    #[test]
    fn budget_pressure_downgrades_to_cheap_with_reason() {
        let choice = selector().select(&req_with_budget(
            ActionKind::Plan,
            "plan the migration",
            0.20,
        ));
        assert_eq!(choice.model, CHEAP, "under $0.50 remaining → cheap");
        assert!(
            choice.reason.contains("budget pressure"),
            "reason: {}",
            choice.reason
        );
    }

    #[test]
    fn healthy_budget_does_not_downgrade() {
        let choice = selector().select(&req_with_budget(
            ActionKind::Plan,
            "plan the migration",
            25.0,
        ));
        assert_eq!(choice.model, STRONG, "healthy budget keeps the tier rule");
    }

    fn selector() -> impl ModelSelector {
        HeuristicSelector::default()
    }

    #[test]
    fn default_tier_models_are_in_catalog() {
        // The overflow tests below depend on this; it is also the deployment
        // invariant (defaults must be lookable for escalation to work).
        assert!(
            lookup(CHEAP).is_some(),
            "default cheap model missing from catalog"
        );
        assert!(
            lookup(STRONG).is_some(),
            "default strong model missing from catalog"
        );
    }

    #[test]
    fn search_and_summarize_route_to_cheap() {
        let s = selector();
        for action in [ActionKind::Search, ActionKind::Summarize] {
            let c = s.select(&req(action, "find the auth code", 1_000));
            assert_eq!(c.model, CHEAP, "action {action:?} should be cheap");
            assert_eq!(c.provider, "anthropic");
            assert!(c.reason.contains("cheap"), "reason: {}", c.reason);
        }
    }

    #[test]
    fn plan_and_edit_route_to_strong() {
        let s = selector();
        for action in [ActionKind::Plan, ActionKind::Edit] {
            let c = s.select(&req(action, "refactor the module", 1_000));
            assert_eq!(c.model, STRONG, "action {action:?} should be strong");
            assert_eq!(c.provider, "anthropic");
            assert!(c.reason.contains("strong"), "reason: {}", c.reason);
        }
    }

    #[test]
    fn chat_short_task_small_context_is_cheap() {
        let c = selector().select(&req(ActionKind::Chat, "hi there", 100));
        assert_eq!(c.model, CHEAP);
        assert!(c.reason.contains("cheap"), "reason: {}", c.reason);
    }

    #[test]
    fn chat_long_task_is_strong() {
        let task = "x".repeat(300);
        let c = selector().select(&req(ActionKind::Chat, &task, 100));
        assert_eq!(c.model, STRONG);
        assert!(c.reason.contains("300 chars"), "reason: {}", c.reason);
    }

    #[test]
    fn chat_at_task_threshold_is_strong() {
        // <240 chars is short; exactly 240 is not.
        let task = "x".repeat(240);
        let c = selector().select(&req(ActionKind::Chat, &task, 100));
        assert_eq!(c.model, STRONG);
    }

    #[test]
    fn chat_large_context_is_strong() {
        let c = selector().select(&req(ActionKind::Chat, "short", 50_000));
        assert_eq!(c.model, STRONG);
        assert!(c.reason.contains("large context"), "reason: {}", c.reason);
    }

    #[test]
    fn chat_just_under_context_threshold_is_cheap() {
        let c = selector().select(&req(ActionKind::Chat, "short", SMALL_CONTEXT_TOKENS - 1));
        assert_eq!(c.model, CHEAP);
    }

    #[test]
    fn overflow_escalates_to_strong_and_says_so() {
        let window = lookup(CHEAP).expect("cataloged").context_window;
        let c = selector().select(&req(ActionKind::Search, "grep it", window + 1));
        assert_eq!(c.model, STRONG, "should step up past cheap window");
        assert!(c.reason.contains("overflow"), "reason: {}", c.reason);
        assert!(c.reason.contains("strong"), "reason: {}", c.reason);
    }

    #[test]
    fn overflow_on_strong_keeps_strong_and_notes_it() {
        let window = lookup(STRONG).expect("cataloged").context_window;
        let c = selector().select(&req(ActionKind::Plan, "plan", window + 1));
        assert_eq!(c.model, STRONG, "strong is the ceiling");
        assert!(c.reason.contains("overflow"), "reason: {}", c.reason);
    }

    #[test]
    fn no_overflow_keeps_tier_choice() {
        let window = lookup(CHEAP).expect("cataloged").context_window;
        let c = selector().select(&req(ActionKind::Search, "grep it", window));
        assert_eq!(c.model, CHEAP);
    }

    #[test]
    fn unknown_models_skip_escalation_without_panic() {
        let s = HeuristicSelector::new(
            ("nonexistent-model".to_string(), "openai-compat".to_string()),
            ("also-missing".to_string(), "openai-compat".to_string()),
        );
        let c = s.select(&req(ActionKind::Search, "grep it", u64::MAX));
        assert_eq!(c.model, "nonexistent-model");
        assert!(!c.reason.is_empty());
    }

    #[test]
    fn every_choice_is_fully_populated() {
        // Exercise via the trait object exactly as the Engine holds it.
        let s: Box<dyn ModelSelector> = Box::new(HeuristicSelector::default());
        let actions = [
            ActionKind::Chat,
            ActionKind::Search,
            ActionKind::Summarize,
            ActionKind::Plan,
            ActionKind::Edit,
        ];
        for action in actions {
            for ctx in [0u64, 1_000, 150_000, 250_000] {
                for task in ["", "short task", "y".repeat(500).as_str()] {
                    let c = s.select(&req(action, task, ctx));
                    assert!(!c.model.is_empty(), "model empty for {action:?}/{ctx}");
                    assert!(
                        !c.provider.is_empty(),
                        "provider empty for {action:?}/{ctx}"
                    );
                    assert!(!c.reason.is_empty(), "reason empty for {action:?}/{ctx}");
                }
            }
        }
    }
}
