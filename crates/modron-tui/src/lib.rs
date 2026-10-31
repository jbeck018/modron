//! modron-tui — the inline-viewport terminal UI (Phase 1d).
//!
//! Design lessons baked in (see LEDGER Phase 1d): Claude Code's inline
//! pattern — completed content is committed to the native scrollback via
//! `Terminal::insert_before`, and only the live region (status, streaming
//! tail, input, approval prompt) is the managed viewport, so output stays
//! selectable and terminal-scrollable. No alternate screen; no 60fps chase.
//! Esc interrupts the running turn; ctrl+o toggles transcript detail;
//! approvals render inline with y/n/a.
//!
//! This module is the pure state machine (`App`) — no TTY required to test.
//! The terminal loop lives in [`run`].

pub mod run;
pub use run::{run_with, TuiConfig, TuiSinks};

use modron_core::{StreamDelta, ToolActivity, ToolCall, Usage};
use ratatui::text::Line;
use std::sync::mpsc::Sender;

/// What the user's last key press asks the run loop to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep going (state changed, redraw).
    None,
    /// Submit the current input as a task (input is taken).
    Submit(String),
    /// Interrupt the running turn (Esc). Partial events stay persisted.
    Abort,
    /// Quit the TUI (ctrl+c while idle; ctrl+c during a turn aborts first).
    Quit,
}

/// A tool call awaiting the user's y/n/a decision.
pub struct PendingApproval {
    pub call: ToolCall,
    pub respond: Sender<bool>,
}

/// The live region's state machine. Pure: events in, [`Action`] out.
#[derive(Default)]
pub struct App {
    pub input: String,
    /// Is a task running? (An approval can be pending while running.)
    pub running: bool,
    /// Assistant text streamed so far this turn.
    pub streaming: String,
    /// Completed tool lines this turn (flushed to scrollback on turn end).
    pub tool_lines: Vec<String>,
    /// Tool currently executing (for the spinner line).
    pub current_tool: Option<String>,
    pub pending_approval: Option<PendingApproval>,
    /// Sticky "a" (approve the rest of the run) — the run loop mirrors this
    /// into the approver's shared flag.
    pub approve_rest: bool,
    pub cumulative: Usage,
    pub turn_count: u32,
    pub session_id: Option<modron_core::SessionId>,
    pub transcript_mode: bool,
    pub status_note: Option<String>,
    pub quit: bool,
    /// Braille spinner frame counter (advanced per tick).
    pub spinner_tick: u64,
    /// Live subagent rows (Phase 5h): id, label, turns, tokens, last_tool.
    pub subagents: Vec<(String, String, u32, u64, Option<String>)>,
}

const SPINNER: [&str; 8] = ["✶", "✻", "✽", "✶", "✳", "✻", "✼", "✽"];

impl App {
    /// Apply a key press. Key filtering (releases vs presses) is the run
    /// loop's job; this sees presses only.
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyCode;
        // Global toggles, any mode.
        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        {
            match key.code {
                KeyCode::Char('o') | KeyCode::Char('O') => {
                    self.transcript_mode = !self.transcript_mode;
                    return Action::None;
                }
                KeyCode::Char('c') => {
                    if self.running {
                        // Abort now; the run loop quits after the abort lands.
                        return Action::Abort;
                    }
                    self.quit = true;
                    return Action::Quit;
                }
                _ => {}
            }
        }

