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
    /// Provider credentials: bare = status, `<provider> <key>` = save,
    /// `remove <provider>` = forget.
    Auth(String),
    /// Alias kept for muscle memory.
    Compress,
    Unknown(String),
}

/// A slash command that needs the run loop's powers (engine mutation, tool
/// execution, store access). `App::handle_slash` parks one here; the loop
/// drains and executes it, then commits the result lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PendingCommand {
    /// /auth — credential status / set / remove against the shared store.
    Auth(String),
    /// The interactive flow's Enter on a key: save it (masked in output).
    AuthSave {
        name: String,
        env_var: String,
        key: String,
    },
    /// The interactive flow's "remove saved credentials": forget the
    /// provider's key and any stored sign-in tokens.
    AuthRemove {
        name: String,
        env_var: String,
        provider_id: String,
    },
    /// The interactive flow's Claude OAuth row: run the browser flow (the
    /// run loop spawns it; Esc on the flow cancels).
    OAuthLogin {
        session: modron_core::auth::oauth::LoginSession,
    },
    /// The interactive flow's ChatGPT row: run the device-code flow (the
    /// run loop spawns start→finish; the interim user code shows in the
    /// modal; Esc cancels).
    ChatGptLogin,
    /// /model <name>: switch the engine's model for future turns (the
    /// provider follows the model). `save_default` also persists the
    /// choice to config.toml.
    Model {
        name: String,
        save_default: bool,
    },
    /// /sandbox <mode>: switch the sandbox policy for future turns.
    Sandbox(String),
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
        ("model", "switch models (opens a picker)"),
        ("context", "show context usage breakdown"),
        ("sessions", "list recent sessions"),
        ("sandbox", "choose sandbox mode (picker)"),
        ("reasoning", "toggle streamed-reasoning display"),
        ("thinking", "alias of /reasoning"),
        ("btw", "quick side question, no tools"),
        ("agent", "switch persona (picker)"),
        ("persona", "alias of /agent"),
        ("allow", "allow a tool (picker)"),
        ("deny", "deny a tool (picker)"),
        ("mode", "approval mode picker: ask | yolo"),
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
        ("theme", "switch color theme (picker)"),
        ("auth", "open the auth manager (view/sign-in/remove)"),
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
            "auth" | "login" => SlashCommand::Auth(arg),
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

/// The interactive auth manager (`/auth`): a modal that owns the keys.
#[derive(Clone, Debug)]
pub enum AuthFlow {
    /// Choosing a provider: index into the registry. Each row shows the
    /// provider's current status (masked key, oauth, not configured).
    Picker { selected: usize },
    /// Second stage for one provider: API key vs browser sign-in vs
    /// remove. `selected` indexes [`auth_methods`].
    MethodChoice {
        info: modron_core::auth::ProviderInfo,
        selected: usize,
    },
    /// Pasting/typing the key for the chosen provider. Append-only buffer
    /// (keys are pasted, not edited); rendered masked.
    KeyEntry {
        info: modron_core::auth::ProviderInfo,
        input: String,
    },
    /// An OAuth flow is running (spawned in the run loop); Esc backs out
    /// to the method screen and the run loop aborts the task.
    /// `user_code` is set for device flows (the code typed at the URL).
    WaitingOAuth {
        info: modron_core::auth::ProviderInfo,
        url: String,
        user_code: Option<String>,
    },
}

/// One action offered on the method-choice screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthMethod {
    ApiKey,
    BrowserSignIn,
    Remove,
}

impl AuthMethod {
    fn label(self, info: &modron_core::auth::ProviderInfo) -> String {
        match self {
            AuthMethod::ApiKey => format!("paste an API key ({})", info.env_var),
            AuthMethod::BrowserSignIn => match info.provider_id {
                "anthropic" => "sign in with Claude subscription (browser)".to_string(),
                _ => "sign in with ChatGPT (browser)".to_string(),
            },
            AuthMethod::Remove => "remove saved credentials".to_string(),
        }
    }
}

/// The actions available for one provider: a key always, browser sign-in
/// for the OAuth-capable two, remove always (it no-ops when nothing is
/// stored).
fn auth_methods(info: &modron_core::auth::ProviderInfo) -> Vec<AuthMethod> {
    let mut methods = vec![AuthMethod::ApiKey];
    if modron_core::auth::oauth_capable(info.provider_id) {
        methods.push(AuthMethod::BrowserSignIn);
    }
    methods.push(AuthMethod::Remove);
    methods
}

/// The status text shown on a picker row: masked key and/or oauth marker,
/// or `not configured`.
fn auth_row_status(info: &modron_core::auth::ProviderInfo) -> String {
    use modron_core::auth::{key_source, mask_key, oauth_signed_in, KeySource};
    let (source, value) = key_source(info.env_var);
    let mut parts: Vec<String> = Vec::new();
    match source {
        KeySource::EnvFile | KeySource::ProcessEnv => parts.push(mask_key(&value)),
        KeySource::None => {}
    }
    if oauth_signed_in(info.provider_id) {
        parts.push("oauth".to_string());
    }
    if parts.is_empty() {
        "not configured".to_string()
    } else {
        parts.join(" · ")
    }
}

/// A generic interactive picker — the arg-less form of a slash command
/// that takes one (the Claude Code/opencode convention): bare `/model`,
/// `/theme`, `/sandbox`, `/mode`, `/agent`, `/allow`, `/deny` open a list
/// instead of demanding remembered syntax. Enter re-dispatches the
/// equivalent typed command, so menus and muscle memory share one path.
#[derive(Clone, Debug)]
pub struct Menu {
    pub title: String,
    pub options: Vec<MenuOption>,
    pub selected: usize,
}

