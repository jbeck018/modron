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
pub mod theme;
pub use theme::Theme;
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
    /// Toggle rendering of streamed model reasoning.
    Reasoning,
    /// Quick side question — answered without tools.
    Btw(String),
    /// List personas or switch (`/agent reviewer`).
    Agent(String),
    /// Session tool allow/deny lists (`/allow bash`, `/deny write`).
    Allow(String),
    Deny(String),
    /// Approval mode: ask | yolo.
    Mode(String),
    /// Background delegate status.
    Tasks,
    /// List delegate worktrees.
    Worktree,
    /// Issue board: list, or `/issues add <text>`.
    Issues(String),
    /// Branch the conversation (fork the session log).
    Fork,
    /// Plan-first mode: submit a task constrained to planning.
    Plan(String),
    /// Review the working diff.
    CodeReview,
    /// Set/clear a natural-language stop condition for long runs.
    Goal(String),
    /// Distill the current session into a reusable SKILL.md.
    Learn(String),
    /// Switch color theme (built-ins + ~/.config/modron/themes/*.json).
    Theme(String),
    /// Alias kept for muscle memory.
    Compress,
    Unknown(String),
}

/// A slash command that needs the run loop's powers (engine mutation, tool
/// execution, store access). `App::handle_slash` parks one here; the loop
/// drains and executes it, then commits the result lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingCommand {
    Agent(String),
    Allow(String),
    Deny(String),
    Mode(String),
    Tasks,
    Worktree,
    Issues(String),
    Fork,
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
            "reasoning" | "thinking" => SlashCommand::Reasoning,
            "btw" => SlashCommand::Btw(arg),
            "agent" | "persona" => SlashCommand::Agent(arg),
            "allow" => SlashCommand::Allow(arg),
            "deny" => SlashCommand::Deny(arg),
            "mode" => SlashCommand::Mode(arg),
            "tasks" => SlashCommand::Tasks,
            "worktree" | "worktrees" => SlashCommand::Worktree,
            "issues" => SlashCommand::Issues(arg),
            "fork" => SlashCommand::Fork,
            "plan" => SlashCommand::Plan(arg),
            "code-review" | "review" => SlashCommand::CodeReview,
            "goal" => SlashCommand::Goal(arg),
            "learn" => SlashCommand::Learn(arg),
            "theme" => SlashCommand::Theme(arg),
            "compress" => SlashCommand::Compact,
            other => SlashCommand::Unknown(other.to_string()),
        })
    }

    /// The help text shown by /help.
    pub fn help_text() -> &'static str {
        "/help            show this help\n\
         /exit            quit the TUI\n\
         /clear           clear the scrollback\n\
         /compact         force a context compaction now\n\
         /model <name>    switch models mid-session\n\
         /context         show context usage breakdown\n\
         /sessions        list recent sessions\n\
         /sandbox <mode>  off | seatbelt | bubblewrap\n\
         /reasoning       toggle streamed-reasoning display\n\
         /btw <question>  quick side question, no tools\n\
         /agent [name]    list or switch personas\n\
         /allow|/deny <tool>  session tool permissions\n\
         /mode ask|yolo   approval mode\n\
         /tasks           background delegate status\n\
         /worktree        list delegate worktrees\n\
         /issues [add <text>]  the project issue board\n\
         /fork            branch this conversation\n\
         /plan <task>     plan first, no edits until approved\n\
         /review          review the working diff\n\
         /goal <condition>  stop condition for long autonomous runs\n\
         /learn [title]   distill this session into a SKILL.md\n\
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
    /// A slash command awaiting run-loop execution (engine/store powers).
    pub pending_command: Option<PendingCommand>,
    /// Streamed model reasoning (shown when `show_reasoning`).
    pub reasoning: String,
    pub show_reasoning: bool,
    /// Natural-language stop condition (/goal), attached to subsequent
    /// tasks until cleared with a bare `/goal`.
    pub goal: Option<String>,
    /// Active color theme (/theme <name> switches live).
    pub theme: Theme,
}

/// Claude Code-inspired palette: a warm accent for activity, dim for meta.
/// These are the DEFAULT-THEME values; live rendering goes through
/// `App.theme` (switchable with /theme).
pub const ACCENT: Color = Color::Rgb(255, 159, 10);
pub const DIM: Color = Color::DarkGray;

