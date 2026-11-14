//! Compaction support — LEDGER X2 (PLAN.md §4.10).
//!
//! Pure, deterministic helpers for the engine's autocompaction step: the
//! stable summarizer system prompt, a deterministic rendering of the
//! transcript-to-summarize, and a byte cap for the model's summary. Plus
//! [`split_older_tail`], the shared boundary rule that decides what is
//! "older" (summarized) versus "recent tail" (kept verbatim) — used both when
//! a compaction runs and when a session is reconstructed from its event log.

use crate::{Role, SessionEvent};

/// The stable summarizer system prompt (X2). Terse on purpose: it is the
/// system prompt of every compaction call, and its fixed output shape
/// (TASK / DECISIONS / EDITS / OPEN ITEMS) keeps compact states load-bearing
/// and comparable across runs. Must stay well under 512 estimated tokens
/// (asserted in the tests below).
pub fn summarizer_prompt() -> &'static str {
    "\
You compress a coding-agent transcript into a compact state for later turns.
Input: a rendered transcript. Output: EXACTLY these four sections, nothing else:

TASK: the user's goal, one line
DECISIONS: key choices and constraints, one line each
EDITS: files changed / actions taken, one line each
OPEN ITEMS: unfinished work and next steps, one line each

Rules: at most 400 tokens total. Telegraphic style; no prose, no markdown, no \
restated tool output. Keep exact file paths and identifiers. Never invent \
facts not present in the transcript."
}

/// Render the transcript-to-summarize deterministically (X2): every
/// transcript-bearing event EXCEPT the last `skip_last_messages` items, in
/// event order.
///
/// Rendering rules:
/// - `Task` → `[TASK] …`, `Message` → `[USER]`/`[ASSISTANT]` plus one
///   `[TOOL_CALL …]` line per tool call, `ToolResult` → `[TOOL_RESULT …]`.
/// - `ModelSelected`/`Usage` carry no transcript content and are skipped.
/// - A prior `Compaction` event renders as a `[COMPACTED]` line so its state
///   folds into the new summary (compaction of a compaction is lossless).
///
/// The tail boundary follows [`split_older_tail`], so the rendered portion
/// never ends by orphaning a tool result away from its assistant message.
pub fn older_transcript_text(events: &[SessionEvent], skip_last_messages: usize) -> String {
    let (older, _) = split_older_tail(events, skip_last_messages);
    render_events(older.into_iter(), false)
}

/// Cap on the text handed to the summarizer (A7). A single compaction input
/// larger than this is elided in the middle rather than sent whole: the
/// summarizer is a cheap model with a finite window, and a request it
/// rejects fails the run.
pub const SUMMARIZER_INPUT_MAX_BYTES: usize = 400_000;

/// The text for the NEXT compaction (A7): only what the previous compaction
/// did not already cover. The previous compact state leads as one
/// `[COMPACTED]` line, followed by the events that have moved from the
/// verbatim tail into the older portion since then. Without a previous
/// compaction this is exactly [`older_transcript_text`].
///
/// Re-rendering the whole log on every compaction (the old behavior) made
/// the summarizer request grow with total session length until the cheap
/// model rejected it, and cost quadratic tokens on the way there. The result
/// is capped at [`SUMMARIZER_INPUT_MAX_BYTES`] by eliding the middle.
pub fn incremental_transcript_text(events: &[SessionEvent], skip_last_messages: usize) -> String {
    let last = events
        .iter()
        .rposition(|event| matches!(event, SessionEvent::Compaction { .. }));
    let text = match last {
        None => older_transcript_text(events, skip_last_messages),
        Some(idx) => {
            let SessionEvent::Compaction { summary, .. } = &events[idx] else {
                unreachable!("rposition matched a Compaction event");
            };
            // What the previous compaction summarized: the older portion of
            // the log as it stood when that compaction was appended.
            let (previously_older, _) = split_older_tail(&events[..idx], skip_last_messages);
            let start = previously_older.len();
            let (older, _) = split_older_tail(events, skip_last_messages);
            let mut out = String::new();
            push_line(&mut out, &format!("[COMPACTED] {summary}"));
            // `older` is a prefix of `events`; the new material is the part
            // of it past the previous boundary. Earlier compaction events in
            // that range are skipped: their state is in `summary` already.
            let fresh = render_events(older.into_iter().skip(start), true);
            out.push_str(&fresh);
            out
        }
    };
    elide_middle(&text, SUMMARIZER_INPUT_MAX_BYTES)
}

