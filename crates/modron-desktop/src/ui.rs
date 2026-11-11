//! [`ui`] -- the GPUI Kit renderer over [`crate::app_state::AppState`].
//!
//! Thin by design: every fact on screen is an [`AppState`] projection;
//! this module owns only presentation (layout, colors, buttons) and the
//! GPUI event loop plumbing (the net thread's events are drained on the
//! UI executor and folded into the state).
//!
//! Parity notes (AGENTS.md): copy and grouping come from modron-view and
//! the TUI's exact wordings; keys diverge by the normative table (a stop
//! button instead of esc-to-interrupt, click-to-open tool panel, buttons
//! for approvals). The model picker keeps the TUI's modal semantics --
//! type-to-filter, enter/click to switch, esc backs out (clearing the
//! filter first) -- and the provider follows the model via the shared
//! catalog ([`modron_router::lookup`]).

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::Icon;
use gpui_kit::component::{ActiveTheme, Selectable, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    actions, div, list, px, AnyElement, App, AppContext, ClickEvent, Context, Entity, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyBinding, ListAlignment, ListState,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowOptions,
};
use gpui_kit::{Animation, AnimationExt};
use modron_core::Attachment;
use modron_protocol::{Decision, Payload};
use modron_router::lookup;
use modron_view::TranscriptEntry;
use std::time::Duration;

use crate::app_state::{decision_label, model_patch, AppState, ConnState, Row};
use crate::wire::{Command, UiEvent};

actions!(
    modron_desktop,
    [PickerClose, PickerUp, PickerDown, CloseToolPanel]
);

/// One model-picker row (the shared router catalog).
struct ModelRow {
    name: String,
    provider: String,
    context_window: u64,
}

fn catalog_rows() -> Vec<ModelRow> {
    modron_router::catalog()
        .iter()
        .map(|m| ModelRow {
            name: m.name.clone(),
            provider: m.provider.clone(),
            context_window: m.context_window,
        })
        .collect()
}

gpui_kit::actions!(modron_desktop, [NewChat, ComposerSubmit]);

static NEXT_IMAGE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A gpui render image from raw bytes (thumbnails for staged photos).
fn render_image(format: gpui_kit::ImageFormat, bytes: Vec<u8>) -> std::sync::Arc<gpui_kit::Image> {
    std::sync::Arc::new(gpui_kit::Image {
        format,
        bytes,
        id: NEXT_IMAGE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
    })
}

fn image_format_of(media_type: &str) -> Option<gpui_kit::ImageFormat> {
    match media_type {
        "image/png" => Some(gpui_kit::ImageFormat::Png),
        "image/jpeg" => Some(gpui_kit::ImageFormat::Jpeg),
        "image/webp" => Some(gpui_kit::ImageFormat::Webp),
        "image/gif" => Some(gpui_kit::ImageFormat::Gif),
        "image/bmp" => Some(gpui_kit::ImageFormat::Bmp),
        _ => None,
    }
}

/// Launch configuration from the CLI.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// `--connect <url>`: the modron server's WebSocket URL.
    pub connect: Option<String>,
    /// `--demo`: run the built-in mock server and connect to it.
    pub demo: bool,
    /// `--resume` (the server's newest session) / `--resume=<id>` (that
    /// session): attach on connect. Parity with the TUI's `--resume`.
    /// `None` = start detached; `Some(None)` = newest; `Some(Some(id))`.
    pub resume: Option<Option<uuid::Uuid>>,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            connect: Some(format!("ws://127.0.0.1:{}", modron_protocol::DEFAULT_PORT)),
            demo: false,
            resume: None,
        }
    }
}

impl LaunchOptions {
    /// Parse `--connect <url>` / `--demo` / `--resume[=<id>]` from the
    /// process args.
    pub fn from_args() -> Self {
        Self::parse(std::env::args().skip(1))
    }

    /// Parse from an explicit arg list (testable).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Self {
        let mut options = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--connect" => {
                    options.connect = args.next();
                    options.demo = false;
                }
                "--demo" => {
                    options.demo = true;
                }
                "--resume" => {
                    options.resume = Some(None);
                }
                other if other.starts_with("--resume=") => {
                    match other["--resume=".len()..].parse::<uuid::Uuid>() {
                        Ok(id) => options.resume = Some(Some(id)),
                        Err(e) => eprintln!("invalid --resume id: {e}"),
                    }
                }
                other => {
                    eprintln!(
                        "unknown argument {other:?}; usage: modron-desktop [--connect <url>|--demo] [--resume[=<id>]]"
                    );
                }
            }
        }
        options
    }
}

/// The desktop app entity. Protocol frames land in `state`; rendering is
/// a pure projection (see `display` and the render_* methods).
pub struct DesktopApp {
    state: AppState,
    cmd_tx: async_channel::Sender<Command>,
    /// The rows the transcript list renders (state.rows(), cached).
    display: Vec<Row>,
    list_state: ListState,
    composer: Entity<TextareaState>,
    /// Live slash-command candidates (a leading "/" in the composer).
    slash_candidates: Vec<&'static str>,
    /// The /help overlay (the shared slash-command table).
    help_open: bool,
    filter: Entity<InputState>,
    /// The sidebar's type-to-filter input (sessions only; the model
    /// picker has its own).
    session_filter: Entity<InputState>,
    session_filter_query: String,
    /// The picker filter text, mirrored here on every refresh so the
    /// render pass can filter without touching the input entity.
    filter_query: String,
    picker_open: bool,
    picker_selected: usize,
    tool_panel_open: bool,
    /// `Some` pins a committed turn's events (clicked from a summary
    /// line); `None` shows the live tracker rows.
    panel_rows: Option<Vec<String>>,
    focus: FocusHandle,
    /// `--resume` waiting for the first Sessions frame (then attaches the
    /// named session, or the newest).
    pending_resume: Option<Option<uuid::Uuid>>,
    /// 16ms animation tick (drives the running-session shimmer).
    anim_tick: u64,
    /// Weak self for callbacks that render-time elements carry (the
    /// composer's paste interception).
    weak_self: gpui_kit::WeakEntity<DesktopApp>,
    /// This window's handle: deferred work (form inputs) needs the window
    /// to construct input state.
    window_handle: Option<gpui_kit::AnyWindowHandle>,
    /// Generative-UI form inputs, cached per transcript row index.
    form_inputs: std::collections::HashMap<
        usize,
        Vec<gpui_kit::Entity<gpui_kit::component::input::InputState>>,
    >,
    /// A session row the user marked for deletion (two-step confirm).
    confirm_delete: Option<uuid::Uuid>,
    /// The sidebar search input is expanded (the header's magnifier
    /// toggles it; closing clears the query).
    search_open: bool,
    /// The session row the pointer is over, driving the hover-revealed
    /// controls (state-driven: gpui group_hover proved unreliable inside
    /// the scroll list).
    hovered_row: Option<uuid::Uuid>,
    /// Cluster sessions by their folder (the list-filter toggle; off
    /// renders one flat newest-first list).
    grouped: bool,
    /// Settings overlay open (gear in the footer).
    settings_open: bool,
    /// Settings-page inputs (session + defaults cards).
    provider_key_input: Entity<InputState>,
    key_target: Option<String>,
    allow_input: Entity<InputState>,
    deny_input: Entity<InputState>,
    goal_input: Entity<InputState>,
    cheap_model_input: Entity<InputState>,
    strong_model_input: Entity<InputState>,
    /// Automations add/edit-form fields (name/prompt/interval).
    auto_name: Entity<InputState>,
    auto_prompt: Entity<InputState>,
    auto_interval: Entity<InputState>,
    /// The automation being edited (remove+add flow on save).
    editing_automation: Option<String>,
    /// Onboarding wizard step (0 = providers, 1 = model wiring).
    onboarding_step: Option<usize>,
    onboarding_cheap: Entity<InputState>,
    onboarding_strong: Entity<InputState>,
    onboarding_strong_fb: Entity<InputState>,
    /// The "New task" inline form (Conductor/Para grammar: a task spawns
    /// an agent in its own worktree + branch).
    new_task_open: bool,
    new_task_title: Entity<InputState>,
    new_task_base: Entity<InputState>,
    /// Per-session review overlay (files +/- counts from task_status).
    review_open: Option<uuid::Uuid>,
    review_files: Vec<modron_protocol::TaskFileStat>,
    review_branch: String,
    /// Generative-UI form inputs, cached per transcript row index.
    /// Images/documents staged for the next submit: drag-and-drop, the
    /// attach button, or clipboard paste. The tuple is `(attachment,
    /// display label, thumbnail)`; the thumbnail is prebuilt so rendering
    /// never re-encodes the bytes.
    ///
    /// The display label is what the chip shows; the thumbnail is the
    /// prebuilt render image (None for text documents, which render a
    /// page glyph instead).
    staged: Vec<(Attachment, String, Option<std::sync::Arc<gpui_kit::Image>>)>,
}