/// Where user theme JSON files live (MODRON_CONFIG or ~/.config/modron/themes).
pub fn theme_dir() -> std::path::PathBuf {
    std::env::var("MODRON_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".config")
                .join("modron")
        })
        .join("themes")
}

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
                // Ctrl+J is raw LF — what a terminal sends when shift+enter
                // is remapped to "\n" (the classic iTerm2 workaround). In
                // raw mode crossterm reports it as exactly this.
                KeyCode::Char('j') | KeyCode::Char('J') => {
                    self.input.push('\n');
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
                    .intersects(
                        crossterm::event::KeyModifiers::SHIFT
                            | crossterm::event::KeyModifiers::ALT,
                    ) =>
            {
                // Newline, not submit. Shift+enter only reaches us with the
                // SHIFT modifier on terminals that speak the kitty keyboard
                // protocol (run.rs enables it when available); option/alt+
                // enter sends ESC+CR everywhere else, which crossterm
                // reports as Enter with ALT.
                self.input.push('\n');
                Action::None
            }
            KeyCode::Enter => {
                // Backslash+Enter: line continuation — the universal
                // fallback that works on every terminal (Claude Code's
                // convention for terminals that cannot report shift+enter).
                if self.input.ends_with('\\') {
                    self.input.pop();
                    self.input.push('\n');
                    return Action::None;
                }
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

    /// Push text as one tool line per row (a committed line renders as one
    /// screen row, so embedded newlines must be split).
    fn push_lines(&mut self, text: impl Into<String>) {
        for line in text.into().lines() {
            self.tool_lines.push(line.to_string());
        }
    }

    /// Handle a slash command locally (Phase 10): returns an Action.
    #[doc(hidden)]
    pub fn handle_slash_test(&mut self, cmd: SlashCommand) -> Action {
        self.handle_slash(cmd)
    }

    /// Handle a slash command locally (Phase 10): returns an Action.
    /// Commands that need the run loop's help (quit, submit) map to Actions;
    /// others update local state and return None.
    fn handle_slash(&mut self, cmd: SlashCommand) -> Action {
        match cmd {
            SlashCommand::Exit => Action::Quit,
            SlashCommand::Help => {
                self.push_lines(SlashCommand::help_text());
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
                self.push_lines(format!(
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
            SlashCommand::Reasoning => {
                self.show_reasoning = !self.show_reasoning;
                self.reasoning.clear();
                self.tool_lines.push(format!(
                    "reasoning display: {}",
                    if self.show_reasoning { "on" } else { "off" }
                ));
                Action::None
            }
            SlashCommand::Btw(question) => {
                if question.trim().is_empty() {
                    self.tool_lines.push("usage: /btw <quick question>".into());
                    return Action::None;
                }
                Action::Submit(format!(
                    "{question}\n\n(Quick side question: answer directly from what you \
                     already know — do not use tools.)"
                ))
            }
            SlashCommand::Agent(name) => {
                self.pending_command = Some(PendingCommand::Agent(name));
                Action::None
            }
            SlashCommand::Allow(tool) => {
                self.pending_command = Some(PendingCommand::Allow(tool));
                Action::None
            }
            SlashCommand::Deny(tool) => {
                self.pending_command = Some(PendingCommand::Deny(tool));
                Action::None
            }
            SlashCommand::Mode(mode) => {
                self.pending_command = Some(PendingCommand::Mode(mode));
                Action::None
            }
            SlashCommand::Tasks => {
                self.pending_command = Some(PendingCommand::Tasks);
                Action::None
            }
            SlashCommand::Worktree => {
                self.pending_command = Some(PendingCommand::Worktree);
                Action::None
            }
            SlashCommand::Issues(arg) => {
                self.pending_command = Some(PendingCommand::Issues(arg));
                Action::None
            }
            SlashCommand::Fork => {
                self.pending_command = Some(PendingCommand::Fork);
                Action::None
            }
            SlashCommand::Plan(task) => {
                if task.trim().is_empty() {
                    self.tool_lines.push("usage: /plan <task>".into());
                    return Action::None;
                }
                Action::Submit(format!(
                    "{task}\n\n(Plan first: produce a concrete, step-by-step plan with file \
                     references. Do NOT edit any files yet — wait for approval.)"
                ))
            }
            SlashCommand::CodeReview => Action::Submit(
                "Review the working tree: run `git diff` (and `git status`), then report \
                 findings only — bugs, risks, and gaps, each with file:line references. \
                 Do not fix anything."
                    .into(),
            ),
            SlashCommand::Goal(condition) => {
                if condition.trim().is_empty() {
                    self.goal = None;
                    self.tool_lines.push("goal cleared — tasks run normally again".into());
                } else {
                    self.goal = Some(condition.trim().to_string());
                    self.push_lines(format!(
                        "goal set: {}\n(every task now runs until you say it's met, the \
                         budget caps hit, or the model reports GOAL COMPLETE; clear with a \
                         bare /goal)",
                        condition.trim()
                    ));
                }
                Action::None
            }
            SlashCommand::Learn(title) => {
                let slug = slugish(if title.is_empty() { "session-skill" } else { &title });
                Action::Submit(format!(
                    "Distill this session into a reusable skill. Write the file \
                     .modron/skills/{slug}/SKILL.md with `---` frontmatter (name: {slug}, \
                     description: one line) followed by a concise, step-by-step workflow \
                     capturing what actually worked here (commands, file paths, pitfalls). \
                     Create the directories. Then reply with just the path you wrote."
                ))
            }
            SlashCommand::Theme(name) => {
                let name = name.trim();
                if name.is_empty() {
                    let dir = crate::theme_dir();
                    let names = Theme::available(&dir);
                    self.push_lines(format!(
                        "themes: {} (switch: /theme <name>; drop <name>.json in {})",
                        names.join(", "),
                        dir.display()
                    ));
                } else if Theme::builtin_names().contains(&name) {
                    self.theme = Theme::builtin(name);
                    self.tool_lines.push(format!("theme: {name}"));
                } else {
                    match Theme::load(&crate::theme_dir(), name) {
                        Some(theme) => {
                            self.theme = theme;
                            self.tool_lines.push(format!("theme: {name}"));
                        }
                        None => self.tool_lines.push(format!(
                            "unknown theme {name:?} — available: {}",
                            Theme::available(&crate::theme_dir()).join(", ")
                        )),
                    }
                }
                Action::None
            }
            SlashCommand::Compress => self.handle_slash(SlashCommand::Compact),
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
            StreamDelta::Reasoning(t) => {
                if self.show_reasoning {
                    self.reasoning.push_str(&t);
                }
            }
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
        let t = self.theme.resolved();
        self.running = false;
        self.streaming.clear();
        self.reasoning.clear();
        self.current_tool = None;
        self.turn_count = turns;
        self.session_id = Some(session_id);
        self.cumulative.input_tokens += usage.input_tokens;
        self.cumulative.output_tokens += usage.output_tokens;
        self.cumulative.cached_input_tokens += usage.cached_input_tokens;

        // Styled scrollback: tool lines dim-cyan, answer default, meta dim.
        let mut lines: Vec<Line<'static>> = Vec::new();
        for tool in std::mem::take(&mut self.tool_lines) {
            lines.push(Line::from(tool).style(Style::default().fg(t.tool)));
        }
        if answer.is_empty() {
            lines.push(
                Line::from("(no answer — turn/budget limit hit; see transcript)")
                    .style(Style::default().fg(t.warning)),
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
        let t = self.theme.resolved();
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
            .style(Style::default().fg(t.dim)),
        );
        lines.push(Line::from(""));
        lines
    }

    /// The turn was interrupted: flush partial tool lines + a note.
    pub fn finish_interrupted(&mut self) -> Vec<Line<'static>> {
        let t = self.theme.resolved();
        self.running = false;
        self.streaming.clear();
        self.current_tool = None;
        self.status_note =
            Some("interrupted — partial work is persisted; resume with --resume".into());
        let mut lines: Vec<Line<'static>> = Vec::new();
        for tool in std::mem::take(&mut self.tool_lines) {
            lines.push(Line::from(tool).style(Style::default().fg(t.tool)));
        }
        lines.push(
            Line::from("(interrupted — partial work is persisted; resume with --resume)")
                .style(Style::default().fg(t.warning)),
        );
        lines.push(Line::from(""));
        lines
    }

    /// Begin a submitted task.
    pub fn begin_task(&mut self, task: &str) {
        self.running = true;
        self.streaming.clear();
        self.status_note = None;
        // A committed line renders as one screen row, so a multi-line task
        // echoes as an indented block.
        let mut lines = task.split('\n');
        self.tool_lines
            .push(format!("❯ {}", lines.next().unwrap_or_default()));
        for line in lines {
            self.tool_lines.push(format!("  {line}"));
        }
        if let Some(goal) = self.goal.clone() {
            self.tool_lines
                .push(format!("  [goal: {goal}]"));
        }
    }

    /// The task text actually submitted: the raw input plus the goal
    /// stop-condition rider when /goal is active.
    pub fn task_with_goal(&self, task: &str) -> String {
        match &self.goal {
            Some(goal) => format!(
                "{task}\n\n(Goal for this run: {goal}. Keep working across turns until it \
                 is met, then end your final message with the exact line: GOAL COMPLETE)"
            ),
            None => task.to_string(),
        }
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
        let th = self.theme.resolved();
        use ratatui::layout::{Constraint, Layout};
        use ratatui::widgets::Paragraph;

        // Split: everything above vs the bordered input box (3 rows).
        // Input box height grows with multi-line content (3 base + 1/line).
        let input_lines = self.input.matches('\n').count() as u16;
        let input_height = (3 + input_lines).min(6).min(area.height.saturating_sub(2));
        let outer =
            Layout::vertical([Constraint::Min(1), Constraint::Length(input_height)]).split(area);
        let top = outer[0];
        let input_area = outer[1];

        // Top zone rows: reasoning tail, streaming (fills), subagents,
        // approval/status.
        let mut rows: Vec<Constraint> = Vec::new();
        if !self.reasoning.is_empty() {
            rows.push(Constraint::Length(2));
        }
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
        if !self.reasoning.is_empty() {
            let tail = streaming_tail(&self.reasoning, 2, chunks[0].width as usize);
            frame.render_widget(
                Paragraph::new(tail).style(Style::default().fg(th.dim).add_modifier(Modifier::ITALIC)),
                chunks[0],
            );
            idx += 1;
        }
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
                        Some(t) if t == "done" => (t.clone(), th.success),
                        Some(t) if t == "failed" => (t.clone(), th.error),
                        _ => (format!("turn {turns} · {tokens} tok"), th.accent),
                    };
                    let display = if label.is_empty() {
                        id.clone()
                    } else {
                        label.clone()
                    };
                    Line::from(vec![
                        Span::styled("  ⏵ ", Style::default().fg(th.accent)),
                        Span::styled(format!("[{display}] "), Style::default().fg(th.dim)),
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
                            .fg(th.warning),
                    ),
                    Line::from(hint).style(Style::default().fg(th.dim)),
                ]),
                chunks[idx],
            );
        } else {
            let status = self.styled_status_line();
            frame.render_widget(Paragraph::new(status), chunks[idx]);
        }

        // ── Bordered input box ──
        let border_color = if self.running { th.accent } else { th.dim };
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
            Style::default().fg(th.dim)
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
                            Span::styled("❯ ", Style::default().fg(th.accent)),
                            Span::styled(line.to_string(), Style::default()),
                        ])
                    } else {
                        Line::from(format!("  {line}"))
                    }
                })
                .collect()
        } else {
            vec![Line::from(vec![
                Span::styled("❯ ", Style::default().fg(th.accent)),
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
        let th = self.theme.resolved();
                    cell.set_symbol("▎").set_style(Style::default().fg(th.accent));
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
        let t = self.theme.resolved();
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
                Span::styled(format!("{spinner} "), Style::default().fg(t.accent)),
                Span::styled(self.model_label.clone(), Style::default().fg(t.dim)),
                Span::styled(
                    format!(
                        " turn {} · {} tok{tool}",
                        self.turn_count + 1,
                        self.cumulative.total()
                    ),
                    Style::default().fg(t.dim),
                ),
                Span::styled("  esc interrupt", Style::default().fg(t.dim)),
            ])
        } else {
            let meter = self.context_meter();
            Line::from(vec![
                Span::styled(format!("{spinner} "), Style::default().fg(t.accent)),
                Span::styled(self.model_label.clone(), Style::default().fg(t.dim)),
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
                    Style::default().fg(t.dim),
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

/// Lowercase kebab slug for skill names from a title.
fn slugish(text: &str) -> String {
    let mut out = String::new();
    for ch in text.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(48).collect()
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