/// Keep the head and the tail of `text` within `max_bytes`, replacing the
/// middle with a marker. Cuts land on char boundaries.
fn elide_middle(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker = "\n[... middle of the transcript elided to fit the summarizer ...]\n";
    let keep = max_bytes.saturating_sub(marker.len()) / 2;
    let mut head_end = keep.min(text.len());
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - keep.min(text.len());
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    format!("{}{marker}{}", &text[..head_end], &text[tail_start..])
}

/// Render events with the rules documented on [`older_transcript_text`].
/// `skip_compactions` drops `Compaction` events (used when their state is
/// already represented by a leading `[COMPACTED]` line).
fn render_events<'a>(
    events: impl Iterator<Item = &'a SessionEvent>,
    skip_compactions: bool,
) -> String {
    let mut out = String::new();
    for event in events {
        match event {
            SessionEvent::TurnTrace { .. } => {}
            SessionEvent::Task { text } => {
                push_line(&mut out, &format!("[TASK] {text}"));
            }
            SessionEvent::Plan { items } => {
                // The latest plan survives compaction as its status table
                // (the run's tracked state must not vanish mid-goal).
                for item in items {
                    push_line(
                        &mut out,
                        &format!(
                            "[PLAN] {} {} - {}",
                            item.status, item.text, item.done_criteria
                        ),
                    );
                }
            }
            SessionEvent::Message { message, .. } => {
                if !message.content.trim().is_empty() {
                    let tag = match message.role {
                        Role::User => "[USER]",
                        Role::Assistant => "[ASSISTANT]",
                        Role::Tool => "[TOOL]",
                    };
                    push_line(&mut out, &format!("{tag} {}", message.content));
                }
                for call in &message.tool_calls {
                    let args =
                        serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string());
                    push_line(
                        &mut out,
                        &format!("[TOOL_CALL id={} name={}] {args}", call.id, call.name),
                    );
                }
            }
            SessionEvent::ToolResult { call_id, outcome } => {
                let marker = if outcome.is_error { " (error)" } else { "" };
                push_line(
                    &mut out,
                    &format!("[TOOL_RESULT id={call_id}]{marker} {}", outcome.content),
                );
            }
            SessionEvent::Compaction { summary, .. } => {
                if !skip_compactions {
                    push_line(&mut out, &format!("[COMPACTED] {summary}"));
                }
            }
            // Checkpoints carry no transcript content the summarizer needs —
            // the file state they protect is reflected in tool results.
            SessionEvent::Checkpoint { .. }
            | SessionEvent::ModelSelected { .. }
            | SessionEvent::Usage { .. } => {}
        }
    }
    out
}

