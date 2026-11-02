//! modron-tui — the fullscreen terminal UI.
//!
//! Design history: Phase 1d used Claude Code's inline pattern (committed
//! content via `Terminal::insert_before` into the native scrollback, a
//! fixed live region below). That kept native text selection but scrolled
//! the whole terminal session, so the TUI now owns the FULL screen
//! (alternate screen) with an internal scrollback you page through with
//! pgup/pgdn, the mouse wheel, and esc-to-live. Esc interrupts the
//! running turn; ctrl+o toggles transcript detail; approvals render
//! inline with y/n/a; the input is a real cursor field (arrows + click).
//!
//! This module is the pure state machine (`App`) — no TTY required to test.
//! The terminal loop lives in [`run`].

pub mod clipboard;
pub mod run;
pub mod theme;
pub use run::{run_with, TuiConfig, TuiSinks};
pub use theme::Theme;

use modron_core::{StreamDelta, ToolActivity, ToolCall, Usage};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders};
use std::sync::mpsc::Sender;

/// One live swarm row: a running or settled child agent.
#[derive(Clone, Debug)]
pub struct SwarmRow {
    pub id: String,
    pub label: String,
    pub turns: u32,
    pub tokens: u64,
    pub last_tool: Option<String>,
    /// When the row appeared (elapsed-time display).
    pub started: std::time::Instant,
    /// Settled state: Some(true) done, Some(false) failed, None running.
    pub done: Option<bool>,
}

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
    /// Paste (ctrl+V): harvest the clipboard — image → attachment, text →
    /// inserted into the input.
    PasteImage,
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
    /// Show configured provider credentials (masked) and how to add more.
    Auth,
    /// Alias kept for muscle memory.
    Compress,
    Unknown(String),
}