#[derive(Clone, Debug)]
pub struct MenuOption {
    /// The option itself (model name, theme name, ...).
    pub label: String,
    /// Dim right-hand hint (provider, platform, ...).
    pub detail: String,
    /// The command Enter runs — identical to typing it.
    pub command: SlashCommand,
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
    /// First visible DISPLAY row of the input box (it scrolls when the
    /// text outgrows the box; the view keeps the cursor row in view).
    pub input_scroll: u16,
    /// Sent inputs, newest last — ↑/↓ recall them when the input is a
    /// single line (shell-style history).
    pub input_history: Vec<String>,
    /// Position in the history recall (usize::MAX = not recalling).
    pub input_history_pos: usize,
    /// The live input saved when recall started (↓ past the newest
    /// restores it).
    pub input_draft: String,
    /// History-zone width from the last render — committed lines wrap to
    /// it at push time so long content stays readable.
    pub last_hist_width: u16,
    /// The input box's inner area as of the last render — maps mouse
    /// clicks to cursor positions.
    pub last_input_inner: ratatui::layout::Rect,
    /// The interactive auth manager (modal; owns all keys while open).
    pub auth_flow: Option<AuthFlow>,
    /// An interactive menu — the arg-less form of a slash command that
    /// takes one (`/model`, `/theme`, ...). Mutually exclusive with the
    /// auth manager; owns all keys while open.
    pub menu: Option<Menu>,
    /// Persona names, injected by the run loop (feeds /agent's menu).
    pub personas: Vec<String>,
    /// Registered tool names, injected by the run loop (feeds /allow and
    /// /deny menus).
    pub tool_names: Vec<String>,
    /// The engine's current model (preselects /model's menu row).
    pub current_model: String,
    /// The active theme name (marks /theme's menu row).
    pub current_theme: String,
    /// A text selection in the history (drag with the mouse); Esc clears.
    pub selection: Option<Selection>,
    /// History-zone geometry from the last render — maps mouse rows to
    /// history indices (start index, visible committed rows, following?,
    /// top screen row).
    pub hist_geom: (usize, usize, bool, u16),
}

/// An in-TUI text selection over history lines: `(history index, column)`
/// anchor and head. Columns are char offsets into the line (the line is
/// not wrapped — one history line renders as one row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, u16),
    pub head: (usize, u16),
}

/// One DISPLAY row of the input box: a logical line (or a slice of one,
/// when it wraps) with its absolute char offset.
#[derive(Clone, Debug)]
struct InputRow {
    logical: usize,
    /// Char offset of the row's first char in the whole input string.
    start: usize,
    text: String,
}

/// Split a styled line into display rows of at most `width` display
/// columns (char granularity — width-1 glyphs assumed, matching the rest
/// of the TUI). Spans are cut at the boundary; every row keeps the
/// styles of the chars it carries.
fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for span in line.iter() {
        for ch in span.content.chars() {
            if col == width {
                rows.push(Line::from(std::mem::take(&mut current)));
                col = 0;
            }
            if let Some(last) = current.last_mut() {
                if last.style == span.style {
                    last.content.to_mut().push(ch);
                    col += 1;
                    continue;
                }
            }
            current.push(Span::styled(
                std::borrow::Cow::Owned(ch.to_string()),
                span.style,
            ));
            col += 1;
        }
    }
    rows.push(Line::from(current));
    rows
}

/// The char-column window a selection covers on history row `idx`
/// (None when the row isn't selected at all).
fn selection_window(sel: Option<Selection>, idx: usize) -> Option<(usize, usize)> {
    let sel = sel?;
    let (mut a, mut b) = (sel.anchor, sel.head);
    if a > b {
        std::mem::swap(&mut a, &mut b);
    }
    if idx < a.0 || idx > b.0 {
        return None;
    }
    let cs = if idx == a.0 { a.1 as usize } else { 0 };
    let ce = if idx == b.0 { b.1 as usize } else { usize::MAX };
    Some((cs, ce))
}