/// Split session events into `(older, tail)` at the compaction boundary.
///
/// "Transcript items" are the `Task`/`Message`/`ToolResult` events — the ones
/// that carry conversation content. The tail is the last `skip_last_messages`
/// items; everything before the boundary is the older portion. Non-transcript
/// events (`ModelSelected`/`Usage`/`Compaction`) ride along on whichever side
/// of the boundary they fall.
///
/// Orphan rule: the verbatim tail must not BEGIN with a `ToolResult` whose
/// owning assistant message falls in the summarized portion (providers reject
/// a tool result with no preceding tool call). Leading tail tool results are
/// therefore folded into the older portion — their content is preserved in
/// the summary instead of being replayed verbatim.
pub fn split_older_tail(
    events: &[SessionEvent],
    skip_last_messages: usize,
) -> (Vec<&SessionEvent>, Vec<&SessionEvent>) {
    let items: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| is_transcript_item(event))
        .map(|(idx, _)| idx)
        .collect();
    let n = items.len();
    let mut tail_len = skip_last_messages.min(n);
    while tail_len > 0 && matches!(events[items[n - tail_len]], SessionEvent::ToolResult { .. }) {
        tail_len -= 1;
    }
    let boundary = if tail_len == 0 {
        events.len()
    } else {
        items[n - tail_len]
    };
    (
        events[..boundary].iter().collect(),
        events[boundary..].iter().collect(),
    )
}

/// Whether the event carries transcript content (counts toward the tail).
fn is_transcript_item(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Task { .. } | SessionEvent::Message { .. } | SessionEvent::ToolResult { .. }
    )
}

/// Marker appended by [`cap_summary`] when truncation occurs.
const TRUNCATION_MARKER: &str = "…[truncated]";

/// Cap `s` at `max_bytes`, truncating on a char boundary with a
/// `…[truncated]` marker (X2). A string within the cap is returned unchanged;
/// the capped result (marker included) never exceeds `max_bytes` unless the
/// cap is smaller than the marker itself, in which case the bare truncation
/// is returned.
pub fn cap_summary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    if max_bytes <= TRUNCATION_MARKER.len() {
        // No room for the marker; return the bare char-boundary truncation.
        return slice_char_boundary(s, max_bytes).to_string();
    }
    let keep = max_bytes - TRUNCATION_MARKER.len();
    let mut out = slice_char_boundary(s, keep).to_string();
    out.push_str(TRUNCATION_MARKER);
    out
}

