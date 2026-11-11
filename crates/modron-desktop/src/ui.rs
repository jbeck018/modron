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
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::theme::{Theme, ThemeMode};
use gpui_kit::component::Icon;
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    actions, div, list, px, AnyElement, App, AppContext, ClickEvent, Context, Entity, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyBinding, ListAlignment, ListState,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowOptions,
};
use modron_core::Attachment;
use modron_protocol::{Decision, Payload};
use modron_router::lookup;
use modron_view::TranscriptEntry;

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

gpui_kit::actions!(modron_desktop, [NewChat]);

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
    composer: Entity<InputState>,
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
    /// Settings overlay open (gear in the footer).
    settings_open: bool,
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
        let composer =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type a task; enter to send"));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("type to filter"));
        let session_filter =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter sessions"));

        // Enter in the composer submits.
        cx.subscribe_in(
            &composer,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let task = this.composer.read(cx).value().to_string();
                    this.submit(&task, window, cx);
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

    /// Stage whatever usable sits on the clipboard: an image, or copied
    /// file paths (Finder copies).
    fn stage_clipboard(&mut self, cx: &App) -> Result<(), String> {
        let Some(item) = cx.read_from_clipboard() else {
            return Err("the clipboard is empty".into());
        };
        let mut staged_any = false;
        let mut last_error = None;
        for entry in item.entries() {
            match entry {
                gpui_kit::ClipboardEntry::Image(image) => {
                    let media_type = match image.format {
                        gpui_kit::ImageFormat::Png => "image/png",
                        gpui_kit::ImageFormat::Jpeg => "image/jpeg",
                        gpui_kit::ImageFormat::Webp => "image/webp",
                        gpui_kit::ImageFormat::Gif => "image/gif",
                        _ => "image/png",
                    };
                    let kb = image.bytes.len() / 1024;
                    let thumb = Some(render_image(image.format, image.bytes.clone()));
                    self.staged.push((
                        Attachment::Image {
                            media_type: media_type.into(),
                            data: modron_core::b64::encode(&image.bytes),
                        },
                        format!("clipboard image · {kb} KB"),
                        thumb,
                    ));
                    staged_any = true;
                }
                gpui_kit::ClipboardEntry::ExternalPaths(paths) => {
                    for path in paths.paths() {
                        match self.stage_from_path(path) {
                            Ok(()) => staged_any = true,
                            Err(e) => last_error = Some(e),
                        }
                    }
                }
                gpui_kit::ClipboardEntry::String(_) => {}
            }
        }
        match (staged_any, last_error) {
            (true, _) => Ok(()),
            (false, Some(e)) => Err(e),
            (false, None) => Err("no image or files on the clipboard - drag one in instead".into()),
        }
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

    /// Populate the automation form from an existing automation for
    /// editing. The old is removed on save (remove + add replaces).
    fn start_edit_automation(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(auto) = self
            .state
            .automations
            .iter()
            .find(|a| a.name == *name)
            .cloned()
        else {
            return;
        };
        self.editing_automation = Some(name.to_string());
        self.auto_name
            .update(cx, |input, cx| input.set_value(&auto.name, window, cx));
        self.auto_prompt
            .update(cx, |input, cx| input.set_value(&auto.prompt, window, cx));
        self.auto_interval.update(cx, |input, cx| {
            input.set_value(auto.interval_minutes.to_string(), window, cx)
        });
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
            // "New task" (Conductor/Para grammar): a task spawns an agent
            // in its own worktree + branch. The inline form collects the
            // description and optional base branch.
            .child(div().px_3().pt_9().pb_2().flex().flex_col().gap_1().child({
                let entity = entity.clone();
                Button::new("new-task")
                    .label(if self.new_task_open {
                        "describe the task..."
                    } else {
                        "New task"
                    })
                    .primary()
                    .on_click(move |_e: &ClickEvent, window, cx: &mut App| {
                        entity.update(cx, |this, cx| this.open_task_form(window, cx));
                    })
            }))
            .children(if self.new_task_open {
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
            })
            .child(div().px_3().pb_2().flex().flex_row().gap_1().child({
                let entity = entity.clone();
                Button::new("new-chat")
                    .label("New chat")
                    .ghost()
                    .small()
                    .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                        entity.update(cx, |this, _cx| {
                            this.send(Command::NewSession { title: None });
                            // Refresh the list once the server notes
                            // the new session (the correlated Note
                            // adopts it).
                            this.send(Command::ListSessions);
                        });
                    })
            }))
            .child(div().px_3().pb_2().child(Input::new(&self.session_filter)));

        let mut shown = 0usize;
        for (ix, session) in self.state.sessions.iter().enumerate() {
            if !query.is_empty() && !session.title.to_ascii_lowercase().contains(&query) {
                continue;
            }
            shown += 1;
            let id = session.id;
            let attached = self.state.attached == Some(id);
            let running = self.row_running(session, id) || (attached && self.state.running);
            let title: SharedString = session.title.clone().into();
            let meta: SharedString = Self::relative_time(&session.updated_at).into();
            let entity = entity.clone();
            let attach_entity = entity.clone();
            // Single line: truncated title, relative time on the right
            // (the desktop chat grammar - message counts and ids live in
            // the transcript header, not the list).
            let row = div()
                .id(("session", ix))
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .mx_2()
                .px_2()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .when(attached, |row| row.bg(theme.secondary))
                .hover(|row| row.bg(theme.secondary))
                .on_click(move |_event: &ClickEvent, _window, cx| {
                    attach_entity.update(cx, |this, cx| this.attach(id, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(12.5))
                        .text_color(theme.foreground)
                        .overflow_hidden()
                        .whitespace_nowrap()
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
                    let running = self.row_running(session, id) || (attached && self.state.running);
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
                        let entity_yes = entity.clone();
                        let entity_no = entity.clone();
                        trailing.push(
                            Button::new(("del-yes", ix))
                                .label("delete")
                                .danger()
                                .xsmall()
                                .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                    entity_yes.update(cx, |this, cx| this.delete_session(id, cx));
                                })
                                .into_any_element(),
                        );
                        trailing.push(
                            Button::new(("del-no", ix))
                                .label("keep")
                                .ghost()
                                .xsmall()
                                .on_click(move |_e: &ClickEvent, _w, cx: &mut App| {
                                    entity_no.update(cx, |this, cx| {
                                        this.confirm_delete = None;
                                        this.refresh(cx);
                                    });
                                })
                                .into_any_element(),
                        );
                    } else {
                        let entity_del = entity.clone();
                        trailing.push(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.muted_foreground)
                                .child(meta)
                                .into_any_element(),
                        );
                        // Worktree tasks carry a branch chip: the visual
                        // mark that this session runs in its own worktree.
                        if let Some(branch) = &session.task_branch {
                            let chip: SharedString = branch.trim_start_matches("modron/").into();
                            trailing.push(
                                div()
                                    .px_1p5()
                                    .rounded_sm()
                                    .bg(theme.accent)
                                    .text_size(px(10.))
                                    .text_color(theme.accent_foreground)
                                    .child(chip)
                                    .into_any_element(),
                            );
                        }
                        // Hover-only: opacity 0 until the row is hovered.
                        trailing.push(
                            div()
                                .opacity(0.)
                                .group_hover("session-row", |style| style.opacity(1.))
                                .child(
                                    Button::new(("del", ix))
                                        .icon(Icon::default().path("icons/trash.svg").size_4())
                                        .ghost()
                                        .xsmall()
                                        .on_click(move |_event: &ClickEvent, _w, cx: &mut App| {
                                            // The x sits inside the clickable row:
                                            // stop propagation so the attach
                                            // handler does not fire too.
                                            cx.stop_propagation();
                                            entity_del.update(cx, |this, cx| {
                                                this.confirm_delete = Some(id);
                                                this.refresh(cx);
                                            });
                                        }),
                                )
                                .into_any_element(),
                        );
                    }
                    trailing
                });
            list = list.child(row);
        }
        if shown == 0 {
            list = list.child(
                div()
                    .px_3()
                    .py_4()
                    .text_size(px(12.))
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
                    .text_size(px(11.))
                    .text_color(theme.accent_foreground)
                    .child(SharedString::from("J")),
            )
            .child(
                div()
                    .flex_1()
                    .text_size(px(12.))
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
                        .text_size(px(13.))
                        .child(label),
                )
                .into_any_element()
        };

        match row {
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
                                    .text_size(px(12.))
                                    .text_color(theme.foreground)
                                    .child(label),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
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
                            .text_size(px(12.))
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
                            .text_size(px(11.))
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
                    .text_size(px(13.5))
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
                    .text_size(px(12.5))
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
                    .text_size(px(28.))
                    .text_color(theme.border)
                    .child(SharedString::from("\u{2712}")),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(theme.foreground)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(12.))
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
                    .text_size(px(13.))
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
            // Share: opens a remote-control tunnel via the server's bridge.
            .children(if self.state.attached.is_some() {
                vec![Button::new("share-remote")
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
                    .into_any_element()]
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
                    .text_size(px(12.))
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
                    .child(div().flex_1().text_size(px(13.)).child(prompt))
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
                    .text_size(px(12.))
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
                                .text_size(px(11.))
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
                        .text_size(px(12.))
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
                    .text_size(px(14.))
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
                            .text_size(px(11.))
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
        let composer = div().px_4().pb_3().pt_1().child(staged_row).child(
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
                .child(
                    Button::new("add-attachment")
                        .label("+")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            if let Err(e) = this.stage_clipboard(cx) {
                                this.state.last_note = Some(e);
                            }
                            this.refresh(cx);
                        })),
                )
                .child(
                    Button::new("mode-chip")
                        .label(mode_chip)
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _e, _w, cx| this.cycle_mode(cx))),
                )
                .child(div().flex_1().child(Input::new(&self.composer).on_paste({
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
                                            gpui_kit::ImageFormat::Png => "image/png",
                                            gpui_kit::ImageFormat::Jpeg => "image/jpeg",
                                            gpui_kit::ImageFormat::Webp => "image/webp",
                                            gpui_kit::ImageFormat::Gif => "image/gif",
                                            _ => "image/png",
                                        };
                                        let kb = image.bytes.len() / 1024;
                                        let thumb =
                                            Some(render_image(image.format, image.bytes.clone()));
                                        this.staged.push((
                                            Attachment::Image {
                                                media_type: media_type.into(),
                                                data: modron_core::b64::encode(&image.bytes),
                                            },
                                            format!("pasted image · {kb} KB"),
                                            thumb,
                                        ));
                                        took = true;
                                    }
                                    gpui_kit::ClipboardEntry::ExternalPaths(paths) => {
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
                })))
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
                .text_size(px(11.))
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
                            .text_size(px(11.))
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
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("no file changes vs base")),
            );
        }
        for file in &self.review_files {
            let label: SharedString =
                format!("{}  +{} -{}", file.path, file.added, file.deleted).into();
            panel = panel.child(
                div()
                    .text_size(px(12.))
                    .text_color(theme.foreground)
                    .child(label),
            );
        }
        Some(panel.into_any_element())
    }

    /// Settings overlay: model chip, approval mode, reasoning, theme,
    /// automations CRUD, onboarding entry. Every control drives the same
    /// protocol commands as the chat surface (parity: the /command set).
    fn render_settings(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        use gpui_kit::component::button::{Button, ButtonVariants};
        use gpui_kit::component::input::Input;
        let theme = cx.theme();
        let backdrop = div()
            .absolute()
            .inset_0()
            .bg(gpui_kit::black())
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _e, _w, cx| {
                    this.settings_open = false;
                    this.refresh(cx);
                }),
            );
        let mut panel = div()
            .id("settings-panel")
            .w(px(560.))
            .max_h(px(600.))
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
                            .text_size(px(15.))
                            .text_color(theme.foreground)
                            .child(SharedString::from("settings")),
                    )
                    .child(
                        Button::new("settings-close")
                            .label("close")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.settings_open = false;
                                this.refresh(cx);
                            })),
                    ),
            )
            // Approval mode.
            .child(
                div().flex().flex_row().gap_2().child(
                    Button::new("settings-mode")
                        .label(format!("mode: {}", self.state.approval_mode))
                        .ghost()
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
                        })),
                ),
            );

        // Automations section.
        let mut autos = div().flex().flex_col().gap_1().child(
            div()
                .text_size(px(12.))
                .text_color(theme.muted_foreground)
                .child(SharedString::from("scheduled automations")),
        );
        for (ix, auto) in self.state.automations.iter().enumerate() {
            let label: SharedString =
                format!("{} - every {}m", auto.name, auto.interval_minutes).into();
            let name = auto.name.clone();
            autos = autos.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().text_size(px(12.5)).child(label))
                    .child(
                        Button::new(("auto-edit", ix))
                            .label("edit")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener({
                                let name = name.clone();
                                move |this, _e: &ClickEvent, window: &mut Window, cx| {
                                    this.start_edit_automation(&name, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new(("auto-remove", ix))
                            .label("remove")
                            .danger()
                            .xsmall()
                            .on_click(cx.listener(move |this, _e, _w, cx| {
                                this.remove_automation(name.clone(), cx);
                            })),
                    ),
            );
        }
        autos = autos
            .child(div().flex().flex_row().gap_1().children([
                Input::new(&self.auto_name).id("auto-name"),
                Input::new(&self.auto_prompt).id("auto-prompt"),
                Input::new(&self.auto_interval).id("auto-interval"),
            ]))
            .child(
                div().flex().flex_row().justify_end().child(
                    Button::new("auto-add")
                        .label("add automation")
                        .small()
                        .on_click(cx.listener(|this, _e, window, cx| {
                            this.add_automation(window, cx);
                        })),
                ),
            );
        panel = panel.child(autos);

        // Onboarding entry.
        panel = panel.child(
            div().flex().flex_row().justify_end().child(
                Button::new("onboarding")
                    .label("run setup wizard")
                    .small()
                    .on_click(cx.listener(|this, _e, _w, cx| this.open_onboarding(cx))),
            ),
        );

        backdrop
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size_full()
                    .child(panel),
            )
            .into_any_element()
    }

    /// Onboarding wizard overlay: step 0 = provider auth status (re-checks
    /// on open), step 1 = cheap/strong model wiring persisted server-side.
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
                    .text_size(px(15.))
                    .text_color(theme.foreground)
                    .child(SharedString::from(format!("setup wizard ({}/2)", step + 1))),
            );

        match step {
            0 => {
                panel = panel.child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(
                            "sign in to providers (browser opens on the server machine)",
                        )),
                );
                for row in &self.state.auth_providers {
                    let label: SharedString = format!("{} - {}", row.label, row.status).into();
                    panel = panel.child(
                        div()
                            .text_size(px(12.5))
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
                        .text_size(px(12.5))
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
            .text_size(px(11.))
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
                    .child(div().text_size(px(13.)).child(name))
                    .child(
                        div()
                            .text_size(px(11.))
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
                            .text_size(px(12.))
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
                    .child(self.render_main(&entity, cx)),
            )
            .child(status);
        if self.picker_open {
            root = root.child(self.render_picker(&entity, cx));
        }
        if self.settings_open {
            root = root.child(self.render_settings(cx));
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
                        .text_size(px(12.))
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
                        .text_size(px(11.))
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
                            .text_size(px(12.5))
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
                        .text_size(px(12.))
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
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(bar_label),
                        )
                        .child(div().h(px(10.)).w(width).rounded_sm().bg(theme.accent))
                        .child(
                            div()
                                .text_size(px(11.))
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
                    .text_size(px(13.))
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
                            .text_size(px(12.))
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
                                        .text_size(px(12.))
                                        .text_color(theme.muted_foreground)
                                        .child(options)
                                        .into_any_element()]
                                }
                                _ => vec![div()
                                    .text_size(px(12.))
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

    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // Follow the system appearance at launch; the footer's moon button
        // flips it manually, and open windows re-follow on OS switches.
        Theme::change(ThemeMode::from(cx.window_appearance()), None, cx);
        cx.bind_keys([
            KeyBinding::new("escape", PickerClose, Some("ModelPicker")),
            KeyBinding::new("up", PickerUp, Some("ModelPicker")),
            KeyBinding::new("down", PickerDown, Some("ModelPicker")),
            KeyBinding::new("escape", CloseToolPanel, Some("ToolPanel")),
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
                cx.new(|cx| DesktopApp::new(window, cx, cmd_tx.clone(), event_rx.clone(), resume))
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
}