/// Rebuild a line with the `[cs, ce)` char range rendered reversed.
/// Original span styling inside the selected range is traded for the
/// highlight (the whole range reads as selected).
fn highlighted(line: &Line<'static>, cs: usize, ce: usize) -> Line<'static> {
    let mut before = String::new();
    let mut chosen = String::new();
    let mut after = String::new();
    let mut col = 0usize;
    for span in line.iter() {
        for ch in span.content.chars() {
            if col < cs {
                before.push(ch);
            } else if col < ce {
                chosen.push(ch);
            } else {
                after.push(ch);
            }
            col += 1;
        }
    }
    let reversed = Style::default().add_modifier(Modifier::REVERSED);
    Line::from(vec![
        Span::raw(before),
        Span::styled(chosen, reversed),
        Span::raw(after),
    ])
}

/// Claude Code-inspired palette: a warm accent for activity, dim for meta.
/// These are the DEFAULT-THEME values; live rendering goes through
/// `App.theme` (switchable with /theme).
pub const ACCENT: Color = Color::Rgb(255, 159, 10);
pub const DIM: Color = Color::DarkGray;

/// Persist the default model + provider to config.toml
/// (MODRON_CONFIG or ~/.config/modron/config.toml): the /model menu's
/// "save as default" path. Returns the path written.
pub fn save_default_model(model: &str, provider: &str) -> std::io::Result<std::path::PathBuf> {
    let path = std::env::var("MODRON_CONFIG")
        .map(|dir| std::path::PathBuf::from(dir).join("config.toml"))
        .unwrap_or_else(|_| {
            std::env::var("HOME")
                .map(|h| {
                    std::path::PathBuf::from(h)
                        .join(".config")
                        .join("modron")
                        .join("config.toml")
                })
                .unwrap_or_else(|_| ".modron-config.toml".into())
        });
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            let key = l.split('=').next().unwrap_or("").trim();
            key != "default_model" && key != "default_provider"
        })
        .map(String::from)
        .collect();
    lines.insert(0, format!("default_provider = \"{provider}\""));
    lines.insert(0, format!("default_model = \"{model}\""));
    std::fs::write(&path, lines.join("\n") + "\n")?;
    Ok(path)
}

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

        // An open menu (arg-less /model, /theme, ...) owns every key.
        if let Some(menu) = self.menu.take() {
            return self.menu_key(menu, key);
        }

        // Interactive auth wizard: the modal owns every key while open
        // (except the global ctrl+ shortcuts handled above).
        if let Some(flow) = self.auth_flow.take() {
            return self.auth_flow_key(flow, key);
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
                // Shell-style history: remember sent inputs for ↑/↓ recall.
                self.input_history_pos = usize::MAX;
                self.input_draft.clear();
                if task.trim().is_empty() {
                    return Action::None;
                }
                if self.input_history.last() != Some(&task) {
                    self.input_history.push(task.clone());
                    let len = self.input_history.len();
                    if len > 100 {
                        self.input_history.drain(..len - 100);
                    }
                }
                // Slash commands are handled locally (dirge pattern) —
                // they never reach the model.
                if let Some(cmd) = SlashCommand::parse(task.trim()) {
                    return self.handle_slash(cmd);
                }
                Action::Submit(task)
            }
            KeyCode::Backspace => {
                self.input_history_pos = usize::MAX;
                if self.cursor > 0 {
                    let byte = char_byte_idx(&self.input, self.cursor - 1);
                    self.input.remove(byte);
                    self.cursor -= 1;
                }
                self.slash_selected = 0;
                Action::None
            }
            KeyCode::Delete => {
                self.input_history_pos = usize::MAX;
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
            // column, clamped to the target line's length); on a
            // single-line input they traverse the sent-input history.
            KeyCode::Up if self.input.contains('\n') => {
                self.move_input_line(true);
                Action::None
            }
            KeyCode::Down if self.input.contains('\n') => {
                self.move_input_line(false);
                Action::None
            }
            KeyCode::Up => {
                self.recall_history(true);
                Action::None
            }
            KeyCode::Down => {
                self.recall_history(false);
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
            // Esc while idle: drop a text selection first, then back to
            // the live tail, then clear the input.
            KeyCode::Esc if !self.running => {
                if self.selection.take().is_some() {
                } else if self.history_offset > 0 {
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

    /// Handle a key inside the auth wizard modal. Takes the flow by value
    /// and puts it back (or transitions/clears it).
    fn auth_flow_key(&mut self, flow: AuthFlow, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyCode;
        match flow {
            AuthFlow::Picker { selected } => {
                let len = modron_core::auth::provider_registry().len();
                let selected = selected.min(len - 1);
                match key.code {
                    KeyCode::Up => {
                        self.auth_flow = Some(AuthFlow::Picker {
                            selected: (selected + len - 1) % len,
                        });
                    }
                    KeyCode::Down => {
                        self.auth_flow = Some(AuthFlow::Picker {
                            selected: (selected + 1) % len,
                        });
                    }
                    KeyCode::Esc => {} // close the manager
                    KeyCode::Enter | KeyCode::Tab => {
                        if let Some(info) = modron_core::auth::provider_registry().get(selected) {
                            self.auth_flow = Some(AuthFlow::MethodChoice {
                                info: info.clone(),
                                selected: 0,
                            });
                        } else {
                            self.auth_flow = None;
                        }
                    }
                    _ => self.auth_flow = Some(AuthFlow::Picker { selected }),
                }
            }
            AuthFlow::MethodChoice { info, selected } => {
                let methods = auth_methods(&info);
                let len = methods.len();
                let selected = selected.min(len - 1);
                match key.code {
                    KeyCode::Up => {
                        self.auth_flow = Some(AuthFlow::MethodChoice {
                            info,
                            selected: (selected + len - 1) % len,
                        });
                    }
                    KeyCode::Down => {
                        self.auth_flow = Some(AuthFlow::MethodChoice {
                            info,
                            selected: (selected + 1) % len,
                        });
                    }
                    KeyCode::Esc => {
                        // Back to the provider list, highlighting the
                        // provider we came from.
                        let selected = modron_core::auth::provider_registry()
                            .iter()
                            .position(|p| p.provider_id == info.provider_id)
                            .unwrap_or(0);
                        self.auth_flow = Some(AuthFlow::Picker { selected });
                    }
                    KeyCode::Enter | KeyCode::Tab => match methods[selected] {
                        AuthMethod::ApiKey => {
                            self.auth_flow = Some(AuthFlow::KeyEntry {
                                info,
                                input: String::new(),
                            });
                        }
                        AuthMethod::BrowserSignIn => {
                            if info.provider_id == "anthropic" {
                                // Claude: prepare the PKCE session now
                                // (pure); the run loop spawns the listener
                                // + token exchange.
                                match modron_core::auth::oauth::prepare_login() {
                                    Ok(session) => {
                                        let url = session.url.clone();
                                        self.pending_command =
                                            Some(PendingCommand::OAuthLogin { session });
                                        self.auth_flow = Some(AuthFlow::WaitingOAuth {
                                            info,
                                            url,
                                            user_code: None,
                                        });
                                    }
                                    Err(e) => self.note(format!("cannot start OAuth: {e}")),
                                }
                            } else {
                                // ChatGPT device flow: the URL + user code
                                // arrive from the spawned task.
                                self.pending_command = Some(PendingCommand::ChatGptLogin);
                                self.auth_flow = Some(AuthFlow::WaitingOAuth {
                                    info,
                                    url: String::new(),
                                    user_code: None,
                                });
                            }
                        }
                        AuthMethod::Remove => {
                            self.pending_command = Some(PendingCommand::AuthRemove {
                                name: info.name.to_string(),
                                env_var: info.env_var.to_string(),
                                provider_id: info.provider_id.to_string(),
                            });
                            self.auth_flow = None;
                        }
                    },
                    _ => self.auth_flow = Some(AuthFlow::MethodChoice { info, selected }),
                }
            }
            AuthFlow::KeyEntry { info, mut input } => {
                match key.code {
                    KeyCode::Char(c) => input.push(c),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Enter => {
                        if input.trim().is_empty() {
                            self.note("no key entered — nothing saved");
                        } else {
                            self.pending_command = Some(PendingCommand::AuthSave {
                                name: info.name.to_string(),
                                env_var: info.env_var.to_string(),
                                key: input.trim().to_string(),
                            });
                        }
                        // Flow ends either way; the saved note lands in the
                        // history masked.
                    }
                    KeyCode::Esc => {
                        // Back to the provider list (the modal stays open).
                        let selected = modron_core::auth::provider_registry()
                            .iter()
                            .position(|p| p.provider_id == info.provider_id)
                            .unwrap_or(0);
                        self.auth_flow = Some(AuthFlow::Picker { selected });
                    }
                    _ => {}
                }
                if !matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
                    self.auth_flow = Some(AuthFlow::KeyEntry { info, input });
                }
            }
            AuthFlow::WaitingOAuth {
                info,
                url,
                user_code,
            } => match key.code {
                // Esc backs out to the method screen (the manager stays
                // open); the run loop aborts the spawned flow when the
                // waiting screen is gone.
                KeyCode::Esc => {
                    self.auth_flow = Some(AuthFlow::MethodChoice { info, selected: 0 });
                }
                _ => {
                    self.auth_flow = Some(AuthFlow::WaitingOAuth {
                        info,
                        url,
                        user_code,
                    })
                }
            },
        }
        Action::None
    }

    /// Handle a key inside an open menu: arrows move, Enter re-dispatches
    /// the selected option's command (the same path as typing it), Esc
    /// closes.
    fn menu_key(&mut self, mut menu: Menu, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyCode;
        if menu.options.is_empty() {
            self.menu = None;
            return Action::None;
        }
        let len = menu.options.len();
        menu.selected = menu.selected.min(len - 1);
        match key.code {
            KeyCode::Up => {
                menu.selected = (menu.selected + len - 1) % len;
                self.menu = Some(menu);
            }
            KeyCode::Down => {
                menu.selected = (menu.selected + 1) % len;
                self.menu = Some(menu);
            }
            KeyCode::Esc => {} // close
            KeyCode::Enter | KeyCode::Tab => {
                let command = menu.options[menu.selected].command.clone();
                self.menu = None;
                return self.handle_slash(command);
            }
            // `s` on a model row: switch AND persist as the default.
            KeyCode::Char('s') | KeyCode::Char('S') => {
                if let SlashCommand::Model(name) = &menu.options[menu.selected].command {
                    let name = name.clone();
                    self.menu = None;
                    self.model_label = name.clone();
                    self.current_model = name.clone();
                    if let Some(info) = modron_router::lookup(&name) {
                        self.context_window = info.context_window;
                    }
                    self.pending_command = Some(PendingCommand::Model {
                        name: name.clone(),
                        save_default: true,
                    });
                    self.note(format!("switched to {name} and saving it as the default"));
                }
            }
            _ => self.menu = Some(menu),
        }
        Action::None
    }

    /// `/model` with no argument: the whole catalog as a picker, the
    /// current model marked and preselected.
    pub fn open_model_menu(&mut self) {
        let current = self.current_model.clone();
        let mut selected = 0;
        let mut options = Vec::new();
        for (i, m) in modron_router::catalog().iter().enumerate() {
            let is_current = m.name == current;
            if is_current {
                selected = i;
            }
            options.push(MenuOption {
                label: if is_current {
                    format!("● {}", m.name)
                } else {
                    m.name.clone()
                },
                detail: format!("{} · {}k", m.provider, m.context_window / 1000),
                command: SlashCommand::Model(m.name.clone()),
            });
        }
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: "model — ↑/↓ · enter switch · s switch+save default · esc close".to_string(),
            options,
            selected,
        });
    }

    /// `/theme` with no argument: built-in + user themes, current marked.
    pub fn open_theme_menu(&mut self) {
        let current = self.current_theme.clone();
        let mut selected = 0;
        let mut options = Vec::new();
        for (i, name) in Theme::available(&crate::theme_dir())
            .into_iter()
            .enumerate()
        {
            if name == current {
                selected = i;
            }
            options.push(MenuOption {
                label: if name == current {
                    format!("● {name}")
                } else {
                    name.clone()
                },
                detail: String::new(),
                command: SlashCommand::Theme(name),
            });
        }
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: "theme — ↑/↓ choose · enter switch · esc close".to_string(),
            options,
            selected,
        });
    }

    /// `/sandbox` with no argument: the three policies with plain-language
    /// detail.
    pub fn open_sandbox_menu(&mut self) {
        let options = [
            ("off", "no OS sandbox (approval gates only)"),
            (
                "seatbelt",
                "macOS: network denied, writes stay in the workspace",
            ),
            (
                "bubblewrap",
                "Linux: network denied, writes stay in the workspace",
            ),
        ]
        .iter()
        .map(|(mode, detail)| MenuOption {
            label: (*mode).to_string(),
            detail: detail.to_string(),
            command: SlashCommand::Sandbox((*mode).to_string()),
        })
        .collect();
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: "sandbox — ↑/↓ choose · enter set · esc close".to_string(),
            options,
            selected: 0,
        });
    }

    /// `/mode` with no argument: approval modes.
    pub fn open_mode_menu(&mut self) {
        let options = vec![
            MenuOption {
                label: "ask".to_string(),
                detail: "mutating tools prompt y/n/a".to_string(),
                command: SlashCommand::Mode("ask".to_string()),
            },
            MenuOption {
                label: "yolo".to_string(),
                detail: "mutating tools auto-approved".to_string(),
                command: SlashCommand::Mode("yolo".to_string()),
            },
        ];
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: "approval mode — ↑/↓ choose · enter set · esc close".to_string(),
            options,
            selected: 0,
        });
    }

    /// `/agent` with no argument: the configured personas.
    pub fn open_persona_menu(&mut self) {
        if self.personas.is_empty() {
            self.note("no personas configured — define [agents.<name>] in config.toml");
            return;
        }
        let options = self
            .personas
            .iter()
            .map(|name| MenuOption {
                label: name.clone(),
                detail: String::new(),
                command: SlashCommand::Agent(name.clone()),
            })
            .collect();
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: "persona — ↑/↓ choose · enter switch · esc close".to_string(),
            options,
            selected: 0,
        });
    }

    /// `/allow` or `/deny` with no argument: the registered tools.
    pub fn open_tool_menu(&mut self, allow: bool) {
        if self.tool_names.is_empty() {
            // Fall back to the pending listing (same as before menus).
            self.pending_command = Some(if allow {
                PendingCommand::Allow(String::new())
            } else {
                PendingCommand::Deny(String::new())
            });
            return;
        }
        let options = self
            .tool_names
            .iter()
            .map(|name| MenuOption {
                label: name.clone(),
                detail: String::new(),
                command: if allow {
                    SlashCommand::Allow(name.clone())
                } else {
                    SlashCommand::Deny(name.clone())
                },
            })
            .collect();
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: format!(
                "{} — ↑/↓ choose · enter apply · esc close",
                if allow { "allow tool" } else { "deny tool" }
            ),
            options,
            selected: 0,
        });
    }

    /// Route pasted text: into the auth flow's key buffer when the wizard
    /// is waiting for a key, into the normal input otherwise.
    pub fn paste_into_auth_or_input(&mut self, text: &str) {
        if self.menu.is_some() {
            return; // an open menu owns the keyboard; paste is ignored
        }
        if let Some(AuthFlow::KeyEntry { input, .. }) = &mut self.auth_flow {
            input.push_str(text);
        } else {
            self.input_history_pos = usize::MAX;
            self.input.push_str(text);
            self.cursor = self.input.chars().count();
        }
    }

    /// Insert a character at the cursor (char-offset domain). Editing
    /// leaves history-recall mode (the text becomes the new live input).
    fn insert_at_cursor(&mut self, c: char) {
        self.input_history_pos = usize::MAX;
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

    /// The input's DISPLAY layout: logical lines wrapped to the box's
    /// width, one [`InputRow`] per rendered row. Long single-line messages
    /// overflow onto extra rows instead of clipping (the Claude Code
    /// behavior the input needs for reading long tasks).
    fn input_rows(&self, width: u16) -> Vec<InputRow> {
        let usable = (width.saturating_sub(2) as usize).max(8);
        let mut rows = Vec::new();
        let mut offset = 0usize; // char offset into the whole input
        for (li, line) in self.input.split('\n').enumerate() {
            let chars: Vec<char> = line.chars().collect();
            if chars.is_empty() {
                rows.push(InputRow {
                    logical: li,
                    start: offset,
                    text: String::new(),
                });
                offset += 1; // the newline
                continue;
            }
            let mut chunk_start = 0usize;
            while chunk_start < chars.len() {
                let end = (chunk_start + usable).min(chars.len());
                rows.push(InputRow {
                    logical: li,
                    start: offset + chunk_start,
                    text: chars[chunk_start..end].iter().collect(),
                });
                chunk_start = end;
            }
            offset += chars.len() + 1; // chars + the newline
        }
        rows
    }

    /// The cursor's DISPLAY position: (row index into [`Self::input_rows`],
    /// column within that row).
    /// Public for tests.
    pub fn cursor_display(&self, width: u16) -> (usize, u16) {
        let rows = self.input_rows(width);
        for (ri, row) in rows.iter().enumerate() {
            let row_len = row.text.chars().count();
            if self.cursor >= row.start && self.cursor <= row.start + row_len {
                // Prefer this row when the cursor is strictly inside it, or
                // sits at its end and the next row starts a new logical
                // line (a real newline) — otherwise the boundary belongs
                // to the next wrapped row's start.
                let next_is_same_line = rows
                    .get(ri + 1)
                    .map(|n| n.logical == row.logical)
                    .unwrap_or(false);
                if self.cursor < row.start + row_len || !next_is_same_line {
                    return (ri, (self.cursor - row.start) as u16);
                }
            }
        }
        (rows.len().saturating_sub(1), 0)
    }

    /// Recall sent inputs with ↑/↓ (single-line input only; multi-line
    /// editing keeps the arrows for moving between lines). The live input
    /// is preserved as a draft and restored when scrolling past newest.
    fn recall_history(&mut self, up: bool) {
        const LIVE: usize = usize::MAX;
        if self.input_history.is_empty() {
            return;
        }
        if self.input_history_pos == LIVE {
            if !up {
                return; // ↓ while typing live: nothing to do
            }
            self.input_draft = std::mem::take(&mut self.input);
            self.input_history_pos = self.input_history.len() - 1;
        } else if up {
            self.input_history_pos = self.input_history_pos.saturating_sub(1);
        } else if self.input_history_pos + 1 >= self.input_history.len() {
            // ↓ past the newest: back to the live draft.
            self.input_history_pos = LIVE;
            self.input = std::mem::take(&mut self.input_draft);
            self.cursor = self.input.chars().count();
            return;
        } else {
            self.input_history_pos += 1;
        }
        self.input = self.input_history[self.input_history_pos].clone();
        self.cursor = self.input.chars().count();
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
            // Outside the input box: start a text selection in the
            // history (click-to-edit keeps the input box itself).
            self.begin_selection(column, row);
            return;
        }
        // A click inside the input abandons any selection.
        self.selection = None;
        let chip_rows = (!self.attachment_chips.is_empty()) as u16;
        let visible_row = row - inner.y;
        if visible_row < chip_rows {
            // Click landed on the attachment-chips row: cursor to the end.
            self.cursor = self.input.chars().count();
            return;
        }
        let text_row = (self.input_scroll + visible_row - chip_rows) as usize;
        let rows = self.input_rows(inner.width);
        match rows.get(text_row) {
            Some(row) => {
                let len = row.text.chars().count();
                let col = (column.saturating_sub(inner.x + 2) as usize).min(len);
                self.cursor = row.start + col;
            }
            // Below the text (empty rows of the box): cursor to the end.
            None => self.cursor = self.input.chars().count(),
        }
    }

    /// Map a screen row to a history index using the last render's
    /// geometry; None when the row isn't a committed history line (the
    /// scrolled-up indicator or a transient streaming tail).
    fn history_index_at(&self, row: u16) -> Option<usize> {
        let (start, body, following, top) = self.hist_geom;
        let indicator = if following { 0u16 } else { 1 };
        let rel = row.checked_sub(top + indicator)? as usize;
        if rel >= body || rel >= self.history.len().saturating_sub(start) {
            return None;
        }
        Some(start + rel)
    }

    /// Mouse press in the history zone: anchor a selection there.
    pub fn begin_selection(&mut self, column: u16, row: u16) {
        if let Some(idx) = self.history_index_at(row) {
            self.selection = Some(Selection {
                anchor: (idx, column),
                head: (idx, column),
            });
        } else {
            self.selection = None;
        }
    }

    /// Mouse drag: extend the selection head.
    pub fn extend_selection(&mut self, column: u16, row: u16) {
        if let Some(idx) = self.history_index_at(row) {
            if let Some(sel) = self.selection.as_mut() {
                sel.head = (idx, column);
            }
        }
    }

    /// The selected text (partial columns on the boundary rows, whole
    /// lines between). Empty selections yield None.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection?;
        let (mut a, mut b) = (sel.anchor, sel.head);
        if a > b {
            std::mem::swap(&mut a, &mut b);
        }
        let mut out = String::new();
        for i in a.0..=b.0 {
            let text: String = self
                .history
                .get(i)?
                .iter()
                .flat_map(|span| span.content.chars())
                .collect();
            let total = text.chars().count();
            let (cs, ce) = if i == a.0 && i == b.0 {
                (a.1 as usize, b.1 as usize)
            } else if i == a.0 {
                (a.1 as usize, total)
            } else if i == b.0 {
                (0, b.1 as usize)
            } else {
                (0, total)
            };
            let row: String = text.chars().skip(cs).take(ce.saturating_sub(cs)).collect();
            out.push_str(&row);
            if i < b.0 {
                out.push('\n');
            }
        }
        if out.trim().is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// The selected text, clearing the selection (used on mouse-up).
    pub fn take_selected_text(&mut self) -> Option<String> {
        let text = self.selected_text();
        self.selection = None;
        text
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
    /// scrolled up, the view stays anchored (offset unchanged). Lines
    /// wrap to the last rendered width so long content stays readable
    /// (the history Paragraph does not reflow).
    pub fn push_history(&mut self, lines: Vec<Line<'static>>) {
        let width = self.hist_wrap_width();
        for line in lines {
            self.history.extend(wrap_line(&line, width));
        }
    }

    /// A dim meta line into the scrollback (slash-command output).
    pub fn note(&mut self, text: impl Into<String>) {
        let dim = Style::default().fg(self.theme.resolved().dim);
        for row in wrap_line(&Line::from(text.into()).style(dim), self.hist_wrap_width()) {
            self.history.push(row);
        }
    }

    /// Push multi-line text as one dim row per line.
    fn push_lines(&mut self, text: impl Into<String>) {
        let dim = Style::default().fg(self.theme.resolved().dim);
        for line in text.into().lines() {
            for row in wrap_line(
                &Line::from(line.to_string()).style(dim),
                self.hist_wrap_width(),
            ) {
                self.history.push(row);
            }
        }
    }

    /// The width committed lines wrap to: the history zone's last rendered
    /// width, or a sane default before the first frame.
    fn hist_wrap_width(&self) -> usize {
        if self.last_hist_width > 20 {
            self.last_hist_width as usize
        } else {
            100
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
                self.selection = None;
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
                    self.open_model_menu();
                } else {
                    self.model_label = name.clone();
                    self.current_model = name.clone();
                    if let Some(info) = modron_router::lookup(&name) {
                        self.context_window = info.context_window;
                    }
                    // The engine itself switches in the run loop (each
                    // submit clones it) — the label alone would lie.
                    self.pending_command = Some(PendingCommand::Model {
                        name: name.clone(),
                        save_default: false,
                    });
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
                if mode.is_empty() {
                    self.open_sandbox_menu();
                } else {
                    // Applied to future tool calls by the run loop (each
                    // submit clones the engine).
                    self.pending_command = Some(PendingCommand::Sandbox(mode));
                }
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
            SlashCommand::Auth(arg) => {
                let arg = arg.trim();
                if arg.is_empty() || arg == "add" || arg == "manage" {
                    // The interactive manager: view status, add/replace,
                    // sign in, remove.
                    self.menu = None;
                    self.auth_flow = Some(AuthFlow::Picker { selected: 0 });
                } else {
                    self.pending_command = Some(PendingCommand::Auth(arg.to_string()));
                }
                Action::None
            }
            SlashCommand::Agent(name) => {
                if name.is_empty() {
                    self.open_persona_menu();
                } else {
                    self.pending_command = Some(PendingCommand::Agent(name));
                }
                Action::None
            }
            SlashCommand::Allow(tool) => {
                if tool.is_empty() {
                    self.open_tool_menu(true);
                } else {
                    self.pending_command = Some(PendingCommand::Allow(tool));
                }
                Action::None
            }
            SlashCommand::Deny(tool) => {
                if tool.is_empty() {
                    self.open_tool_menu(false);
                } else {
                    self.pending_command = Some(PendingCommand::Deny(tool));
                }
                Action::None
            }
            SlashCommand::Mode(mode) => {
                if mode.is_empty() {
                    self.open_mode_menu();
                } else {
                    self.pending_command = Some(PendingCommand::Mode(mode));
                }
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
                    self.open_theme_menu();
                } else if Theme::builtin_names().contains(&name) {
                    self.theme = Theme::builtin(name);
                    self.current_theme = name.to_string();
                    self.note(format!("theme: {name}"));
                } else {
                    match Theme::load(&crate::theme_dir(), name) {
                        Some(theme) => {
                            self.theme = theme;
                            self.current_theme = name.to_string();
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
        // The task echoes into the scrollback as an indented block,
        // wrapped to the history width so long messages stay readable.
        let mut lines = task.split('\n');
        let accent = Style::default().fg(self.theme.resolved().accent);
        let width = self.hist_wrap_width();
        for row in wrap_line(
            &Line::from(format!("❯ {}", lines.next().unwrap_or_default())).style(accent),
            width,
        ) {
            self.history.push(row);
        }
        let plain = Style::default();
        for line in lines {
            for row in wrap_line(&Line::from(format!("  {line}")).style(plain), width) {
                self.history.push(row);
            }
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
        // borders + chips row + one row per DISPLAY row — long lines wrap)
        // up to half the screen, then scrolls internally.
        let chip_rows = (!self.attachment_chips.is_empty()) as u16;
        let input_display_rows = self.input_rows(area.width.saturating_sub(2)).len() as u16;
        let input_height = (2 + chip_rows + input_display_rows)
            .min((area.height / 2).max(4))
            .min(area.height.saturating_sub(2));

        let matches = if self.auth_flow.is_none() && self.menu.is_none() {
            self.slash_matches()
        } else {
            Vec::new()
        };
        let sel = self.slash_selected.min(matches.len().saturating_sub(1));
        let popup_h = (matches.len() as u16).min(5);

        // The auth manager / command menus take the same slot as the
        // slash popup.
        let flow_h = if let Some(menu) = &self.menu {
            (menu.options.len() as u16 + 1).min(8)
        } else {
            match &self.auth_flow {
                None => 0,
                Some(AuthFlow::Picker { .. }) => {
                    (modron_core::auth::provider_registry().len() as u16 + 1).min(8)
                }
                Some(AuthFlow::MethodChoice { info, .. }) => auth_methods(info).len() as u16 + 1,
                Some(AuthFlow::KeyEntry { .. }) => 4,
                Some(AuthFlow::WaitingOAuth { .. }) => 3,
            }
        };

        let mut rows: Vec<Constraint> = vec![Constraint::Min(1)]; // scrollback
        if !self.subagents.is_empty() {
            rows.push(Constraint::Length(self.subagents.len() as u16 + 1));
        }
        rows.push(if self.pending_approval.is_some() {
            Constraint::Length(2)
        } else {
            Constraint::Length(1)
        });
        if flow_h > 0 {
            rows.push(Constraint::Length(flow_h));
        }
        if popup_h > 0 {
            rows.push(Constraint::Length(popup_h));
        }
        rows.push(Constraint::Length(input_height));
        let chunks = Layout::vertical(rows).split(area);
        let input_area = chunks[chunks.len() - 1];

        // ── Internal scrollback ──
        let hist_area = chunks[0];
        self.last_history_height = hist_area.height;
        self.last_hist_width = hist_area.width;
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
        // Geometry for mouse->history mapping (selections).
        self.hist_geom = (start, body.min(end - start), following, hist_area.y);
        let mut hist: Vec<Line> = Vec::new();
        if !following {
            let indicator = if start == 0 {
                "↑ top of session · pgdn scroll · esc live".to_string()
            } else {
                format!("↑ {start} lines above · pgup/pgdn scroll · esc live")
            };
            hist.push(Line::from(indicator).style(Style::default().fg(th.dim)));
        }
        for (k, line) in self.history[start..end].iter().enumerate() {
            let idx = start + k;
            match selection_window(self.selection, idx) {
                Some((cs, ce)) => hist.push(highlighted(line, cs, ce)),
                None => hist.push(line.clone()),
            }
        }
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

        // ── Interactive auth wizard modal ──
        if let Some(flow) = &self.auth_flow {
            let lines: Vec<Line> = match flow {
                AuthFlow::Picker { selected } => {
                    let registry = modron_core::auth::provider_registry();
                    let len = registry.len();
                    let sel = (*selected).min(len - 1);
                    // Window: 8 modal rows = 1 header + up to 7 rows,
                    // scrolled to keep the highlight visible.
                    let vis = len.min(7);
                    let start = sel.saturating_sub(vis - 1).min(len - vis);
                    let mut lines =
                        vec![Line::from("  auth — ↑/↓ choose · enter manage · esc close")
                            .style(Style::default().fg(th.dim))];
                    for (i, info) in registry.iter().enumerate().skip(start).take(vis) {
                        let selected_here = i == sel;
                        let pointer = if selected_here { "  ❯ " } else { "    " };
                        let style = if selected_here {
                            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        lines.push(Line::from(vec![
                            Span::styled(pointer.to_string(), Style::default().fg(th.accent)),
                            Span::styled(
                                format!("{:<20} {}", info.name, auth_row_status(info)),
                                style,
                            ),
                        ]));
                    }
                    lines
                }
                AuthFlow::MethodChoice { info, selected } => {
                    let methods = auth_methods(info);
                    let len = methods.len();
                    let sel = (*selected).min(len - 1);
                    let mut lines = vec![Line::from(format!(
                        "  {} — ↑/↓ choose · enter select · esc back",
                        info.name
                    ))
                    .style(Style::default().fg(th.dim))];
                    for (i, method) in methods.iter().enumerate() {
                        let selected_here = i == sel;
                        let pointer = if selected_here { "  ❯ " } else { "    " };
                        let style = if selected_here {
                            Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        };
                        lines.push(Line::from(vec![
                            Span::styled(pointer.to_string(), Style::default().fg(th.accent)),
                            Span::styled(method.label(info), style),
                        ]));
                    }
                    lines
                }
                AuthFlow::KeyEntry { info, input } => {
                    let masked = "•".repeat(input.chars().count());
                    vec![
                        Line::from(format!("  {} — get a key: {}", info.name, info.key_url))
                            .style(Style::default().fg(th.dim)),
                        Line::from(vec![
                            Span::styled(
                                format!("  paste {} ", info.env_var),
                                Style::default().fg(th.accent),
                            ),
                            Span::styled(masked, Style::default()),
                            Span::styled("▎", Style::default().fg(th.accent)),
                        ]),
                        Line::from("  enter save · esc back (ctrl+V pastes)")
                            .style(Style::default().fg(th.dim)),
                        Line::from(""),
                    ]
                }
                AuthFlow::WaitingOAuth {
                    info,
                    url,
                    user_code,
                } => {
                    let mut lines = Vec::new();
                    match user_code {
                        Some(code) => {
                            lines.push(
                                Line::from(format!("  {} — device flow", info.name))
                                    .style(Style::default().fg(th.accent)),
                            );
                            let open = if url.is_empty() {
                                "https://auth.openai.com/codex/device"
                            } else {
                                url.as_str()
                            };
                            lines.push(
                                Line::from(format!(
                                    "  code: {code}   browser opening (or visit: {open})"
                                ))
                                .style(Style::default().add_modifier(Modifier::BOLD)),
                            );
                        }
                        None => {
                            lines.push(
                                Line::from(format!("  {} — browser sign-in", info.name))
                                    .style(Style::default().fg(th.accent)),
                            );
                            lines.push(
                                Line::from(format!("  browser opening (or visit): {url}"))
                                    .style(Style::default().fg(th.dim)),
                            );
                        }
                    }
                    lines.push(
                        Line::from("  waiting for the sign-in… esc backs out")
                            .style(Style::default().fg(th.dim)),
                    );
                    lines
                }
            };
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            idx += 1;
        }

        // ── Interactive command menu (/model, /theme, ...) ──
        if let Some(menu) = &self.menu {
            let len = menu.options.len();
            let sel = menu.selected.min(len.saturating_sub(1));
            // Same window math as the auth picker: 8 modal rows = 1
            // header + up to 7 options.
            let vis = len.min(7);
            let start = sel.saturating_sub(vis - 1).min(len - vis);
            let mut lines =
                vec![Line::from(format!("  {}", menu.title)).style(Style::default().fg(th.dim))];
            for (i, option) in menu.options.iter().enumerate().skip(start).take(vis) {
                let selected_here = i == sel;
                let pointer = if selected_here { "  ❯ " } else { "    " };
                let style = if selected_here {
                    Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(pointer.to_string(), Style::default().fg(th.accent)),
                    Span::styled(option.label.clone(), style),
                    if option.detail.is_empty() {
                        Span::raw(String::new())
                    } else {
                        Span::styled(format!("  {}", option.detail), Style::default().fg(th.dim))
                    },
                ]));
            }
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            idx += 1;
        }

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

        // Keep the cursor's DISPLAY row inside the visible window (the box
        // scrolls vertically once the text outgrows it).
        let (cursor_row, cursor_col) = self.cursor_display(inner.width);
        let visible_text_rows = inner.height.saturating_sub(chip_rows);
        if visible_text_rows > 0 {
            let cr = cursor_row as u16;
            if cr < self.input_scroll {
                self.input_scroll = cr;
            } else if cr >= self.input_scroll + visible_text_rows {
                self.input_scroll = cr + 1 - visible_text_rows;
            }
        }

        // Pending-attachment chips ride as the first line inside the box,
        // then the scrolled window of DISPLAY rows (the prompt marks the
        // very first row of the input, not of the window). An empty input
        // shows the placeholder instead of a blank prompt row.
        let mut lines: Vec<Line> = Vec::new();
        if let Some(chips) =
            (!self.attachment_chips.is_empty()).then(|| self.attachment_chips.join(" "))
        {
            lines.push(Line::from(chips).style(Style::default().fg(th.accent)));
        }
        let scroll = self.input_scroll as usize;
        if self.input.is_empty() {
            let (text, style) = if self.auth_flow.is_some() {
                ("auth manager — esc to close", Style::default().fg(th.dim))
            } else if self.menu.is_some() {
                ("menu open — esc to close", Style::default().fg(th.dim))
            } else if self.running {
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
            let rows = self.input_rows(inner.width);
            let mut first = true;
            for row in rows.iter().skip(scroll) {
                if first {
                    // The prompt marks the input's first row.
                    lines.push(Line::from(vec![
                        Span::styled("❯ ", Style::default().fg(th.accent)),
                        Span::styled(row.text.clone(), Style::default()),
                    ]));
                    first = false;
                } else {
                    lines.push(Line::from(format!("  {}", row.text)));
                }
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);

        // Cursor block at the cursor's display (row, column) — the reason
        // the cursor is a char offset, not "the end of the string".
        if !self.running && !self.input.is_empty() {
            let visible_row = cursor_row.saturating_sub(scroll) as u16;
            let cursor_x = inner.x + 2 + cursor_col.min(inner.width.saturating_sub(3));
            let cursor_y = inner.y + chip_rows + visible_row;
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