/// Slice `s` to at most `max` bytes without splitting a UTF-8 code point.
fn slice_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimate_tokens;
    use crate::{Message, ToolCall, ToolOutcome, Usage};
    use serde_json::json;

    fn task_event(text: &str) -> SessionEvent {
        SessionEvent::Task { text: text.into() }
    }

    fn user_event(text: &str) -> SessionEvent {
        SessionEvent::Message {
            parent_id: None,
            message: Message::user(text),
        }
    }

    fn assistant_call_event(id: &str) -> SessionEvent {
        let mut message = Message::assistant("working on it");
        message.tool_calls = vec![ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: json!({"path": "a.rs"}),
        }];
        SessionEvent::Message {
            parent_id: None,
            message,
        }
    }

    fn tool_result_event(id: &str) -> SessionEvent {
        SessionEvent::ToolResult {
            call_id: id.into(),
            outcome: ToolOutcome::ok("ok"),
        }
    }

    /// items: [task, user, assistant(c1), tool(c1), user2] with accounting
    /// events interleaved; mirrors a real engine event log.
    fn sample_events() -> Vec<SessionEvent> {
        vec![
            task_event("fix it"),
            user_event("please"),
            SessionEvent::ModelSelected {
                model: "mock-model".into(),
                reason: "static test selector".into(),
                action: None,
                context_tokens: None,
            },
            SessionEvent::Usage {
                model: "mock-model".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                },
            },
            assistant_call_event("c1"),
            tool_result_event("c1"),
            user_event("thanks"),
        ]
    }

    #[test]
    fn summarizer_prompt_is_terse_and_stable() {
        let prompt = summarizer_prompt();
        assert_eq!(
            prompt,
            summarizer_prompt(),
            "prompt must be a stable constant"
        );
        for section in ["TASK:", "DECISIONS:", "EDITS:", "OPEN ITEMS:"] {
            assert!(prompt.contains(section), "missing section {section}");
        }
        let tokens = estimate_tokens(prompt);
        assert!(
            tokens < 512,
            "summarizer prompt too expensive: {tokens} tokens"
        );
    }

    #[test]
    fn older_transcript_renders_deterministically_and_skips_tail() {
        let events = sample_events();
        // Tail = last 3 items [assistant(c1), tool(c1), user2]; older = task + user.
        let text = older_transcript_text(&events, 3);
        assert_eq!(text, "[TASK] fix it\n[USER] please\n");

        // Accounting events are never rendered wherever they fall.
        assert!(!text.contains("mock-model"));
        assert!(!text.contains("input_tokens"));
    }

    #[test]
    fn older_transcript_renders_tool_calls_and_results() {
        let events = vec![
            task_event("t"),
            assistant_call_event("c9"),
            tool_result_event("c9"),
        ];
        assert_eq!(
            older_transcript_text(&events, 0),
            "[TASK] t\n\
             [ASSISTANT] working on it\n\
             [TOOL_CALL id=c9 name=read] {\"path\":\"a.rs\"}\n\
             [TOOL_RESULT id=c9] ok\n"
        );
    }

    #[test]
    fn older_transcript_folds_orphan_tail_tool_results() {
        // tail_messages = 1 would leave only the tool result in the tail; it
        // must be folded into the summarized portion instead (orphan rule).
        let events = vec![
            task_event("t"),
            assistant_call_event("c9"),
            tool_result_event("c9"),
        ];
        let (older, tail) = split_older_tail(&events, 1);
        assert_eq!(older.len(), 3);
        assert!(tail.is_empty());
        assert!(older_transcript_text(&events, 1).contains("[TOOL_RESULT id=c9]"));
    }

    #[test]
    fn older_transcript_skip_beyond_all_items_leaves_nothing_older() {
        let events = sample_events();
        // A skip larger than the item count means everything is recent tail:
        // nothing older remains to summarize (the engine skips the
        // compaction call in that case).
        assert_eq!(older_transcript_text(&events, 100), "");

        // skip = 0 renders every transcript item, in order.
        let text = older_transcript_text(&events, 0);
        assert!(text.contains("[TASK] fix it"));
        assert!(text.contains("[USER] please"));
        assert!(text.contains("[ASSISTANT] working on it"));
        assert!(text.contains("[TOOL_RESULT id=c1]"));
        assert!(text.contains("[USER] thanks"));
        // Accounting events are skipped wherever they fall.
        assert!(!text.contains("mock-model"));
        assert!(!text.contains("input_tokens"));
    }

    #[test]
    fn older_transcript_includes_prior_compaction_marker() {
        let mut events = sample_events();
        events.push(SessionEvent::Compaction {
            summary: "earlier state".into(),
            summarized_events: 2,
        });
        let text = older_transcript_text(&events, 0);
        assert!(text.contains("[COMPACTED] earlier state\n"));
    }

    #[test]
    fn empty_transcript_renders_empty() {
        assert_eq!(older_transcript_text(&[], 6), "");
        let events = vec![
            SessionEvent::ModelSelected {
                model: "m".into(),
                reason: "r".into(),
                action: None,
                context_tokens: None,
            },
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                },
            },
        ];
        assert_eq!(older_transcript_text(&events, 0), "");
    }

    #[test]
    fn cap_summary_passes_short_strings_through() {
        assert_eq!(cap_summary("hello", 100), "hello");
        assert_eq!(cap_summary("", 10), "");
        // Exactly at the cap is not over it.
        assert_eq!(cap_summary("abcde", 5), "abcde");
    }

    #[test]
    fn cap_summary_truncates_with_marker_at_byte_cap() {
        let long = "a".repeat(500);
        let capped = cap_summary(&long, 100);
        assert!(
            capped.len() <= 100,
            "capped length {} exceeds cap",
            capped.len()
        );
        assert!(capped.ends_with(TRUNCATION_MARKER));
        assert!(capped.starts_with(&"a".repeat(100 - TRUNCATION_MARKER.len())));
    }

    #[test]
    fn cap_summary_never_splits_a_code_point() {
        // 2 bytes per char; a naive byte cut would split one mid-sequence.
        let long = "é".repeat(200); // 400 bytes
        let capped = cap_summary(&long, 99);
        assert!(capped.len() <= 99);
        assert!(capped.ends_with(TRUNCATION_MARKER));
        let body = capped.trim_end_matches(TRUNCATION_MARKER);
        assert!(
            body.chars().all(|c| c == 'é'),
            "char boundary must hold: {body:?}"
        );
    }

    #[test]
    fn cap_summary_tiny_cap_returns_bare_truncation() {
        // Cap smaller than the marker: bare truncation, still within the cap.
        let capped = cap_summary("é".repeat(50).as_str(), 3);
        assert!(capped.len() <= 3);
        assert!(capped.chars().all(|c| c == 'é'));
    }

    #[test]
    fn incremental_text_without_a_prior_compaction_is_the_full_older_text() {
        let events = sample_events();
        assert_eq!(
            incremental_transcript_text(&events, 2),
            older_transcript_text(&events, 2)
        );
    }

    #[test]
    fn incremental_text_covers_only_what_the_last_compaction_did_not() {
        // Ten user turns, compact (tail 2), then six more turns.
        let mut events: Vec<SessionEvent> =
            (0..10).map(|i| user_event(&format!("early-{i}"))).collect();
        events.push(SessionEvent::Compaction {
            summary: "STATE-ONE".into(),
            summarized_events: 8,
        });
        events.extend((0..6).map(|i| user_event(&format!("late-{i}"))));

        let text = incremental_transcript_text(&events, 2);
        // The previous state leads; what it already covered is not re-read.
        assert!(text.starts_with("[COMPACTED] STATE-ONE\n"), "{text}");
        for i in 0..8 {
            assert!(
                !text.contains(&format!("early-{i}")),
                "early-{i} re-rendered: {text}"
            );
        }
        // The two turns that were the verbatim tail back then, plus the new
        // turns that have left the tail since, are the fresh material.
        for kept in ["early-8", "early-9", "late-0", "late-1", "late-2", "late-3"] {
            assert!(text.contains(kept), "{kept} missing: {text}");
        }
        // The current tail stays verbatim, so it is not summarized.
        assert!(
            !text.contains("late-4") && !text.contains("late-5"),
            "{text}"
        );
        // The compaction event itself is not rendered twice.
        assert_eq!(text.matches("[COMPACTED]").count(), 1, "{text}");
    }

    #[test]
    fn incremental_text_stays_bounded_across_many_compactions() {
        // Simulate a long session: every 20 turns a compaction is appended.
        let mut events: Vec<SessionEvent> = Vec::new();
        let mut sizes = Vec::new();
        for round in 0..12 {
            events.extend(
                (0..20).map(|i| user_event(&format!("round-{round}-turn-{i} {}", "x".repeat(200)))),
            );
            let text = incremental_transcript_text(&events, 4);
            sizes.push(text.len());
            events.push(SessionEvent::Compaction {
                summary: format!("STATE-{round}"),
                summarized_events: 0,
            });
        }
        // Old behavior grew linearly with the log; now every round after the
        // first renders about one round of turns.
        let first = sizes[1];
        let last = *sizes.last().unwrap();
        assert!(last <= first + 64, "summarizer input grew: {sizes:?}");
    }

    #[test]
    fn oversized_summarizer_input_is_elided_in_the_middle_on_char_boundaries() {
        let text = format!("HEAD{}TAIL", "\u{e9}".repeat(300_000));
        let cut = elide_middle(&text, 10_000);
        assert!(cut.len() <= 10_000, "{}", cut.len());
        assert!(cut.starts_with("HEAD") && cut.ends_with("TAIL"));
        assert!(cut.contains("middle of the transcript elided"));
        assert_eq!(elide_middle("short", 10_000), "short");
    }
}