        // Approval modal: keys route to the decision, not the input line.
        if let Some(pending) = self.pending_approval.take() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let _ = pending.respond.send(true);
                    return Action::None;
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.approve_rest = true;
                    let _ = pending.respond.send(true);
                    return Action::None;
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    let _ = pending.respond.send(false);
                    return Action::None;
                }
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    let _ = pending.respond.send(false);
                    return Action::Quit;
                }
                _ => {
                    // Not a decision key: put the approval back.
                    self.pending_approval = Some(pending);
                    return Action::None;
                }
            }
        }

        // Esc while running interrupts (type-ahead edits land in the input
        // for the next task).
        if self.running && key.code == KeyCode::Esc {
            return Action::Abort;
        }

        match key.code {
            KeyCode::Enter => {
                let task = std::mem::take(&mut self.input);
                if task.trim().is_empty() {
                    Action::None
                } else {
                    Action::Submit(task)
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
                Action::None
            }
            KeyCode::Esc if !self.running => {
                self.input.clear();
                Action::None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Apply a streamed delta.
    pub fn on_delta(&mut self, delta: StreamDelta) {
        match delta {
            StreamDelta::Text(t) => self.streaming.push_str(&t),
            StreamDelta::Reasoning(_) => {}
            StreamDelta::Done => {}
        }
    }

    /// Apply a tool-activity event (live `●` lines).
    pub fn on_tool(&mut self, activity: ToolActivity) {
        match activity {
            ToolActivity::Start { name } => {
                self.current_tool = Some(name.clone());
                self.tool_lines.push(format!("● {name} …"));
            }
            ToolActivity::Done {
                name,
                is_error,
                bytes,
            } => {
                self.current_tool = None;
                let mark = if is_error { "error" } else { "done" };
                self.tool_lines
                    .push(format!("● {name} — {mark} ({bytes} B)"));
            }
            ToolActivity::SubagentStarted { id, label } => {
                self.subagents.push((id, label, 0, 0, None));
            }
            ToolActivity::SubagentProgress {
                id,
                turns,
                tokens,
                last_tool,
                ..
            } => {
                if let Some(row) = self.subagents.iter_mut().find(|(rid, ..)| *rid == id) {
                    row.2 = row.2.max(turns);
                    row.3 = row.3.max(tokens);
                    row.4 = last_tool.clone();
                }
            }
            ToolActivity::SubagentFinished {
                id,
                tokens,
                turns,
                is_error,
                ..
            } => {
                if let Some(row) = self.subagents.iter_mut().find(|(rid, ..)| *rid == id) {
                    row.2 = turns;
                    row.3 = tokens;
                    row.4 = Some(if is_error {
                        "failed".into()
                    } else {
                        "done".into()
                    });
                }
            }
        }
    }

    /// Drop settled subagent rows (called when a turn finishes).
    pub fn clear_subagents(&mut self) {
        self.subagents.clear();
    }

    /// A turn ended: returns the lines to flush into scrollback (tool
    /// activity, then the answer — or the interruption note).
    pub fn finish_turn(
        &mut self,
        answer: String,
        usage: Usage,
        turns: u32,
        _tool_calls: u32,
        session_id: modron_core::SessionId,
    ) -> Vec<String> {
        self.running = false;
        self.streaming.clear();
        self.current_tool = None;
        self.turn_count = turns;
        self.session_id = Some(session_id);
        self.cumulative.input_tokens += usage.input_tokens;
        self.cumulative.output_tokens += usage.output_tokens;
        self.cumulative.cached_input_tokens += usage.cached_input_tokens;

        let mut lines = std::mem::take(&mut self.tool_lines);
        if answer.is_empty() {
            lines.push("(no answer — turn/budget limit hit; see transcript)".to_string());
        } else {
            for line in answer.lines() {
                lines.push(line.to_string());
            }
        }
        lines.push(String::new()); // blank separator before the next prompt
        lines
    }

    /// The turn was interrupted: flush partial tool lines + a note.
    pub fn finish_interrupted(&mut self) -> Vec<String> {
        self.running = false;
        self.streaming.clear();
        self.current_tool = None;
        self.status_note =
            Some("interrupted — partial work is persisted; resume with --resume".into());
        let mut lines = std::mem::take(&mut self.tool_lines);
        lines.push("(interrupted — partial work is persisted)".to_string());
        lines.push(String::new());
        lines
    }

    /// Begin a submitted task.
    pub fn begin_task(&mut self, task: &str) {
        self.running = true;
        self.streaming.clear();
        self.status_note = None;
        // Echo the submitted task into the scrollback (the run loop owns the
        // flush; recorded here so the view can show what is running).
        self.tool_lines.push(format!("❯ {task}"));
    }

    /// The one-line status: spinner/model, tool in flight, or approval prompt.
    pub fn status_line(&self) -> String {
        if let Some(pending) = &self.pending_approval {
            return format!(
                "⚠ allow {} {}? [y/N/a]",
                pending.call.name,
                short(&pending.call.arguments.to_string(), 80)
            );
        }
        let spinner = SPINNER[(self.spinner_tick as usize) % SPINNER.len()];
        if self.running {
            let tool = match &self.current_tool {
                Some(t) => format!(" · {t}"),
                None => String::new(),
            };
            return format!(
                "{spinner} running (turn {} · {} tok{}){} — esc to interrupt",
                self.turn_count + 1,
                self.cumulative.total(),
                tool,
                self.status_note
                    .as_ref()
                    .map(|n| format!(" · {n}"))
                    .unwrap_or_default()
            );
        }
        format!(
            "{spinner} idle · {} tok · {} · ctrl+o {} · esc clears input",
            self.cumulative.total(),
            self.session_id
                .map(|id| format!("session {}", &id.to_string()[..8]))
                .unwrap_or_else(|| "no session".into()),
            if self.transcript_mode {
                "full"
            } else {
                "compact"
            }
        )
    }

    /// Render the live region into `frame` (the run loop sizes the area).
    pub fn view(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::layout::{Constraint, Layout};
        use ratatui::style::{Modifier, Style};
        use ratatui::widgets::Paragraph;

        let show_streaming = !self.streaming.is_empty();
        let show_subagents = !self.subagents.is_empty();
        let mut rows: Vec<Constraint> = Vec::new();
        if show_streaming {
            rows.push(Constraint::Min(1)); // streaming tail
        }
        if show_subagents {
            rows.push(Constraint::Length(self.subagents.len() as u16));
        }
        if self.pending_approval.is_some() {
            rows.push(Constraint::Length(2)); // prompt + decision hint
        } else {
            rows.push(Constraint::Length(1)); // status
        }
        rows.push(Constraint::Length(1)); // input
        let chunks = Layout::vertical(rows).split(area);

        let mut idx = 0;
        if show_streaming {
            let tail = streaming_tail(&self.streaming, chunks[idx].height as usize);
            frame.render_widget(Paragraph::new(tail).style(Style::default()), chunks[idx]);
            idx += 1;
        }

        if show_subagents {
            let rows: Vec<Line> = self
                .subagents
                .iter()
                .map(|(id, label, turns, tokens, last)| {
                    let state = match last {
                        Some(t) if t == "done" || t == "failed" => t.clone(),
                        _ => format!("turn {turns} · {tokens} tok"),
                    };
                    let display = if label.is_empty() {
                        id.clone()
                    } else {
                        label.clone()
                    };
                    Line::from(format!("  ⏵ [{display}] {state}"))
                })
                .collect();
            frame.render_widget(Paragraph::new(rows), chunks[idx]);
            idx += 1;
        }

        let status = self.status_line();
        if self.pending_approval.is_some() {
            // Approval prompt replaces the status; decision hint on its own line.
            let hint = "y = allow once · a = allow for this run · n/Esc = deny";
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(status.clone()).style(Style::default().add_modifier(Modifier::BOLD)),
                    Line::from(hint.to_string()),
                ]),
                chunks[idx],
            );
            idx += 1;
        } else {
            frame.render_widget(Paragraph::new(vec![Line::from(status)]), chunks[idx]);
            idx += 1;
        }

        let prompt = if self.running { "  " } else { "❯ " };
        frame.render_widget(
            Paragraph::new(vec![Line::from(format!("{prompt}{}", self.input))]),
            chunks[idx],
        );
    }
}

/// Last `max_lines` lines of streamed text, each hard-wrapped to `width`
/// chars (manual wrapping: ratatui's Paragraph wrap inside an inline
/// viewport is a known misbehavior source — see LEDGER Phase 1d).
pub fn streaming_tail(text: &str, max_lines: usize) -> Vec<Line<'static>> {
    use ratatui::text::Line;
    const WIDTH: usize = 110;
    let mut wrapped: Vec<String> = Vec::new();
    for line in text.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.len() <= WIDTH {
            wrapped.push(line.to_string());
        } else {
            for chunk in chars.chunks(WIDTH) {
                wrapped.push(chunk.iter().collect());
            }
        }
    }
    let start = wrapped.len().saturating_sub(max_lines);
    wrapped[start..]
        .iter()
        .map(|l| Line::from(l.clone()))
        .collect()
}

fn short(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…")
    }
}

/// Lines to commit to scrollback via `Terminal::insert_before`.
pub fn to_insert_lines(lines: Vec<String>) -> Vec<Line<'static>> {
    lines.into_iter().map(Line::from).collect()
}
