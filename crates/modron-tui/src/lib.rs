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
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};
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

/// A parsed slash command (dirge/Claude Code pattern): `/name arg`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Exit,
    Clear,
    Compact,
    Model(String),
    Context,
    Sessions,
    Sandbox(String),
    Unknown(String),
}

impl SlashCommand {
    /// Parse `/name arg` from input; None if input doesn't start with `/`.
    pub fn parse(input: &str) -> Option<Self> {
        let trimmed = input.trim();
        if !trimmed.starts_with('/') {
            return None;
        }
        let mut parts = trimmed[1..].splitn(2, ' ');
        let cmd = parts.next().unwrap_or_default();
        let arg = parts.next().unwrap_or_default().trim().to_string();
        Some(match cmd {
            "help" | "?" => SlashCommand::Help,
            "exit" | "quit" | "q" => SlashCommand::Exit,
            "clear" => SlashCommand::Clear,
            "compact" => SlashCommand::Compact,
            "model" => SlashCommand::Model(arg),
            "context" => SlashCommand::Context,
            "sessions" => SlashCommand::Sessions,
            "sandbox" => SlashCommand::Sandbox(arg),
            other => SlashCommand::Unknown(other.to_string()),
        })
    }

    /// The help text shown by /help.
    pub fn help_text() -> &'static str {
        "/help          show this help\n\
         /exit          quit the TUI\n\
         /clear         clear the scrollback\n\
         /compact       force a context compaction now\n\
         /model <name>  switch models mid-session\n\
         /context       show context usage breakdown\n\
         /sessions      list recent sessions\n\
         /sandbox <mode> off | seatbelt | bubblewrap\n\
         (anything else is sent as a task to the agent)"
    }
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
    /// Upgrade notice (Phase 6e): rendered in the status line when a newer
    /// release exists.
    pub upgrade_notice: Option<String>,
    /// Model label for the status line (set by the run loop).
    pub model_label: String,
    /// The model's context window (for the meter bar).
    pub context_window: u64,
    /// Estimated current context usage (tokens, updated per turn).
    pub context_used: u64,
    /// Live subagent rows (Phase 5h): id, label, turns, tokens, last_tool.
    pub subagents: Vec<(String, String, u32, u64, Option<String>)>,
}

/// Claude Code-inspired palette: a warm accent for activity, dim for meta.
pub const ACCENT: Color = Color::Rgb(255, 159, 10);
pub const DIM: Color = Color::DarkGray;

