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
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    actions, div, list, px, AnyElement, App, AppContext, ClickEvent, Context, Entity, FocusHandle,
    Focusable, InteractiveElement, IntoElement, KeyBinding, ListAlignment, ListState,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Window, WindowOptions,
};
use modron_protocol::Decision;
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

/// Launch configuration from the CLI.
#[derive(Clone, Debug)]
pub struct LaunchOptions {
    /// `--connect <url>`: the modron server's WebSocket URL.
    pub connect: Option<String>,
    /// `--demo`: run the built-in mock server and connect to it.
    pub demo: bool,
}

impl Default for LaunchOptions {
    fn default() -> Self {
        Self {
            connect: Some(format!("ws://127.0.0.1:{}", modron_protocol::DEFAULT_PORT)),
            demo: false,
        }
    }
}

impl LaunchOptions {
    /// Parse `--connect <url>` / `--demo` from the process args.
    pub fn from_args() -> Self {
        let mut options = Self::default();
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--connect" => {
                    options.connect = args.next();
                    options.demo = false;
                }
                "--demo" => {
                    options.demo = true;
                }
                other => {
                    eprintln!(
                        "unknown argument {other:?}; usage: modron-desktop [--connect <url>|--demo]"
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
}

impl DesktopApp {
    fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        cmd_tx: async_channel::Sender<Command>,
        events: async_channel::Receiver<UiEvent>,
    ) -> Self {
        let composer =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type a task; enter to send"));
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("type to filter"));

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
            filter_query: String::new(),
            picker_open: false,
            picker_selected: 0,
            tool_panel_open: false,
            panel_rows: None,
            focus: cx.focus_handle(),
        };
        app.refresh(cx);
        app
    }

    /// Recompute the renderer-visible projections and redraw.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.display = self.state.rows();
        self.filter_query = self.filter.read(cx).value().to_string();
        self.list_state.reset(self.display.len());
        cx.notify();
    }

    fn send(&self, command: Command) {
        let _ = self.cmd_tx.send_blocking(command);
    }

    fn apply_event(&mut self, event: UiEvent, cx: &mut Context<Self>) {
        match event {
            UiEvent::Frame(frame) => self.state.apply_frame(*frame),
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
        if task.is_empty() {
            return;
        }
        self.composer.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        self.state.mark_running();
        self.state.last_note = None;
        let session_id = self.state.attached;
        self.send(Command::Submit { session_id, task });
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

    fn render_sidebar(&self, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let mut sidebar = div()
            .id("sidebar")
            .flex()
            .flex_col()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_size(px(12.))
                    .text_color(theme.muted_foreground)
                    .child(SharedString::from("sessions")),
            );
        for (ix, session) in self.state.sessions.iter().enumerate() {
            let id = session.id;
            let attached = self.state.attached == Some(id);
            let title: SharedString = session.title.clone().into();
            let meta: SharedString = format!(
                "{} messages · session {}",
                session.message_count,
                &id.to_string()[..8]
            )
            .into();
            let entity = entity.clone();
            sidebar = sidebar.child(
                div()
                    .id(("session", ix))
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .when(attached, |row| {
                        row.bg(theme.secondary).text_color(theme.accent_foreground)
                    })
                    .hover(|row| row.bg(theme.secondary))
                    .on_click(move |_event: &ClickEvent, _window, cx| {
                        entity.update(cx, |this, cx| this.attach(id, cx));
                    })
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme.foreground)
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(meta),
                    ),
            );
        }
        sidebar.into_any_element()
    }

    /// One transcript row as an element (called from the list's
    /// render_item; all data is read from the cached `display`).
    fn transcript_row(row: &Row, ix: usize, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let mut text_el = div().px_4().py_1().text_size(px(13.));
        match row {
            Row::Entry(TranscriptEntry::Task { text })
            | Row::Entry(TranscriptEntry::User { text }) => {
                let label: SharedString = format!("❯ {text}").into();
                text_el = text_el.text_color(theme.accent).child(label);
            }
            Row::Entry(TranscriptEntry::Assistant { text }) => {
                let label: SharedString = text.clone().into();
                text_el = text_el.text_color(theme.foreground).child(label);
            }
            Row::Entry(TranscriptEntry::ToolSummary { line, events }) => {
                let label: SharedString = line.clone().into();
                let hint: SharedString = "click to expand".into();
                let panel: Vec<String> = events.iter().map(|event| event.summary()).collect();
                let entity = entity.clone();
                text_el = text_el.text_color(theme.accent).child(
                    div()
                        .id(("tool-summary", ix))
                        .cursor_pointer()
                        .hover(|row| row.text_color(theme.accent_foreground))
                        .on_click(move |_event: &ClickEvent, _window, cx| {
                            entity.update(cx, |this, cx| {
                                this.open_tool_panel(Some(panel.clone()), cx);
                            });
                        })
                        .child(label)
                        .child(
                            div()
                                .ml_2()
                                .text_size(px(11.))
                                .text_color(theme.muted_foreground)
                                .child(hint),
                        ),
                );
            }
            Row::Entry(TranscriptEntry::Swarm { lines }) => {
                text_el = text_el.text_color(theme.muted_foreground);
                for line in lines {
                    let label: SharedString = line.clone().into();
                    text_el = text_el.child(label);
                }
            }
            Row::Entry(TranscriptEntry::Model { model, reason }) => {
                let label: SharedString = format!("model: {model} — {reason}").into();
                text_el = text_el.text_color(theme.muted_foreground).child(label);
            }
            Row::Entry(TranscriptEntry::Usage { model, usage }) => {
                let label: SharedString = format!("  {} tok · {model}", usage.total()).into();
                text_el = text_el.text_color(theme.muted_foreground).child(label);
            }
            Row::Entry(TranscriptEntry::Compaction {
                summary,
                summarized_events,
            }) => {
                let label: SharedString =
                    format!("{summary} ({summarized_events} events summarized)").into();
                text_el = text_el.text_color(theme.muted_foreground).child(label);
            }
            Row::Streaming { text } => {
                let label: SharedString = format!("{text} ▍").into();
                text_el = text_el.text_color(theme.accent).child(label);
            }
            Row::Reasoning { text } => {
                let label: SharedString = text.clone().into();
                text_el = text_el
                    .text_color(theme.muted_foreground)
                    .italic()
                    .child(label);
            }
        }
        text_el.into_any_element()
    }

    fn render_main(&self, entity: &Entity<Self>, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();

        // Transcript: virtualized list, follows the tail while scrolled to
        // the bottom (thousands of rows render only the visible range).
        let display_entity = entity.clone();
        let row_entity = entity.clone();
        let transcript = div().flex_1().min_h_0().overflow_hidden().child(list(
            self.list_state.clone(),
            move |ix, _window, cx| {
                let row = display_entity.read(cx).display.get(ix).cloned();
                match row {
                    Some(row) => Self::transcript_row(&row, ix, &row_entity, cx),
                    None => div().into_any_element(),
                }
            },
        ));

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

        // Composer: input + send/stop (the stop control replaces the
        // TUI's esc-to-interrupt by the divergence table).
        let composer = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_4()
            .py_2()
            .border_t_1()
            .border_color(theme.border)
            .child(div().flex_1().child(Input::new(&self.composer)))
            .children(if self.state.running {
                vec![Button::new("stop")
                    .label("stop")
                    .danger()
                    .small()
                    .on_click(cx.listener(|this, _e, _w, cx| this.interrupt(cx)))]
            } else {
                vec![Button::new("send")
                    .label("send")
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _e, window, cx| {
                        let task = this.composer.read(cx).value().to_string();
                        this.submit(&task, window, cx);
                    }))]
            });

        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .bg(theme.background)
            .child(transcript)
            .child(live)
            .child(composer)
            .into_any_element()
    }

    fn render_status_bar(&self, entity: &Entity<Self>, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let status: SharedString = self.state.status_line().into();
        let session: SharedString = self.state.session_label().into();
        let reasoning = self.state.show_reasoning;
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
            .child(div().flex_1().child(status))
            .child(
                Button::new("model-picker")
                    .label("model")
                    .ghost()
                    .xsmall()
                    .on_click({
                        let entity = entity.clone();
                        move |_e: &ClickEvent, window: &mut Window, cx: &mut App| {
                            entity.update(cx, |this, cx| this.open_picker(window, cx));
                        }
                    }),
            )
            .child(
                Button::new("reasoning-toggle")
                    .label("reasoning")
                    .ghost()
                    .xsmall()
                    .toggled(reasoning)
                    .on_click({
                        let entity = entity.clone();
                        move |_e: &ClickEvent, _window: &mut Window, cx: &mut App| {
                            entity.update(cx, |this, cx| this.toggle_reasoning(cx));
                        }
                    }),
            )
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
        root
    }
}

/// Launch the GPUI app: net thread (tokio) first, then the event loop.
pub fn run(options: LaunchOptions) {
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
        cx.bind_keys([
            KeyBinding::new("escape", PickerClose, Some("ModelPicker")),
            KeyBinding::new("up", PickerUp, Some("ModelPicker")),
            KeyBinding::new("down", PickerDown, Some("ModelPicker")),
            KeyBinding::new("escape", CloseToolPanel, Some("ToolPanel")),
        ]);
        gpui_kit::open_window(
            WindowOptions {
                titlebar: Some(gpui_kit::TitlebarOptions {
                    title: Some("modron desktop".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                ..WindowOptions::default()
            },
            cx,
            move |window, cx| {
                cx.new(|cx| DesktopApp::new(window, cx, cmd_tx.clone(), event_rx.clone()))
            },
        )
        .expect("failed to open the modron desktop window");
    });
}