impl DesktopApp {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        cmd_tx: async_channel::Sender<Command>,
        events: async_channel::Receiver<UiEvent>,
        resume: Option<Option<uuid::Uuid>>,
    ) -> Self {
        let composer = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .placeholder("type a task; cmd+enter sends, shift+enter = newline");
            state.set_auto_grow(1, 8, cx);
            state
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("type to filter"));
        let session_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter sessions"));
        let provider_key_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("paste an api key"));
        let allow_input = cx.new(|cx| InputState::new(window, cx).placeholder("grep, read"));
        let deny_input = cx.new(|cx| InputState::new(window, cx).placeholder("bash"));
        let goal_input = cx.new(|cx| InputState::new(window, cx).placeholder("a standing goal"));
        let cheap_model_input = cx.new(|cx| InputState::new(window, cx).placeholder("cheap model"));
        let strong_model_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("strong model"));

        // Typing updates the slash-command popup: a leading "/" filters
        // the shared table (modron_view::slash); Enter-free selection via
        // click keeps the grammar simple (cmd+enter sends).
        cx.subscribe_in(
            &composer,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                    let text = this.composer.read(cx).value().to_string();
                    let next = if let Some(token) = text.strip_prefix('/') {
                        let token = token.split_whitespace().next().unwrap_or("");
                        modron_view::slash::SLASH_COMMANDS
                            .iter()
                            .filter(|c| c.name.starts_with(token))
                            .map(|c| c.name)
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    if next != this.slash_candidates {
                        this.slash_candidates = next;
                        this.refresh(cx);
                    }
                }
            },
        )
        .detach();

        // Enter in the picker filter confirms the highlighted model.
        cx.subscribe_in(
            &filter,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    if this.picker_open {
                        this.confirm_picker(cx);
                    }
                }
            },
        )
        .detach();

        // Drain the net thread: every protocol event folds into AppState.
        cx.spawn(async move |this, cx| loop {
            let Ok(event) = events.recv().await else {
                break;
            };
            let Ok(()) = this.update(cx, |app, cx| app.apply_event(event, cx)) else {
                break;
            };
        })
        .detach();

        let mut state = AppState::new();
        state.conn = ConnState::Connecting;
        let mut app = Self {
            state,
            cmd_tx,
            display: Vec::new(),
            list_state: ListState::new(0, ListAlignment::Bottom, px(256.)),
            composer,
            slash_candidates: Vec::new(),
            provider_key_input,
            key_target: None,
            allow_input,
            deny_input,
            goal_input,
            cheap_model_input,
            strong_model_input,
            help_open: false,
            filter,
            session_filter,
            session_filter_query: String::new(),
            filter_query: String::new(),
            picker_open: false,
            picker_selected: 0,
            tool_panel_open: false,
            panel_rows: None,
            focus: cx.focus_handle(),
            pending_resume: resume,
            staged: Vec::new(),
            weak_self: cx.weak_entity(),
            window_handle: None,
            form_inputs: Default::default(),
            confirm_delete: None,
            search_open: false,
            hovered_row: None,
            grouped: true,
            anim_tick: 0,
            settings_open: false,
            new_task_open: false,
            auto_name: cx.new(|cx| InputState::new(window, cx).placeholder("name")),
            auto_prompt: cx
                .new(|cx| InputState::new(window, cx).placeholder("prompt to run on schedule")),
            auto_interval: cx
                .new(|cx| InputState::new(window, cx).placeholder("interval (minutes)")),
            editing_automation: None,
            onboarding_step: None,
            onboarding_cheap: cx.new(|cx| {
                InputState::new(window, cx).placeholder("cheap model (e.g. gpt-5.6-terra)")
            }),
            onboarding_strong: cx.new(|cx| {
                InputState::new(window, cx).placeholder("strong model (e.g. gpt-6-astra)")
            }),
            onboarding_strong_fb: cx
                .new(|cx| InputState::new(window, cx).placeholder("strong fallback (optional)")),
            review_open: None,
            new_task_title: cx
                .new(|cx| InputState::new(window, cx).placeholder("what should the agent do?")),
            new_task_base: cx.new(|cx| {
                InputState::new(window, cx).placeholder("base branch (default: current)")
            }),
            review_files: Vec::new(),
            review_branch: String::new(),
        };
        // Typewriter streaming: advance the visible cursor a few chars per
        // tick so chunk-granular deltas read like a per-character stream.
        // The same tick drives the sidebar shimmer on running tasks.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(16))
                .await;
            let Ok(()) = this.update(cx, |app, cx| {
                if app.state.advance_stream() {
                    app.refresh(cx);
                }
            }) else {
                break;
            };
        })
        .detach();
        // Follow OS appearance switches on this window (light <-> dark).
        window
            .observe_window_appearance(|_window, cx| {
                Theme::change(ThemeMode::from(cx.window_appearance()), None, cx);
            })
            .detach();
        app.refresh(cx);
        app
    }

    /// Recompute the renderer-visible projections and redraw.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.display = self.state.rows();
        self.filter_query = self.filter.read(cx).value().to_string();
        self.session_filter_query = self.session_filter.read(cx).value().to_string();
        self.list_state.reset(self.display.len());
        cx.notify();
    }

    fn send(&self, command: Command) {
        let _ = self.cmd_tx.send_blocking(command);
    }

    fn apply_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Frame(frame) => {
                // A finished sign-in changes credential statuses; refresh
                // the providers card without a manual reload.
                if let Payload::OAuthProgress { stage, .. } = &frame.payload {
                    if stage == "done" {
                        self.send(Command::AuthProviders {});
                    }
                }
                let is_sessions = matches!(frame.payload, Payload::Sessions { .. });
                // Review-overlay updates: task_status/merged answers land in
                // DesktopApp fields (not the transcript).
                match &frame.payload {
                    Payload::TaskStatus {
                        session_id,
                        worktree,
                        branch,
                        running,
                        ahead,
                        files,
                    } => {
                        if self.review_open.as_ref() == Some(session_id) {
                            self.review_branch = format!(
                                "{branch} · {ahead} ahead{} · {worktree}",
                                if *running { " · running" } else { "" }
                            );
                            self.review_files = files.clone();
                        }
                    }
                    Payload::TaskMerged {
                        session_id,
                        ok,
                        summary,
                    } => {
                        if self.review_open.as_ref() == Some(session_id) {
                            self.review_open = None;
                        }
                        self.state.last_note = Some(if *ok {
                            summary.clone()
                        } else {
                            format!("merge failed: {summary}")
                        });
                    }
                    _ => {}
                }
                self.state.apply_frame(*frame);
                // --resume: attach as soon as the session list is known
                // (bare = the server's newest; an id = that session).
                if is_sessions {
                    if let Some(want) = self.pending_resume.take() {
                        let target = want.or_else(|| self.state.sessions.first().map(|s| s.id));
                        if let Some(id) = target {
                            self.attach(id, cx);
                        }
                    }
                }
            }
            UiEvent::Connected(server_version) => {
                self.state.conn = ConnState::Connected { server_version };
                self.send(Command::ListSessions);
            }
            UiEvent::Disconnected(reason) => {
                self.state.conn = ConnState::Offline;
                self.state.running = false;
                self.state.last_note = Some(reason);
            }
        }
        self.refresh(cx);
        self.sync_form_inputs(cx);
    }

    // -- commands ----------------------------------------------------------

    fn attach(&mut self, id: uuid::Uuid, cx: &mut Context<Self>) {
        self.state.attached = Some(id);
        self.state.last_note = None;
        self.send(Command::Attach(id));
        self.refresh(cx);
    }

    fn submit(&mut self, task: &str, window: &mut Window, cx: &mut Context<Self>) {
        let task = task.trim().to_string();
        if task.is_empty() && self.staged.is_empty() {
            return;
        }
        self.composer.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        // The shared slash system (modron_view::slash): commands typed in
        // the composer execute client-side/server-side and never reach
        // the model. Same names as the TUI and the web/mobile shell.
        if let Some(command) = task.strip_prefix('/') {
            let (name, rest) = command.split_once(' ').unwrap_or((command, ""));
            self.execute_slash(name.trim(), rest.trim(), window, cx);
            return;
        }
        self.state.mark_running();
        self.state.last_note = None;
        let session_id = self.state.attached;
        let attachments: Vec<Attachment> = std::mem::take(&mut self.staged)
            .into_iter()
            .map(|(attachment, _, _)| attachment)
            .collect();
        self.send(Command::Submit {
            session_id,
            task,
            attachments,
        });
        self.refresh(cx);
    }

    /// Create missing generative-UI form inputs (input state needs the
    /// window). Called after every applied event; keyed by transcript row
    /// index, which is stable between rebuilds of the same log.
    fn sync_form_inputs(&mut self, cx: &mut Context<Self>) {
        let Some(handle) = self.window_handle else {
            return;
        };
        let mut missing: Vec<(usize, Vec<String>)> = Vec::new();
        for (ix, row) in self.display.iter().enumerate() {
            if let Row::Entry(TranscriptEntry::GenerativeUi {
                ui: modron_view::GenUi::Form { fields, .. },
            }) = row
            {
                if self.form_inputs.contains_key(&ix) {
                    continue;
                }
                let placeholders = fields
                    .iter()
                    .map(|field| {
                        field
                            .placeholder
                            .clone()
                            .or_else(|| field.label.clone())
                            .unwrap_or_else(|| field.name.clone())
                    })
                    .collect();
                missing.push((ix, placeholders));
            }
        }
        if missing.is_empty() {
            return;
        }
        // The handle's update yields (&mut Window, &mut App) WITHOUT
        // borrowing this view, so input entities are created there and
        // inserted into self afterwards (no re-entrant view borrow).
        let created = handle.update(cx, |_root, window, cx| {
            missing
                .iter()
                .map(|(_, placeholders)| {
                    placeholders
                        .iter()
                        .map(|placeholder| {
                            cx.new(|cx| {
                                InputState::new(window, cx).placeholder(placeholder.clone())
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        });
        if let Ok(created) = created {
            for ((ix, _), inputs) in missing.into_iter().zip(created) {
                self.form_inputs.insert(ix, inputs);
            }
            self.refresh(cx);
        }
    }

    /// Delete a session through the server (two-step confirm happens in
    /// the sidebar row). Detaches first if it was attached.
    /// Publish the attached session's transcript as a read-only page on
    /// the bridge (the server answers `page_published`; the URL lands as
    /// a note in the transcript). 2026-11-11 audit: the wire path existed
    /// end to end (protocol, server, bridge) but nothing sent it.
    fn publish_page(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.state.attached else {
            return;
        };
        let title = self
            .state
            .sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.title.clone())
            .unwrap_or_else(|| "modron transcript".to_string());
        let mut md = String::from("# modron transcript\n\n");
        for row in &self.state.rows() {
            match row {
                crate::app_state::Row::Entry(entry) => match entry {
                    modron_view::TranscriptEntry::Task { text }
                    | modron_view::TranscriptEntry::User { text } => {
                        md.push_str(&format!("**{text}**\n\n"));
                    }
                    modron_view::TranscriptEntry::Assistant { text } => {
                        md.push_str(text);
                        md.push_str("\n\n");
                    }
                    modron_view::TranscriptEntry::ToolSummary { line, .. } => {
                        md.push_str(&format!("- {line}\n"));
                    }
                    modron_view::TranscriptEntry::Swarm { lines } => {
                        for line in lines {
                            md.push_str(&format!("- {line}\n"));
                        }
                    }
                    modron_view::TranscriptEntry::Model { model, reason } => {
                        md.push_str(&format!("> model: {model} ({reason})\n\n"));
                    }
                    modron_view::TranscriptEntry::Usage { .. } => {}
                    _ => {}
                },
                crate::app_state::Row::Divider { text } => {
                    md.push_str(&format!("\n---\n\n{text}\n\n"));
                }
                crate::app_state::Row::Streaming { text } => {
                    md.push_str(text);
                    md.push_str("\n\n");
                }
                crate::app_state::Row::Reasoning { .. } => {}
            }
        }
        self.send(Command::PublishPage {
            session_id: id,
            title,
            content_md: md,
        });
        self.refresh(cx);
    }

    /// Execute one shared slash command (the client-side arm; wire
    /// effects ride the existing Command enum).
    fn execute_slash(
        &mut self,
        name: &str,
        arg: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match name {
            "model" => {
                if arg.is_empty() {
                    self.open_picker(window, cx);
                } else {
                    let hit = catalog_rows()
                        .into_iter()
                        .find(|row| row.name == arg || row.name.contains(arg));
                    if let Some(row) = hit {
                        let patch = crate::app_state::model_patch(&row.name);
                        self.send(Command::SetConfig(patch));
                        self.state.model = row.name.clone();
                        self.state.last_note =
                            Some(format!("model set to {} (next turn)", row.name));
                    } else {
                        self.state.last_note = Some(format!("no model matching '{arg}'"));
                    }
                    self.refresh(cx);
                }
            }
            "mode" => {
                if arg.is_empty() {
                    self.cycle_mode(cx);
                } else {
                    let patch = crate::app_state::model_patch(self.state.model.as_str());
                    let mut patch = patch;
                    patch.approval_mode = Some(match arg {
                        "yolo" | "auto" => modron_protocol::ApprovalModeWire::ApproveAll,
                        "deny" => modron_protocol::ApprovalModeWire::Deny,
                        _ => modron_protocol::ApprovalModeWire::Ask,
                    });
                    self.send(Command::SetConfig(patch));
                    self.state.last_note = Some(format!("mode set to {arg} (next turn)"));
                    self.refresh(cx);
                }
            }
            "sandbox" => {
                let policy = match arg {
                    "seatbelt" => modron_core::SandboxPolicy::Seatbelt,
                    "bwrap" | "bubblewrap" => modron_core::SandboxPolicy::Bubblewrap,
                    _ => modron_core::SandboxPolicy::Off,
                };
                let mut patch = crate::app_state::model_patch(self.state.model.as_str());
                patch.sandbox = Some(policy);
                self.send(Command::SetConfig(patch));
                self.state.last_note = Some(format!(
                    "sandbox set to {} (next turn)",
                    match policy {
                        modron_core::SandboxPolicy::Seatbelt => "seatbelt",
                        modron_core::SandboxPolicy::Bubblewrap => "bwrap",
                        modron_core::SandboxPolicy::Off => "off",
                    }
                ));
                self.refresh(cx);
            }
            "new" => {
                self.send(Command::NewSession { title: None });
                self.send(Command::ListSessions);
                self.refresh(cx);
            }
            "fork" => {
                if let Some(id) = self.state.attached {
                    self.send(Command::Fork { session_id: id });
                    self.refresh(cx);
                }
            }
            "tasks" => {
                if let Some(id) = self.state.attached {
                    self.send(Command::TaskStatus(id));
                    self.refresh(cx);
                }
            }
            "share" => {
                self.publish_page(cx);
            }
            "remote-control" => {
                if let Some(id) = self.state.attached {
                    self.send(Command::RemoteControl {
                        session_id: id,
                        bridge_url: arg.to_string(),
                    });
                    self.refresh(cx);
                }
            }
            "auth" | "settings" => {
                self.open_settings(window, cx);
            }
            "help" => {
                self.toggle_help(cx);
            }
            other => {
                self.state.last_note = Some(format!("unknown command /{other} - try /help"));
                self.refresh(cx);
            }
        }
    }

    /// Open the settings page: refresh providers + automations and seed
    /// the editable session fields from the live state.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings_open = true;
        self.send(Command::AuthProviders {});
        self.send(Command::AutomationsList {});
        let goal = self.state.goal.clone();
        self.goal_input.update(cx, |input, cx| {
            input.set_value(&goal, window, cx);
        });
        self.refresh(cx);
    }

    /// The /help overlay toggle (renders the shared slash table).
    fn toggle_help(&mut self, cx: &mut Context<Self>) {
        self.help_open = !self.help_open;
        self.refresh(cx);
    }

    /// The "+" control: a platform file finder (files + folders), staged
    /// as attachments. The clipboard path stays for paste.
    fn open_file_finder(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui_kit::gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Ok(()) = this.update(cx, |this, cx| {
                for path in paths {
                    let _ = this.stage_from_path(&path);
                }
                this.refresh(cx);
            }) else {
                return;
            };
        })
        .detach();
    }

    fn delete_session(&mut self, id: uuid::Uuid, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        if self.state.attached == Some(id) {
            self.state.attached = None;
        }
        self.send(Command::DeleteSession(id));
        self.send(Command::ListSessions);
        self.refresh(cx);
    }

    /// Submit the values of a generative-UI form (row `form_ix`) as the
    /// next chat message - one `label: value` line per field.
    fn submit_form(&mut self, form_ix: usize, cx: &mut Context<Self>) {
        if self.state.attached.is_none() {
            self.state.last_note = Some("attach a session before submitting a form".into());
            self.refresh(cx);
            return;
        }
        let Some(inputs) = self.form_inputs.get(&form_ix) else {
            return;
        };
        let Some(Row::Entry(TranscriptEntry::GenerativeUi {
            ui: modron_view::GenUi::Form { title, fields, .. },
        })) = self.display.get(form_ix)
        else {
            return;
        };
        let mut lines = vec![title.clone()];
        for (i, field) in fields.iter().enumerate() {
            let label = field.label.clone().unwrap_or_else(|| field.name.clone());
            let value = inputs
                .get(i)
                .map(|input| input.read(cx).value().to_string())
                .unwrap_or_default();
            let value = if value.is_empty() {
                field.placeholder.clone().unwrap_or_default()
            } else {
                value
            };
            lines.push(format!("{}: {}", label, value));
        }
        self.state.mark_running();
        self.state.last_note = None;
        let session_id = self.state.attached;
        self.send(Command::Submit {
            session_id,
            task: lines.join("\n"),
            attachments: Vec::new(),
        });
        self.refresh(cx);
    }

    // -- attachments -------------------------------------------------------

    /// Stage one dropped file (image -> base64 attachment; known text
    /// extensions -> inline document; anything else refused with a note).
    fn stage_from_path(&mut self, path: &std::path::Path) -> Result<(), String> {
        const MAX_BYTES: u64 = 8 * 1024 * 1024;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let meta = std::fs::metadata(path).map_err(|e| format!("{name}: {e}"))?;
        if meta.len() > MAX_BYTES {
            return Err(format!(
                "{name} is {} MB - the limit is 8 MB",
                meta.len() / 1024 / 1024
            ));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
        let (attachment, label, thumb) = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "gif" => {
                let media_type = match ext.as_str() {
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "webp" => "image/webp",
                    _ => "image/gif",
                };
                let format =
                    image_format_of(media_type).expect("image extension mapped to a known format");
                let thumb = Some(render_image(format, bytes.clone()));
                (
                    Attachment::Image {
                        media_type: media_type.into(),
                        data: modron_core::b64::encode(&bytes),
                    },
                    format!("{name} · {} KB", meta.len() / 1024),
                    thumb,
                )
            }
            "txt" | "md" | "json" | "toml" | "csv" | "rs" | "py" | "js" | "ts" | "sh" => {
                let text =
                    String::from_utf8(bytes).map_err(|_| format!("{name} is not UTF-8 text"))?;
                (
                    Attachment::Document {
                        name: name.clone(),
                        text,
                    },
                    name,
                    None,
                )
            }
            other => return Err(format!("unsupported file type: .{other}")),
        };
        self.staged.push((attachment, label, thumb));
        Ok(())
    }

    // -- settings / automations / onboarding -------------------------------

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        if self.settings_open {
            // Providers + automations refresh on open.
            self.send(Command::AuthProviders {});
            self.send(Command::AutomationsList {});
        }
        self.refresh(cx);
    }

    fn add_automation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // If editing an existing automation, remove the old one first
        // (the new add replaces it).
        if let Some(old_name) = self.editing_automation.take() {
            self.send(Command::AutomationRemove { name: old_name });
        }
        let name = self.auto_name.read(cx).value().trim().to_string();
        let prompt = self.auto_prompt.read(cx).value().trim().to_string();
        let interval = self.auto_interval.read(cx).value().trim().to_string();
        let Ok(minutes) = interval.parse::<u32>() else {
            self.state.last_note = Some("interval must be a number of minutes".into());
            self.refresh(cx);
            return;
        };
        if name.is_empty() || prompt.is_empty() || minutes == 0 {
            self.state.last_note = Some("name, prompt and interval are required".into());
            self.refresh(cx);
            return;
        }
        self.auto_name
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.auto_prompt
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.auto_interval
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.send(Command::AutomationAdd {
            name,
            prompt,
            interval_minutes: minutes,
        });
        self.refresh(cx);
    }

    fn remove_automation(&mut self, name: String, cx: &mut Context<Self>) {
        self.send(Command::AutomationRemove { name });
        self.refresh(cx);
    }

    fn open_onboarding(&mut self, cx: &mut Context<Self>) {
        self.onboarding_step = Some(0);
        self.settings_open = false;
        self.send(Command::AuthProviders {});
        self.refresh(cx);
    }

    fn onboarding_next(&mut self, cx: &mut Context<Self>) {
        match self.onboarding_step {
            Some(0) => {
                self.onboarding_step = Some(1);
            }
            Some(1) => {
                let cheap = self.onboarding_cheap.read(cx).value().trim().to_string();
                let strong = self.onboarding_strong.read(cx).value().trim().to_string();
                if !cheap.is_empty() && !strong.is_empty() {
                    self.send(Command::PersistRouter {
                        cheap,
                        strong,
                        cheap_fallback: None,
                        strong_fallback: None,
                    });
                }
                self.onboarding_step = None;
            }
            _ => self.onboarding_step = None,
        }
        self.refresh(cx);
    }

    // -- worktree tasks ----------------------------------------------------

    fn create_task(&mut self, cx: &mut Context<Self>) {
        let title = self.new_task_title.read(cx).value().trim().to_string();
        let base_raw = self.new_task_base.read(cx).value().trim().to_string();
        if title.is_empty() {
            self.state.last_note = Some("give the task a description first".into());
            self.refresh(cx);
            return;
        }
        let base = if base_raw.is_empty() {
            None
        } else {
            Some(base_raw)
        };
        self.new_task_open = false;
        self.state.last_note = Some(format!("starting task: {title}"));
        self.send(Command::CreateTask { title, base });
        self.send(Command::ListSessions);
        self.refresh(cx);
    }

    fn open_task_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.new_task_open = true;
        self.new_task_title.update(cx, |input, cx| {
            let handle = input.focus_handle(cx);
            window.focus(&handle, cx);
        });
        self.refresh(cx);
    }

    fn request_task_status(&mut self, id: uuid::Uuid, cx: &mut Context<Self>) {
        self.review_open = Some(id);
        self.review_files.clear();
        self.review_branch.clear();
        self.refresh(cx);
        self.send(Command::TaskStatus(id));
    }

    fn merge_task(&mut self, id: uuid::Uuid, cx: &mut Context<Self>) {
        self.send(Command::MergeTask {
            session_id: id,
            message: None,
        });
        self.refresh(cx);
    }

    fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        use modron_protocol::{ApprovalModeWire, ConfigPatch};
        let (label, wire) = match self.state.approval_mode.as_str() {
            "ask" => ("yolo", ApprovalModeWire::ApproveAll),
            "yolo" => ("deny", ApprovalModeWire::Deny),
            _ => ("ask", ApprovalModeWire::Ask),
        };
        self.state.approval_mode = label.to_string();
        self.send(Command::SetConfig(ConfigPatch {
            approval_mode: Some(wire),
            ..ConfigPatch::default()
        }));
        self.refresh(cx);
    }

    fn interrupt(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.state.attached else {
            return;
        };
        self.send(Command::Interrupt(id));
        self.refresh(cx);
    }

    fn approve(&mut self, decision: Decision, cx: &mut Context<Self>) {
        let Some(pending) = self.state.pending_approval.clone() else {
            return;
        };
        self.send(Command::Approve {
            request_id: pending.request_id,
            decision,
        });
        // Optimistic clear; the server echo is a no-op (first responder
        // wins).
        self.state.pending_approval = None;
        self.refresh(cx);
    }

    fn confirm_picker(&mut self, cx: &mut Context<Self>) {
        let rows = self.filtered_models();
        let Some(row) = rows.get(self.picker_selected) else {
            return;
        };
        let name = row.name.clone();
        self.picker_open = false;
        self.send(Command::SetConfig(model_patch(&name)));
        // The TUI's exact switch wording (provider follows the model;
        // future-turn semantics are the server's to apply and echo).
        let note = match lookup(&name) {
            Some(info) => format!(
                "model: {name} — provider {}, {}k window (applies from the next turn)",
                info.provider,
                info.context_window / 1000
            ),
            None => format!("model: {name} (unknown to the catalog; applies from the next turn)"),
        };
        self.state.last_note = Some(note);
        self.refresh(cx);
    }

    fn picker_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.picker_open {
            return;
        }
        // Parity: esc clears the filter before closing.
        if !self.filter_query.is_empty() {
            self.filter.update(cx, |input, cx| {
                input.set_value("", window, cx);
            });
            self.picker_selected = 0;
            self.refresh(cx);
            return;
        }
        self.picker_open = false;
        cx.notify();
    }

    fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.picker_open = true;
        self.picker_selected = 0;
        self.filter.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        self.filter.update(cx, |input, cx| {
            let handle = input.focus_handle(cx);
            window.focus(&handle, cx);
        });
        self.refresh(cx);
    }

    fn toggle_reasoning(&mut self, cx: &mut Context<Self>) {
        self.state.show_reasoning = !self.state.show_reasoning;
        self.refresh(cx);
    }

    fn open_tool_panel(&mut self, rows: Option<Vec<String>>, cx: &mut Context<Self>) {
        self.tool_panel_open = true;
        self.panel_rows = rows;
        cx.notify();
    }

    fn close_tool_panel(&mut self, cx: &mut Context<Self>) {
        self.tool_panel_open = false;
        self.panel_rows = None;
        cx.notify();
    }

    fn filtered_models(&self) -> Vec<ModelRow> {
        let query = self.filter_query.to_ascii_lowercase();
        catalog_rows()
            .into_iter()
            .filter(|row| {
                query.is_empty()
                    || row.name.to_ascii_lowercase().contains(&query)
                    || row.provider.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }

    // -- rendering ---------------------------------------------------------

    /// `2026-11-10T09:00:00Z` -> "now" / "5m" / "2h" / "3d" / the date.
    fn relative_time(rfc3339: &str) -> String {
        let Ok(updated) = chrono::DateTime::parse_from_rfc3339(rfc3339) else {
            return String::new();
        };
        let secs = (chrono::Utc::now() - updated.with_timezone(&chrono::Utc)).num_seconds();
        match secs {
            s if s < 60 => "now".into(),
            s if s < 3600 => format!("{}m", s / 60),
            s if s < 86_400 => format!("{}h", s / 3600),
            s if s < 7 * 86_400 => format!("{}d", s / 86_400),
            _ => updated.format("%b %-d").to_string(),
        }
    }

    /// Whether this sidebar row is actively working (task running or the
    /// attached session's run in flight).
    fn row_running(&self, session: &crate::app_state::SessionRow, id: uuid::Uuid) -> bool {
        session.task_state.as_deref() == Some("running")
            || (self.state.attached == Some(id) && self.state.running)
    }

    /// Sidebar grouping: sessions cluster under the folder they were
    /// started in (the empty key = "no folder", for servers predating the
    /// wire field). `sessions` arrives newest-first, so groups come out
    /// ordered by their newest session and rows stay newest-first within
    /// each group. Carries each row's position in `state.sessions` so
    /// element ids stay stable across renders; `query` (already lowercase)
    /// matches the title or the folder label.
    fn sidebar_groups<'a>(
        sessions: &'a [crate::app_state::SessionRow],
        query: &str,
    ) -> Vec<(String, Vec<(usize, &'a crate::app_state::SessionRow)>)> {
        let mut groups: Vec<(String, Vec<(usize, &crate::app_state::SessionRow)>)> = Vec::new();
        for (ix, session) in sessions.iter().enumerate() {
            if !query.is_empty()
                && !session.title.to_ascii_lowercase().contains(query)
                && !session
                    .working_dir
                    .as_deref()
                    .unwrap_or("no folder")
                    .to_ascii_lowercase()
                    .contains(query)
            {
                continue;
            }
            let key = session.working_dir.clone().unwrap_or_default();
            match groups.iter_mut().find(|(dir, _)| *dir == key) {
                Some(group) => group.1.push((ix, session)),
                None => groups.push((key, vec![(ix, session)])),
            }
        }
        groups
    }

    /// One-line group header: the folder path itself when it fits, else
    /// its last component.
    fn group_label(dir: &str) -> String {
        if dir.is_empty() || dir == "." {
            return "this folder".into();
        }
        if dir.chars().count() <= 20 {
            return dir.to_string();
        }
        dir.trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|last| !last.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| dir.to_string())
    }

    fn render_sidebar(&self, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let query = self.session_filter_query.to_ascii_lowercase();

        // New chat: the primary action of the surface (the desktop chat
        // grammar) - creates a server-side shell and adopts it.
        let mut list = div()
            .id("sidebar-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            // Clear the transparent-titlebar drag band + traffic lights.
            .pt_10();

        // Iconed action rows (the reference UI's grammar): icon + label,
        // full-width, generous hit area. New chat is cmd+n.
        fn action_row<F>(
            theme: &Theme,
            id: &'static str,
            label: &'static str,
            icon: &'static str,
            hint: Option<&'static str>,
            on_click: F,
        ) -> gpui_kit::AnyElement
        where
            F: Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        {
            let hint = hint.map(SharedString::from);
            div()
                .id(id)
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .mx_2()
                .px_2()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .hover(|style| style.bg(theme.secondary.opacity(0.5)))
                .on_click(move |e, w, cx| on_click(e, w, cx))
                .child(
                    Icon::default()
                        .path(icon)
                        .size_4()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_1()
                        .text_size(px(crate::brand::text::BASE))
                        .text_color(theme.foreground)
                        .child(SharedString::from(label)),
                )
                .children(hint.map(|h| {
                    div()
                        .text_size(px(crate::brand::text::XS))
                        .text_color(theme.muted_foreground)
                        .child(h)
                }))
                .into_any_element()
        }
        list = list
            .child({
                let entity = entity.clone();
                action_row(
                    theme,
                    "act-new-chat",
                    "New chat",
                    "icons/plus.svg",
                    Some("\u{2318}N"),
                    move |_e, _w, cx| {
                        entity.update(cx, |this, _cx| {
                            this.send(Command::NewSession { title: None });
                            // Refresh the list once the server notes the
                            // new session (the correlated Note adopts it).
                            this.send(Command::ListSessions);
                        });
                    },
                )
            })
            .child({
                let entity = entity.clone();
                action_row(
                    theme,
                    "act-new-task",
                    "New task",
                    "icons/square-pen.svg",
                    None,
                    move |_e, window, cx| {
                        entity.update(cx, |this, cx| this.open_task_form(window, cx));
                    },
                )
            })
            .child({
                let entity = entity.clone();
                action_row(
                    theme,
                    "act-automations",
                    "Automations",
                    "icons/calendar-clock.svg",
                    None,
                    move |_e, _w, cx| {
                        entity.update(cx, |this, cx| {
                            this.settings_open = true;
                            this.refresh(cx);
                        });
                    },
                )
            });

        // The new-task form (opened from its row): description + optional
        // base branch, then create/cancel.
        let list = list.children(if self.new_task_open {
            let title_entity = entity.clone();
            let cancel_entity = entity.clone();
            vec![
                div().px_3().pb_1().child(Input::new(&self.new_task_title)),
                div().px_3().pb_1().child(Input::new(&self.new_task_base)),
                div()
                    .px_3()
                    .pb_2()
                    .flex()
                    .flex_row()
                    .gap_1()
                    .child(
                        Button::new("task-create")
                            .label("create")
                            .primary()
                            .small()
                            .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                title_entity.update(cx, |this, cx| this.create_task(cx));
                            }),
                    )
                    .child(
                        Button::new("task-cancel")
                            .label("cancel")
                            .ghost()
                            .small()
                            .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                cancel_entity.update(cx, |this, cx| {
                                    this.new_task_open = false;
                                    this.refresh(cx);
                                });
                            }),
                    ),
            ]
        } else {
            vec![]
        });

        // Section header: the list's name plus its two controls - the
        // magnifier expands the search input (closing clears it), the
        // filter toggles folder grouping.
        let mut header_row = div()
            .flex()
            .flex_row()
            .items_center()
            .mx_2()
            .px_2()
            .pt_2()
            .pb_1()
            .h(px(28.));
        if self.search_open {
            // Expanded: the input takes the title's place in the same row
            // (fades in; the x collapses back to the label).
            let entity_close = entity.clone();
            header_row = header_row
                .child(
                    div()
                        .flex_1()
                        .with_animation(
                            "search-expand",
                            Animation::new(Duration::from_millis(160)),
                            |style, progress| style.opacity(progress),
                        )
                        .child(Input::new(&self.session_filter)),
                )
                .child(
                    Button::new("search-close")
                        .icon(Icon::default().path("icons/x.svg").size_4())
                        .ghost()
                        .xsmall()
                        .on_click(move |_e, window, cx: &mut App| {
                            entity_close.update(cx, |this, cx| {
                                this.search_open = false;
                                this.session_filter_query.clear();
                                this.session_filter
                                    .update(cx, |input, cx| input.set_value("", window, cx));
                                this.refresh(cx);
                            });
                        }),
                );
        } else {
            header_row = header_row
                .child(
                    div()
                        .flex_1()
                        .text_size(px(crate::brand::text::SM))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from("Sessions")),
                )
                .child({
                    let entity = entity.clone();
                    Button::new("search-toggle")
                        .icon(Icon::default().path("icons/search.svg").size_4())
                        .ghost()
                        .xsmall()
                        .selected(self.search_open)
                        .on_click(move |_e, window, cx: &mut App| {
                            entity.update(cx, |this, cx| {
                                this.search_open = !this.search_open;
                                if this.search_open {
                                    this.session_filter.update(cx, |input, cx| {
                                        let handle = input.focus_handle(cx);
                                        window.focus(&handle, cx);
                                    });
                                } else {
                                    // Esc grammar: closing the search clears it.
                                    this.session_filter_query.clear();
                                    this.session_filter
                                        .update(cx, |input, cx| input.set_value("", window, cx));
                                }
                                this.refresh(cx);
                            });
                        })
                })
                .child({
                    let entity = entity.clone();
                    Button::new("group-toggle")
                        .icon(Icon::default().path("icons/list-filter.svg").size_4())
                        .ghost()
                        .xsmall()
                        .selected(self.grouped)
                        .on_click(move |_e, _w, cx: &mut App| {
                            entity.update(cx, |this, cx| {
                                this.grouped = !this.grouped;
                                this.refresh(cx);
                            });
                        })
                });
        }
        let mut list = list.child(header_row).children(if self.search_open {
            vec![div().px_3().pb_1().child(Input::new(&self.session_filter))]
        } else {
            vec![]
        });

        let rows: Vec<(String, Vec<(usize, &crate::app_state::SessionRow)>)> = if self.grouped {
            Self::sidebar_groups(&self.state.sessions, &query)
        } else {
            let flat: Vec<(usize, &crate::app_state::SessionRow)> = self
                .state
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, session)| {
                    query.is_empty()
                        || session.title.to_ascii_lowercase().contains(&query)
                        || session
                            .working_dir
                            .as_deref()
                            .unwrap_or("no folder")
                            .to_ascii_lowercase()
                            .contains(&query)
                })
                .collect();
            vec![(String::new(), flat)]
        };
        let mut shown = 0usize;
        for (dir, group) in rows {
            shown += group.len();
            // Group divider + header: a thin hairline above the cluster,
            // then the folder as a muted one-line label (truncated).
            if self.grouped {
                let header: SharedString = Self::group_label(&dir).into();
                list = list.child(
                    div()
                        .mx_2()
                        .pt_2()
                        .border_t_1()
                        .border_color(theme.border)
                        .child(
                            div()
                                .px_2()
                                .pb_1()
                                .truncate()
                                .text_size(px(crate::brand::text::XS))
                                .text_color(theme.muted_foreground)
                                .child(header),
                        ),
                );
            }
            for (ix, session) in group {
                let id = session.id;
                let attached = self.state.attached == Some(id);
                let running = self.row_running(session, id) || (attached && self.state.running);
                let title: SharedString = session.title.clone().into();
                let meta: SharedString = Self::relative_time(&session.updated_at).into();
                let entity = entity.clone();
                let attach_entity = entity.clone();
                // Hover reads subtler than selection (half-strength
                // secondary), so the attached row stays clearly distinct.
                let hover_bg = if attached {
                    theme.secondary
                } else {
                    theme.secondary.opacity(0.5)
                };
                // Single line: truncated title, relative time on the right
                // (the desktop chat grammar - message counts and ids live in
                // the transcript header, not the list). Fixed row height so
                // hover-revealed controls never reflow the list.
                let hovered = self.hovered_row == Some(id);
                let row = div()
                    .id(("session", ix))
                    // Hover is state-driven (the trash button reads
                    // self.hovered_row): gpui's group_hover was unreliable
                    // inside the scroll list, so the row reports enter/
                    // leave explicitly.
                    .on_hover({
                        let entity = entity.clone();
                        move |hovered: &bool, _w, cx: &mut App| {
                            entity.update(cx, |this, cx| {
                                let was = this.hovered_row;
                                this.hovered_row = if *hovered {
                                    Some(id)
                                } else if was == Some(id) {
                                    None
                                } else {
                                    was
                                };
                                // Leaving the row cancels a pending delete.
                                if !hovered && this.confirm_delete == Some(id) {
                                    this.confirm_delete = None;
                                    this.refresh(cx);
                                }
                                if was != this.hovered_row {
                                    this.refresh(cx);
                                }
                            });
                        }
                    })
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .mx_2()
                    .px_2()
                    .py_1()
                    .min_h(px(32.))
                    .rounded_md()
                    .cursor_pointer()
                    .when(attached, |row| row.bg(theme.secondary))
                    .hover(move |row| row.bg(hover_bg))
                    .on_click(move |_event: &ClickEvent, _window, cx| {
                        attach_entity.update(cx, |this, cx| this.attach(id, cx));
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(crate::brand::text::BASE))
                            .text_color(theme.foreground)
                            .child(title),
                    )
                    .children(if running {
                        // Shimmer sweep: a short accent segment slides along the
                        // bottom edge while the task runs (16ms tick).
                        let sweep = ((self.anim_tick as f32) * 0.02).fract();
                        vec![div()
                            .flex_1()
                            .min_w_0()
                            .relative()
                            .h(px(2.))
                            .child(
                                div()
                                    .absolute()
                                    .bottom_0()
                                    .left(gpui_kit::relative((sweep * 90.0).clamp(0.0, 90.0)))
                                    .w(px(24.))
                                    .h(px(2.))
                                    .rounded_sm()
                                    .bg(theme.accent),
                            )
                            .into_any_element()]
                    } else {
                        Vec::new()
                    })
                    .children({
                        let mut trailing: Vec<gpui_kit::AnyElement> = Vec::new();
                        if running {
                            // Working: rotating loader next to the title.
                            let spin = (self.anim_tick as f32) * 9.0;
                            trailing.push(
                                gpui_kit::component::Icon::default()
                                    .path("icons/loader-circle.svg")
                                    .text_color(theme.accent)
                                    .rotate(gpui_kit::radians(spin))
                                    .into_any_element(),
                            );
                        }
                        if self.confirm_delete == Some(id) {
                            // Two-click grammar: the trash becomes a
                            // destructive check; the second click removes.
                            // Hovering out resets (the on_hover arm).
                            let entity_yes = entity.clone();
                            trailing.push(
                                div()
                                    .flex_shrink_0()
                                    .child(
                                        Button::new(("del-check", ix))
                                            .icon(
                                                Icon::default()
                                                    .path("icons/check.svg")
                                                    .size_4()
                                                    .text_color(theme.danger),
                                            )
                                            .ghost()
                                            .xsmall()
                                            .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                                entity_yes.update(cx, |this, cx| {
                                                    this.delete_session(id, cx)
                                                });
                                            }),
                                    )
                                    .into_any_element(),
                            );
                        } else {
                            let entity_del = entity.clone();
                            trailing.push(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(crate::brand::text::XS))
                                    .text_color(theme.muted_foreground)
                                    .child(meta)
                                    .into_any_element(),
                            );
                            // Worktree tasks carry a branch chip: the visual
                            // mark that this session runs in its own worktree.
                            if let Some(branch) = &session.task_branch {
                                let chip: SharedString =
                                    branch.trim_start_matches("modron/").into();
                                trailing.push(
                                    div()
                                        .px_1p5()
                                        .rounded_sm()
                                        .bg(theme.accent)
                                        .text_size(px(crate::brand::text::XS))
                                        .text_color(theme.accent_foreground)
                                        .child(chip)
                                        .into_any_element(),
                                );
                            }
                            // Hover-only: hidden until the row reports
                            // hover (state-driven, see on_hover above).
                            trailing.push(
                                div()
                                    .flex_shrink_0()
                                    .opacity(if hovered { 1. } else { 0. })
                                    .child(
                                        Button::new(("del", ix))
                                            .icon(Icon::default().path("icons/trash.svg").size_4())
                                            .ghost()
                                            .xsmall()
                                            .on_click(
                                                move |_event: &ClickEvent, _w, cx: &mut App| {
                                                    // The x sits inside the clickable row:
                                                    // stop propagation so the attach
                                                    // handler does not fire too.
                                                    cx.stop_propagation();
                                                    entity_del.update(cx, |this, cx| {
                                                        this.confirm_delete = Some(id);
                                                        this.refresh(cx);
                                                    });
                                                },
                                            ),
                                    )
                                    .into_any_element(),
                            );
                        }
                        trailing
                    });
                list = list.child(row);
            }
        }
        if shown == 0 {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from(if query.is_empty() {
                        "no sessions yet - type a task to start one"
                    } else {
                        "no sessions match the filter"
                    })),
            );
        }

        // Footer: the account row (cosmetic until settings exist), pinned
        // under the scroll area like the desktop chat clients.
        let footer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .child(
                div()
                    .size(px(24.))
                    .rounded_full()
                    .bg(theme.accent)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(crate::brand::text::XS))
                    .text_color(theme.accent_foreground)
                    .child(SharedString::from("J")),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.foreground)
                    .child(SharedString::from("Jacob Beck")),
            )
            .child({
                let entity = entity.clone();
                Button::new("settings-gear")
                    .icon(Icon::default().path("icons/settings.svg").size_4())
                    .ghost()
                    .small()
                    .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                        entity.update(cx, |this, cx| this.toggle_settings(cx));
                    })
            })
            .child(
                Button::new("theme-toggle")
                    .label(
                        if theme.mode == gpui_kit::component::theme::ThemeMode::Dark {
                            "\u{2600}"
                        } else {
                            "\u{263e}"
                        },
                    )
                    .ghost()
                    .small()
                    .on_click({
                        let entity = entity.clone();
                        move |_e: &ClickEvent, _w, cx: &mut App| {
                            let mode =
                                if cx.theme().mode == gpui_kit::component::theme::ThemeMode::Dark {
                                    gpui_kit::component::theme::ThemeMode::Light
                                } else {
                                    gpui_kit::component::theme::ThemeMode::Dark
                                };
                            Theme::change(mode, None, cx);
                            let _ = &entity;
                        }
                    }),
            );

        div()
            .id("sidebar")
            .flex()
            .flex_col()
            .w(px(264.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .child(list)
            .child(footer)
            .into_any_element()
    }

    /// One transcript row as an element (called from the list's
    /// render_item; all data is read from the cached `display`). Layout
    /// follows the chat-app grammar: the user's task is a right-aligned
    /// accent-tinted bubble, the assistant speaks full-width plain text,
    /// tool summaries are collapsed chips, and routing/compaction notes
    /// are centered divider chips (per-turn usage lives in the status
    /// bar - see AppState::rows).
    fn transcript_row(row: &Row, ix: usize, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();

        // Right-aligned user bubble: the row spans the full width so
        // justify_end actually pushes the bubble to the right edge; the
        // machine's answer stays full-width on the left.
        let user_bubble = |text: &str| {
            let label: SharedString = text.into();
            div()
                .id(("transcript-row", ix))
                .w_full()
                .flex()
                .flex_row()
                .justify_end()
                .px_4()
                .py_0p5()
                .child(
                    div()
                        .max_w(px(520.))
                        .px_3()
                        .py_2()
                        .rounded_lg()
                        .rounded_br_sm()
                        .bg(theme.primary)
                        .text_color(theme.primary_foreground)
                        .text_size(px(crate::brand::text::BASE))
                        .child(label),
                )
                .into_any_element()
        };

        match row {
            Row::Entry(TranscriptEntry::Plan { items }) => {
                // The standing plan: a bordered card of checkbox rows
                // (the shared modron_view::plan_lines copy).
                let mut card = div().id(("transcript-row", ix)).px_4().py_1().child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .px_3()
                        .py_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.muted),
                );
                let mut card_body = div().flex().flex_col().gap_0p5();
                card_body = card_body.child(
                    div()
                        .text_size(px(crate::brand::text::XS))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from("plan")),
                );
                for line in modron_view::plan_lines(items) {
                    let mark_char = line.chars().next().unwrap_or('o');
                    let mark = mark_char.to_string();
                    let rest: SharedString = line.chars().skip(1).collect::<String>().trim().into();
                    let color = match mark_char {
                        '\u{2713}' => theme.foreground,
                        '\u{25b8}' => theme.accent_foreground,
                        _ => theme.muted_foreground,
                    };
                    let done = mark_char == '\u{2713}';
                    card_body = card_body.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(crate::brand::text::SM))
                                    .text_color(color)
                                    .child(SharedString::from(mark)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(crate::brand::text::SM))
                                    .text_color(if done {
                                        theme.muted_foreground
                                    } else {
                                        theme.foreground
                                    })
                                    .child(rest),
                            ),
                    );
                }
                card = card.child(card_body);
                card.into_any_element()
            }
            Row::Entry(TranscriptEntry::Task { text })
            | Row::Entry(TranscriptEntry::User { text }) => user_bubble(text),
            Row::Entry(TranscriptEntry::Assistant { text }) => div()
                .id(("transcript-row", ix))
                .px_4()
                .py_1()
                .child(crate::markdown::render_markdown(text, theme))
                .into_any_element(),
            Row::Entry(TranscriptEntry::ToolSummary { line, events }) => {
                let label: SharedString = line.clone().into();
                let panel: Vec<String> = events.iter().map(|event| event.summary()).collect();
                let entity = entity.clone();
                div()
                    .id(("transcript-row", ix))
                    .px_4()
                    .py_0p5()
                    .child(
                        div()
                            .id(("tool-summary", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.muted)
                            .cursor_pointer()
                            .hover(|row| row.border_color(theme.accent))
                            .on_click(move |_event: &ClickEvent, _window, cx| {
                                entity.update(cx, |this, cx| {
                                    this.open_tool_panel(Some(panel.clone()), cx);
                                });
                            })
                            .child(
                                div()
                                    .text_size(px(crate::brand::text::SM))
                                    .text_color(theme.foreground)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(crate::brand::text::XS))
                                    .text_color(theme.muted_foreground)
                                    .child(SharedString::from("expand")),
                            ),
                    )
                    .into_any_element()
            }
            Row::Entry(TranscriptEntry::Swarm { lines }) => {
                let mut card = div()
                    .id(("transcript-row", ix))
                    .mx_4()
                    .my_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .flex()
                    .flex_col()
                    .gap_0p5();
                for line in lines {
                    let label: SharedString = line.clone().into();
                    card = card.child(
                        div()
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    );
                }
                card.into_any_element()
            }
            // Routing/compaction notes: a centered hairline chip.
            Row::Divider { text } => {
                let label: SharedString = text.clone().into();
                div()
                    .id(("transcript-row", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_1()
                    .child(div().flex_1().h(px(1.)).bg(theme.border))
                    .child(
                        div()
                            .text_size(px(crate::brand::text::XS))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(div().flex_1().h(px(1.)).bg(theme.border))
                    .into_any_element()
            }
            Row::Streaming { text } => {
                let label: SharedString = format!("{text} \u{258d}").into();
                div()
                    .id(("transcript-row", ix))
                    .px_4()
                    .py_1()
                    .text_size(px(crate::brand::text::BASE))
                    .text_color(theme.foreground)
                    .child(label)
                    .into_any_element()
            }
            Row::Reasoning { text } => {
                let label: SharedString = text.clone().into();
                div()
                    .id(("transcript-row", ix))
                    .px_4()
                    .py_1()
                    .text_size(px(crate::brand::text::BASE))
                    .text_color(theme.muted_foreground)
                    .italic()
                    .child(label)
                    .into_any_element()
            }
            Row::Entry(TranscriptEntry::GenerativeUi { ui }) => {
                let theme_colors = cx.theme();
                let inputs = entity.read(cx).form_inputs.get(&ix).cloned();
                let attached = entity.read(cx).state.attached;
                let row_entity = entity.clone();
                let _ = ix;
                div()
                    .id(("transcript-row", ix))
                    .px_4()
                    .py_1()
                    .child(crate::ui::render_gen_ui(
                        ui,
                        inputs.as_ref(),
                        attached,
                        ix,
                        move |form_ix: usize, cx: &mut App| {
                            row_entity.update(cx, |this, cx| this.submit_form(form_ix, cx));
                        },
                        theme_colors,
                    ))
                    .into_any_element()
            }
            // rows() folds these into dividers; the arm keeps the match
            // total for hand-built rows in tests.
            Row::Entry(TranscriptEntry::Model { .. })
            | Row::Entry(TranscriptEntry::Usage { .. })
            | Row::Entry(TranscriptEntry::Compaction { .. }) => {
                div().id(("transcript-row", ix)).into_any_element()
            }
        }
    }

    /// The empty transcript state (no attached session, or an attached
    /// one with nothing in it yet): the surface never opens onto a void.
    fn render_empty_transcript(&self, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let (title, hint) = if self.state.attached.is_some() {
            (
                "no messages yet",
                "type below to start - the run happens on the server and survives this window",
            )
        } else {
            (
                "no session attached",
                "pick one on the left, hit New chat, or just start typing",
            )
        };
        let title: SharedString = title.into();
        let hint: SharedString = hint.into();
        div()
            .id("empty-transcript")
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .size_full()
            .gap_2()
            .child(
                div()
                    .font_family(crate::brand::DISPLAY_FONT)
                    .text_size(px(crate::brand::text::DISPLAY))
                    .text_color(theme.border)
                    .child(SharedString::from("\u{2712}")),
            )
            .child(
                div()
                    .font_family(crate::brand::DISPLAY_FONT)
                    .text_size(px(crate::brand::text::LG))
                    .text_color(theme.foreground)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.muted_foreground)
                    .child(hint),
            )
            .into_any_element()
    }

    fn render_main(&self, entity: &Entity<Self>, cx: &mut Context<Self>) -> AnyElement {
        // The review overlay needs &mut Context (listeners); compute it
        // before taking the shared theme borrow.
        let review = self.render_review(cx);
        let theme = cx.theme();

        // Header: what am I looking at (session title) + the model chip
        // (opens the picker) + the reasoning toggle. The TUI carries these
        // in its status strip; a windowed surface gets a real header.
        let title: String = self
            .state
            .attached
            .and_then(|id| {
                self.state
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| s.title.clone())
            })
            .unwrap_or_else(|| "no session".to_string());
        let model_chip: SharedString = if self.state.model.is_empty() {
            "model".into()
        } else {
            self.state.model.clone().into()
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_4()
            .h(px(44.))
            .border_b_1()
            .border_color(theme.border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(crate::brand::text::BASE))
                    .text_color(theme.foreground)
                    .overflow_hidden()
                    .child(title),
            )
            .child(
                Button::new("model-chip")
                    .label(model_chip)
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _e, window, cx| {
                        this.open_picker(window, cx);
                    })),
            )
            // Share: opens a remote-control tunnel via the server's
            // bridge; page publishes the transcript as a read-only web
            // page on the same bridge (paradev "pages" grammar). Both
            // need a --bridge'd server; the URL lands as a note.
            .children(if self.state.attached.is_some() {
                vec![
                    Button::new("share-remote")
                        .label("share")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            let Some(id) = this.state.attached else {
                                return;
                            };
                            this.send(Command::RemoteControl {
                                session_id: id,
                                bridge_url: String::new(),
                            });
                            this.refresh(cx);
                        }))
                        .into_any_element(),
                    Button::new("publish-page")
                        .label("page")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            this.publish_page(cx);
                        }))
                        .into_any_element(),
                ]
            } else {
                vec![]
            })
            .child(
                Button::new("reasoning-chip")
                    .label("reasoning")
                    .ghost()
                    .small()
                    .toggled(self.state.show_reasoning)
                    .on_click(cx.listener(|this, _e, _w, cx| this.toggle_reasoning(cx))),
            )
            // Worktree-task controls (Conductor/Para grammar): branch chip,
            // review toggle, merge. Present only on task sessions.
            .children(self.task_ui_for_attached(cx));

        // Transcript: virtualized list, follows the tail while scrolled to
        // the bottom (thousands of rows render only the visible range).
        let display_entity = entity.clone();
        let row_entity = entity.clone();
        // The list MUST carry its own size: a virtualized list reports zero
        // intrinsic height (ListSizingBehavior::Auto lays out from its own
        // style), so without size_full() the pane rendered blank no matter
        // how many rows the state held — found by the first human click.
        let transcript = div().flex_1().min_h_0().overflow_hidden().child(
            list(self.list_state.clone(), move |ix, _window, cx| {
                let row = display_entity.read(cx).display.get(ix).cloned();
                match row {
                    Some(row) => Self::transcript_row(&row, ix, &row_entity, cx),
                    None => div().into_any_element(),
                }
            })
            .size_full(),
        );
        let body = if self.display.is_empty() {
            div()
                .flex_1()
                .min_h_0()
                .child(self.render_empty_transcript(cx))
        } else {
            transcript
        };

        // Live region: note toast, approval strip, activity strip, tool
        // panel - the TUI's live zone with buttons instead of keys.
        let mut live = div().flex().flex_col().gap_1().px_4().pb_1();
        if let Some(note) = &self.state.last_note {
            let label: SharedString = note.clone().into();
            live = live.child(
                div()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.muted_foreground)
                    .child(label),
            );
        }
        if let Some(prompt) = self.state.approval_prompt() {
            let prompt: SharedString = prompt.into();
            live = live.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(crate::brand::text::BASE))
                            .child(prompt),
                    )
                    .child(
                        Button::new("allow-once")
                            .label(decision_label(Decision::AllowOnce))
                            .small()
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.approve(Decision::AllowOnce, cx)
                            })),
                    )
                    .child(
                        Button::new("allow-all")
                            .label(decision_label(Decision::AllowAll))
                            .small()
                            .on_click(
                                cx.listener(|this, _e, _w, cx| {
                                    this.approve(Decision::AllowAll, cx)
                                }),
                            ),
                    )
                    .child(
                        Button::new("deny")
                            .label(decision_label(Decision::Deny))
                            .danger()
                            .small()
                            .on_click(
                                cx.listener(|this, _e, _w, cx| this.approve(Decision::Deny, cx)),
                            ),
                    ),
            );
        }
        if let Some((latest, count)) = self.state.live_strip() {
            let strip: SharedString = format!("{latest} · {count} in flight").into();
            let strip_entity = entity.clone();
            live = live.child(
                div()
                    .id("tool-strip")
                    .cursor_pointer()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.accent)
                    .on_click(move |_e: &ClickEvent, _w, cx| {
                        strip_entity.update(cx, |this, cx| this.open_tool_panel(None, cx));
                    })
                    .child(strip),
            );
        }
        if let Some(review) = review {
            live = live.child(review);
        }
        if self.tool_panel_open {
            let rows: Vec<String> = match &self.panel_rows {
                Some(rows) => rows.clone(),
                None => self.state.tool_panel_rows(),
            };
            let mut panel = div()
                .id("tool-panel")
                .key_context("ToolPanel")
                .track_focus(&self.focus)
                .max_h(px(240.))
                .overflow_y_scroll()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.muted)
                .flex()
                .flex_col()
                .gap_0p5()
                .on_action(cx.listener(|this, _action: &CloseToolPanel, _window, cx| {
                    this.close_tool_panel(cx);
                }))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(crate::brand::text::XS))
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from("tool activity (esc closes)")),
                        )
                        .child(
                            Button::new("close-panel")
                                .label("close")
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.close_tool_panel(cx);
                                })),
                        ),
                );
            for row in rows {
                let label: SharedString = row.into();
                panel = panel.child(
                    div()
                        .text_size(px(crate::brand::text::SM))
                        .text_color(theme.foreground)
                        .child(label),
                );
            }
            live = live.child(panel);
        }

        // Composer: a floating rounded pill — [+][mode] input [model]
        // [send/stop] — with staged attachment chips above it. The whole
        // pane accepts image/document drops.
        let mut staged_row = div().flex().flex_row().flex_wrap().gap_1().px_4().pb_1();
        for (ix, (_attachment, label, thumb)) in self.staged.iter().enumerate() {
            let label: SharedString = label.clone().into();
            // Images get a real thumbnail (the prebuilt render image);
            // documents get a page glyph.
            let chip: gpui_kit::AnyElement = if let Some(image) = thumb {
                gpui_kit::img(image.clone())
                    .size(px(32.))
                    .rounded_sm()
                    .into_any_element()
            } else {
                div()
                    .size(px(32.))
                    .rounded_sm()
                    .bg(theme.secondary)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(crate::brand::text::LG))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("\u{1f4c4}"))
                    .into_any_element()
            };
            staged_row = staged_row.child(
                div()
                    .id(("staged", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .p_1()
                    .pr_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted)
                    .child(chip)
                    .child(
                        div()
                            .text_size(px(crate::brand::text::XS))
                            .text_color(theme.foreground)
                            .child(label),
                    )
                    .child(
                        Button::new(("unstage", ix))
                            .label("x")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.staged.remove(ix);
                                this.refresh(cx);
                            })),
                    ),
            );
        }

        let mode_chip: SharedString = self.state.approval_mode.clone().into();
        let model_chip: SharedString = if self.state.model.is_empty() {
            "model".into()
        } else {
            self.state.model.clone().into()
        };
        let pill_entity = entity.clone();
        // The slash popup: candidate commands for the leading "/" token,
        // rendered above the pill; click executes.
        let slash_focus = self.composer.read(cx).focus_handle(cx);
        let composer = div().px_4().pb_3().pt_1().child(staged_row).child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .children(if self.slash_candidates.is_empty() {
                    vec![]
                } else {
                    let entity_popup = cx.entity();
                    let mut popup = div()
                        .id("slash-popup")
                        .flex()
                        .flex_col()
                        .rounded_lg()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .shadow_sm()
                        .p_1()
                        .max_h(px(240.))
                        .overflow_y_scroll();
                    let query = self.composer.read(cx).value().to_string();
                    let token = query
                        .strip_prefix('/')
                        .and_then(|t| t.split_whitespace().next())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    for spec in modron_view::slash::SLASH_COMMANDS {
                        if !spec.name.starts_with(&token) {
                            continue;
                        }
                        let entity_click = entity_popup.clone();
                        let name = spec.name;
                        popup = popup.child(
                            div()
                                .id(SharedString::from(format!("slash-{name}")))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.secondary.opacity(0.5)))
                                .on_click(move |_e, window, cx: &mut App| {
                                    entity_click.update(cx, |this, cx| {
                                        // The command executes; any typed
                                        // token clears with the submit path.
                                        this.composer.update(cx, |input, cx| {
                                            input.set_value("", window, cx);
                                        });
                                        this.slash_candidates.clear();
                                        let arg = "";
                                        this.execute_slash(name, arg, window, cx);
                                    });
                                })
                                .child(
                                    div()
                                        .text_size(px(crate::brand::text::SM))
                                        .text_color(theme.accent_foreground)
                                        .child(SharedString::from(format!("/{}", spec.name))),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .truncate()
                                        .text_size(px(crate::brand::text::XS))
                                        .text_color(theme.muted_foreground)
                                        .child(SharedString::from(spec.about)),
                                ),
                        );
                    }
                    vec![popup]
                })
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1p5()
                        .rounded_xl()
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .shadow_sm()
                        .key_context("Composer")
                        .track_focus(&slash_focus)
                        .on_action(cx.listener(|this, _action: &ComposerSubmit, window, cx| {
                            let task = this.composer.read(cx).value().to_string();
                            this.submit(&task, window, cx);
                        }))
                        .child(
                            Button::new("add-attachment")
                                .icon(Icon::default().path("icons/plus.svg").size_4())
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    // The finder (files) is the "+" grammar;
                                    // clipboard staging stays on paste.
                                    this.open_file_finder(cx);
                                })),
                        )
                        .child(
                            Button::new("mode-chip")
                                .label(mode_chip)
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _e, _w, cx| this.cycle_mode(cx))),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Textarea::new(&self.composer).on_paste({
                                    let weak_self = self.weak_self.clone();
                                    move |item, _window, cx| -> bool {
                                        // Images and copied files stage as attachments
                                        // (true = consumed); plain text falls through
                                        // to normal paste.
                                        let Some(app) = weak_self.upgrade() else {
                                            return false;
                                        };
                                        let mut took = false;
                                        app.update(cx, |this, cx| {
                                            for entry in item.entries() {
                                                match entry {
                                                    gpui_kit::ClipboardEntry::Image(image) => {
                                                        let media_type = match image.format {
                                                            gpui_kit::ImageFormat::Png => {
                                                                "image/png"
                                                            }
                                                            gpui_kit::ImageFormat::Jpeg => {
                                                                "image/jpeg"
                                                            }
                                                            gpui_kit::ImageFormat::Webp => {
                                                                "image/webp"
                                                            }
                                                            gpui_kit::ImageFormat::Gif => {
                                                                "image/gif"
                                                            }
                                                            _ => "image/png",
                                                        };
                                                        let kb = image.bytes.len() / 1024;
                                                        let thumb = Some(render_image(
                                                            image.format,
                                                            image.bytes.clone(),
                                                        ));
                                                        this.staged.push((
                                                            Attachment::Image {
                                                                media_type: media_type.into(),
                                                                data: modron_core::b64::encode(
                                                                    &image.bytes,
                                                                ),
                                                            },
                                                            format!("pasted image · {kb} KB"),
                                                            thumb,
                                                        ));
                                                        took = true;
                                                    }
                                                    gpui_kit::ClipboardEntry::ExternalPaths(
                                                        paths,
                                                    ) => {
                                                        for path in paths.paths() {
                                                            if this.stage_from_path(path).is_ok() {
                                                                took = true;
                                                            }
                                                        }
                                                    }
                                                    gpui_kit::ClipboardEntry::String(_) => {}
                                                }
                                            }
                                            if took {
                                                this.refresh(cx);
                                            }
                                        });
                                        took
                                    }
                                })),
                        )
                        .child(
                            Button::new("model-chip")
                                .label(model_chip)
                                .ghost()
                                .small()
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    this.open_picker(window, cx);
                                })),
                        )
                        .children(if self.state.running {
                            vec![Button::new("stop")
                                .label("stop")
                                .danger()
                                .small()
                                .on_click(cx.listener(|this, _e, _w, cx| this.interrupt(cx)))]
                        } else {
                            vec![Button::new("send")
                                .label("\u{2191}")
                                .primary()
                                .small()
                                .on_click(cx.listener(|this, _e, window, cx| {
                                    let task = this.composer.read(cx).value().to_string();
                                    this.submit(&task, window, cx);
                                }))]
                        }),
                ),
        );
        let _ = pill_entity;

        // The whole main pane is a drop target (drag images/documents in
        // from Finder). External file drags surface as a PathBuf drag.
        let drop_entity = entity.clone();
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .bg(theme.background)
            .on_drop::<std::path::PathBuf>(move |path: &std::path::PathBuf, _window, cx| {
                drop_entity.update(cx, |this, cx| {
                    let result = this.stage_from_path(std::path::Path::new(path));
                    if let Err(e) = result {
                        this.state.last_note = Some(e);
                    }
                    this.refresh(cx);
                });
            })
            .child(header)
            .child(body)
            .child(live)
            .child(composer)
            .into_any_element()
    }

    /// Header controls for the attached session when it is a worktree
    /// task: branch chip, review toggle, merge button.
    fn task_ui_for_attached(&self, cx: &Context<Self>) -> Vec<gpui_kit::AnyElement> {
        use gpui_kit::component::button::{Button, ButtonVariants};
        let Some(id) = self.state.attached else {
            return Vec::new();
        };
        let Some(row) = self.state.sessions.iter().find(|s| s.id == id) else {
            return Vec::new();
        };
        let Some(branch) = &row.task_branch else {
            return Vec::new();
        };
        let theme = cx.theme();
        let chip: SharedString = branch.trim_start_matches("modron/").into();
        let review_open = self.review_open.as_ref() == Some(&id);
        let mut out = vec![
            // Branch chip (the mark that this session runs in a worktree).
            div()
                .px_2()
                .py_0p5()
                .rounded_sm()
                .bg(theme.secondary)
                .text_size(px(crate::brand::text::XS))
                .text_color(theme.muted_foreground)
                .child(chip)
                .into_any_element(),
        ];
        out.push(
            Button::new("review")
                .label(if review_open { "hide review" } else { "review" })
                .ghost()
                .small()
                .on_click(cx.listener(|this, _e, _w, cx| {
                    let Some(id) = this.state.attached else {
                        return;
                    };
                    if this.review_open.as_ref() == Some(&id) {
                        this.review_open = None;
                        this.refresh(cx);
                    } else {
                        this.request_task_status(id, cx);
                    }
                }))
                .into_any_element(),
        );
        out.push(
            Button::new("merge")
                .label("merge")
                .primary()
                .small()
                .on_click(cx.listener(|this, _e, _w, cx| {
                    let Some(id) = this.state.attached else {
                        return;
                    };
                    this.merge_task(id, cx);
                }))
                .into_any_element(),
        );
        out
    }

    /// The review overlay: per-file +/- counts for the attached task, with
    /// merge/close controls (rendered above the live region).
    fn render_review(&self, cx: &mut Context<Self>) -> Option<gpui_kit::AnyElement> {
        use gpui_kit::component::button::{Button, ButtonVariants};
        let id = self.review_open?;
        if self.state.attached.as_ref() != Some(&id) {
            return None;
        }
        let theme = cx.theme();
        let mut panel = div()
            .id("review-panel")
            .key_context("ToolPanel")
            .track_focus(&self.focus)
            .max_h(px(260.))
            .overflow_y_scroll()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(crate::brand::text::XS))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(if self.review_branch.is_empty() {
                                "review (loading diff...)".into()
                            } else {
                                self.review_branch.clone()
                            })),
                    )
                    .child(
                        Button::new("merge-review")
                            .label("merge")
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                let Some(id) = this.state.attached else {
                                    return;
                                };
                                this.merge_task(id, cx);
                            })),
                    )
                    .child(
                        Button::new("close-review")
                            .label("close")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.review_open = None;
                                this.refresh(cx);
                            })),
                    ),
            );
        if self.review_files.is_empty() && !self.review_branch.is_empty() {
            panel = panel.child(
                div()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("no file changes vs base")),
            );
        }
        for file in &self.review_files {
            let label: SharedString =
                format!("{}  +{} -{}", file.path, file.added, file.deleted).into();
            panel = panel.child(
                div()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.foreground)
                    .child(label),
            );
        }
        Some(panel.into_any_element())
    }

    /// Settings overlay: model chip, approval mode, reasoning, theme,
    /// automations CRUD, onboarding entry. Every control drives the same
    /// protocol commands as the chat surface (parity: the /command set).
    /// The /help overlay: the shared slash-command table
    /// (modron_view::slash), rendered verbatim - the same copy the TUI's
    /// help and the web/mobile shells show.
    fn render_help(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants};
        let theme = cx.theme();
        let mut rows = div().flex().flex_col().gap_1();
        for spec in modron_view::slash::SLASH_COMMANDS {
            rows = rows.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(150.))
                            .truncate()
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.accent_foreground)
                            .child(SharedString::from(spec.usage)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(spec.about)),
                    ),
            );
        }
        div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black())
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.help_open = false;
                    this.refresh(cx);
                }),
            )
            .child(
                div()
                    .id("help-panel")
                    .w(px(560.))
                    .max_h(px(520.))
                    .overflow_y_scroll()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(crate::brand::text::XL))
                                    .text_color(theme.foreground)
                                    .child(SharedString::from("slash commands")),
                            )
                            .child(
                                Button::new("help-close")
                                    .label("close")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.help_open = false;
                                        this.refresh(cx);
                                    })),
                            ),
                    )
                    .child(rows),
            )
            .into_any_element()
    }

    /// The settings PAGE (2026-11-11: was a cramped modal): the main pane
    /// swaps to this view. shadcn-flavored - section headers, bordered
    /// cards, label-left control-right rows, ghost icon controls.
    /// The settings PAGE: shadcn-form sections - title + description on
    /// the left of each card, controls right. Everything here maps to a
    /// real knob: SetConfig (approval, sandbox, allow/deny, goal),
    /// PersistRouter (saved defaults), AuthProviders (sign-in state),
    /// and the automations registry.
    fn render_settings_page(
        &self,
        entity: &Entity<Self>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants};
        use gpui_kit::component::input::Input;
        let theme = cx.theme();
        let entity_back = entity.clone();
        let entity_wizard = entity.clone();

        // Section: a muted heading + description, then the bordered card.
        let section = |title: &'static str,
                       description: &'static str,
                       card: gpui_kit::AnyElement|
         -> gpui_kit::AnyElement {
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .text_size(px(crate::brand::text::BASE))
                        .text_color(theme.foreground)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(crate::brand::text::XS))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(description)),
                )
                .child(card)
                .into_any_element()
        };

        // Card row: title + hint left, control right, hairline separators.
        let row = |title: &'static str,
                   hint: &'static str,
                   control: gpui_kit::AnyElement|
         -> gpui_kit::AnyElement {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_3()
                .px_3()
                .py_2p5()
                .border_color(theme.border)
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .gap_0p5()
                        .child(
                            div()
                                .text_size(px(crate::brand::text::SM))
                                .text_color(theme.foreground)
                                .child(SharedString::from(title)),
                        )
                        .child(
                            div()
                                .text_size(px(crate::brand::text::XS))
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from(hint)),
                        ),
                )
                .child(control)
                .into_any_element()
        };

        let card_wrap = |rows: Vec<gpui_kit::AnyElement>| {
            // shadcn divide-y: hairlines BETWEEN rows, never on the last
            // one (the card border is the bottom edge).
            let mut wrapped: Vec<gpui_kit::AnyElement> = Vec::new();
            for (ix, child) in rows.into_iter().enumerate() {
                if ix > 0 {
                    wrapped.push(div().w_full().h(px(1.)).bg(theme.border).into_any_element());
                }
                wrapped.push(child);
            }
            div()
                .flex()
                .flex_col()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .bg(theme.background)
                .overflow_hidden()
                .children(wrapped)
        };

        // -- general ------------------------------------------------------
        let general = card_wrap(vec![
            row(
                "appearance",
                "light or dark, follows the system at launch",
                Button::new("settings-theme")
                    .icon(Icon::default().path("icons/moon.svg").size_4())
                    .ghost()
                    .small()
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        let mode = if cx.theme().mode == ThemeMode::Dark {
                            ThemeMode::Light
                        } else {
                            ThemeMode::Dark
                        };
                        Theme::change(mode, None, cx);
                        this.refresh(cx);
                    }))
                    .into_any_element(),
            ),
            row(
                "setup wizard",
                "sign in to providers and wire the default models",
                Button::new("run-setup-wizard")
                    .label("run")
                    .outline()
                    .small()
                    .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                        entity_wizard.update(cx, |this, cx| this.open_onboarding(cx));
                    })
                    .into_any_element(),
            ),
        ]);

        // -- session (SetConfig: future turns) ----------------------------
        let apply_patch = |entity: &Entity<Self>,
                           build: fn() -> modron_protocol::ConfigPatch|
         -> gpui_kit::AnyElement {
            let entity = entity.clone();
            Button::new(SharedString::from(format!(
                "apply-{}",
                std::any::type_name::<fn() -> modron_protocol::ConfigPatch>()
            )))
            .label("apply")
            .outline()
            .xsmall()
            .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                entity.update(cx, |this, cx| {
                    this.send(Command::SetConfig(build()));
                    this.refresh(cx);
                });
            })
            .into_any_element()
        };
        let _ = apply_patch; // (per-row closures below capture state directly)

        let mode_control = Button::new("settings-mode")
            .label(self.state.approval_mode.to_string())
            .outline()
            .small()
            .on_click(cx.listener(|this, _e, _w, cx| {
                let next = match this.state.approval_mode.as_str() {
                    "ask" => "yolo",
                    "yolo" => "deny",
                    _ => "ask",
                };
                this.state.approval_mode = next.into();
                this.send(Command::SetConfig(modron_protocol::ConfigPatch {
                    approval_mode: Some(match next {
                        "yolo" => modron_protocol::ApprovalModeWire::ApproveAll,
                        "deny" => modron_protocol::ApprovalModeWire::Deny,
                        _ => modron_protocol::ApprovalModeWire::Ask,
                    }),
                    ..Default::default()
                }));
                this.refresh(cx);
            }))
            .into_any_element();

        let sandbox_label = if self.state.sandbox.is_empty() {
            "off"
        } else {
            self.state.sandbox.as_str()
        };
        let sandbox_control = Button::new("settings-sandbox")
            .label(sandbox_label)
            .outline()
            .small()
            .on_click(cx.listener(|this, _e, _w, cx| {
                let next = match this.state.sandbox.as_str() {
                    "" | "off" => "seatbelt",
                    "seatbelt" => "bwrap",
                    _ => "off",
                };
                let policy = match next {
                    "seatbelt" => modron_core::SandboxPolicy::Seatbelt,
                    "bwrap" => modron_core::SandboxPolicy::Bubblewrap,
                    _ => modron_core::SandboxPolicy::Off,
                };
                this.state.sandbox = next.into();
                this.send(Command::SetConfig(modron_protocol::ConfigPatch {
                    sandbox: Some(policy),
                    ..Default::default()
                }));
                this.refresh(cx);
            }))
            .into_any_element();

        // One editable row: title + hint, input, apply. The patch builder
        // reads the input's live value inside the click handler.
        fn editable_row<F>(
            theme: &Theme,
            entity: &Entity<DesktopApp>,
            id: &'static str,
            title: &'static str,
            hint: &'static str,
            input: &Entity<InputState>,
            build: F,
        ) -> gpui_kit::AnyElement
        where
            F: Fn(String) -> modron_protocol::ConfigPatch + 'static,
        {
            let entity_apply = entity.clone();
            let input_apply = input.clone();
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .px_3()
                .py_2p5()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(px(crate::brand::text::SM))
                        .text_color(theme.foreground)
                        .child(SharedString::from(title)),
                )
                .child(
                    div()
                        .text_size(px(crate::brand::text::XS))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(hint)),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(input)))
                        .child(Button::new(id).label("apply").outline().xsmall().on_click(
                            move |_e: &ClickEvent, _w, cx: &mut App| {
                                let value = input_apply.read(cx).value().to_string();
                                let patch = build(value);
                                entity_apply.update(cx, |this, cx| {
                                    this.send(Command::SetConfig(patch));
                                    this.refresh(cx);
                                });
                            },
                        )),
                )
                .into_any_element()
        }

        let session = card_wrap(vec![
            row(
                "approval mode",
                "ask prompts per run; yolo approves everything; deny blocks",
                mode_control,
            ),
            row(
                "sandbox",
                "off, seatbelt (macOS), or bwrap (linux) for future turns",
                sandbox_control,
            ),
            editable_row(
                theme,
                entity,
                "apply-allow",
                "allowed tools",
                "comma-separated; these bypass ask mode",
                &self.allow_input,
                |value| {
                    let tools: Vec<String> = value
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    modron_protocol::ConfigPatch {
                        allow: Some(tools),
                        ..Default::default()
                    }
                },
            ),
            editable_row(
                theme,
                entity,
                "apply-deny",
                "blocked tools",
                "comma-separated; always denied, whatever the mode",
                &self.deny_input,
                |value| {
                    let tools: Vec<String> = value
                        .split(',')
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect();
                    modron_protocol::ConfigPatch {
                        deny: Some(tools),
                        ..Default::default()
                    }
                },
            ),
            editable_row(
                theme,
                entity,
                "apply-goal",
                "standing goal",
                "prepended to every task until cleared",
                &self.goal_input,
                |value| modron_protocol::ConfigPatch {
                    goal: Some(value),
                    ..Default::default()
                },
            ),
        ]);

        // -- defaults (PersistRouter: saved to config.toml) ----------------
        let cheap_input = self.cheap_model_input.clone();
        let strong_input = self.strong_model_input.clone();
        let entity_save = entity.clone();
        let defaults_card = div()
            .flex()
            .flex_col()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&cheap_input).id("cheap-model")),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&strong_input).id("strong-model")),
                            ),
                    )
                    .child(
                        div().flex().flex_row().justify_end().child(
                            Button::new("save-defaults")
                                .label("save as default")
                                .primary()
                                .small()
                                .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                    entity_save.update(cx, |this, cx| {
                                        let cheap = this
                                            .cheap_model_input
                                            .read(cx)
                                            .value()
                                            .trim()
                                            .to_string();
                                        let strong = this
                                            .strong_model_input
                                            .read(cx)
                                            .value()
                                            .trim()
                                            .to_string();
                                        if cheap.is_empty() || strong.is_empty() {
                                            this.state.last_note =
                                                Some("cheap and strong models are required".into());
                                        } else {
                                            this.send(Command::PersistRouter {
                                                cheap,
                                                strong,
                                                cheap_fallback: None,
                                                strong_fallback: None,
                                            });
                                            this.state.last_note = Some("defaults saved".into());
                                        }
                                        this.refresh(cx);
                                    });
                                }),
                        ),
                    ),
            );

        // -- providers -----------------------------------------------------
        // OAuth providers get a real sign-in button (the flow runs on the
        // server; progress lands in the session's note strip). API-key
        // providers get set-key buttons targeting the scratch input at
        // the bottom of the card.
        let mut provider_rows: Vec<gpui_kit::AnyElement> = Vec::new();
        let mut key_provider: Option<String> = None;
        for provider in &self.state.auth_providers {
            let name: SharedString = provider.label.clone().into();
            let status: SharedString = provider.status.clone().into();
            let signed_in =
                provider.status.starts_with("signed in") || provider.status == "api key";
            let control = if signed_in {
                div()
                    .text_size(px(crate::brand::text::XS))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("ok"))
                    .into_any_element()
            } else if provider.oauth_capable {
                let entity_sign = entity.clone();
                let id = provider.id.clone();
                let session = self.state.attached;
                Button::new(SharedString::from(format!("sign-in-{id}")))
                    .label("sign in")
                    .outline()
                    .small()
                    .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                        entity_sign.update(cx, |this, cx| {
                            let Some(session) = session else {
                                this.state.last_note =
                                    Some("attach a session first (progress lands there)".into());
                                this.refresh(cx);
                                return;
                            };
                            this.send(Command::AuthStart {
                                session_id: session,
                                provider: id.clone(),
                            });
                            this.refresh(cx);
                        });
                    })
                    .into_any_element()
            } else {
                let entity_target = entity.clone();
                let id = provider.id.clone();
                if key_provider.is_none() {
                    key_provider = Some(id.clone());
                }
                let is_target = self.key_target.as_deref() == Some(id.as_str());
                Button::new(SharedString::from(format!("set-key-{id}")))
                    .label(if is_target { "key below" } else { "set key" })
                    .ghost()
                    .small()
                    .on_click(move |_e: &ClickEvent, window, cx: &mut App| {
                        entity_target.update(cx, |this, cx| {
                            this.key_target = Some(id.clone());
                            this.provider_key_input.update(cx, |input, cx| {
                                let handle = input.focus_handle(cx);
                                window.focus(&handle, cx);
                            });
                            this.refresh(cx);
                        });
                    })
                    .into_any_element()
            };
            provider_rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_3()
                    .px_3()
                    .py_2p5()
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(
                                div()
                                    .text_size(px(crate::brand::text::SM))
                                    .text_color(theme.foreground)
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_size(px(crate::brand::text::XS))
                                    .text_color(theme.muted_foreground)
                                    .child(status),
                            ),
                    )
                    .child(control)
                    .into_any_element(),
            );
        }
        if provider_rows.is_empty() {
            provider_rows.push(
                div()
                    .px_3()
                    .py_2p5()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("no providers configured yet"))
                    .into_any_element(),
            );
        }
        // The api-key scratch row: targeted by a row's set-key button.
        let target = self.key_target.clone();
        let key_label: SharedString = match &target {
            Some(id) => format!("api key for {id}").into(),
            None => "api key - press set key on a provider".into(),
        };
        let key_save_enabled = target.is_some();
        let mut key_save = Button::new("save-provider-key")
            .label("save")
            .outline()
            .xsmall();
        if key_save_enabled {
            let entity_save = entity.clone();
            let target = target.clone().unwrap_or_default();
            key_save = key_save.on_click(move |_e: &ClickEvent, window, cx: &mut App| {
                entity_save.update(cx, |this, cx| {
                    let key = this.provider_key_input.read(cx).value().trim().to_string();
                    if key.is_empty() {
                        this.state.last_note = Some("paste the key first".into());
                    } else {
                        let session = this.state.attached;
                        this.send(Command::AuthSetKey {
                            session_id: session.unwrap_or(uuid::Uuid::nil()),
                            provider: target.clone(),
                            key,
                        });
                        this.provider_key_input.update(cx, |input, cx| {
                            input.set_value("", window, cx);
                        });
                        this.key_target = None;
                    }
                    this.refresh(cx);
                });
            });
        }
        let _ = key_label;
        let providers = card_wrap(provider_rows).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .border_t_1()
                .border_color(theme.border)
                .p_3()
                .child(
                    div()
                        .flex_1()
                        .text_size(px(crate::brand::text::XS))
                        .text_color(theme.muted_foreground)
                        .child(key_label),
                )
                .child(
                    div()
                        .w(px(240.))
                        .child(Input::new(&self.provider_key_input)),
                )
                .child(key_save),
        );

        // -- automations ---------------------------------------------------
        let mut auto_rows: Vec<gpui_kit::AnyElement> = Vec::new();
        for auto in &self.state.automations {
            let name: SharedString = auto.name.clone().into();
            let meta: SharedString = format!("every {}m", auto.interval_minutes).into();
            let entity_del = entity.clone();
            let del_name = auto.name.clone();
            let btn_id = SharedString::from(format!("automation-remove-{}", auto.name));
            auto_rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2p5()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.foreground)
                            .child(name),
                    )
                    .child(
                        div()
                            .text_size(px(crate::brand::text::XS))
                            .text_color(theme.muted_foreground)
                            .child(meta),
                    )
                    .child(
                        Button::new(btn_id)
                            .icon(Icon::default().path("icons/trash.svg").size_4())
                            .ghost()
                            .xsmall()
                            .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                entity_del.update(cx, |this, cx| {
                                    this.remove_automation(del_name.clone(), cx)
                                });
                            }),
                    )
                    .into_any_element(),
            );
        }
        if auto_rows.is_empty() {
            auto_rows.push(
                div()
                    .px_3()
                    .py_2p5()
                    .text_size(px(crate::brand::text::SM))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("no automations yet"))
                    .into_any_element(),
            );
        }
        let automations = card_wrap(auto_rows).child(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .border_t_1()
                .border_color(theme.border)
                .p_3()
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.auto_name).id("auto-name")),
                        )
                        .child(
                            div()
                                .flex_1()
                                .child(Input::new(&self.auto_prompt).id("auto-prompt")),
                        )
                        .child(
                            div()
                                .w(px(110.))
                                .child(Input::new(&self.auto_interval).id("auto-interval")),
                        ),
                )
                .child(
                    div().flex().flex_row().justify_end().child(
                        Button::new("automation-add")
                            .label("add automation")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.add_automation(window, cx);
                            })),
                    ),
                ),
        );

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .bg(theme.background)
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("settings-back")
                            .icon(Icon::default().path("icons/arrow-left.svg").size_4())
                            .ghost()
                            .small()
                            .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                entity_back.update(cx, |this, cx| {
                                    this.settings_open = false;
                                    this.refresh(cx);
                                });
                            }),
                    )
                    .child(
                        div()
                            .text_size(px(crate::brand::text::XL))
                            .text_color(theme.foreground)
                            .child(SharedString::from("Settings")),
                    ),
            )
            .child(
                div()
                    .id("settings-scroll")
                    .flex_1()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .px_6()
                    .py_5()
                    .child(section(
                        "general",
                        "appearance and first-run setup",
                        general.into_any_element(),
                    ))
                    .child(section(
                        "session",
                        "applies to future turns of this session",
                        session.into_any_element(),
                    ))
                    .child(section(
                        "defaults",
                        "cheap and strong models saved for new sessions (config.toml)",
                        defaults_card.into_any_element(),
                    ))
                    .child(section(
                        "providers",
                        "credential status per provider",
                        providers.into_any_element(),
                    ))
                    .child(section(
                        "automations",
                        "prompts the server runs on an interval, each in a fresh session",
                        automations.into_any_element(),
                    )),
            )
            .into_any_element()
    }

    fn render_onboarding(&self, step: usize, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants};
        use gpui_kit::component::input::Input;
        let theme = cx.theme();
        let overlay = div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black())
            .flex()
            .items_center()
            .justify_center();
        let mut panel = div()
            .w(px(560.))
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .text_size(px(crate::brand::text::XL))
                    .text_color(theme.foreground)
                    .child(SharedString::from(format!("setup wizard ({}/2)", step + 1))),
            );

        match step {
            0 => {
                panel = panel.child(
                    div()
                        .text_size(px(crate::brand::text::BASE))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(
                            "sign in to providers (browser opens on the server machine)",
                        )),
                );
                for row in &self.state.auth_providers {
                    let label: SharedString = format!("{} - {}", row.label, row.status).into();
                    panel = panel.child(
                        div()
                            .text_size(px(crate::brand::text::BASE))
                            .text_color(if row.status == "not configured" {
                                theme.warning
                            } else {
                                theme.success
                            })
                            .child(label),
                    );
                }
                panel = panel.child(
                    div().flex().flex_row().justify_end().child(
                        Button::new("ob-next")
                            .label("next: model wiring")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _e, _w, _cx| {
                                this.send(Command::AuthProviders {});
                            })),
                    ),
                );
            }
            _ => {
                panel = panel.child(
                    div()
                        .text_size(px(crate::brand::text::BASE))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(
                            "wire the tiers (persisted in the server config)",
                        )),
                );
                panel = panel.child(div().child(Input::new(&self.onboarding_cheap)));
                panel = panel.child(div().child(Input::new(&self.onboarding_strong)));
                panel = panel.child(div().child(Input::new(&self.onboarding_strong_fb)));
                panel = panel.child(
                    div().flex().flex_row().justify_end().gap_1().child(
                        Button::new("ob-save")
                            .label("save")
                            .primary()
                            .small()
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.onboarding_next(cx);
                            })),
                    ),
                );
            }
        }

        overlay.child(panel).into_any_element()
    }

    fn render_status_bar(&self, _entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let status: SharedString = self.state.status_line().into();
        let session: SharedString = self.state.session_label().into();
        let (dot, dot_color) = match &self.state.conn {
            ConnState::Connected { .. } => ("\u{25cf}", theme.success),
            ConnState::Connecting => ("\u{25cb}", theme.warning),
            ConnState::Offline => ("\u{25cf}", theme.danger),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .text_size(px(crate::brand::text::XS))
            .text_color(theme.muted_foreground)
            .child(div().text_color(dot_color).child(SharedString::from(dot)))
            .child(div().child(status))
            .child(div().flex_1())
            .child(div().child(session))
            .into_any_element()
    }

    fn render_picker(&self, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let rows = self.filtered_models();
        let selected = self.picker_selected.min(rows.len().saturating_sub(1));
        let mut list_el = div().flex().flex_col().gap_0p5();
        for (ix, row) in rows.iter().enumerate() {
            let name: SharedString = row.name.clone().into();
            let meta: SharedString = format!(
                "{} \u{b7} {}k window",
                row.provider,
                row.context_window / 1000
            )
            .into();
            let is_selected = ix == selected;
            let row_entity = entity.clone();
            list_el = list_el.child(
                div()
                    .id(("model-row", ix))
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .when(is_selected, |el| {
                        el.bg(theme.accent).text_color(theme.accent_foreground)
                    })
                    .hover(|el| el.bg(theme.secondary))
                    .on_click(move |_e: &ClickEvent, _window, cx: &mut App| {
                        row_entity.update(cx, |this, cx| {
                            this.picker_selected = ix;
                            this.confirm_picker(cx);
                        });
                    })
                    .child(div().text_size(px(crate::brand::text::BASE)).child(name))
                    .child(
                        div()
                            .text_size(px(crate::brand::text::XS))
                            .text_color(theme.muted_foreground)
                            .child(meta),
                    ),
            );
        }

        // The modal: type-to-filter, enter/click switch, esc backs out one
        // screen (clearing the filter first - parity semantics).
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .bg(theme.background.opacity(0.4))
            .child(
                div()
                    .key_context("ModelPicker")
                    .mt_16()
                    .w(px(480.))
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .flex()
                    .flex_col()
                    .gap_2()
                    .on_action({
                        let close_entity = entity.clone();
                        move |_action: &PickerClose, window: &mut Window, cx: &mut App| {
                            close_entity.update(cx, |this, cx| this.picker_close(window, cx));
                        }
                    })
                    .on_action({
                        let nav_entity = entity.clone();
                        move |_action: &PickerUp, _window: &mut Window, cx: &mut App| {
                            nav_entity.update(cx, |this, cx| {
                                this.picker_selected = this.picker_selected.saturating_sub(1);
                                cx.notify();
                            });
                        }
                    })
                    .on_action({
                        let nav_entity = entity.clone();
                        move |_action: &PickerDown, _window: &mut Window, cx: &mut App| {
                            nav_entity.update(cx, |this, cx| {
                                this.picker_selected += 1;
                                cx.notify();
                            });
                        }
                    })
                    .child(
                        div()
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(
                            "model \u{2014} type to filter \u{b7} enter switch \u{b7} esc close",
                        )),
                    )
                    .child(div().child(Input::new(&self.filter)))
                    .child(
                        div()
                            .id("model-list")
                            .max_h(px(320.))
                            .overflow_y_scroll()
                            .child(list_el),
                    ),
            )
            .into_any_element()
    }
}

