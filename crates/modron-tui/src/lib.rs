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
pub mod remote;
pub mod run;
pub mod theme;
pub use run::{run_with, RemoteSpec, TuiConfig, TuiSinks};
pub use theme::Theme;

use modron_core::{StreamDelta, ToolActivity, ToolCall, Usage};
use modron_view::{tool_summary_line, ToolEvent, TranscriptEntry};
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
    /// The interactive flow's Z.ai row: browser sign-in that mints a
    /// durable API key (the run loop spawns the callback + mint; the
    /// key is saved as ZAI_API_KEY).
    ZaiLogin {
        session: modron_core::auth::oauth::zai::ZaiLoginSession,
    },
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
}

/// Which tab of the help modal is showing (Claude Code's /help pattern:
/// an overview, the command table, the key bindings — three short pages
/// beat one long wall of text nobody scrolls).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HelpTab {
    #[default]
    General,
    Commands,
    Shortcuts,
}

impl HelpTab {
    /// Tab order with the labels the indicator line shows.
    const ALL: [(HelpTab, &'static str); 3] = [
        (HelpTab::General, "General"),
        (HelpTab::Commands, "Commands"),
        (HelpTab::Shortcuts, "Shortcuts"),
    ];

    /// ←/→ wrap around (a three-item ring — wrapping beats a dead key at
    /// either end, same as the popups' arrow handling).
    fn next(self) -> Self {
        match self {
            HelpTab::General => HelpTab::Commands,
            HelpTab::Commands => HelpTab::Shortcuts,
            HelpTab::Shortcuts => HelpTab::General,
        }
    }

    fn prev(self) -> Self {
        match self {
            HelpTab::General => HelpTab::Shortcuts,
            HelpTab::Commands => HelpTab::General,
            HelpTab::Shortcuts => HelpTab::Commands,
        }
    }
}

/// The General tab: what modron is and the core flow. Wrapped to 70
/// columns here rather than trusting the Paragraph — the modal renders
/// raw rows, and a long line would silently clip on a narrow terminal
/// (the same manual-wrap discipline as the history).
fn help_general_lines() -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for paragraph in [
        "modron is a token-efficient coding agent that lives in this \
         terminal: you type a task, the engine works it through tools \
         (shell, file edits, search) asking y/n before anything \
         destructive, and the answer streams into the scrollback above.",
        "Slash commands configure the session — /model, /sandbox, \
         /review — and never reach the model: type / to browse them all.",
        "This help has three tabs; see Shortcuts for every key binding \
         the TUI actually has.",
    ] {
        for row in wrap_line(&Line::from(format!("  {paragraph}")), 70) {
            lines.push(row);
        }
        lines.push(Line::from(""));
    }
    lines.pop(); // no trailing blank row (the footer hint sits there)
    lines
}

/// The Commands tab: [`SlashCommand::CATALOG`] one `/name` + description
/// per row — the exact table the completion popup filters, so a command
/// that parses is a command this page lists (a test holds that line).
fn help_command_lines(t: theme::Resolved) -> Vec<Line<'static>> {
    let name_w = SlashCommand::CATALOG
        .iter()
        .map(|(n, _)| n.len())
        .max()
        .unwrap_or(0);
    SlashCommand::CATALOG
        .iter()
        .map(|(name, desc)| {
            Line::from(vec![
                Span::raw(format!("  /{name:<name_w$}")),
                Span::styled(format!("  {desc}"), Style::default().fg(t.dim)),
            ])
        })
        .collect()
}

/// The Shortcuts tab: the key grid. Hand-written NEXT to the handlers it
/// documents — every row was verified against `on_key`/`run.rs` (the
/// requested "double-esc clears input" died here: esc's idle behavior is
/// a single-press cascade — selection, then jump-to-live, then input —
/// and no double-press state exists anywhere).
fn help_shortcut_lines(t: theme::Resolved) -> Vec<Line<'static>> {
    const W: usize = 18; // key column; the longest binding is "shift/alt+enter"
    let rows: [(&str, &str); 16] = [
        ("enter", "send the task"),
        (
            "shift/alt+enter",
            "newline in the input (ctrl+j works on any terminal)",
        ),
        (
            "\\ + enter",
            "line continuation — the same newline, everywhere",
        ),
        (
            "esc",
            "interrupt the running turn · close modals · clear input",
        ),
        (
            "pgup/pgdn",
            "page the session history (the mouse wheel scrolls too)",
        ),
        ("ctrl+t", "expand this turn's tool activity (esc closes)"),
        ("ctrl+o", "toggle transcript detail (compact/full)"),
        ("ctrl+c", "abort the running turn · quit when idle"),
        ("ctrl+v", "paste: image → attachment · text → input"),
        ("shift+tab", "flip the approval mode: ask | yolo"),
        ("↑/↓", "recall sent inputs · move between input lines"),
        (
            "mouse",
            "click positions the cursor · drag selects · release copies",
        ),
        ("?", "shortcuts (this modal — /help does the same)"),
        (
            "!command",
            "shell passthrough — output lands here, never the model",
        ),
        (
            "@path",
            "attach a workspace file to the task (tab completes)",
        ),
        ("tab", "complete the highlighted /command or @path"),
    ];
    rows.iter()
        .map(|(k, action)| {
            Line::from(vec![
                Span::styled(format!("  {:<W$}", k), Style::default().fg(t.accent)),
                Span::styled(*action, Style::default()),
            ])
        })
        .collect()
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
                "zai" => "sign in with Z.ai / GLM Coding Plan (browser)".to_string(),
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

