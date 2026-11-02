//! Long-horizon goal runs (Phase 14, dirge "Long-Horizon Sessions" /
//! MiMo-Code style): hold an autonomous run to a NATURAL-LANGUAGE stop
//! condition instead of a fixed turn count.
//!
//! The loop re-submits to the same session until the model reports
//! `GOAL COMPLETE`, a cap trips (iterations, or the engine's own
//! budget/turn governors — their stop notes end the loop), or an error
//! surfaces. The session id is stable across iterations, so the whole run
//! is one resumable, compactable transcript.

use crate::{Engine, Result, RunSummary, SessionId};

/// What the goal loop ran to.
#[derive(Clone, Debug)]
pub struct GoalOutcome {
    pub summary: RunSummary,
    /// How many task submissions ran (each may span several model turns).
    pub iterations: u32,
    /// The model reported the goal met.
    pub goal_met: bool,
}

/// Marker the model must emit on its final line when the goal is met.
pub const GOAL_COMPLETE_MARKER: &str = "GOAL COMPLETE";

/// Default iteration cap when the caller does not set one.
pub const DEFAULT_MAX_ITERATIONS: u32 = 10;

/// Run `task` under `goal` until the stop condition is reported or a cap
/// trips. `session` resumes an existing session (the loop continues it);
/// `None` starts a fresh one and the created id is carried across
/// iterations.
pub async fn run_goal_loop(
    engine: &Engine,
    session: Option<SessionId>,
    task: &str,
    goal: &str,
    max_iterations: u32,
) -> Result<GoalOutcome> {
    let max_iterations = max_iterations.max(1);
    let mut current_session = session;
    let mut summary: Option<RunSummary> = None;
    let mut iterations = 0u32;

    for i in 0..max_iterations {
        let prompt = if i == 0 {
            format!(
                "{task}\n\n(Goal for this run: {goal}. Keep working across turns until it is \
                 met, then end your final message with the exact line: {GOAL_COMPLETE_MARKER})"
            )
        } else {
            format!(
                "Continue toward the goal: {goal}. Re-assess the current state of the work \
                 (files, tests, prior results), continue from where the last turn stopped, and \
                 end your final message with the exact line: {GOAL_COMPLETE_MARKER} when the \
                 goal is fully met."
            )
        };
        let run = match current_session {
            Some(id) => engine.run_task_resuming(id, &prompt).await?,
            None => engine.run_task(&prompt).await?,
        };
        current_session = Some(run.session_id);
        iterations += 1;
        let answer = run.answer.clone();
        let stopped_by_budget = answer.contains("[stopped:");
        let goal_met = answer.contains(GOAL_COMPLETE_MARKER);
        summary = Some(run);
        if goal_met || stopped_by_budget {
            break;
        }
    }

    let summary = summary.expect("max_iterations >= 1 guarantees one run");
    let goal_met = summary.answer.contains(GOAL_COMPLETE_MARKER);
    Ok(GoalOutcome {
        summary,
        iterations,
        goal_met,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{InMemoryStore, MockProvider, StaticSelector};
    use crate::{EngineConfig, ToolRegistry};
    use std::collections::HashMap;
    use std::sync::Arc;

    fn engine_with(script: Vec<crate::TurnResponse>) -> Engine {
        let mut provider = MockProvider::new();
        for response in script {
            provider = provider.then_response(response);
        }
        let mut providers: HashMap<String, Arc<dyn crate::ModelProvider>> = HashMap::new();
        providers.insert("mock".into(), Arc::new(provider));
        Engine {
            selector: Arc::new(StaticSelector::new("mock-model", "mock")),
            providers,
            tools: Arc::new(ToolRegistry::new()),
            store: Arc::new(InMemoryStore::default()),
            config: EngineConfig::default(),
        }
    }

    fn text(t: &str) -> crate::TurnResponse {
        crate::TurnResponse {
            text: t.into(),
            tool_calls: vec![],
            usage: crate::Usage::default(),
            stop_reason: crate::StopReason::EndTurn,
        }
    }

    #[tokio::test]
    async fn goal_reported_complete_stops_after_one_iteration() {
        let engine = engine_with(vec![text(&format!(
            "did the thing\n\n{GOAL_COMPLETE_MARKER}"
        ))]);
        let out = run_goal_loop(&engine, None, "do it", "it is done", 5)
            .await
            .unwrap();
        assert!(out.goal_met);
        assert_eq!(out.iterations, 1);
        assert!(out.summary.answer.contains(GOAL_COMPLETE_MARKER));
    }

    #[tokio::test]
    async fn no_marker_runs_to_the_iteration_cap_same_session() {
        // Each iteration consumes one scripted response; repeat the last.
        let engine = engine_with(vec![
            text("still working"),
            text("still working 2"),
            text("still working 3"),
        ]);
        let out = run_goal_loop(&engine, None, "do it", "it is done", 3)
            .await
            .unwrap();
        assert!(!out.goal_met);
        assert_eq!(out.iterations, 3);
        // One stable session across iterations.
        let events = engine
            .store
            .load(out.summary.session_id)
            .await
            .unwrap();
        let tasks = events
            .iter()
            .filter(|e| matches!(e, crate::SessionEvent::Task { .. }))
            .count();
        assert_eq!(tasks, 3, "all iterations in ONE session: {events:#?}");
    }

    #[tokio::test]
    async fn budget_stop_note_ends_the_loop_early() {
        let engine = engine_with(vec![
            text("working"),
            text("ran out\n\n[stopped: USD spend cap exceeded — spent ~$1.0000 of $1.0000 cap]"),
        ]);
        let out = run_goal_loop(&engine, None, "do it", "it is done", 9)
            .await
            .unwrap();
        assert!(!out.goal_met);
        assert_eq!(out.iterations, 2, "stop note breaks the loop");
    }

    #[tokio::test]
    async fn marker_on_a_later_iteration_completes() {
        let engine = engine_with(vec![
            text("step 1 done"),
            text(&format!("all done\n{GOAL_COMPLETE_MARKER}")),
        ]);
        let out = run_goal_loop(&engine, None, "do it", "it is done", 5)
            .await
            .unwrap();
        assert!(out.goal_met);
        assert_eq!(out.iterations, 2);
    }
}