impl Focusable for DesktopApp {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for DesktopApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entity = cx.entity();
        let sidebar = self.render_sidebar(&entity, cx);
        let status = self.render_status_bar(&entity, cx);
        let mut root = div()
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            // cmd+n (the sidebar row's hint) creates a new chat anywhere
            // in the window.
            .on_action(cx.listener(|this, _action: &NewChat, _window, cx| {
                this.send(Command::NewSession { title: None });
                this.send(Command::ListSessions);
                this.refresh(cx);
            }))
            // Behind everything: the transparent-titlebar drag band. The
            // system traffic lights overlay the sidebar's top padding;
            // interactive content sits above this strip and wins clicks.
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_full()
                    .h(px(38.))
                    .window_control_area(gpui_kit::WindowControlArea::Drag),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    .child(if self.settings_open {
                        self.render_settings_page(&entity, cx)
                    } else {
                        self.render_main(&entity, cx)
                    }),
            )
            .child(status);
        if self.picker_open {
            root = root.child(self.render_picker(&entity, cx));
        }
        if self.help_open {
            root = root.child(self.render_help(cx));
        }
        if let Some(step) = self.onboarding_step {
            root = root.child(self.render_onboarding(step, cx));
        }
        root
    }
}

/// Render one generative-UI widget with native controls:
/// - table: styled header + row grid;
/// - chart: horizontal bars scaled to the max value (pie adds percent);
/// - form: real inputs per field (state cached per transcript row via
///   `inputs`); submit sends the answers as the next chat message.
#[allow(clippy::too_many_arguments)]
pub fn render_gen_ui(
    ui: &modron_view::GenUi,
    inputs: Option<&Vec<gpui_kit::Entity<gpui_kit::component::input::InputState>>>,
    attached: Option<uuid::Uuid>,
    ix: usize,
    on_submit: impl Fn(usize, &mut App) + 'static,
    theme: &gpui_kit::component::theme::Theme,
) -> gpui_kit::AnyElement {
    use gpui_kit::component::button::{Button, ButtonVariants};
    use gpui_kit::component::input::Input;

    let card = |inner: gpui_kit::AnyElement| -> gpui_kit::AnyElement {
        div()
            .max_w(px(640.))
            .mx_2()
            .my_1()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .child(inner)
            .into_any_element()
    };

    match ui {
        modron_view::GenUi::Table {
            title,
            columns,
            rows,
        } => {
            let mut grid = div().flex().flex_col().gap_0p5();
            if let Some(title) = title {
                let t: SharedString = title.clone().into();
                grid = grid.child(
                    div()
                        .text_size(px(crate::brand::text::SM))
                        .text_color(theme.muted_foreground)
                        .child(t),
                );
            }
            let header = div()
                .flex()
                .flex_row()
                .gap_3()
                .children(columns.iter().map(|col| {
                    let c: SharedString = col.clone().into();
                    div()
                        .flex_1()
                        .text_size(px(crate::brand::text::XS))
                        .text_color(theme.muted_foreground)
                        .child(c)
                }));
            grid = grid.child(header).child(div().h(px(1.)).bg(theme.border));
            for row in rows {
                grid = grid.child(div().flex().flex_row().gap_3().children(
                    columns.iter().enumerate().map(|(i, _)| {
                        let cell: SharedString = row.get(i).cloned().unwrap_or_default().into();
                        div()
                            .flex_1()
                            .text_size(px(crate::brand::text::BASE))
                            .text_color(theme.foreground)
                            .child(cell)
                    }),
                ));
            }
            card(grid.into_any_element())
        }
        modron_view::GenUi::Chart {
            title,
            kind,
            points,
        } => {
            let mut inner = div().flex().flex_col().gap_1();
            if let Some(title) = title {
                let t: SharedString = title.clone().into();
                inner = inner.child(
                    div()
                        .text_size(px(crate::brand::text::SM))
                        .text_color(theme.muted_foreground)
                        .child(t),
                );
            }
            let max = points
                .iter()
                .map(|p| p.value.abs())
                .fold(0.0_f64, f64::max)
                .max(1.0);
            let total: f64 = points.iter().map(|p| p.value).sum();
            for point in points {
                let width = px(((point.value.abs() / max) * 240.0).round().max(4.0) as f32);
                let bar_label: SharedString = point.label.clone().into();
                let value: SharedString = if *kind == modron_view::ChartKind::Pie {
                    let pct = if total > 0.0 {
                        (point.value / total) * 100.0
                    } else {
                        0.0
                    };
                    format!("{:.0}%", pct).into()
                } else {
                    format!("{}", point.value).into()
                };
                inner = inner.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(88.))
                                .truncate()
                                .text_size(px(crate::brand::text::XS))
                                .text_color(theme.muted_foreground)
                                .child(bar_label),
                        )
                        .child(div().h(px(10.)).w(width).rounded_sm().bg(theme.accent))
                        .child(
                            div()
                                .text_size(px(crate::brand::text::XS))
                                .text_color(theme.foreground)
                                .child(value),
                        ),
                );
            }
            card(inner.into_any_element())
        }
        modron_view::GenUi::Form {
            title,
            submit_label,
            fields,
        } => {
            let mut inner = div().flex().flex_col().gap_2();
            let t: SharedString = title.clone().into();
            inner = inner.child(
                div()
                    .text_size(px(crate::brand::text::BASE))
                    .text_color(theme.foreground)
                    .child(t),
            );
            for (f, field) in fields.iter().enumerate() {
                let label_text = field.label.clone().unwrap_or_else(|| field.name.clone());
                let label: SharedString = label_text.clone().into();
                let input = inputs.and_then(|v| v.get(f)).cloned();
                let field_row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .w(px(120.))
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .children(match (&field.kind, input) {
                                (_, Some(input)) => {
                                    vec![Input::new(&input).into_any_element()]
                                }
                                (modron_view::GenFieldKind::Select, None) => {
                                    let options: SharedString = field.options.join(" / ").into();
                                    vec![div()
                                        .text_size(px(crate::brand::text::SM))
                                        .text_color(theme.muted_foreground)
                                        .child(options)
                                        .into_any_element()]
                                }
                                _ => vec![div()
                                    .text_size(px(crate::brand::text::SM))
                                    .text_color(theme.muted_foreground)
                                    .child(SharedString::from(
                                        field.placeholder.clone().unwrap_or_else(|| {
                                            "(input appears once the row syncs)".into()
                                        }),
                                    ))
                                    .into_any_element()],
                            }),
                    );
                inner = inner.child(field_row);
            }
            let submit_text_label: String =
                submit_label.clone().unwrap_or_else(|| "submit".to_string());
            let can_submit = attached.is_some() && inputs.is_some();
            let submit_button = Button::new(("gen-form-submit", ix))
                .label(submit_text_label)
                .primary()
                .small()
                .when(can_submit, |b| {
                    b.on_click({
                        move |_e: &ClickEvent, _w, cx: &mut App| {
                            on_submit(ix, cx);
                        }
                    })
                });
            inner = inner.child(div().flex().flex_row().justify_end().child(submit_button));
            card(inner.into_any_element())
        }
    }
}