/// Case-insensitive substring match for list filters.
fn list_matches(filter: &str, fields: &[&str]) -> bool {
    let needle = filter.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    fields.iter().any(|s| s.to_lowercase().contains(&needle))
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
    /// Ticks left before the toast (`status_note`) auto-clears. Notes must
    /// expire — a status line that never changes is wallpaper nobody reads.
    pub status_note_ticks: u32,
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
    /// Highlighted row in the `@file` completion popup (same grammar as
    /// the slash popup; typing resets it to the top).
    pub at_selected: usize,
    /// The workspace walk feeding `@file` completion, cached on the first
    /// live `@` token (the walk runs on the RENDER thread — once per
    /// @-session, never per keystroke) and invalidated on submit.
    pub file_completions: Option<Vec<String>>,
    /// The engine's working dir — the walk root for `@file` completion
    /// (injected by the run loop; empty = the process cwd).
    pub working_dir: std::path::PathBuf,
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
    /// This turn's tool events (grouped view: the history stays clean,
    /// one summary line lands at turn end; the panel expands on demand).
    pub turn_tools: Vec<ToolEvent>,
    /// Approval mode label for the status strip (`ask`/`yolo`/`deny`),
    /// mirrored from the engine config by the run loop; shift+tab and
    /// /mode keep it current. Empty = the engine's default (ask).
    pub approval_mode: String,
    /// Committed summary lines -> their events, newest last (capped) —
    /// clicking a summary line re-opens the panel for that turn.
    pub tool_log: Vec<(usize, Vec<ToolEvent>)>,
    /// The expanded tool-activity overlay (events snapshot).
    pub tool_panel: Option<Vec<ToolEvent>>,
    /// Scroll offset of the expanded tool panel.
    pub tool_panel_scroll: usize,
    /// The live tool strip's rect from the last render (click target).
    pub last_tool_strip: ratatui::layout::Rect,
    /// The tool panel's rect from the last render (outside-click close).
    pub last_tool_panel: ratatui::layout::Rect,
    /// Type-to-filter query for open list modals (menus + the auth
    /// picker). Cleared when a modal opens/closes; Esc clears it first.
    pub list_filter: String,
    /// The input box's inner area as of the last render — maps mouse
    /// clicks to cursor positions.
    pub last_input_inner: ratatui::layout::Rect,
    /// The interactive auth manager (modal; owns all keys while open).
    pub auth_flow: Option<AuthFlow>,
    /// An interactive menu — the arg-less form of a slash command that
    /// takes one (`/model`, `/theme`, ...). Mutually exclusive with the
    /// auth manager; owns all keys while open.
    pub menu: Option<Menu>,
    /// The tabbed help modal (/help, bare `?`): General / Commands /
    /// Shortcuts. Preempts menu/auth on open (two key owners would fight
    /// over esc) and owns all keys while open, like the menu.
    pub help: Option<HelpTab>,
    /// Scroll offset of the help modal's content (the Commands tab is
    /// taller than any honest window); reset on open and tab switch,
    /// clamped at render.
    pub help_scroll: usize,
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
    pub hist_geom: (usize, usize, bool, u16, u16),
}

/// An in-TUI text selection over history lines: `(history index, column)`
/// anchor and head. Columns are char offsets into the line (the line is
/// not wrapped — one history line renders as one row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub anchor: (usize, u16),
    pub head: (usize, u16),
}

// The grouped tool view (`ToolEvent`, `human_bytes`, `tool_summary_line`)
// moved to modron-view so servers, the desktop app, and mobile clients
// group identically; imported at the top of this module.

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
/// Screen column -> char offset for a rendered line: walk the spans
/// counting each char's real glyph width (CJK/emoji are 2 columns) until
/// the column is reached. Keeps mouse selection aligned with what the
/// user sees when wide glyphs precede the click point.
fn screen_col_to_char_line(line: &Line<'_>, col: usize) -> usize {
    let mut width = 0usize;
    let mut chars = 0usize;
    for span in line.spans.iter() {
        for ch in span.content.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + w > col {
                return chars;
            }
            width += w;
            chars += 1;
        }
    }
    chars
}

/// `screen_col_to_char_line` for plain text (input-box rows).
fn screen_col_to_char_str(text: &str, col: usize) -> usize {
    let mut width = 0usize;
    for (i, ch) in text.chars().enumerate() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > col {
            return i;
        }
        width += w;
    }
    text.chars().count()
}

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
/// "save as default" path. Returns the path written. MODRON_CONFIG follows
/// the same convention as the CLI loader: a directory holding config.toml,
/// with a value naming a `.toml` file honored as-is.
pub fn save_default_model(model: &str, provider: &str) -> std::io::Result<std::path::PathBuf> {
    let path = match std::env::var("MODRON_CONFIG") {
        Ok(dir) if !dir.is_empty() => {
            let dir = std::path::PathBuf::from(dir);
            if dir.extension().is_some_and(|e| e == "toml") || dir.is_file() {
                dir
            } else {
                dir.join("config.toml")
            }
        }
        _ => std::env::var("HOME")
            .map(|h| {
                std::path::PathBuf::from(h)
                    .join(".config")
                    .join("modron")
                    .join("config.toml")
            })
            .unwrap_or_else(|_| ".modron-config.toml".into()),
    };
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
    match std::env::var("MODRON_CONFIG") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
            .join(".config")
            .join("modron"),
    }
    .join("themes")
}