/// A slash command that needs the run loop's powers (engine mutation, tool
/// execution, store access). `App::handle_slash` parks one here; the loop
/// drains and executes it, then commits the result lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingCommand {
    /// /auth — status of provider credentials (reads the auth store).
    Auth,
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
    /// Every accepted `/name` (canonical commands and their aliases) with a
    /// one-line description. The completion popup shown while typing `/`
    /// filters this table; it must stay in sync with [`SlashCommand::parse`]
    /// (a test asserts every name here parses to a real command).
    pub const CATALOG: &[(&str, &str)] = &[
        ("help", "show this help"),
        ("?", "alias of /help"),
        ("exit", "quit the TUI"),
        ("quit", "alias of /exit"),
        ("q", "alias of /exit"),
        ("clear", "clear the scrollback"),
        ("compact", "force a context compaction now"),
        ("compress", "alias of /compact"),
        ("model", "switch models mid-session"),
        ("context", "show context usage breakdown"),
        ("sessions", "list recent sessions"),
        ("sandbox", "off | seatbelt | bubblewrap"),
        ("reasoning", "toggle streamed-reasoning display"),
        ("thinking", "alias of /reasoning"),
        ("btw", "quick side question, no tools"),
        ("agent", "list or switch personas"),
        ("persona", "alias of /agent"),
        ("allow", "allow a tool for the session"),
        ("deny", "deny a tool for the session"),
        ("mode", "approval mode: ask | yolo"),
        ("tasks", "background delegate status"),
        ("worktree", "list delegate worktrees"),
        ("worktrees", "alias of /worktree"),
        ("issues", "the project issue board"),
        ("fork", "branch this conversation"),
        ("plan", "plan first, no edits until approved"),
        ("review", "review the working diff"),
        ("code-review", "alias of /review"),
        ("goal", "stop condition for long runs"),
        ("learn", "distill this session into a SKILL.md"),
        ("theme", "switch color theme"),
        ("auth", "show provider keys (add: `modron auth add`)"),
        ("login", "alias of /auth"),
    ];

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
            "auth" | "login" => SlashCommand::Auth,
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
    /// Assistant text streamed so far this turn (rendered as the live tail
    /// of the history zone; committed to `history` at turn end).
    pub streaming: String,
    /// The TUI's own scrollback (fullscreen mode): completed lines live
    /// here, never in the terminal's native buffer.
    pub history: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom of `history` (0 = following the
    /// live tail). New content does NOT move the view while scrolled.
    pub history_offset: usize,
    /// History zone height from the last render (the page step for
    /// PgUp/PgDn and the offset clamp).
    pub last_history_height: u16,
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
    /// Live subagent rows (Phase 5h / 14 swarm v2).
    pub subagents: Vec<SwarmRow>,
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
    /// Pending multimodal attachments (ctrl+V images) for the next task.
    pub attachments: Vec<modron_core::Attachment>,
    /// Display labels for the pending attachments (rendered as chips).
    pub attachment_chips: Vec<String>,
    /// Highlighted row in the slash-command popup (clamped at use; typing
    /// resets it to the top).
    pub slash_selected: usize,
    /// Cursor position in `input`, as a CHAR offset (0 = before the first
    /// char, len = after the last). All edits happen here, so the input is
    /// a real editable field, not just an append-only line.
    pub cursor: usize,
    /// First visible text row of the input box (it scrolls when the text
    /// outgrows the box; the view keeps the cursor row in view).
    pub input_scroll: u16,
    /// The input box's inner area as of the last render — maps mouse
    /// clicks to cursor positions.
    pub last_input_inner: ratatui::layout::Rect,
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
                    self.insert_at_cursor('\n');
                    return Action::None;
                }
                KeyCode::Char('v') | KeyCode::Char('V') => {
                    return Action::PasteImage;
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

        // Slash-command popup (input is a bare `/word` prefix): arrows move
        // the highlight, tab completes it into the input. Bound only while
        // the popup is open so the keys stay free otherwise.
        let matches = self.slash_matches();
        if !matches.is_empty() {
            let len = matches.len();
            let sel = self.slash_selected.min(len - 1);
            match key.code {
                KeyCode::Up => {
                    self.slash_selected = (sel + len - 1) % len;
                    return Action::None;
                }
                KeyCode::Down => {
                    self.slash_selected = (sel + 1) % len;
                    return Action::None;
                }
                KeyCode::Tab => {
                    self.input = format!("/{} ", matches[sel].0);
                    self.cursor = self.input.chars().count();
                    return Action::None;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Enter
                if key.modifiers.intersects(
                    crossterm::event::KeyModifiers::SHIFT | crossterm::event::KeyModifiers::ALT,
                ) =>
            {
                // Newline, not submit. Shift+enter only reaches us with the
                // SHIFT modifier on terminals that speak the kitty keyboard
                // protocol (run.rs enables it when available); option/alt+
                // enter sends ESC+CR everywhere else, which crossterm
                // reports as Enter with ALT.
                self.insert_at_cursor('\n');
                Action::None
            }
            KeyCode::Enter => {
                // Backslash+Enter: line continuation — the universal
                // fallback that works on every terminal (Claude Code's
                // convention for terminals that cannot report shift+enter).
                if self.input.ends_with('\\') {
                    self.input.pop();
                    self.input.push('\n');
                    self.cursor = self.input.chars().count();
                    return Action::None;
                }
                let task = std::mem::take(&mut self.input);
                self.cursor = 0;
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
                if self.cursor > 0 {
                    let byte = char_byte_idx(&self.input, self.cursor - 1);
                    self.input.remove(byte);
                    self.cursor -= 1;
                }
                self.slash_selected = 0;
                Action::None
            }
            KeyCode::Delete => {
                if self.cursor < self.input.chars().count() {
                    let byte = char_byte_idx(&self.input, self.cursor);
                    self.input.remove(byte);
                }
                self.slash_selected = 0;
                Action::None
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::None
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
                Action::None
            }
            KeyCode::Home => {
                self.cursor = self.cursor_line_start();
                Action::None
            }
            KeyCode::End => {
                self.cursor = self.cursor_line_end();
                Action::None
            }
            // Up/Down inside a multi-line input move between lines (same
            // column, clamped to the target line's length).
            KeyCode::Up if self.input.contains('\n') => {
                self.move_input_line(true);
                Action::None
            }
            KeyCode::Down if self.input.contains('\n') => {
                self.move_input_line(false);
                Action::None
            }
            // Internal scrollback paging (fullscreen mode).
            KeyCode::PageUp => {
                let page = (self.last_history_height as i32).max(3);
                self.scroll_history(page);
                Action::None
            }
            KeyCode::PageDown => {
                let page = (self.last_history_height as i32).max(3);
                self.scroll_history(-page);
                Action::None
            }
            // Esc while idle: back to the live tail first, then clear the
            // input on a second press.
            KeyCode::Esc if !self.running => {
                if self.history_offset > 0 {
                    self.history_offset = 0;
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
                Action::None
            }
            KeyCode::Char(c) => {
                self.insert_at_cursor(c);
                self.slash_selected = 0;
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Insert a character at the cursor (char-offset domain).
    fn insert_at_cursor(&mut self, c: char) {
        let byte = char_byte_idx(&self.input, self.cursor);
        self.input.insert(byte, c);
        self.cursor += 1;
    }

    /// Char offset of the start of the cursor's line.
    fn cursor_line_start(&self) -> usize {
        self.cursor_line_layout().1
    }

    /// Char offset of the end of the cursor's line (before the newline).
    fn cursor_line_end(&self) -> usize {
        let (_, start, len) = self.cursor_line_layout();
        start + len
    }

    /// (line_index, line_start_char_offset, line_len_chars) for the
    /// cursor's line. The cursor at a line's end belongs to that line,
    /// not the next one.
    fn cursor_line_layout(&self) -> (usize, usize, usize) {
        let mut start = 0;
        let mut last = (0, 0, 0);
        for (i, line) in self.input.split('\n').enumerate() {
            let len = line.chars().count();
            last = (i, start, len);
            if self.cursor <= start + len {
                return (i, start, len);
            }
            start += len + 1;
        }
        last // cursor at the very end of a trailing newline
    }

    /// Move the cursor one line up/down, keeping the column when the
    /// target line is long enough (standard editor behavior).
    fn move_input_line(&mut self, up: bool) {
        let (li, start, _) = self.cursor_line_layout();
        let col = self.cursor - start;
        let target = if up { li.checked_sub(1) } else { Some(li + 1) };
        let Some(target) = target else { return };
        let mut t_start = 0;
        for (i, line) in self.input.split('\n').enumerate() {
            let t_len = line.chars().count();
            if i == target {
                self.cursor = t_start + col.min(t_len);
                return;
            }
            t_start += t_len + 1;
        }
    }

    /// A mouse click inside the input box positions the cursor. `column`
    /// and `row` are terminal-wide coordinates (from crossterm); clicks
    /// outside the input's inner area are ignored.
    pub fn on_click(&mut self, column: u16, row: u16) {
        let inner = self.last_input_inner;
        if inner.width == 0 || row < inner.y || row >= inner.y + inner.height {
            return;
        }
        let chip_rows = (!self.attachment_chips.is_empty()) as u16;
        let visible_row = row - inner.y;
        if visible_row < chip_rows {
            // Click landed on the attachment-chips row: cursor to the end.
            self.cursor = self.input.chars().count();
            return;
        }
        let text_row = (self.input_scroll + visible_row - chip_rows) as usize;
        let mut start = 0;
        for (i, line) in self.input.split('\n').enumerate() {
            let len = line.chars().count();
            if i == text_row {
                let col = (column.saturating_sub(inner.x + 2) as usize).min(len);
                self.cursor = start + col;
                return;
            }
            start += len + 1;
        }
        // Below the text (empty rows of the box): cursor to the end.
        self.cursor = self.input.chars().count();
    }

    /// Commands matching the input for the completion popup: the input must
    /// start with `/` and still be the command word (no whitespace typed
    /// yet). A bare `/` matches everything; empty means "no popup".
    pub fn slash_matches(&self) -> Vec<&'static (&'static str, &'static str)> {
        let Some(rest) = self.input.strip_prefix('/') else {
            return Vec::new();
        };
        if rest.is_empty() {
            return SlashCommand::CATALOG.iter().collect();
        }
        if rest.chars().any(char::is_whitespace) {
            return Vec::new();
        }
        SlashCommand::CATALOG
            .iter()
            .filter(|(name, _)| name.starts_with(rest))
            .collect()
    }

    /// Commit lines into the internal scrollback. When the user is
    /// scrolled up, the view stays anchored (offset unchanged).
    pub fn push_history(&mut self, lines: Vec<Line<'static>>) {
        self.history.extend(lines);
    }

    /// A dim meta line into the scrollback (slash-command output).
    pub fn note(&mut self, text: impl Into<String>) {
        let dim = Style::default().fg(self.theme.resolved().dim);
        self.history.push(Line::from(text.into()).style(dim));
    }

    /// Push multi-line text as one dim row per line.
    fn push_lines(&mut self, text: impl Into<String>) {
        let dim = Style::default().fg(self.theme.resolved().dim);
        for line in text.into().lines() {
            self.history.push(Line::from(line.to_string()).style(dim));
        }
    }

    /// Scroll the internal history: positive lines go up (older), negative
    /// goes back down; 0 offset = following the live tail.
    pub fn scroll_history(&mut self, up: i32) {
        let max = self.history.len().saturating_sub(1) as i32;
        self.history_offset = (self.history_offset as i32 + up).clamp(0, max) as usize;
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
                self.history.clear();
                self.history_offset = 0;
                self.streaming.clear();
                self.reasoning.clear();
                Action::None
            }
            SlashCommand::Compact => {
                // Signal the run loop to force a compaction before the next
                // turn; the engine handles it when context is over threshold.
                self.note(
                    "(compaction will trigger on the next turn if context is over threshold)",
                );
                Action::None
            }
            SlashCommand::Model(name) => {
                if name.is_empty() {
                    self.note(format!("current model: {}", self.model_label));
                    self.note("usage: /model glm-5.3-flash");
                } else {
                    self.model_label = name.clone();
                    self.note(format!(
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
                self.note("(session listing: run `modron sessions` outside the TUI)");
                Action::None
            }
            SlashCommand::Sandbox(mode) => {
                self.note(format!(
                    "sandbox: {mode} (takes effect on restart — the sandbox is set at launch)"
                ));
                Action::None
            }
            SlashCommand::Reasoning => {
                self.show_reasoning = !self.show_reasoning;
                self.reasoning.clear();
                self.note(format!(
                    "reasoning display: {}",
                    if self.show_reasoning { "on" } else { "off" }
                ));
                Action::None
            }
            SlashCommand::Btw(question) => {
                if question.trim().is_empty() {
                    self.note("usage: /btw <quick question>");
                    return Action::None;
                }
                Action::Submit(format!(
                    "{question}\n\n(Quick side question: answer directly from what you \
                     already know — do not use tools.)"
                ))
            }
            SlashCommand::Auth => {
                self.pending_command = Some(PendingCommand::Auth);
                Action::None
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
                    self.note("usage: /plan <task>");
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
                    self.note("goal cleared — tasks run normally again");
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
                let slug = slugish(if title.is_empty() {
                    "session-skill"
                } else {
                    &title
                });
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
                    self.note(format!("theme: {name}"));
                } else {
                    match Theme::load(&crate::theme_dir(), name) {
                        Some(theme) => {
                            self.theme = theme;
                            self.note(format!("theme: {name}"));
                        }
                        None => self.note(format!(
                            "unknown theme {name:?} — available: {}",
                            Theme::available(&crate::theme_dir()).join(", ")
                        )),
                    }
                }
                Action::None
            }
            SlashCommand::Compress => self.handle_slash(SlashCommand::Compact),
            SlashCommand::Unknown(name) => {
                self.note(format!("unknown command /{name} — try /help"));
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
                let style = Style::default().fg(self.theme.resolved().tool);
                self.history
                    .push(Line::from(format!("● {name} …")).style(style));
            }
            ToolActivity::Done {
                name,
                is_error,
                bytes,
            } => {
                self.current_tool = None;
                let mark = if is_error { "error" } else { "done" };
                let style = Style::default().fg(self.theme.resolved().tool);
                self.history
                    .push(Line::from(format!("● {name} — {mark} ({bytes} B)")).style(style));
            }
            ToolActivity::SubagentStarted { id, label } => {
                self.subagents.push(SwarmRow {
                    id,
                    label,
                    turns: 0,
                    tokens: 0,
                    last_tool: None,
                    started: std::time::Instant::now(),
                    done: None,
                });
            }
            ToolActivity::SubagentProgress {
                id,
                turns,
                tokens,
                last_tool,
                ..
            } => {
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == id) {
                    row.turns = row.turns.max(turns);
                    row.tokens = row.tokens.max(tokens);
                    row.last_tool = last_tool.clone();
                }
            }
            ToolActivity::SubagentFinished {
                id,
                tokens,
                turns,
                is_error,
                ..
            } => {
                if let Some(row) = self.subagents.iter_mut().find(|r| r.id == id) {
                    row.turns = turns;
                    row.tokens = tokens;
                    row.done = Some(!is_error);
                }
            }
        }
    }

    /// Scrollback summary of the swarm (committed at turn end), then clear.
    /// Returns lines when any agent ran, empty otherwise.
    pub fn swarm_summary(&mut self) -> Vec<Line<'static>> {
        let t = self.theme.resolved();
        if self.subagents.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![Line::from(format!(
            "⎇ swarm: {} agent(s), {} tok total",
            self.subagents.len(),
            self.subagents.iter().map(|r| r.tokens).sum::<u64>()
        ))
        .style(Style::default().fg(t.accent))];
        for row in &self.subagents {
            let (mark, color) = match row.done {
                Some(true) => ("done", t.success),
                Some(false) => ("failed", t.error),
                None => ("unfinished", t.warning),
            };
            lines.push(
                Line::from(format!(
                    "  ⏵ [{}] {} · {} turns · {} tok · last: {}",
                    row.label,
                    mark,
                    row.turns,
                    row.tokens,
                    row.last_tool.as_deref().unwrap_or("-")
                ))
                .style(Style::default().fg(color)),
            );
        }
        self.subagents.clear();
        lines
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

        // Commit into the internal scrollback (returned for tests).
        let mut lines: Vec<Line<'static>> = Vec::new();
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
        self.history.extend(lines.clone());
        lines
    }

    /// The turn was interrupted: note it in the scrollback.
    pub fn finish_interrupted(&mut self) -> Vec<Line<'static>> {
        let t = self.theme.resolved();
        self.running = false;
        self.streaming.clear();
        self.current_tool = None;
        self.status_note =
            Some("interrupted — partial work is persisted; resume with --resume".into());
        let lines = vec![
            Line::from("(interrupted — partial work is persisted; resume with --resume)")
                .style(Style::default().fg(t.warning)),
            Line::from(""),
        ];
        self.history.extend(lines.clone());
        lines
    }

    /// Begin a submitted task.
    pub fn begin_task(&mut self, task: &str) {
        self.running = true;
        self.streaming.clear();
        self.status_note = None;
        // The task echoes into the scrollback as an indented block.
        let mut lines = task.split('\n');
        let accent = Style::default().fg(self.theme.resolved().accent);
        self.history
            .push(Line::from(format!("❯ {}", lines.next().unwrap_or_default())).style(accent));
        for line in lines {
            self.history.push(Line::from(format!("  {line}")));
        }
        if let Some(goal) = self.goal.clone() {
            let dim = Style::default().fg(self.theme.resolved().dim);
            self.history
                .push(Line::from(format!("  [goal: {goal}]")).style(dim));
        }
    }

    /// Attach a clipboard image (ctrl+V): loads, base64s, and stages it for
    /// the next submit. Returns the chip label.
    pub fn attach_image(&mut self, path: &std::path::Path) -> Option<String> {
        let media = match path
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_lowercase)
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg") | Some("jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => return None,
        };
        let bytes = std::fs::read(path).ok()?;
        if bytes.is_empty() {
            return None;
        }
        let chip = format!(
            "[img {} · {} KiB]",
            path.file_name()?.to_string_lossy(),
            bytes.len() / 1024
        );
        self.attachments.push(modron_core::Attachment::Image {
            media_type: media.to_string(),
            data: modron_core::b64::encode(&bytes),
        });
        self.attachment_chips.push(chip.clone());
        Some(chip)
    }

    /// Consume staged attachments (called on submit).
    pub fn take_attachments(&mut self) -> Vec<modron_core::Attachment> {
        self.attachment_chips.clear();
        std::mem::take(&mut self.attachments)
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
        let note = self
            .status_note
            .as_ref()
            .map(|n| format!(" · {n}"))
            .unwrap_or_default();
        format!(
            "{spinner} idle · {} tok ·{note} {} · ctrl+o {} · esc clears input",
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
    /// for meta, cyan for tools, red for errors). Takes `&mut self` to
    /// record the input box's geometry and scroll state for mouse mapping.
    pub fn view(&mut self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect) {
        let th = self.theme.resolved();
        use ratatui::layout::{Constraint, Layout};
        use ratatui::widgets::Paragraph;

        // Fullscreen layout, bottom-up: the bordered input box, the slash
        // popup, the status line, the subagent panel — everything above is
        // the internal scrollback. The box grows with its content (2
        // borders + chips row + one row per text line) up to 8 rows, then
        // scrolls internally.
        let chip_rows = (!self.attachment_chips.is_empty()) as u16;
        let text_rows = self.input.matches('\n').count() as u16 + 1;
        let input_height = (2 + chip_rows + text_rows)
            .min(8)
            .min(area.height.saturating_sub(2));

        let matches = self.slash_matches();
        let sel = self.slash_selected.min(matches.len().saturating_sub(1));
        let popup_h = (matches.len() as u16).min(5);

        let mut rows: Vec<Constraint> = vec![Constraint::Min(1)]; // scrollback
        if !self.subagents.is_empty() {
            rows.push(Constraint::Length(self.subagents.len() as u16 + 1));
        }
        rows.push(if self.pending_approval.is_some() {
            Constraint::Length(2)
        } else {
            Constraint::Length(1)
        });
        if popup_h > 0 {
            rows.push(Constraint::Length(popup_h));
        }
        rows.push(Constraint::Length(input_height));
        let chunks = Layout::vertical(rows).split(area);
        let input_area = chunks[chunks.len() - 1];

        // ── Internal scrollback ──
        let hist_area = chunks[0];
        self.last_history_height = hist_area.height;
        self.history_offset = self
            .history_offset
            .min(self.history.len().saturating_sub(1));
        let following = self.history_offset == 0;
        let mut body = hist_area.height as usize;
        if !following {
            body = body.saturating_sub(1); // the "lines above" indicator
        }
        // Live tails (reasoning above the answer) render below the
        // committed lines while following.
        let mut tails: Vec<Line> = Vec::new();
        if following {
            if !self.reasoning.is_empty() {
                tails.extend(
                    streaming_tail(&self.reasoning, 2, hist_area.width as usize)
                        .into_iter()
                        .map(|l| {
                            l.style(Style::default().fg(th.dim).add_modifier(Modifier::ITALIC))
                        }),
                );
            }
            if !self.streaming.is_empty() {
                let left = body.saturating_sub(tails.len()).max(1);
                tails.extend(streaming_tail(
                    &self.streaming,
                    left,
                    hist_area.width as usize,
                ));
            }
            body = body.saturating_sub(tails.len());
        }
        let end = self.history.len() - self.history_offset;
        let start = end.saturating_sub(body);
        let mut hist: Vec<Line> = Vec::new();
        if !following {
            let indicator = if start == 0 {
                "↑ top of session · pgdn scroll · esc live".to_string()
            } else {
                format!("↑ {start} lines above · pgup/pgdn scroll · esc live")
            };
            hist.push(Line::from(indicator).style(Style::default().fg(th.dim)));
        }
        hist.extend(self.history[start..end].iter().cloned());
        hist.extend(tails);
        frame.render_widget(Paragraph::new(hist), hist_area);

        let mut idx = 1;

        if !self.subagents.is_empty() {
            let active = self.subagents.iter().filter(|r| r.done.is_none()).count();
            let total_tokens = self.subagents.iter().map(|r| r.tokens).sum::<u64>();
            let mut lines = vec![Line::from(format!(
                "⎇ {} agent(s) · {} active · {total_tokens} tok",
                self.subagents.len(),
                active
            ))
            .style(Style::default().fg(th.dim))];
            let pulse = ["·", "•", "●", "•"];
            for row in &self.subagents {
                let (state_str, color) = match row.done {
                    Some(true) => ("done".to_string(), th.success),
                    Some(false) => ("failed".to_string(), th.error),
                    None => (
                        format!(
                            "{} {} · t{} · {}s",
                            pulse[(self.spinner_tick as usize) % pulse.len()],
                            row.last_tool.as_deref().unwrap_or("working"),
                            row.turns,
                            row.started.elapsed().as_secs()
                        ),
                        th.accent,
                    ),
                };
                let display = if row.label.is_empty() {
                    row.id.chars().take(8).collect::<String>()
                } else {
                    row.label.clone()
                };
                lines.push(Line::from(vec![
                    Span::styled("  ⏵ ", Style::default().fg(th.accent)),
                    Span::styled(format!("[{display}] "), Style::default().fg(th.dim)),
                    Span::styled(state_str, Style::default().fg(color)),
                ]));
            }
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            idx += 1;
        }

        if self.pending_approval.is_some() {
            let hint = "y = allow once · a = allow for this run · n/Esc = deny";
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(self.status_line())
                        .style(Style::default().add_modifier(Modifier::BOLD).fg(th.warning)),
                    Line::from(hint).style(Style::default().fg(th.dim)),
                ]),
                chunks[idx],
            );
        } else {
            let status = self.styled_status_line();
            frame.render_widget(Paragraph::new(status), chunks[idx]);
        }
        idx += 1;

        // Slash-command popup (window around the highlight when the
        // filtered list outgrows the space).
        if popup_h > 0 {
            let h = popup_h as usize;
            let start = sel.saturating_sub(h - 1).min(matches.len() - h);
            let name_w = matches.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
            let lines: Vec<Line> = matches[start..start + h]
                .iter()
                .enumerate()
                .map(|(i, (name, desc))| {
                    let selected = start + i == sel;
                    let pointer = if selected { "❯ " } else { "  " };
                    let pad = " ".repeat(name_w + 1 - name.len());
                    Line::from(vec![
                        Span::styled(pointer, Style::default().fg(th.accent)),
                        Span::styled(
                            format!("/{name}"),
                            if selected {
                                Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            format!("{pad}{desc}"),
                            if selected {
                                Style::default()
                            } else {
                                Style::default().fg(th.dim)
                            },
                        ),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
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
        self.last_input_inner = inner;

        // Keep the cursor's row inside the visible window (the box scrolls
        // vertically once the text outgrows it).
        let (cursor_line, cursor_line_start, _) = self.cursor_line_layout();
        let visible_text_rows = inner.height.saturating_sub(chip_rows);
        if visible_text_rows > 0 {
            let cl = cursor_line as u16;
            if cl < self.input_scroll {
                self.input_scroll = cl;
            } else if cl >= self.input_scroll + visible_text_rows {
                self.input_scroll = cl + 1 - visible_text_rows;
            }
        }

        // Pending-attachment chips ride as the first line inside the box,
        // then the scrolled window of text rows (prompt on the first line
        // of the INPUT, not of the window). An empty input shows the
        // placeholder instead of a blank prompt row.
        let mut lines: Vec<Line> = Vec::new();
        if let Some(chips) =
            (!self.attachment_chips.is_empty()).then(|| self.attachment_chips.join(" "))
        {
            lines.push(Line::from(chips).style(Style::default().fg(th.accent)));
        }
        let scroll = self.input_scroll as usize;
        if self.input.is_empty() {
            let (text, style) = if self.running {
                ("", Style::default())
            } else {
                (
                    "type a task (or / for commands)…",
                    Style::default().fg(th.dim),
                )
            };
            lines.push(Line::from(vec![
                Span::styled("❯ ", Style::default().fg(th.accent)),
                Span::styled(text, style),
            ]));
        } else {
            for (i, line) in self.input.split('\n').enumerate().skip(scroll) {
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled("❯ ", Style::default().fg(th.accent)),
                        Span::styled(line.to_string(), Style::default()),
                    ]));
                } else {
                    lines.push(Line::from(format!("  {line}")));
                }
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // Cursor block at the cursor's (line, column) — the reason the
        // cursor is a char offset, not "the end of the string".
        if !self.running && !self.input.is_empty() {
            let visible_line = cursor_line.saturating_sub(scroll) as u16;
            let col = (self.cursor - cursor_line_start) as u16;
            let cursor_x = inner.x + 2 + col.min(inner.width.saturating_sub(3));
            let cursor_y = inner.y + chip_rows + visible_line;
            if cursor_x < inner.x + inner.width && cursor_y < inner.y + inner.height {
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut(ratatui::layout::Position::new(cursor_x, cursor_y))
                {
                    cell.set_symbol("▎")
                        .set_style(Style::default().fg(th.accent));
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

/// Byte index of the `ci`-th char of `s` (s.len() when past the end) —
/// String::insert/remove speak bytes, the cursor speaks chars.
fn char_byte_idx(s: &str, ci: usize) -> usize {
    s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len())
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