/// Launch the GPUI app: net thread (tokio) first, then the event loop.
pub fn run(options: LaunchOptions) {
    let resume = options.resume;
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<Command>();
    let (event_tx, event_rx) = async_channel::unbounded::<UiEvent>();

    // The net thread owns the tokio runtime: starts the demo server when
    // requested, then pumps the protocol client until the app exits.
    std::thread::Builder::new()
        .name("modron-desktop-net".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(async move {
                let url = if options.demo {
                    crate::demo::spawn()
                        .await
                        .expect("demo server failed to start")
                } else {
                    match options.connect.clone() {
                        Some(url) => url,
                        None => {
                            let _ = event_tx
                                .send(UiEvent::Disconnected("no --connect url".into()))
                                .await;
                            return;
                        }
                    }
                };
                crate::wire::run_client(url, cmd_rx, event_tx).await;
            });
        })
        .expect("spawn net thread");

    gpui_kit::application()
        .with_assets(gpui_kit_assets::AllAssets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            // Brand install (2026-11-11, iron-rain port): bundled DM Sans +
            // Instrument Serif, the Oculus gold-on-black palette for both
            // schemes, and the base font size. Later Theme::change calls
            // (mode toggle, OS switch) reload from these configs.
            crate::brand::install(cx);
            cx.bind_keys([
                KeyBinding::new("escape", PickerClose, Some("ModelPicker")),
                KeyBinding::new("up", PickerUp, Some("ModelPicker")),
                KeyBinding::new("down", PickerDown, Some("ModelPicker")),
                KeyBinding::new("escape", CloseToolPanel, Some("ToolPanel")),
                // The sidebar's "New chat" row advertises this hint; the
                // action is handled on the window root.
                KeyBinding::new("cmd-n", NewChat, None),
                // The composer's send (textarea Enter = newline).
                KeyBinding::new("cmd-enter", ComposerSubmit, Some("Composer")),
            ]);
            let (window, view) = gpui_kit::open_window(
                WindowOptions {
                    titlebar: Some(gpui_kit::TitlebarOptions {
                        title: Some("modron desktop".into()),
                        // Our surface paints the whole window (dark or light);
                        // a drag strip under the content keeps the empty top
                        // band draggable while buttons stay clickable.
                        appears_transparent: true,
                        traffic_light_position: None,
                    }),
                    ..WindowOptions::default()
                },
                cx,
                move |window, cx| {
                    cx.new(|cx| {
                        DesktopApp::new(window, cx, cmd_tx.clone(), event_rx.clone(), resume)
                    })
                },
            )
            .expect("failed to open the modron desktop window");
            // Give the view its window handle: deferred work (form inputs)
            // needs a &mut Window to construct input state.
            view.update(cx, |app, _cx| {
                app.window_handle = Some(window);
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resume_flag_parses_bare_and_explicit() {
        let none = LaunchOptions::parse(args(&["--demo"]));
        assert!(none.resume.is_none());

        let bare = LaunchOptions::parse(args(&["--demo", "--resume"]));
        assert_eq!(bare.resume, Some(None), "bare --resume = newest session");

        let id = uuid::Uuid::new_v4();
        let explicit = LaunchOptions::parse(args(&[&format!("--resume={id}")]));
        assert_eq!(explicit.resume, Some(Some(id)));

        let bad = LaunchOptions::parse(args(&["--resume=not-a-uuid"]));
        assert!(bad.resume.is_none(), "unparseable id stays unset");
    }

    #[test]
    fn connect_flag_still_defaults_to_loopback() {
        let parsed = LaunchOptions::parse(args(&[]));
        assert_eq!(
            parsed.connect.as_deref(),
            Some("ws://127.0.0.1:7699"),
            "DEFAULT_PORT loopback default"
        );
        let explicit = LaunchOptions::parse(args(&["--connect", "ws://10.0.0.5:7699"]));
        assert_eq!(explicit.connect.as_deref(), Some("ws://10.0.0.5:7699"));
    }

    fn sidebar_row(
        title: &str,
        updated_at: &str,
        working_dir: Option<&str>,
    ) -> crate::app_state::SessionRow {
        crate::app_state::SessionRow {
            id: uuid::Uuid::new_v4(),
            title: title.into(),
            updated_at: updated_at.into(),
            message_count: 0,
            task_branch: None,
            task_state: None,
            working_dir: working_dir.map(Into::into),
        }
    }

    #[test]
    fn sidebar_groups_by_folder_newest_first() {
        // state.sessions arrives newest-first (AppState sorts on apply);
        // groups order by their newest member, rows keep that order too.
        let sessions = vec![
            sidebar_row("a", "2026-11-09T12:00:00Z", Some("/home/j/dev/bridge")),
            sidebar_row("b", "2026-11-09T11:00:00Z", None),
            sidebar_row("c", "2026-11-09T10:00:00Z", Some("/home/j/dev/bridge")),
            sidebar_row("d", "2026-11-09T09:00:00Z", Some("/home/j/dev/modron")),
        ];
        let groups = DesktopApp::sidebar_groups(&sessions, "");
        assert_eq!(groups.len(), 3, "three folders: {groups:?}");
        assert_eq!(groups[0].0, "/home/j/dev/bridge", "newest group first");
        assert_eq!(groups[1].0, "", "no-folder sessions share a group");
        assert_eq!(groups[2].0, "/home/j/dev/modron");
        assert_eq!(
            groups[0].1.iter().map(|(ix, _)| *ix).collect::<Vec<_>>(),
            vec![0, 2],
            "row positions preserved for stable element ids"
        );
    }

    #[test]
    fn sidebar_filter_matches_title_or_folder() {
        let sessions = vec![sidebar_row(
            "deploy",
            "2026-11-09T12:00:00Z",
            Some("/home/j/dev/bridge"),
        )];
        assert_eq!(DesktopApp::sidebar_groups(&sessions, "deploy").len(), 1);
        assert_eq!(DesktopApp::sidebar_groups(&sessions, "bridge").len(), 1);
        assert_eq!(DesktopApp::sidebar_groups(&sessions, "zzz").len(), 0);
    }

    #[test]
    fn group_labels_shorten_long_paths() {
        assert_eq!(DesktopApp::group_label(""), "this folder");
        assert_eq!(DesktopApp::group_label("."), "this folder");
        assert_eq!(DesktopApp::group_label("/a/b"), "/a/b");
        assert_eq!(DesktopApp::group_label("/Users/jacob/dev/modron"), "modron");
    }
}