/// Mathematical spinner: rotating phase-space glyphs (quarter-circle phases,
/// like a phase portrait rotating) — deliberately NOT Claude Code's braille.
const SPINNER: [&str; 4] = ["◐", "◓", "◑", "◒"];
/// Working spinner: calculus operators cycling (partial → nabla → delta →
/// increment) — signals active computation.
const SPINNER_WORKING: [&str; 4] = ["∂", "∇", "∆", "∇"];
/// Toast lifetime in spinner ticks (~2s at the 50ms poll). Notices that
/// never expire turn into wallpaper — the old `status_note` did exactly
/// that, so every note is now a toast with a fuse.
const TOAST_TICKS: u32 = 40;
/// Cap on the `@file` completion walk (entries collected): the render
/// thread pays for this once per @-session, and 200 paths already
/// outnumbers any honest completion need.
const FILE_COMPLETION_CAP: usize = 200;

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
                // ctrl+t: expand the grouped tool activity (Esc closes).
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    let events = self.latest_tool_events();
                    if events.is_empty() {
                        self.note("no tool activity yet");
                    } else {
                        self.open_tool_panel(events);
                    }
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

        // The expanded tool panel owns the keyboard while open (Esc must
        // close it, not abort a running turn).
        if self.tool_panel.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.tool_panel = None;
                    return Action::None;
                }
                KeyCode::Up => {
                    self.tool_panel_scroll = self.tool_panel_scroll.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Down => {
                    let len = self.tool_panel.as_ref().map_or(0, |e| e.len());
                    self.tool_panel_scroll =
                        (self.tool_panel_scroll + 1).min(len.saturating_sub(1));
                    return Action::None;
                }
                _ => return Action::None,
            }
        }

        // The tabbed help modal (/help, bare `?`) owns every key while
        // open — placed BEFORE the Esc-abort check so esc closes the
        // modal instead of killing a running turn (the menu's contract).
        if self.help.is_some() {
            return self.help_key(key);
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

        // Esc while running interrupts (type-ahead edits land in the input
        // for the next task). AFTER the modal blocks above: Esc with an
        // open menu or auth wizard must close the modal, not abort the
        // turn (both modals document "owns every key").
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

        // @file completion popup (a live `@token` sits in the input): the
        // same binding grammar as the slash popup — arrows move the
        // highlight, Tab splices the selected path into the input. Bound
        // only while the popup is open.
        let at_files = self.file_matches();
        if !at_files.is_empty() {
            let len = at_files.len();
            let sel = self.at_selected.min(len - 1);
            match key.code {
                KeyCode::Up => {
                    self.at_selected = (sel + len - 1) % len;
                    return Action::None;
                }
                KeyCode::Down => {
                    self.at_selected = (sel + 1) % len;
                    return Action::None;
                }
                KeyCode::Tab => {
                    self.complete_at(&at_files[sel]);
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
                // The @-completion cache dies with the submit: the task may
                // create or delete files, and the next `@` deserves a fresh
                // walk.
                self.file_completions = None;
                self.at_selected = 0;
                // Bare `?`: the status strip's "? shortcuts" hint — same
                // help as /help, minus the slash.
                if task.trim() == "?" {
                    return self.handle_slash(SlashCommand::Help);
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
                self.at_selected = 0;
                Action::None
            }
            KeyCode::Delete => {
                self.input_history_pos = usize::MAX;
                if self.cursor < self.input.chars().count() {
                    let byte = char_byte_idx(&self.input, self.cursor);
                    self.input.remove(byte);
                }
                self.slash_selected = 0;
                self.at_selected = 0;
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
            // shift+tab: flip the approval mode (the strip's key hint).
            // The label flips immediately; the engine mutation rides
            // PendingCommand::Mode exactly like the /mode menu — it
            // applies from the next submit (each submit clones the engine).
            KeyCode::BackTab => {
                let next = if self.approval_mode == "yolo" {
                    "ask"
                } else {
                    "yolo"
                };
                self.approval_mode = next.to_string();
                self.pending_command = Some(PendingCommand::Mode(next.to_string()));
                self.toast(format!("approval mode: {next}"));
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
                self.at_selected = 0;
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
                // Type-to-filter over the provider rows (name, id, status);
                // Enter opens the highlighted match.
                let matches = self.filtered_registry_indices();
                if matches.is_empty() {
                    let mut closed = false;
                    match key.code {
                        KeyCode::Backspace => {
                            self.list_filter.pop();
                        }
                        KeyCode::Char(c) => {
                            self.list_filter.push(c);
                        }
                        KeyCode::Esc => {
                            if self.list_filter.is_empty() {
                                self.auth_flow = None;
                                closed = true;
                            } else {
                                self.list_filter.clear();
                            }
                        }
                        _ => {}
                    }
                    // Esc may have CLOSED the manager above — do not
                    // resurrect it unconditionally.
                    if !closed {
                        self.auth_flow = Some(AuthFlow::Picker { selected: 0 });
                    }
                    return Action::None;
                }
                let len = matches.len();
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
                    KeyCode::Backspace => {
                        self.list_filter.pop();
                        self.auth_flow = Some(AuthFlow::Picker { selected: 0 });
                    }
                    KeyCode::Char(c) => {
                        self.list_filter.push(c);
                        self.auth_flow = Some(AuthFlow::Picker { selected: 0 });
                    }
                    KeyCode::Esc => {
                        if self.list_filter.is_empty() {
                            // close the manager
                        } else {
                            self.list_filter.clear();
                            self.auth_flow = Some(AuthFlow::Picker { selected: 0 });
                        }
                    }
                    KeyCode::Enter | KeyCode::Tab => {
                        if let Some(info) =
                            modron_core::auth::provider_registry().get(matches[selected])
                        {
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
                            } else if info.provider_id == "zai" {
                                // Z.ai: browser sign-in that mints a durable
                                // API key; the run loop spawns the rest.
                                match modron_core::auth::oauth::zai::prepare_login() {
                                    Ok(session) => {
                                        let url = session.url.clone();
                                        self.pending_command =
                                            Some(PendingCommand::ZaiLogin { session });
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

    /// Handle a key inside an open menu: typing filters (label + detail),
    /// arrows move within the matches, Enter re-dispatches the selected
    /// option's command (the same path as typing it), Esc clears an
    /// active filter before closing. Shift+S on a model row saves it as
    /// the default (plain letters belong to the filter).
    fn menu_key(&mut self, mut menu: Menu, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyCode;
        let matches = self.filtered_menu_indices(&menu);
        if matches.is_empty() {
            // Nothing matches: only editing the filter or leaving helps.
            let mut closed = false;
            match key.code {
                KeyCode::Backspace => {
                    self.list_filter.pop();
                }
                KeyCode::Char(c) => {
                    self.list_filter.push(c);
                }
                KeyCode::Esc => {
                    if self.list_filter.is_empty() {
                        self.menu = None;
                        closed = true;
                    } else {
                        self.list_filter.clear();
                    }
                }
                _ => {}
            }
            // Esc may have CLOSED the menu above — do not resurrect it.
            if !closed {
                menu.selected = 0;
                self.menu = Some(menu);
            }
            return Action::None;
        }
        let len = matches.len();
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
            KeyCode::Esc => {
                if self.list_filter.is_empty() {
                    // close
                } else {
                    self.list_filter.clear();
                    menu.selected = 0;
                    self.menu = Some(menu);
                }
            }
            KeyCode::Backspace => {
                self.list_filter.pop();
                menu.selected = 0;
                self.menu = Some(menu);
            }
            KeyCode::Enter | KeyCode::Tab => {
                let command = menu.options[matches[menu.selected]].command.clone();
                self.menu = None;
                self.list_filter.clear();
                return self.handle_slash(command);
            }
            // Shift+S on a model row: switch AND persist as the default.
            KeyCode::Char('S') => {
                let option = &menu.options[matches[menu.selected]];
                if let SlashCommand::Model(name) = &option.command {
                    let name = name.clone();
                    self.menu = None;
                    self.list_filter.clear();
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
                } else {
                    self.menu = Some(menu);
                }
            }
            KeyCode::Char(c) => {
                self.list_filter.push(c);
                menu.selected = 0;
                self.menu = Some(menu);
            }
            _ => self.menu = Some(menu),
        }
        Action::None
    }

    /// Test seam: how many menu options match the active filter.
    pub fn filtered_menu_indices_count(&self) -> usize {
        self.menu
            .as_ref()
            .map(|m| self.filtered_menu_indices(m).len())
            .unwrap_or(0)
    }

    /// Indices of the menu options matching the active filter.
    fn filtered_menu_indices(&self, menu: &Menu) -> Vec<usize> {
        menu.options
            .iter()
            .enumerate()
            .filter(|(_, option)| list_matches(&self.list_filter, &[&option.label, &option.detail]))
            .map(|(i, _)| i)
            .collect()
    }

    /// Open the help modal on its first tab. The modal slots are mutually
    /// exclusive (help renders in the menu's chunk, and two key owners
    /// would fight over esc), so opening help closes whatever else is up
    /// — the same preemption /auth does to the menu.
    fn open_help(&mut self) {
        self.menu = None;
        self.auth_flow = None;
        self.help_scroll = 0;
        self.help = Some(HelpTab::default());
    }

    /// Handle a key inside the help modal: ←/→ (or h/l) switch tabs —
    /// wrapping, and resetting the scroll since each tab starts at its
    /// top — ↑/↓ (pgup/pgdn) page a tab taller than the window, esc
    /// closes. Everything else is swallowed: a modal that let keys leak
    /// to the input underneath would type ghost text into a box the user
    /// cannot see.
    fn help_key(&mut self, key: crossterm::event::KeyEvent) -> Action {
        use crossterm::event::KeyCode;
        let tab = self.help.unwrap_or_default();
        match key.code {
            KeyCode::Esc => self.help = None,
            KeyCode::Left | KeyCode::Char('h') => {
                self.help = Some(tab.prev());
                self.help_scroll = 0;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.help = Some(tab.next());
                self.help_scroll = 0;
            }
            KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
            KeyCode::Down => self.help_scroll += 1, // clamped at render
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
            KeyCode::PageDown => self.help_scroll += 10,
            _ => {}
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
            // Unauthenticated models stay selectable (the user may be
            // mid-setup) but visibly need credentials.
            let authed = modron_core::auth::catalog_provider_authenticated(&m.provider);
            options.push(MenuOption {
                label: if is_current {
                    format!("● {}", m.name)
                } else {
                    m.name.clone()
                },
                detail: if authed {
                    format!("{} · {}k", m.provider, m.context_window / 1000)
                } else {
                    format!("{} · {}k · no key", m.provider, m.context_window / 1000)
                },
                command: SlashCommand::Model(m.name.clone()),
            });
        }
        self.list_filter.clear();
        self.auth_flow = None;
        self.menu = Some(Menu {
            title: "model — type to filter · enter switch · S save default".to_string(),
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
        self.list_filter.clear();
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
        self.list_filter.clear();
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
        self.list_filter.clear();
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
        self.list_filter.clear();
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
        self.list_filter.clear();
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
        if self.menu.is_some() || self.help.is_some() {
            return; // an open modal owns the keyboard; paste is ignored
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
    /// Registry indices matching the active filter (name, id, status).
    fn filtered_registry_indices(&self) -> Vec<usize> {
        modron_core::auth::provider_registry()
            .iter()
            .enumerate()
            .filter(|(_, info)| {
                list_matches(
                    &self.list_filter,
                    &[info.name, info.provider_id, &auth_row_status(info)],
                )
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn on_click(&mut self, column: u16, row: u16) {
        // The expanded tool panel: any click OUTSIDE it collapses it.
        if self.tool_panel.is_some() {
            let panel = self.last_tool_panel;
            if row < panel.y || row >= panel.y + panel.height {
                self.tool_panel = None;
            }
            return;
        }
        // The live tool strip opens the panel.
        let strip = self.last_tool_strip;
        if strip.height > 0 && row >= strip.y && row < strip.y + strip.height {
            let events = self.turn_tools.clone();
            if !events.is_empty() {
                self.open_tool_panel(events);
            }
            return;
        }
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
                // Width-aware column conversion (double-width glyphs).
                let col =
                    screen_col_to_char_str(&row.text, column.saturating_sub(inner.x + 2) as usize);
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
        let (start, body, following, top, _) = self.hist_geom;
        let indicator = if following { 0u16 } else { 1 };
        let rel = row.checked_sub(top + indicator)? as usize;
        if rel >= body || rel >= self.history.len().saturating_sub(start) {
            return None;
        }
        Some(start + rel)
    }

    /// The history zone's left screen column (last render).
    fn history_x(&self) -> u16 {
        self.hist_geom.4
    }

    /// Mouse press in the history zone: anchor a selection there —
    /// unless the row is a grouped tool summary, which expands instead.
    /// Screen columns convert to CHAR offsets (double-width glyphs count
    /// once) so selection/copy match what the user sees highlighted.
    pub fn begin_selection(&mut self, column: u16, row: u16) {
        if let Some(idx) = self.history_index_at(row) {
            if let Some((_, events)) = self.tool_log.iter().find(|(i, _)| *i == idx) {
                self.open_tool_panel(events.clone());
                return;
            }
        }
        if let Some(idx) = self.history_index_at(row) {
            let col = self.history_char_column(idx, column);
            self.selection = Some(Selection {
                anchor: (idx, col),
                head: (idx, col),
            });
        } else {
            self.selection = None;
        }
    }

    /// Mouse drag: extend the selection head (same column conversion).
    pub fn extend_selection(&mut self, column: u16, row: u16) {
        if let Some(idx) = self.history_index_at(row) {
            let col = self.history_char_column(idx, column);
            if let Some(sel) = self.selection.as_mut() {
                sel.head = (idx, col);
            }
        }
    }

    /// Screen column -> char offset on history line `idx`, walking the
    /// line's spans with real glyph widths.
    fn history_char_column(&self, idx: usize, column: u16) -> u16 {
        let Some(line) = self.history.get(idx) else {
            return 0;
        };
        let col = column.saturating_sub(self.history_x()) as usize;
        screen_col_to_char_line(line, col) as u16
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

    /// The live `@` token for file completion: (char offset of the `@`,
    /// fragment after it). The `@` must START a token (input start or after
    /// whitespace — `a@b` is an email address, not a completion) and the
    /// fragment must hold no whitespace (that token ended; the user is on
    /// to the next word). Cursor-independent: Tab splices over the token
    /// wherever the caret sits.
    #[doc(hidden)]
    pub fn at_token(&self) -> Option<(usize, String)> {
        let (at, _) = self.input.char_indices().rev().find(|(_, ch)| *ch == '@')?;
        if at > 0 && !self.input[..at].ends_with(char::is_whitespace) {
            return None;
        }
        let frag: String = self.input[at + 1..]
            .chars()
            .take_while(|ch| !ch.is_whitespace())
            .collect();
        Some((at, frag))
    }

    /// The `@`-completion candidates: the cached workspace walk filtered
    /// by the live fragment (case-insensitive substring — friendlier than
    /// the slash popup's prefix for paths). The walk itself runs on the
    /// render thread, so it happens ONCE per @-session ([`App::tick`] does
    /// not touch it; the Enter submit invalidates it).
    pub fn file_matches(&mut self) -> Vec<String> {
        let Some((_, frag)) = self.at_token() else {
            return Vec::new();
        };
        if self.file_completions.is_none() {
            self.file_completions = Some(collect_files(self.walk_root()));
        }
        let files = self.file_completions.as_deref().unwrap_or_default();
        files
            .iter()
            .filter(|path| list_matches(&frag, &[path]))
            .cloned()
            .collect()
    }

    /// Tab on an `@` completion: splice the chosen path over the live
    /// token (`@fragment` → `@path `), leaving the rest of the input
    /// untouched, cursor landed after the inserted space.
    fn complete_at(&mut self, path: &str) {
        let Some((at, frag)) = self.at_token() else {
            return;
        };
        let start = char_byte_idx(&self.input, at);
        let end = char_byte_idx(&self.input, at + 1 + frag.chars().count());
        let mut out = String::with_capacity(self.input.len() - (end - start) + path.len() + 2);
        out.push_str(&self.input[..start]);
        out.push('@');
        out.push_str(path);
        out.push(' ');
        out.push_str(&self.input[end..]);
        self.input = out;
        self.cursor = at + 1 + path.chars().count() + 1;
        self.at_selected = 0;
    }

    /// The walk root for `@file` completion: the engine's working dir once
    /// the run loop injects it; the process cwd before that (and for
    /// directly-constructed App states).
    fn walk_root(&self) -> &std::path::Path {
        if self.working_dir.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            &self.working_dir
        }
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

    /// Rebuild scrollback from a replayed transcript (remote attach): the
    /// server's store is the source of truth and modron-view did the
    /// grouping; this renders each entry with the same styles the live
    /// path commits (echo accent, tool color, dim meta). Tool summaries
    /// also land in `tool_log` so ctrl+t / click re-opens their panels.
    pub fn backfill_transcript(&mut self, entries: Vec<TranscriptEntry>) {
        let t = self.theme.resolved();
        for entry in entries {
            match entry {
                TranscriptEntry::Task { text } | TranscriptEntry::User { text } => {
                    let mut lines = text.split('\n');
                    let accent = Style::default().fg(t.accent);
                    for row in wrap_line(
                        &Line::from(format!("❯ {}", lines.next().unwrap_or_default()))
                            .style(accent),
                        self.hist_wrap_width(),
                    ) {
                        self.history.push(row);
                    }
                    let plain = Style::default();
                    for line in lines {
                        for row in wrap_line(
                            &Line::from(format!("  {line}")).style(plain),
                            self.hist_wrap_width(),
                        ) {
                            self.history.push(row);
                        }
                    }
                }
                TranscriptEntry::Assistant { text } => {
                    let plain = Style::default();
                    for line in text.split('\n') {
                        for row in wrap_line(
                            &Line::from(line.to_string()).style(plain),
                            self.hist_wrap_width(),
                        ) {
                            self.history.push(row);
                        }
                    }
                    self.history.push(Line::from(""));
                }
                TranscriptEntry::ToolSummary { line, events } => {
                    let index = self.history.len();
                    let tool = Style::default().fg(t.tool);
                    for row in wrap_line(&Line::from(line).style(tool), self.hist_wrap_width()) {
                        self.history.push(row);
                    }
                    self.tool_log.push((index, events));
                }
                TranscriptEntry::Swarm { lines } => {
                    self.push_lines(lines.join("\n"));
                }
                TranscriptEntry::Model { model, reason } => {
                    self.note(format!("model {model} ({reason})"));
                }
                TranscriptEntry::Usage { model, usage } => {
                    let denom = usage.input_tokens + usage.cached_input_tokens;
                    let cache = if denom > 0 {
                        format!(
                            " · cache {:.0}%",
                            100.0 * usage.cached_input_tokens as f64 / denom as f64
                        )
                    } else {
                        String::new()
                    };
                    self.note(format!("{} · {} tok{}", model, usage.total(), cache));
                }
                TranscriptEntry::Compaction {
                    summarized_events, ..
                } => {
                    let warn = Style::default().fg(t.warning);
                    for row in wrap_line(
                        &Line::from(format!("[compacted {summarized_events} earlier events]"))
                            .style(warn),
                        self.hist_wrap_width(),
                    ) {
                        self.history.push(row);
                    }
                }
            }
        }
        // Same cap as the live path (commit_tool_summary).
        let len = self.tool_log.len();
        if len > 20 {
            self.tool_log.drain(..len - 20);
        }
    }

    /// A dim meta line into the scrollback (slash-command output).
    pub fn note(&mut self, text: impl Into<String>) {
        let dim = Style::default().fg(self.theme.resolved().dim);
        for row in wrap_line(&Line::from(text.into()).style(dim), self.hist_wrap_width()) {
            self.history.push(row);
        }
    }

    /// Raise a toast: the note renders as a one-line highlighted strip
    /// directly above the input and auto-clears after [`TOAST_TICKS`]
    /// (~2s at the 50ms poll). Every transient notice goes through here —
    /// `status_note` written by hand never expires (the old bug).
    pub fn toast(&mut self, text: impl Into<String>) {
        self.status_note = Some(text.into());
        self.status_note_ticks = TOAST_TICKS;
    }

    /// One render-loop tick: advance the spinner and age the toast.
    /// A note whose fuse hit zero is cleared — fire-and-forget notices
    /// must not outlive their usefulness.
    pub fn tick(&mut self) {
        self.spinner_tick += 1;
        if self.status_note_ticks > 0 {
            self.status_note_ticks -= 1;
            if self.status_note_ticks == 0 {
                self.status_note = None;
            }
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
                // The tabbed modal (Claude Code's pattern) replaces the old
                // static text block — a scrollback dump can't be tabbed,
                // filtered, or closed, and it pushed real content out of
                // the history the user came to read.
                self.open_help();
                Action::None
            }
            SlashCommand::Clear => {
                self.history.clear();
                self.history_offset = 0;
                self.streaming.clear();
                self.reasoning.clear();
                self.selection = None;
                // Stale (index, events) pairs would match NEW history rows
                // against the WRONG turn's tool log on the next click.
                self.tool_log.clear();
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
                    self.list_filter.clear();
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
            StreamDelta::ToolCallReady(_) => {}
            StreamDelta::Done => {}
        }
    }

    /// Apply a tool-activity event (live `●` lines).
    pub fn on_tool(&mut self, activity: ToolActivity) {
        match activity {
            ToolActivity::Start { name } => {
                self.current_tool = Some(name.clone());
                // Grouped view: tools never hit the scrollback one by one
                // (the wall of `● glob` lines made sessions unreadable);
                // the live strip shows the latest, the panel expands.
                self.turn_tools.push(ToolEvent {
                    name,
                    bytes: None,
                    is_error: false,
                    done: false,
                });
            }
            ToolActivity::Done {
                name,
                is_error,
                bytes,
            } => {
                self.current_tool = None;
                if let Some(event) = self
                    .turn_tools
                    .iter_mut()
                    .rev()
                    .find(|e| e.name == name && !e.done)
                {
                    event.done = true;
                    event.is_error = is_error;
                    event.bytes = Some(bytes as u64);
                } else {
                    self.turn_tools.push(ToolEvent {
                        name,
                        bytes: Some(bytes as u64),
                        is_error,
                        done: true,
                    });
                }
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
    /// Commit this turn's tool events as one grouped summary line and
    /// remember them for the expandable panel (capped at 20 turns).
    fn commit_tool_summary(&mut self) {
        if self.turn_tools.is_empty() {
            return;
        }
        let summary = tool_summary_line(&self.turn_tools);
        let index = self.history.len();
        let t = self.theme.resolved();
        // One row only — it is the click target for the expandable panel.
        let row = wrap_line(
            &Line::from(summary).style(Style::default().fg(t.tool)),
            self.hist_wrap_width(),
        )
        .into_iter()
        .next()
        .unwrap_or_default();
        self.history.push(row);
        self.tool_log
            .push((index, std::mem::take(&mut self.turn_tools)));
        let len = self.tool_log.len();
        if len > 20 {
            self.tool_log.drain(..len - 20);
        }
    }

    /// Open the expanded panel for a set of events (current turn or a
    /// logged one), scrolled to the bottom.
    pub fn open_tool_panel(&mut self, events: Vec<ToolEvent>) {
        self.tool_panel_scroll = events.len().saturating_sub(1);
        self.tool_panel = Some(events);
    }

    /// The current-turn events, or the newest logged turn's.
    fn latest_tool_events(&self) -> Vec<ToolEvent> {
        if !self.turn_tools.is_empty() {
            return self.turn_tools.clone();
        }
        self.tool_log
            .last()
            .map(|(_, e)| e.clone())
            .unwrap_or_default()
    }

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

        // The turn's tool activity lands as ONE grouped summary line
        // (click it — or ctrl+t — to expand the full log).
        self.commit_tool_summary();

        // Commit into the internal scrollback (returned for tests). Answer
        // lines wrap like every other commit path — the history paragraph
        // has no wrap of its own, and unwrapped lines break the
        // one-row-per-line selection/click geometry.
        let mut lines: Vec<Line<'static>> = Vec::new();
        if answer.is_empty() {
            lines.push(
                Line::from("(no answer — turn/budget limit hit; see transcript)")
                    .style(Style::default().fg(t.warning)),
            );
        } else {
            let width = self.hist_wrap_width();
            for line in answer.lines() {
                lines.extend(wrap_line(&Line::from(line.to_string()), width));
            }
        }
        // Cache discipline: per-turn ratio plus the SESSION cumulative —
        // a falling cache hit rate is the cheapest early warning that
        // request prefixes went unstable (cost multiplies silently).
        let session_cache = {
            let all = self.cumulative.input_tokens + self.cumulative.cached_input_tokens;
            if all > 0 {
                format!(
                    " (session {:.0}%)",
                    100.0 * self.cumulative.cached_input_tokens as f64 / all as f64
                )
            } else {
                String::new()
            }
        };
        lines.push(
            Line::from(format!(
                "  {} tok · {} turns · {} cache{session_cache}",
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
        self.commit_tool_summary();
        self.streaming.clear();
        self.current_tool = None;
        self.toast("interrupted — partial work is persisted; resume with --resume");
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
        self.status_note_ticks = 0;
        self.turn_tools.clear();
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

    /// The one-line status strip: `<model> · <tok> tok · cache <pct>% ·
    /// ctx <est%> · <mode> (shift+tab) · ? shortcuts …` — everything the
    /// operator glances at, one dense key-hinted line (the old line
    /// sprawled and still hid the cache rate and approval mode). Transient
    /// notes are NOT part of it: they render as a toast above the input
    /// and expire (see [`App::toast`]).
    fn status_spans(&self) -> Vec<Span<'static>> {
        let t = self.theme.resolved();
        let dim = Style::default().fg(t.dim);
        let spinner = if self.running {
            SPINNER_WORKING[(self.spinner_tick as usize) % SPINNER_WORKING.len()]
        } else {
            SPINNER[(self.spinner_tick as usize) % SPINNER.len()]
        };
        let mut spans = vec![
            Span::styled(format!("{spinner} "), Style::default().fg(t.accent)),
            Span::styled(self.model_label.clone(), dim),
            Span::styled(format!(" · {} tok", self.cumulative.total()), dim),
        ];
        // Session-cumulative cache hit rate — a falling number is the
        // cheapest early warning that request prefixes went unstable
        // (same arithmetic as finish_turn's summary line).
        let cache_denom = self.cumulative.input_tokens + self.cumulative.cached_input_tokens;
        if cache_denom > 0 {
            spans.push(Span::styled(
                format!(
                    " · cache {:.0}%",
                    100.0 * self.cumulative.cached_input_tokens as f64 / cache_denom as f64
                ),
                dim,
            ));
        }
        // Estimated context pressure; at the 80% compaction trigger the
        // segment flips to the warning color and shouts `!`.
        if self.context_window > 0 {
            let pct = self.ctx_pct();
            spans.push(Span::styled(
                format!(" · ctx {pct}%{}", if pct >= 80 { "!" } else { "" }),
                if pct >= 80 {
                    Style::default().fg(t.warning)
                } else {
                    dim
                },
            ));
        }
        spans.push(Span::styled(
            format!(" · {} (shift+tab)", self.approval_label()),
            dim,
        ));
        if self.running {
            spans.push(Span::styled(
                format!(" · turn {}", self.turn_count + 1),
                dim,
            ));
            if let Some(tool) = &self.current_tool {
                spans.push(Span::styled(
                    format!(" · {tool}"),
                    Style::default().fg(t.tool),
                ));
            }
            spans.push(Span::styled(" · esc interrupt", dim));
        } else {
            spans.push(Span::styled(" · ? shortcuts", dim));
            spans.push(Span::styled(
                format!(
                    " · ctrl+o {}",
                    if self.transcript_mode {
                        "full"
                    } else {
                        "compact"
                    }
                ),
                dim,
            ));
            spans.push(Span::styled(format!(" · {}", self.session_label()), dim));
            if !self.input.is_empty() {
                spans.push(Span::styled(" · esc clears", dim));
            }
        }
        spans
    }

    /// Estimated context-window pressure as a whole percent.
    fn ctx_pct(&self) -> u64 {
        if self.context_window == 0 {
            return 0;
        }
        let frac = (self.context_used as f64 / self.context_window as f64).min(1.0);
        (frac * 100.0) as u64
    }

    /// The approval-mode label (`""` = unset = the engine's ask default).
    fn approval_label(&self) -> &str {
        if self.approval_mode.is_empty() {
            "ask"
        } else {
            &self.approval_mode
        }
    }

    /// The session id, shortened to its first 8 chars (or "no session").
    fn session_label(&self) -> String {
        self.session_id
            .map(|id| format!("session {}", &id.to_string()[..8]))
            .unwrap_or_else(|| "no session".into())
    }

    /// The plain-text status line: the approval prompt while one is
    /// pending, otherwise the consolidated strip (see [`App::status_spans`]).
    pub fn status_line(&self) -> String {
        if let Some(pending) = &self.pending_approval {
            return format!(
                "⚠ allow {} {}? [y/N/a]",
                pending.call.name,
                short(&pending.call.arguments.to_string(), 80)
            );
        }
        self.status_spans()
            .iter()
            .map(|s| s.content.as_ref())
            .collect()
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

        let modal_open = self.auth_flow.is_some() || self.menu.is_some() || self.help.is_some();
        let matches = if modal_open {
            Vec::new()
        } else {
            self.slash_matches()
        };
        // @file completion shares the popup slot — slash wins (the two
        // tokens can't be live at once: a `/word` prefix has no whitespace
        // yet, so no @ boundary).
        let at_files = if modal_open || !matches.is_empty() {
            Vec::new()
        } else {
            self.file_matches()
        };
        let sel = self.slash_selected.min(matches.len().saturating_sub(1));
        let at_sel = self.at_selected.min(at_files.len().saturating_sub(1));
        let popup_h = (matches.len().max(at_files.len()) as u16).min(5);

        // The auth manager / command menus take the same slot as the
        // slash popup.
        let flow_h = if let Some(menu) = &self.menu {
            let n = self.filtered_menu_indices(menu).len() as u16;
            (n + 1).min(8).max(if n == 0 { 2 } else { 1 })
        } else {
            match &self.auth_flow {
                None => 0,
                Some(AuthFlow::Picker { .. }) => {
                    let n = self.filtered_registry_indices().len() as u16;
                    (n + 1).min(8).max(if n == 0 { 2 } else { 1 })
                }
                Some(AuthFlow::MethodChoice { info, .. }) => auth_methods(info).len() as u16 + 1,
                Some(AuthFlow::KeyEntry { .. }) => 4,
                Some(AuthFlow::WaitingOAuth { .. }) => 3,
            }
        };

        // The help modal: tab strip + up to half the screen of content +
        // the footer hint (a floor of 6 keeps the tab strip readable on
        // cramped terminals; taller tabs page through with ↑/↓).
        let help_h = match self.help {
            None => 0,
            Some(tab) => {
                let content = match tab {
                    HelpTab::General => help_general_lines().len(),
                    HelpTab::Commands => help_command_lines(th).len(),
                    HelpTab::Shortcuts => help_shortcut_lines(th).len(),
                } as u16;
                (content + 2).min((area.height / 2).max(6))
            }
        };

        let tool_panel_rows = self.tool_panel.as_ref().map_or(0, |events| {
            (events.len() as u16 + 2).min((area.height / 2).max(4))
        });
        let tool_strip_rows =
            (self.running && !self.turn_tools.is_empty() && self.tool_panel.is_none()) as u16;
        let mut rows: Vec<Constraint> = vec![Constraint::Min(1)]; // scrollback
        if !self.subagents.is_empty() {
            rows.push(Constraint::Length(self.subagents.len() as u16 + 1));
        }
        if tool_panel_rows > 0 {
            rows.push(Constraint::Length(tool_panel_rows));
        }
        if tool_strip_rows > 0 {
            rows.push(Constraint::Length(1));
        }
        rows.push(if self.pending_approval.is_some() {
            Constraint::Length(2)
        } else {
            Constraint::Length(1)
        });
        if flow_h > 0 {
            rows.push(Constraint::Length(flow_h));
        }
        if help_h > 0 {
            rows.push(Constraint::Length(help_h));
        }
        if popup_h > 0 {
            rows.push(Constraint::Length(popup_h));
        }
        if self.status_note.is_some() {
            // The toast strip hugs the input: transient notices sit where
            // the eye already is, not in the status line's old slot.
            rows.push(Constraint::Length(1));
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
        self.hist_geom = (
            start,
            body.min(end - start),
            following,
            hist_area.y,
            hist_area.x,
        );
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

        // ── Expanded tool-activity panel (ctrl+t / click) ──
        if let Some(events) = &self.tool_panel {
            let len = events.len();
            let vis = (len + 2).min((area.height / 2).max(4) as usize) - 2;
            self.tool_panel_scroll = self.tool_panel_scroll.min(len.saturating_sub(1));
            let start = self
                .tool_panel_scroll
                .saturating_sub(vis.saturating_sub(1))
                .min(len.saturating_sub(vis));
            let mut lines =
                vec![
                    Line::from(format!("  tool activity ({len}) — ↑/↓ scroll · esc close"))
                        .style(Style::default().fg(th.dim)),
                ];
            for event in events.iter().skip(start).take(vis) {
                let style = if event.is_error {
                    Style::default().fg(th.error)
                } else {
                    Style::default().fg(th.tool)
                };
                lines.push(Line::from(format!("  {}", event.summary())).style(style));
            }
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            self.last_tool_panel = chunks[idx];
            idx += 1;
        } else {
            self.last_tool_panel = ratatui::layout::Rect::default();
        }

        // ── Live tool strip: the LATEST tool + count (grouped view) ──
        if tool_strip_rows > 0 {
            let latest = self
                .turn_tools
                .last()
                .map(|e| e.summary())
                .unwrap_or_default();
            let line = Line::from(format!(
                "  {latest} · {} tools this turn — ctrl+t to expand",
                self.turn_tools.len()
            ))
            .style(Style::default().fg(th.tool));
            frame.render_widget(Paragraph::new(vec![line]), chunks[idx]);
            self.last_tool_strip = chunks[idx];
            idx += 1;
        } else {
            self.last_tool_strip = ratatui::layout::Rect::default();
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
                    let matches = self.filtered_registry_indices();
                    let len = matches.len().max(1);
                    let sel = (*selected).min(len.saturating_sub(1));
                    // Window: 8 modal rows = 1 header + up to 7 rows,
                    // scrolled to keep the highlight visible.
                    let vis = matches.len().min(7);
                    let start = sel.saturating_sub(vis.max(1) - 1).min(len - vis.max(1));
                    let header = if self.list_filter.is_empty() {
                        "  auth — type to filter · enter manage · esc close".to_string()
                    } else {
                        format!("  auth · filter: {:?}", self.list_filter)
                    };
                    let mut lines = vec![Line::from(header).style(Style::default().fg(th.dim))];
                    if matches.is_empty() {
                        lines.push(
                            Line::from("  (no matches — backspace edits the filter)")
                                .style(Style::default().fg(th.dim)),
                        );
                    }
                    for (i, info) in matches
                        .iter()
                        .enumerate()
                        .skip(start)
                        .take(vis)
                        .map(|(i, opt)| (i, &registry[*opt]))
                    {
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
            let matches = self.filtered_menu_indices(menu);
            let len = matches.len().max(1);
            let sel = menu.selected.min(len.saturating_sub(1));
            // Same window math as the auth picker: 8 modal rows = 1
            // header + up to 7 options.
            let vis = matches.len().min(7);
            let start = sel.saturating_sub(vis.max(1) - 1).min(len - vis.max(1));
            let header = if self.list_filter.is_empty() {
                format!("  {}", menu.title)
            } else {
                format!("  {} · filter: {:?}", menu.title, self.list_filter)
            };
            let mut lines = vec![Line::from(header).style(Style::default().fg(th.dim))];
            if matches.is_empty() {
                lines.push(
                    Line::from("  (no matches — backspace edits the filter)")
                        .style(Style::default().fg(th.dim)),
                );
            }
            for (i, option) in matches
                .iter()
                .enumerate()
                .skip(start)
                .take(vis)
                .map(|(i, opt)| (i, &menu.options[*opt]))
            {
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

        // ── Tabbed help modal (/help, bare ?) — the menu's render style
        // (dim header row, plain content, dim hint) with a tab strip for
        // a header: the active tab is accent+underlined, the others dim.
        if let Some(tab) = self.help {
            let content = match tab {
                HelpTab::General => help_general_lines(),
                HelpTab::Commands => help_command_lines(th),
                HelpTab::Shortcuts => help_shortcut_lines(th),
            };
            let vis = (help_h as usize).saturating_sub(2).max(1);
            self.help_scroll = self.help_scroll.min(content.len().saturating_sub(vis));
            let mut strip: Vec<Span> = vec![Span::raw("  ")];
            for (i, (t, label)) in HelpTab::ALL.iter().enumerate() {
                if i > 0 {
                    strip.push(Span::styled("   ", Style::default().fg(th.dim)));
                }
                let style = if *t == tab {
                    Style::default()
                        .fg(th.accent)
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                } else {
                    Style::default().fg(th.dim)
                };
                strip.push(Span::styled((*label).to_string(), style));
            }
            let mut lines = vec![Line::from(strip)];
            for line in content.iter().skip(self.help_scroll).take(vis) {
                lines.push(line.clone());
            }
            lines.push(
                Line::from("  ←/→ tabs · ↑/↓ scroll · esc close")
                    .style(Style::default().fg(th.dim)),
            );
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            idx += 1;
        }

        // Slash-command popup (window around the highlight when the
        // filtered list outgrows the space), or the @file popup sharing
        // the same slot and grammar.
        if popup_h > 0 && !matches.is_empty() {
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
            idx += 1;
        } else if popup_h > 0 {
            let h = popup_h as usize;
            let start = at_sel.saturating_sub(h - 1).min(at_files.len() - h);
            let lines: Vec<Line> = at_files[start..start + h]
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    let selected = start + i == at_sel;
                    let pointer = if selected { "❯ " } else { "  " };
                    Line::from(vec![
                        Span::styled(pointer, Style::default().fg(th.accent)),
                        Span::styled(
                            format!("@{path}"),
                            if selected {
                                Style::default().fg(th.accent).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                    ])
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), chunks[idx]);
            idx += 1;
        }

        // Toast strip: the transient status note, highlighted, directly
        // above the input. Aged out by App::tick — a note that never
        // expires is wallpaper.
        if let Some(note) = self.status_note.as_ref() {
            // REVERSED flips the accent into a background: a strip that
            // reads as an overlay, not as more history.
            let highlight = Style::default()
                .fg(th.accent)
                .add_modifier(Modifier::REVERSED);
            let line = Line::from(format!(" {note} ")).style(highlight);
            frame.render_widget(Paragraph::new(line), chunks[idx]);
            // No idx bump — the input box (next) addresses its own chunk
            // as the last row.
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
            } else if self.help.is_some() {
                (
                    "help open — ←/→ tabs · esc close",
                    Style::default().fg(th.dim),
                )
            } else if self.running {
                ("", Style::default())
            } else {
                // Rotating capability hints (claude-code/opencode pattern):
                // the empty input teaches the surface organically, cycling
                // on the spinner tick so it stays alive but quiet.
                const HINTS: [&str; 8] = [
                    "type a task (or / for commands)…",
                    "try /model — switch models mid-session",
                    "try /auth — sign in or reuse existing logins",
                    "try @file — attach a file to the task",
                    "try !command — run shell passthrough",
                    "try /tasks — check background subagents",
                    "try /fork — branch the session to explore",
                    "try ctrl+t — expand the tool activity panel",
                ];
                (
                    HINTS[(self.spinner_tick.div_euclid(8) as usize) % HINTS.len()],
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

    /// The status line as styled spans — a thin wrapper over
    /// [`App::status_spans`] (single source of truth for the strip).
    fn styled_status_line(&self) -> Line<'static> {
        Line::from(self.status_spans())
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

/// Walk the workspace for `@`-completion candidates: the `ignore` crate's
/// walker (respects .gitignore, skips hidden files), with the two heavy
/// build dirs filtered even when no .gitignore covers them. Capped hard
/// ([`FILE_COMPLETION_CAP`]) — the render thread pays for this walk, once
/// per @-session, and 200 sorted paths outnumber any honest completion.
fn collect_files(root: &std::path::Path) -> Vec<String> {
    let mut out: Vec<String> = ignore::WalkBuilder::new(root)
        .require_git(false) // gitignore discipline without a .git dir
        .filter_entry(|e| {
            // The root passes (depth 0); below it, never descend into the
            // trees every workspace could do without.
            e.depth() == 0 || (e.file_name() != "target" && e.file_name() != "node_modules")
        })
        .build()
        .flatten()
        .filter_map(|entry| {
            let rel = entry.path().strip_prefix(root).ok()?;
            if rel.as_os_str().is_empty() {
                return None; // the root itself
            }
            Some(rel.to_string_lossy().replace('\\', "/"))
        })
        .take(FILE_COMPLETION_CAP)
        .collect();
    out.sort();
    out
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