/// Mathematical spinner: rotating phase-space glyphs (quarter-circle phases,
/// like a phase portrait rotating) — deliberately NOT Claude Code's braille.
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
/// Working spinner: calculus operators cycling (partial → nabla → delta →
/// increment) — signals active computation.
const SPINNER_WORKING: [&str; 4] = ["∂", "∇", "∆", "∇"];

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
            KeyCode::Enter
                if key
                    .modifiers
                    .contains(crossterm::event::KeyModifiers::SHIFT) =>
            {
                // Shift+Enter: newline (multi-line input, the editor
                // convention every TUI uses).
                self.input.push('\n');
                Action::None
            }
            KeyCode::Enter => {
                let task = std::mem::take(&mut self.input);
                if task.trim().is_empty() {
                    return Action::None;
                }
                // Slash commands are handled locally (dirge pattern) —
                // they never reach the model.
                if let Some(cmd) = SlashCommand::parse(task.trim()) {
                    return self.handle_slash(cmd);
                }
                Action::Submit(task)
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

    /// Handle a slash command locally (Phase 10): returns an Action.
    /// Commands that need the run loop's help (quit, submit) map to Actions;
    /// others update local state and return None.
    fn handle_slash(&mut self, cmd: SlashCommand) -> Action {
        match cmd {
            SlashCommand::Exit => Action::Quit,
            SlashCommand::Help => {
                self.tool_lines.push(SlashCommand::help_text().to_string());
                Action::None
            }
            SlashCommand::Clear => {
                self.tool_lines.clear();
                self.streaming.clear();
                Action::None
            }
            SlashCommand::Compact => {
                // Signal the run loop to force a compaction before the next
                // turn; the engine handles it when context is over threshold.
                self.tool_lines.push(
                    "(compaction will trigger on the next turn if context is over threshold)"
                        .to_string(),
                );
                Action::None
            }
            SlashCommand::Model(name) => {
                if name.is_empty() {
                    self.tool_lines
                        .push(format!("current model: {}", self.model_label));
                    self.tool_lines
                        .push("usage: /model glm-5.3-flash".to_string());
                } else {
                    self.model_label = name.clone();
                    self.tool_lines.push(format!(
                        "switched to {name} (takes effect on the next turn)"
                    ));
                }
                Action::None
            }
            SlashCommand::Context => {
                let total = self.cumulative.total();
                self.tool_lines.push(format!(
                    "context usage: {total} tokens this session (input + output)\nmodel window: check /model for the current model\nest. remaining: the engine compacts at 80% of the model window"
                ));
                Action::None
            }
            SlashCommand::Sessions => {
                self.tool_lines
                    .push("(session listing: run `modron sessions` outside the TUI)".to_string());
                Action::None
            }
            SlashCommand::Sandbox(mode) => {
                self.tool_lines.push(format!(
                    "sandbox: {mode} (takes effect on restart — the sandbox is set at launch)"
                ));
                Action::None
            }
            SlashCommand::Unknown(name) => {
                self.tool_lines
                    .push(format!("unknown command /{name} — try /help"));
                Action::None
            }
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
    ) -> Vec<Line<'static>> {
        self.running = false;
        self.streaming.clear();
        self.current_tool = None;
        self.turn_count = turns;
        self.session_id = Some(session_id);
        self.cumulative.input_tokens += usage.input_tokens;
        self.cumulative.output_tokens += usage.output_tokens;
        self.cumulative.cached_input_tokens += usage.cached_input_tokens;

        // Styled scrollback: tool lines dim-cyan, answer default, meta dim.
        let mut lines: Vec<Line<'static>> = Vec::new();
        for tool in std::mem::take(&mut self.tool_lines) {
            lines.push(Line::from(tool).style(Style::default().fg(Color::Cyan)));
        }
        if answer.is_empty() {
            lines.push(
                Line::from("(no answer — turn/budget limit hit; see transcript)")
                    .style(Style::default().fg(Color::Yellow)),
            );
        } else {
            for line in answer.lines() {
                lines.push(Line::from(line.to_string()));
            }
        }
        lines.push(
            Line::from(format!(
                "  {} tok · {} turns · {} cache",
                usage.total(),
                turns,
                {
                    let all = usage.input_tokens + usage.cached_input_tokens;
                    if all > 0 {
                        format!(
                            "{:.0}%",
                            100.0 * usage.cached_input_tokens as f64 / all as f64
                        )
                    } else {
                        "—".to_string()
                    }
                }
            ))
            .style(Style::default().fg(DIM)),
        );
        lines.push(Line::from(""));
        lines
    }

    /// The turn was interrupted: flush partial tool lines + a note.
    pub fn finish_interrupted(&mut self) -> Vec<Line<'static>> {
        self.running = false;
        self.streaming.clear();
        self.current_tool = None;
        self.status_note =
            Some("interrupted — partial work is persisted; resume with --resume".into());
        let mut lines: Vec<Line<'static>> = Vec::new();
        for tool in std::mem::take(&mut self.tool_lines) {
            lines.push(Line::from(tool).style(Style::default().fg(Color::Cyan)));
        }
        lines.push(
            Line::from("(interrupted — partial work is persisted; resume with --resume)")
                .style(Style::default().fg(Color::Yellow)),
        );
        lines.push(Line::from(""));
        lines
    }

    /// Begin a submitted task.
    pub fn begin_task(&mut self, task: &str) {
        self.running = true;
        self.streaming.clear();
        self.status_note = None;
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
    /// Claude Code-style: streaming/status zone on top, a rounded bordered
    /// input box at the bottom, color throughout (accent for activity, dim
    /// for meta, cyan for tools, red for errors).
    pub fn view(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        use ratatui::layout::{Constraint, Layout};
        use ratatui::widgets::Paragraph;

        // Split: everything above vs the bordered input box (3 rows).
        // Input box height grows with multi-line content (3 base + 1/line).
        let input_lines = self.input.matches('\n').count() as u16;
        let input_height = (3 + input_lines).min(area.height.saturating_sub(2));
        let outer =
            Layout::vertical([Constraint::Min(1), Constraint::Length(input_height)]).split(area);
        let top = outer[0];
        let input_area = outer[1];

        // Top zone rows: streaming (fills), subagents, approval/status.
        let mut rows: Vec<Constraint> = Vec::new();
        if !self.streaming.is_empty() {
            rows.push(Constraint::Min(1));
        }
        if !self.subagents.is_empty() {
            rows.push(Constraint::Length(self.subagents.len() as u16));
        }
        if self.pending_approval.is_some() {
            rows.push(Constraint::Length(2));
        } else {
            rows.push(Constraint::Length(1));
        }
        let chunks = Layout::vertical(rows).split(top);

        let mut idx = 0;
        if !self.streaming.is_empty() {
            let tail = streaming_tail(
                &self.streaming,
                chunks[idx].height as usize,
                chunks[idx].width as usize,
            );
            frame.render_widget(Paragraph::new(tail), chunks[idx]);
            idx += 1;
        }

        if !self.subagents.is_empty() {
            let lines: Vec<Line> = self
                .subagents
                .iter()
                .map(|(id, label, turns, tokens, last)| {
                    let (state_str, color) = match last {
                        Some(t) if t == "done" => (t.clone(), Color::Green),
                        Some(t) if t == "failed" => (t.clone(), Color::Red),
                        _ => (format!("turn {turns} · {tokens} tok"), ACCENT),
                    };
                    let display = if label.is_empty() {
                        id.clone()
                    } else {
                        label.clone()
                    };
                    Line::from(vec![
                        Span::styled("  ⏵ ", Style::default().fg(ACCENT)),
                        Span::styled(format!("[{display}] "), Style::default().fg(DIM)),
                        Span::styled(state_str, Style::default().fg(color)),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            idx += 1;
        }

        if self.pending_approval.is_some() {
            let hint = "y = allow once · a = allow for this run · n/Esc = deny";
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(self.status_line()).style(
                        Style::default()
                            .add_modifier(Modifier::BOLD)
                            .fg(Color::Yellow),
                    ),
                    Line::from(hint).style(Style::default().fg(DIM)),
                ]),
                chunks[idx],
            );
        } else {
            let status = self.styled_status_line();
            frame.render_widget(Paragraph::new(status), chunks[idx]);
        }

        // ── Bordered input box ──
        let border_color = if self.running { ACCENT } else { DIM };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(Span::styled(
                if self.running {
                    " modron — working (esc to interrupt) "
                } else {
                    " modron "
                },
                Style::default().fg(border_color),
            ));
        let inner = block.inner(input_area);
        frame.render_widget(block, input_area);

        // Input text; cursor rendered as a block at the text end.
        let display_input = if self.input.is_empty() && !self.running {
            "type a task…" // placeholder
        } else {
            &self.input
        };
        let input_style = if self.input.is_empty() && !self.running {
            Style::default().fg(DIM)
        } else {
            Style::default()
        };
        // Multi-line input: split on \n, render each as its own line with
        // the prompt on the first.
        let lines: Vec<Line> = if self.input.contains('\n') {
            self.input
                .split('\n')
                .enumerate()
                .map(|(i, line)| {
                    if i == 0 {
                        Line::from(vec![
                            Span::styled("❯ ", Style::default().fg(ACCENT)),
                            Span::styled(line.to_string(), Style::default()),
                        ])
                    } else {
                        Line::from(format!("  {line}"))
                    }
                })
                .collect()
        } else {
            vec![Line::from(vec![
                Span::styled("❯ ", Style::default().fg(ACCENT)),
                Span::styled(display_input.to_string(), input_style),
            ])]
        };
        frame.render_widget(Paragraph::new(lines), inner);
        // Blinking-style cursor block after the text (only when not running
        // and there's no placeholder shift).
        if !self.running && !self.input.is_empty() {
            let cursor_x = inner.x + 2 + self.input.chars().count() as u16;
            let cursor_y = inner.y;
            if cursor_x < inner.x + inner.width {
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut(ratatui::layout::Position::new(cursor_x, cursor_y))
                {
                    cell.set_symbol("▎").set_style(Style::default().fg(ACCENT));
                }
            }
        }
    }

    /// The status line as styled spans (spinner accent, model dim, tokens).
    /// A visual context-usage bar (dirge pattern): `▓▓▓░░░░░░░ 30%` —
    /// shows a compact warning at 80%+ (the compaction trigger).
    pub fn context_meter(&self) -> String {
        if self.context_window == 0 {
            return String::new();
        }
        let frac = (self.context_used as f64 / self.context_window as f64).min(1.0);
        let filled = (frac * 10.0).round() as usize;
        let bar: String = "▓".repeat(filled) + &"░".repeat(10 - filled);
        let pct = (frac * 100.0) as u64;
        let marker = if pct >= 80 { " !compact" } else { "" };
        format!("{bar} {pct}%{marker}")
    }

    fn styled_status_line(&self) -> Line<'static> {
        let spinner = if self.running {
            SPINNER_WORKING[(self.spinner_tick as usize) % SPINNER_WORKING.len()]
        } else {
            SPINNER[(self.spinner_tick as usize) % SPINNER.len()]
        };
        if self.running {
            let tool = self
                .current_tool
                .as_ref()
                .map(|t| format!(" · {t}"))
                .unwrap_or_default();
            Line::from(vec![
                Span::styled(format!("{spinner} "), Style::default().fg(ACCENT)),
                Span::styled(self.model_label.clone(), Style::default().fg(DIM)),
                Span::styled(
                    format!(
                        " turn {} · {} tok{tool}",
                        self.turn_count + 1,
                        self.cumulative.total()
                    ),
                    Style::default().fg(DIM),
                ),
                Span::styled("  esc interrupt", Style::default().fg(DIM)),
            ])
        } else {
            let meter = self.context_meter();
            Line::from(vec![
                Span::styled(format!("{spinner} "), Style::default().fg(ACCENT)),
                Span::styled(self.model_label.clone(), Style::default().fg(DIM)),
                Span::styled(
                    format!(
                        " · {} tok · {meter} ctrl+o {}",
                        self.cumulative.total(),
                        if self.transcript_mode {
                            "full"
                        } else {
                            "compact"
                        }
                    ),
                    Style::default().fg(DIM),
                ),
            ])
        }
    }
}

/// Last `max_lines` lines of streamed text, each hard-wrapped to `width`
/// chars (manual wrapping: ratatui's Paragraph wrap inside an inline
/// viewport is a known misbehavior source — see LEDGER Phase 1d).
pub fn streaming_tail(text: &str, max_lines: usize, width: usize) -> Vec<Line<'static>> {
    let width = width.max(20); // floor for pathological cases
    let mut wrapped: Vec<String> = Vec::new();
    for line in text.lines() {
        let chars: Vec<char> = line.chars().collect();
        if chars.len() <= width {
            wrapped.push(line.to_string());
        } else {
            for chunk in chars.chunks(width) {
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
