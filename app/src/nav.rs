//! Screen navigation, shared by both hosts. The host owns the state: it
//! routes keys before the view sees them (`chat::route_key`), so the screen
//! stack, the filters and selections, the settings rows, the model list, the
//! sub-agents, the inline confirmation and the composer's command list live
//! in [`Nav`]. The view renders the [`NavView`] the host sends whenever it
//! changes. Presentation only: every word comes from the core or the shared
//! table.

use modron_mobile_core::client::TASK_STATUS_REFRESH_BATCH;
use modron_mobile_core::view::{
    auth_is_signed_in, auth_row_line, delete_confirm_text, kill_confirm_text, remove_confirm_text,
    sandbox_label, AgentRowView, OAuthProgressView, MODEL_ROLE_CHEAP, MODEL_ROLE_CURRENT,
    MODEL_ROLE_FALLBACK, MODEL_ROLE_STRONG, PERSONA_CLEAR_NOTE, PICKER_NONE_LABEL,
    SETTING_AGENT_LABEL, SETTING_ALLOW_LABEL, SETTING_AUTH_LABEL, SETTING_DENY_LABEL,
    SETTING_THEME_LABEL,
};
use modron_mobile_core::view::{
    automation_interval_error, automation_row, merge_confirm_text, AUTOMATION_FIELDS_NEEDED,
    AUTOMATION_INTERVAL_LABEL, AUTOMATION_NAME_LABEL, AUTOMATION_PROMPT_LABEL, TASK_TITLE_LABEL,
    TASK_TITLE_NEEDED,
};
use modron_mobile_core::view::{credential_remove_confirm_text, PersonaRowView};
use modron_mobile_core::{
    phase_next, ApprovalModeWire, Effort, SandboxPolicy, ViewModelSnapshot, EFFORT_UNSET_LABEL,
    RULES_NONE_LABEL, SETTING_APPROVAL_LABEL, SETTING_COMPACT_LABEL, SETTING_CONTEXT_LABEL,
    SETTING_EFFORT_LABEL, SETTING_MODEL_LABEL, SETTING_PHASE_LABEL, SETTING_PROVIDER_LABEL,
    SETTING_RULES_LABEL, SETTING_SANDBOX_LABEL,
};
use modron_protocol::AutomationWire;
use std::time::Duration;

use crate::clock::Instant;

/// The Tasks screen asks for its statuses at most this often (the timer).
const TASK_REFRESH_EVERY: Duration = Duration::from_secs(5);
use uuid::Uuid;

use crate::chat::UiAction;
use crate::help::{self, HelpTab, HELP_VISIBLE};
use crate::portal::{self, PortalLine, PortalRow};
use crate::slash;
use crate::theme::{Choice, CHOICES};

/// The screens. Chat is the root; the others open over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Chat,
    Sessions,
    Settings,
    /// The key and slash-command help.
    Help,
    /// The model picker (`/model`, or the Settings model row).
    Models,
    /// One sub-agent. Its id is kept beside the stack ([`Nav::agent_id`]),
    /// so the screen type stays `Copy`.
    Agent,
    /// The owner sign-in (web, portal off the URL): one token input.
    SignIn,
    /// The owner's session list (web, portal mode).
    Portal,
    /// Credentials (`/auth`, the Settings auth row): the provider rows, the
    /// sign-in progress, and the masked key entry.
    Auth,
    /// Scheduled automations (`/automations`): the list, the inline add form,
    /// and the inline remove confirmation.
    Automations,
    /// Task sessions (`/tasks`, `/worktree`): the list, the inline create
    /// form, attach, and the inline merge confirmation.
    Tasks,
    /// A list of values (`/mode`, `/sandbox`, `/effort`, `/agent`, `/theme`,
    /// or their Settings rows): type to filter, Enter applies and closes.
    Picker,
}

/// One navigation step. Keys and taps both become steps; [`Nav::apply`]
/// performs them and returns the core calls they imply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavStep {
    /// Open a screen on top of the stack (Sessions and Models also refresh
    /// their list from the server).
    Open(Screen),
    /// Open the agent screen for a sub-agent id (Chat only).
    OpenAgent(String),
    /// Esc with no filter text: back one screen (never past Chat).
    Back,
    /// Esc with filter text: clear the filter first.
    ClearFilter,
    /// A typed character appended to the sessions or models filter.
    Filter(char),
    /// Backspace in the filter.
    FilterBack,
    /// Up / Down, or j / k with an empty sessions filter. Negative moves up.
    Move(isize),
    /// Enter or a tap on a session row: attach it and go to Chat.
    Attach(Uuid),
    /// `n` with an empty filter, or the new-session control: create one
    /// and go to Chat.
    NewSession,
    /// Enter or a tap on a model row: switch to it and go to Chat.
    PickModel(String),
    /// Enter or a tap on a portal row (index into the visible rows): open it
    /// when it is online.
    OpenPortal(usize),
    /// `d` on a session row, or its delete control: ask first (the row index).
    AskDelete(usize),
    /// `k` on an empty steer draft, or the kill control: ask first.
    AskKill,
    /// `y` or the yes control on the inline confirmation.
    Confirm,
    /// `n`, Esc or the no control on the inline confirmation.
    Cancel,
    /// Up / Down in the composer's command suggestions.
    SlashMove(isize),
    /// Esc with the suggestions open: hide them (the text stays).
    SlashHide,
    /// Tab (or Enter on a partial command): complete the selected command.
    CompleteSelected,
    /// A tap on a suggestion row: complete that command.
    Complete(String),
    /// Enter on the selected credentials row: start the browser sign-in
    /// (OAuth-capable provider) or open the masked key entry.
    Activate,
    /// A tap on a credentials row (web): select it, then as [`NavStep::Activate`].
    ActivateRow(usize),
    /// Esc during the key entry: close it, the typed text is dropped.
    AuthCancelKey,
    /// The key entry was submitted: close it (the view has sent the key).
    AuthKeyDone,
    /// `a` on an empty filter (Automations) or `n` (Tasks), or the add
    /// control: open the inline form for that kind.
    OpenForm(FormKind),
    /// A typed character into the open form's active field.
    FormChar(char),
    /// Backspace in the open form.
    FormBack,
    /// Tab: the next field (the last field stays).
    FormNext,
    /// Enter: the next field, or on the last field the submit.
    FormEnter,
    /// The form's save control (web): submit whatever field is active.
    FormSubmit,
    /// Esc in the open form (or its cancel control): close it.
    FormCancel,
    /// `d` on an automation row (or its remove control): ask first (row index).
    AskRemove(usize),
    /// `m` on a task row (or its merge control): ask first (row index).
    AskMerge(usize),
    /// Open a picker over the screen beneath it (the bare command, or the
    /// Settings row).
    OpenPicker(PickerKind),
    /// Bare `/allow` (true) or `/deny` (false): Settings with the cursor on
    /// that list's rows, where `d` removes an entry (the removal flow).
    OpenToolList(bool),
    /// `/auth <provider>`: the credentials screen, on that provider's row,
    /// starting key entry or sign-in for it once its row is known.
    AuthFor(String),
    /// Enter, or a tap on a picker row (index into the filtered rows): apply
    /// the row's action and close the picker.
    PickRow(usize),
    /// Save and draw the theme `Choice` (the picker's rows, `/theme <name>`).
    SetTheme(Choice),
    /// `d` on a removable Settings row (a rule, an allow or deny entry, or
    /// its remove control): ask first (row index).
    AskSettingRemove(usize),
    /// `d` (or the web remove control) on a signed-in credentials row: asks
    /// y / n first, then sends `auth_remove`.
    AskAuthRemove(usize),
    /// Left and Right, Tab and Shift+Tab, or a tap on a help tab: step the
    /// tab ring (negative steps back).
    HelpCycle(isize),
    /// Open a help tab (a tap on its label).
    HelpTo(HelpTab),
    /// Up and Down, PgUp and PgDn, or the wheel: move the help rows.
    HelpScroll(isize),
    /// Home (top) or End (bottom) on the help rows.
    HelpEdge(bool),
}

/// Which list a picker shows. [`picker_rows`] builds its rows; a row's action
/// is the one the bare command or the Settings row sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    /// Approval mode: ask, yolo (approve all), deny.
    Mode,
    /// Sandbox policy: off, seatbelt, bwrap.
    Sandbox,
    /// Reasoning effort: unset, then the five levels.
    Effort,
    /// Personas (`/agent <name>` applies directly).
    Agent,
    /// Theme: oculus dark, oculus light, system.
    Theme,
}

/// One picker row: its label, whether it is the current value (drawn with a
/// dot), whether Enter may pick it, and the action Enter sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerRow {
    pub label: String,
    pub current: bool,
    pub enabled: bool,
    pub action: Option<UiAction>,
}

/// The values a picker marks as current: the snapshot's labels (approval
/// mode, sandbox, effort), the host's theme and the persona names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PickerNow {
    pub mode: String,
    pub sandbox: String,
    pub effort: String,
    pub theme: Choice,
    pub personas: Vec<PersonaRowView>,
}

/// The theme change the view applies once: `seq` rises by one per change, and
/// `remember` says whether the choice is saved (a restore on startup is not).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThemeChange {
    pub seq: u64,
    pub choice: Choice,
    pub remember: bool,
}

/// The picker's title: the Settings row label it shares.
pub fn picker_title(kind: PickerKind) -> &'static str {
    match kind {
        PickerKind::Mode => SETTING_APPROVAL_LABEL,
        PickerKind::Sandbox => SETTING_SANDBOX_LABEL,
        PickerKind::Effort => SETTING_EFFORT_LABEL,
        PickerKind::Agent => SETTING_AGENT_LABEL,
        PickerKind::Theme => SETTING_THEME_LABEL,
    }
}

/// The rows of one picker, with the current value marked. Agent lists the
/// personas the core sends (`personas_req` when the picker opens), the row
/// text verbatim. Until they arrive, or when the server refuses the list, the
/// row is the shared "none" and it is not selectable. There is no row that
/// clears the persona: the server refuses an empty persona name.
pub fn picker_rows(kind: PickerKind, now: &PickerNow) -> Vec<PickerRow> {
    let row = |label: String, current: bool, action: UiAction| PickerRow {
        label,
        current,
        enabled: true,
        action: Some(action),
    };
    match kind {
        PickerKind::Mode => {
            [ApprovalModeWire::Ask, ApprovalModeWire::ApproveAll, ApprovalModeWire::Deny]
                .into_iter()
                .map(|mode| {
                    row(
                        mode.label().to_string(),
                        now.mode == mode.label(),
                        UiAction::SetApprovalMode(mode),
                    )
                })
                .collect()
        }
        PickerKind::Sandbox => {
            [SandboxPolicy::Off, SandboxPolicy::Seatbelt, SandboxPolicy::Bubblewrap]
                .into_iter()
                .map(|policy| {
                    let label = sandbox_label(policy);
                    row(label.to_string(), now.sandbox == label, UiAction::SetSandbox(policy))
                })
                .collect()
        }
        PickerKind::Effort => {
            let mut rows = vec![row(
                EFFORT_UNSET_LABEL.to_string(),
                now.effort.is_empty(),
                UiAction::SetEffort(None),
            )];
            rows.extend(
                [Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max]
                    .into_iter()
                    .map(|e| {
                        row(
                            e.label().to_string(),
                            now.effort == e.label(),
                            UiAction::SetEffort(Some(e)),
                        )
                    }),
            );
            rows
        }
        PickerKind::Agent if now.personas.is_empty() => vec![PickerRow {
            label: PICKER_NONE_LABEL.to_string(),
            current: false,
            enabled: false,
            action: None,
        }],
        PickerKind::Agent => now
            .personas
            .iter()
            .map(|persona| PickerRow {
                label: persona.text.clone(),
                current: persona.current,
                enabled: true,
                action: Some(UiAction::SetPersona(persona.name.clone())),
            })
            .collect(),
        PickerKind::Theme => CHOICES
            .into_iter()
            .map(|choice| {
                row(
                    choice.name().to_string(),
                    now.theme == choice,
                    UiAction::Nav(NavStep::SetTheme(choice)),
                )
            })
            .collect(),
    }
}

/// Which inline form is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormKind {
    /// Name, prompt and interval minutes.
    Automation,
    /// The task title.
    Task,
}

impl FormKind {
    fn labels(self) -> Vec<&'static str> {
        match self {
            FormKind::Automation => {
                vec![AUTOMATION_NAME_LABEL, AUTOMATION_PROMPT_LABEL, AUTOMATION_INTERVAL_LABEL]
            }
            FormKind::Task => vec![TASK_TITLE_LABEL],
        }
    }
}

/// The open inline form as the view shows it: each field's label and text,
/// the active field, and the refusal (the client's check, or none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormView {
    pub kind: FormKind,
    pub labels: Vec<&'static str>,
    pub values: Vec<String>,
    pub active: usize,
    pub error: Option<String>,
}

/// One automation row: its name (for the remove confirmation) and the
/// shared row text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AutomationRow {
    pub name: String,
    pub text: String,
}

/// One task row: the session id (attach, status, merge), the branch (the
/// merge confirmation) and the shared row text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRow {
    pub id: Uuid,
    pub branch: String,
    pub text: String,
    /// The latest status summary (`ahead 3 - 1 file (+120 -14)`), empty until
    /// one arrives.
    pub summary: String,
}

/// One credentials row: the core's label and status (shown verbatim), and
/// whether Enter starts the browser sign-in or opens the key entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthRow {
    pub id: String,
    pub label: String,
    pub status: String,
    pub oauth_capable: bool,
}

impl AuthRow {
    /// The row as the screen shows it: `label: status`.
    pub fn line(&self) -> String {
        auth_row_line(&self.label, &self.status)
    }

    /// Whether the core counts this row as signed in (not "not configured").
    pub fn signed_in(&self) -> bool {
        auth_is_signed_in(&self.status)
    }
}

/// One sessions row: the id to attach, the title (for confirmations) and
/// the core's row text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub id: Uuid,
    pub title: String,
    pub text: String,
}

/// One settings row: a core label, its value, and the action a press sends
/// (None for read-only rows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingRow {
    pub label: String,
    pub value: String,
    pub action: Option<UiAction>,
    /// The action a confirmed removal sends (permission rules and allow or
    /// deny entries). None for rows that cannot be removed.
    pub remove: Option<UiAction>,
}

impl SettingRow {
    /// The row as the screens show it: `label: value`, or the bare label
    /// when there is no value (the compact action).
    pub fn line(&self) -> String {
        if self.value.is_empty() {
            self.label.clone()
        } else {
            format!("{}: {}", self.label, self.value)
        }
    }
}

/// The credentials rows from the snapshot's provider list, in the core's
/// order. A row is OAuth-capable when the core says so.
pub fn auth_rows(s: &ViewModelSnapshot) -> Vec<AuthRow> {
    s.providers
        .iter()
        .map(|p| AuthRow {
            id: p.id.clone(),
            label: p.label.clone(),
            status: p.status.clone(),
            oauth_capable: p.oauth_capable,
        })
        .collect()
}

/// One model picker row: the model name and its line (`model (role)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRow {
    pub model: String,
    /// The provider the model runs on (`persist_default_model` saves the pair).
    /// Empty for a router-default row that has no catalog entry yet.
    pub provider: String,
    pub text: String,
    /// Catalog rows that the server cannot run are dim and not selectable.
    pub available: bool,
    /// The session's current model: marked, and preselected when the picker
    /// opens.
    pub current: bool,
}

/// One invite row of the Settings share section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteLine {
    pub id: String,
    pub text: String,
}

/// The Settings share section (owner, web, a connected session).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ShareView {
    pub invites: Vec<InviteLine>,
    /// The URL of the invite just created, shown as selectable text.
    pub created: Option<String>,
    /// The shared word after a successful copy.
    pub note: Option<String>,
}

/// What the view renders.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NavView {
    /// Bumped by every transcript scroll step; the view scrolls once per step.
    pub scroll_seq: u64,
    /// Lines the transcript is scrolled up from the newest row (0 = following).
    pub scroll_offset: usize,
    /// Rows arrived while scrolled up.
    pub new_below: bool,
    pub screen: Screen,
    pub filter: String,
    pub selected: usize,
    /// Every session the core holds (0 shows the empty state).
    pub total: usize,
    /// Why the list is empty when the server refused it (the snapshot's last
    /// error, shown only while no session is listed). None otherwise.
    pub sessions_note: Option<String>,
    /// The rows that match the filter, in the core's order.
    pub sessions: Vec<SessionRow>,
    pub settings: Vec<SettingRow>,
    /// The model rows that match the filter, and every model row.
    pub models: Vec<ModelRow>,
    pub models_total: usize,
    /// The save-as-default action for the highlighted model (web control).
    pub save_default: Option<UiAction>,
    /// Every sub-agent of the attached session, in the core's order.
    pub agents: Vec<AgentRowView>,
    /// The agent the Agent screen shows (its last seen row).
    pub agent: Option<AgentRowView>,
    /// Whether the open agent still takes a steer or a kill.
    pub agent_live: bool,
    /// The inline confirmation line, when one waits for y or n.
    pub confirm: Option<String>,
    /// Whether the composer's command suggestions are hidden (Esc).
    pub slash_hidden: bool,
    pub slash_selected: usize,
    /// The tool detail panel's first visible row, while it is open.
    pub tool_panel: Option<usize>,
    /// A completed composer text, with a sequence number so the view applies
    /// each one once.
    pub completion: (u64, String),
    /// The portal list rows that match the filter.
    pub portal: Vec<PortalLine>,
    /// Every portal row the list holds (0 shows the empty state).
    pub portal_total: usize,
    /// The sign-in message from the last attempt.
    pub signin_error: Option<String>,
    /// The share section, when the owner is connected to a session.
    pub share: Option<ShareView>,
    /// The credentials rows (Auth screen).
    pub auth: Vec<AuthRow>,
    /// The provider whose key entry is open (masked input), if any.
    pub auth_key: Option<String>,
    /// The latest browser sign-in progress from the snapshot.
    pub auth_progress: Option<OAuthProgressView>,
    /// The automation rows that match the filter, in the server's order.
    pub automations: Vec<AutomationRow>,
    /// Every automation the server listed (0 shows the empty state).
    pub automations_total: usize,
    /// The task rows that match the filter, and every task's count.
    pub tasks: Vec<TaskRow>,
    pub tasks_total: usize,
    /// The inline form, while one is open.
    pub form: Option<FormView>,
    /// The snapshot's last note (a task's status or merge result) while it
    /// is still fresh; the host expires it (see `Host::push`).
    pub note: Option<String>,
    /// The picker on top: its title and the rows that match the filter.
    pub picker_title: String,
    /// A note under the picker's filter line (the agent picker's clear note).
    pub picker_note: String,
    pub picker: Vec<PickerRow>,
    /// The theme in use, and the change the view applies once.
    pub theme: Choice,
    pub theme_change: ThemeChange,
    /// The help tab on show, its rows, and the first visible row.
    pub help_tab: HelpTab,
    pub help_rows: Vec<String>,
    pub help_scroll: usize,
}

impl NavView {
    /// Whether the owner portal (sign-in or the list) is the screen.
    pub fn is_portal(&self) -> bool {
        matches!(self.screen, Screen::SignIn | Screen::Portal)
    }
}

/// An action held until y confirms it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Pending {
    text: String,
    actions: Vec<UiAction>,
}

/// The navigation state the host owns.
#[derive(Debug, Default)]
pub struct Nav {
    /// Screens above Chat, oldest first. Empty means Chat.
    stack: Vec<Screen>,
    /// The snapshot's last error (see [`NavView::sessions_note`]).
    last_error: Option<String>,
    filter: String,
    selected: usize,
    all: Vec<SessionRow>,
    settings: Vec<SettingRow>,
    models: Vec<ModelRow>,
    agents: Vec<AgentRowView>,
    agent_id: Option<String>,
    agent_row: Option<AgentRowView>,
    pending: Option<Pending>,
    /// The command names the composer's suggestions show, in order.
    slash_items: Vec<String>,
    slash_sel: usize,
    slash_hidden: bool,
    completion: (u64, String),
    /// The screen under the stack: Chat, or the portal's sign-in or list.
    root: Screen,
    portal: Vec<PortalLine>,
    signin_error: Option<String>,
    /// The owner is signed in and the page has a session (Settings share).
    share_enabled: bool,
    session: Option<Uuid>,
    invites: Vec<InviteLine>,
    created: Option<String>,
    share_note: Option<String>,
    auth: Vec<AuthRow>,
    auth_progress: Option<OAuthProgressView>,
    /// The provider whose masked key entry is open.
    auth_key: Option<String>,
    /// The provider `/auth <provider>` named, until its row is on screen.
    auth_target: Option<String>,
    /// The first-run check ran (see [`Nav::first_run`]).
    first_run_done: bool,
    automations: Vec<AutomationWire>,
    /// Sessions that carry a task branch (the Tasks screen's rows).
    tasks: Vec<TaskRow>,
    /// The personas the core lists (the agent picker's rows).
    personas: Vec<PersonaRowView>,
    /// When the last task status refresh was asked for (Tasks only).
    task_refresh_at: Option<Instant>,
    /// How many task rows the refreshes so far have been asked to cover.
    task_covered: usize,
    /// The open inline form: its kind, field texts and the active field.
    form: Option<Form>,
    form_error: Option<String>,
    /// The snapshot's last note, as the host passed it (expired by the host).
    last_note: Option<String>,
    /// The snapshot's approval mode, sandbox and effort labels (the pickers
    /// mark them as current).
    mode_label: String,
    sandbox_label: String,
    effort_label: String,
    /// The open picker's list, while one is on top.
    picker: Option<PickerKind>,
    /// The theme in use and the last change (see [`ThemeChange`]).
    theme: Choice,
    theme_change: ThemeChange,
    /// The help tab on show and its first visible row.
    help_tab: HelpTab,
    help_scroll: usize,
}

/// The inline form's state. `values` has one text per field of the kind.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Form {
    kind: FormKind,
    values: Vec<String>,
    active: usize,
}

impl Form {
    fn new(kind: FormKind) -> Self {
        Self { kind, values: vec![String::new(); kind.labels().len()], active: 0 }
    }
}

impl Nav {
    /// The screen on top of the stack.
    pub fn top(&self) -> Screen {
        self.stack.last().copied().unwrap_or(self.root)
    }

    /// Whether the portal (sign-in or list) is the app's screen.
    pub fn is_portal(&self) -> bool {
        matches!(self.root, Screen::SignIn | Screen::Portal)
    }

    /// Makes `screen` the bottom of the stack and clears what is above it.
    pub fn set_root(&mut self, screen: Screen) {
        self.root = screen;
        self.stack.clear();
        self.filter.clear();
        self.selected = 0;
        self.pending = None;
        self.clamp();
    }

    /// The portal list's rows (`GET /api/sessions`, in the server's order).
    pub fn set_portal(&mut self, rows: &[PortalRow]) {
        self.portal = portal::lines(rows);
        self.clamp();
    }

    pub fn set_signin_error(&mut self, error: Option<String>) {
        self.signin_error = error;
    }

    /// The owner's share section: on when `/api/me` answered 200.
    pub fn set_share_enabled(&mut self, enabled: bool) {
        self.share_enabled = enabled;
    }

    pub fn set_invites(&mut self, invites: Vec<InviteLine>) {
        self.invites = invites;
    }

    pub fn set_created(&mut self, url: Option<String>) {
        self.created = url;
    }

    pub fn set_share_note(&mut self, note: Option<String>) {
        self.share_note = note;
    }

    /// Whether the credentials screen has a key entry open.
    pub fn auth_keying(&self) -> bool {
        self.top() == Screen::Auth && self.auth_key.is_some()
    }

    /// The session the page is attached to (from the latest snapshot).
    pub fn session(&self) -> Option<Uuid> {
        self.session
    }

    /// Whether the share section is shown.
    fn share_active(&self) -> bool {
        self.share_enabled && self.session.is_some()
    }

    /// The portal rows that match the filter (the row text).
    fn visible_portal(&self) -> Vec<&PortalLine> {
        let needle = self.filter.to_lowercase();
        self.portal
            .iter()
            .filter(|row| needle.is_empty() || row.text.to_lowercase().contains(&needle))
            .collect()
    }

    /// The filter text (Sessions and Models).
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The selected row index of the open list.
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// Takes the latest snapshot's sessions, settings, models and agents,
    /// then keeps the selection inside the list it now shows.
    pub fn observe(&mut self, snapshot: &ViewModelSnapshot) {
        self.last_error = snapshot.last_error.clone();
        self.all = snapshot
            .sessions
            .iter()
            .zip(snapshot.session_rows.iter())
            .map(|(meta, text)| SessionRow {
                id: meta.id,
                title: meta.title.clone(),
                text: text.clone(),
            })
            .collect();
        self.session = snapshot.session_id;
        self.settings = settings_rows(snapshot);
        self.set_theme_row();
        self.mode_label = snapshot.approval_mode_label.clone();
        self.sandbox_label = snapshot.sandbox.clone();
        self.effort_label = snapshot.effort_label.clone();
        self.auth = auth_rows(snapshot);
        self.auth_progress = snapshot.oauth.clone();
        self.models = model_rows(snapshot);
        self.agents = snapshot.agents.clone();
        self.automations = snapshot.automations.clone();
        self.last_note = snapshot.last_note.clone();
        self.personas = snapshot.personas.clone();
        // The core's rows (text and summary); the branch comes from the session.
        self.tasks = snapshot
            .task_rows
            .iter()
            .map(|row| TaskRow {
                id: row.session_id,
                branch: snapshot
                    .sessions
                    .iter()
                    .find(|meta| meta.id == row.session_id)
                    .and_then(|meta| meta.task_branch.clone())
                    .unwrap_or_default(),
                text: row.text.clone(),
                summary: row.summary.clone(),
            })
            .collect();
        if let Some(id) = &self.agent_id {
            if let Some(row) = self.agents.iter().find(|a| &a.id == id) {
                self.agent_row = Some(row.clone());
            }
        }
        self.clamp();
    }

    /// The rows that match the sessions filter: a case-insensitive substring
    /// of the row text.
    fn visible(&self) -> Vec<&SessionRow> {
        let needle = self.filter.to_lowercase();
        self.all
            .iter()
            .filter(|row| needle.is_empty() || row.text.to_lowercase().contains(&needle))
            .collect()
    }

    /// The model rows that match the filter (the model name).
    fn visible_models(&self) -> Vec<&ModelRow> {
        let needle = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|row| needle.is_empty() || row.model.to_lowercase().contains(&needle))
            .collect()
    }

    /// The automation rows that match the filter (the row text).
    fn visible_automations(&self) -> Vec<(&AutomationWire, String)> {
        let needle = self.filter.to_lowercase();
        self.automations
            .iter()
            .map(|a| (a, automation_row(a)))
            .filter(|(_, text)| needle.is_empty() || text.to_lowercase().contains(&needle))
            .collect()
    }

    /// The task rows that match the filter (the row text).
    fn visible_tasks(&self) -> Vec<&TaskRow> {
        let needle = self.filter.to_lowercase();
        self.tasks
            .iter()
            .filter(|row| needle.is_empty() || row.text.to_lowercase().contains(&needle))
            .collect()
    }

    /// Whether an inline form is open on the automations or tasks screen.
    pub fn form_open(&self) -> bool {
        self.form.is_some() && matches!(self.top(), Screen::Automations | Screen::Tasks)
    }

    /// Whether the Tasks screen is on top.
    pub fn on_tasks(&self) -> bool {
        self.top() == Screen::Tasks
    }

    /// The task refresh timer. True at most once every [`TASK_REFRESH_EVERY`]
    /// while the Tasks screen is on top, and never on another screen. A true
    /// answer is the request: the host sends `refresh_task_statuses`, and this
    /// records the time and the batch it covers.
    pub fn task_refresh_due(&mut self, now: Instant) -> bool {
        if !self.on_tasks() {
            return false;
        }
        if let Some(last) = self.task_refresh_at {
            if now < last + TASK_REFRESH_EVERY {
                return false;
            }
        }
        self.task_refresh_at = Some(now);
        self.task_covered = (self.task_covered + TASK_STATUS_REFRESH_BATCH).min(self.tasks.len());
        true
    }

    /// The task Enter would attach (Tasks on top only).
    pub fn selected_task(&self) -> Option<Uuid> {
        if self.top() != Screen::Tasks {
            return None;
        }
        self.visible_tasks().get(self.selected).map(|row| row.id)
    }

    /// The status request for the selected task row (Tasks on top only).
    fn status_for_selected(&self) -> Option<UiAction> {
        self.selected_task().map(UiAction::TaskStatusOf)
    }

    /// True on the screens with a filter line.
    fn filtering(&self) -> bool {
        matches!(
            self.top(),
            Screen::Sessions
                | Screen::Models
                | Screen::Portal
                | Screen::Automations
                | Screen::Tasks
                | Screen::Picker
        )
    }

    /// The session Enter would attach (Sessions on top only).
    pub fn selected_session(&self) -> Option<Uuid> {
        if self.top() != Screen::Sessions {
            return None;
        }
        self.visible().get(self.selected).map(|row| row.id)
    }

    /// The model Enter would switch to (Models on top only).
    pub fn selected_model(&self) -> Option<String> {
        if self.top() != Screen::Models {
            return None;
        }
        self.visible_models()
            .get(self.selected)
            .filter(|row| row.available)
            .map(|row| row.model.clone())
    }

    /// Shift+S (terminal) and the save control (web) on the Models screen:
    /// save the highlighted row's model and provider as the server's default
    /// for future sessions. None when the row is unavailable or has no
    /// provider (the catalog has not arrived). The current session keeps its
    /// model either way.
    pub fn save_default_action(&self) -> Option<UiAction> {
        if self.top() != Screen::Models {
            return None;
        }
        self.visible_models()
            .get(self.selected)
            .filter(|row| row.available && !row.provider.is_empty())
            .map(|row| UiAction::SaveDefaultModel {
                model: row.model.clone(),
                provider: row.provider.clone(),
            })
    }

    /// The action Enter would send for the selected settings row.
    pub fn enter_setting(&self) -> Option<UiAction> {
        if self.top() != Screen::Settings {
            return None;
        }
        self.settings.get(self.selected).and_then(|row| row.action.clone())
    }

    /// The agent Ctrl+A opens: the first running one, else the first.
    pub fn first_agent(&self) -> Option<String> {
        self.agents.iter().find(|a| a.running).or(self.agents.first()).map(|a| a.id.clone())
    }

    /// Whether the open agent still takes a steer or a kill.
    pub fn agent_live(&self) -> bool {
        self.top() == Screen::Agent && self.agent_row.as_ref().is_some_and(|r| r.running)
    }

    /// Whether a confirmation waits for y or n.
    pub fn confirming(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the composer's command suggestions show (Chat, not hidden).
    pub fn suggestions_shown(&self) -> bool {
        self.top() == Screen::Chat && !self.slash_hidden
    }

    /// The command names the composer's text suggests now. The host sets
    /// this before every key, and the selection stays inside it.
    pub fn set_slash(&mut self, items: Vec<String>) {
        self.slash_items = items;
        self.slash_sel = self.slash_sel.min(self.slash_items.len().saturating_sub(1));
    }

    /// A typed character or a deletion changes the text: the suggestions
    /// show again with the first one selected. Returns true when that
    /// changed the state.
    pub fn reopen(&mut self) -> bool {
        let changed = self.slash_hidden || self.slash_sel != 0;
        self.slash_hidden = false;
        self.slash_sel = 0;
        changed
    }

    /// The first-run check, once per process: the first snapshot that
    /// carries provider rows (the driver requests them on connect). When
    /// nothing is signed in and Chat is showing, the credentials screen
    /// opens once. Returns the core calls to send.
    /// Moves to the `/auth <provider>` row once it is listed, and starts key
    /// entry or sign-in for it. None while the rows have not arrived. An id
    /// the list does not have is dropped (the screen stays on the list).
    fn resolve_auth_target(&mut self) -> Option<Vec<UiAction>> {
        let provider = self.auth_target.clone()?;
        if self.auth.is_empty() {
            return None;
        }
        self.auth_target = None;
        let index = self.auth.iter().position(|row| row.id == provider)?;
        self.selected = index;
        Some(self.apply(NavStep::Activate))
    }

    pub fn first_run(&mut self, snapshot: &ViewModelSnapshot) -> Vec<UiAction> {
        // `/auth <provider>` waits for the provider's row to arrive.
        if let Some(actions) = self.resolve_auth_target() {
            return actions;
        }
        // The session's provider comes with its snapshot, which can arrive
        // after the provider rows: wait for it, or a mock session would be
        // judged as having no provider at all.
        if self.first_run_done || snapshot.providers.is_empty() || snapshot.provider.is_empty() {
            return Vec::new();
        }
        self.first_run_done = true;
        let none = snapshot.providers.iter().all(|p| !auth_is_signed_in(&p.status));
        // The session's own provider decides too: a provider with no
        // credentials row (the mock, a local one) needs no sign-in, so the
        // screen must not cover a session that already runs on it.
        let active_needs_no_sign_in = !snapshot.provider.is_empty()
            && !snapshot.providers.iter().any(|p| p.id == snapshot.provider);
        // Only on a blank session: a session with history or a run in
        // progress (a share link, a resumed session, a local provider that
        // needs no sign-in) must not be covered by the credentials screen.
        let blank = snapshot.history.is_empty() && !snapshot.running;
        if none && blank && !active_needs_no_sign_in && self.top() == Screen::Chat {
            self.apply(NavStep::Open(Screen::Auth))
        } else {
            Vec::new()
        }
    }

    fn list_len(&self) -> usize {
        match self.top() {
            Screen::Chat | Screen::Help | Screen::Agent | Screen::SignIn => 0,
            Screen::Auth => self.auth.len(),
            Screen::Portal => self.visible_portal().len(),
            Screen::Sessions => self.visible().len(),
            Screen::Models => self.visible_models().len(),
            Screen::Settings => self.settings.len(),
            Screen::Automations => self.visible_automations().len(),
            Screen::Tasks => self.visible_tasks().len(),
            Screen::Picker => self.visible_picker().len(),
        }
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.list_len().saturating_sub(1));
    }

    fn push(&mut self, screen: Screen) {
        self.stack.push(screen);
        self.filter.clear();
        self.selected = 0;
        self.pending = None;
        self.form = None;
        self.form_error = None;
    }

    fn leave_to_chat(&mut self) {
        self.stack.clear();
        self.picker = None;
        self.filter.clear();
        self.selected = 0;
        self.pending = None;
        self.agent_id = None;
        self.agent_row = None;
        self.auth_key = None;
        self.form = None;
        self.form_error = None;
    }

    /// Submits the open form: the automation (checked here against the
    /// shared interval floor; the server stays the authority) or the task.
    /// A refused form stays open with its message.
    fn submit_form(&mut self) -> Vec<UiAction> {
        let Some(form) = self.form.clone() else {
            return Vec::new();
        };
        match form.kind {
            FormKind::Automation => {
                let name = form.values[0].trim().to_string();
                let prompt = form.values[1].trim().to_string();
                let interval = form.values[2].trim().to_string();
                if name.is_empty() || prompt.is_empty() || interval.is_empty() {
                    self.form_error = Some(AUTOMATION_FIELDS_NEEDED.to_string());
                    return Vec::new();
                }
                if let Some(error) = automation_interval_error(&interval) {
                    self.form_error = Some(error);
                    return Vec::new();
                }
                let Ok(interval_minutes) = interval.parse::<u32>() else {
                    return Vec::new();
                };
                self.form = None;
                self.form_error = None;
                vec![
                    UiAction::AddAutomation { name, prompt, interval_minutes },
                    UiAction::ListAutomations,
                ]
            }
            FormKind::Task => {
                let title = form.values[0].trim().to_string();
                if title.is_empty() {
                    self.form_error = Some(TASK_TITLE_NEEDED.to_string());
                    return Vec::new();
                }
                self.leave_to_chat();
                vec![UiAction::CreateTask(title)]
            }
        }
    }

    fn complete(&mut self, name: &str) {
        self.completion = (self.completion.0 + 1, slash::complete_text(name));
    }

    /// Replaces the composer text (recall, or an empty composer after a
    /// queue). The view applies it once per call.
    pub fn set_text(&mut self, text: &str) {
        self.completion = (self.completion.0 + 1, text.to_string());
    }

    /// Performs one step and returns the core calls it implies. The host
    /// sends those and then sends the view its new [`NavView`].
    pub fn apply(&mut self, step: NavStep) -> Vec<UiAction> {
        match step {
            NavStep::Open(screen) => {
                if self.top() == screen || screen == Screen::Chat || screen == Screen::Agent {
                    return Vec::new();
                }
                self.push(screen);
                match screen {
                    Screen::Sessions => return vec![UiAction::ListSessions],
                    Screen::Models => {
                        // Preselect the current model (the first row when none).
                        self.selected = self.models.iter().position(|r| r.current).unwrap_or(0);
                        return vec![UiAction::ListModels];
                    }
                    Screen::Settings if self.share_active() => return vec![UiAction::ListInvites],
                    Screen::Help => self.help_scroll = 0,
                    Screen::Auth => return vec![UiAction::ListAuth],
                    Screen::Automations => return vec![UiAction::ListAutomations],
                    // The list and, when a row is selected, its status.
                    Screen::Tasks => {
                        let mut actions =
                            vec![UiAction::ListSessions, UiAction::RefreshTaskStatuses];
                        actions.extend(self.status_for_selected());
                        self.task_refresh_at = Some(Instant::now());
                        self.task_covered = TASK_STATUS_REFRESH_BATCH.min(self.tasks.len());
                        return actions;
                    }
                    _ => {}
                }
            }
            NavStep::OpenAgent(id) => {
                if self.top() == Screen::Chat {
                    self.agent_row = self.agents.iter().find(|a| a.id == id).cloned();
                    self.agent_id = Some(id);
                    self.push(Screen::Agent);
                }
            }
            NavStep::Back => {
                if self.top() == Screen::Agent {
                    self.agent_id = None;
                    self.agent_row = None;
                }
                self.auth_key = None;
                self.form = None;
                self.form_error = None;
                if self.top() == Screen::Picker {
                    self.picker = None;
                }
                self.stack.pop();
                self.filter.clear();
                self.selected = 0;
                self.pending = None;
            }
            NavStep::ClearFilter if self.filtering() => {
                self.filter.clear();
                self.selected = 0;
            }
            NavStep::Filter(c) if self.filtering() => {
                self.filter.push(c);
                self.selected = 0;
                return self.status_for_selected().into_iter().collect();
            }
            NavStep::FilterBack if self.filtering() => {
                self.filter.pop();
                self.selected = 0;
                return self.status_for_selected().into_iter().collect();
            }
            NavStep::Move(delta) => {
                let last = self.list_len().saturating_sub(1) as isize;
                self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
                let mut actions: Vec<UiAction> = self.status_for_selected().into_iter().collect();
                // Past the rows the refreshes covered so far: ask for the next batch.
                if self.top() == Screen::Tasks && self.selected >= self.task_covered {
                    actions.push(UiAction::RefreshTaskStatuses);
                    self.task_covered =
                        (self.task_covered + TASK_STATUS_REFRESH_BATCH).min(self.tasks.len());
                }
                return actions;
            }
            NavStep::OpenForm(kind) => {
                let fits = matches!(
                    (kind, self.top()),
                    (FormKind::Automation, Screen::Automations) | (FormKind::Task, Screen::Tasks)
                );
                if fits {
                    self.form = Some(Form::new(kind));
                    self.form_error = None;
                }
            }
            NavStep::FormChar(c) => {
                if let Some(form) = self.form.as_mut() {
                    form.values[form.active].push(c);
                    self.form_error = None;
                }
            }
            NavStep::FormBack => {
                if let Some(form) = self.form.as_mut() {
                    form.values[form.active].pop();
                    self.form_error = None;
                }
            }
            NavStep::FormNext => {
                if let Some(form) = self.form.as_mut() {
                    form.active = (form.active + 1).min(form.values.len() - 1);
                }
            }
            NavStep::FormEnter => {
                let last = self.form.as_ref().map(|f| f.active + 1 >= f.values.len());
                return match last {
                    Some(false) => self.apply(NavStep::FormNext),
                    Some(true) => self.submit_form(),
                    None => Vec::new(),
                };
            }
            NavStep::FormSubmit => return self.submit_form(),
            NavStep::FormCancel => {
                self.form = None;
                self.form_error = None;
            }
            NavStep::AskRemove(index) if self.top() == Screen::Automations => {
                let found = self.visible_automations().get(index).map(|(a, _)| a.name.clone());
                if let Some(name) = found {
                    self.selected = index;
                    self.pending = Some(Pending {
                        text: delete_confirm_text(&name),
                        actions: vec![UiAction::RemoveAutomation(name), UiAction::ListAutomations],
                    });
                }
            }
            NavStep::AskMerge(index) if self.top() == Screen::Tasks => {
                if let Some(row) = self.visible_tasks().get(index).map(|r| (*r).clone()) {
                    self.selected = index;
                    self.pending = Some(Pending {
                        text: merge_confirm_text(&row.branch),
                        actions: vec![UiAction::MergeTask(row.id), UiAction::ListSessions],
                    });
                }
            }
            NavStep::OpenPortal(index) if self.top() == Screen::Portal => {
                if let Some(url) =
                    self.visible_portal().get(index).and_then(|l| portal::open_target(l))
                {
                    return vec![UiAction::OpenUrl(url)];
                }
            }
            NavStep::Attach(id) => {
                self.leave_to_chat();
                return vec![UiAction::Attach(id)];
            }
            NavStep::NewSession => {
                self.leave_to_chat();
                return vec![UiAction::NewSession];
            }
            NavStep::PickModel(model) => {
                self.leave_to_chat();
                return vec![UiAction::SelectModel(model)];
            }
            NavStep::AskDelete(index) if self.top() == Screen::Sessions => {
                if let Some(row) = self.visible().get(index).map(|r| (*r).clone()) {
                    self.selected = index;
                    self.pending = Some(Pending {
                        text: delete_confirm_text(&row.title),
                        actions: vec![UiAction::DeleteSession(row.id), UiAction::ListSessions],
                    });
                }
            }
            NavStep::AskKill if self.agent_live() => {
                if let Some(row) = self.agent_row.clone() {
                    self.pending = Some(Pending {
                        text: kill_confirm_text(&row.label),
                        actions: vec![UiAction::KillAgent(row.id)],
                    });
                }
            }
            NavStep::AskAuthRemove(index) if self.top() == Screen::Auth => {
                if let Some(row) = self.auth.get(index).filter(|r| r.signed_in()).cloned() {
                    self.selected = index;
                    self.pending = Some(Pending {
                        text: credential_remove_confirm_text(&row.label),
                        actions: vec![UiAction::AuthRemove(row.id)],
                    });
                }
            }
            NavStep::Activate if self.top() == Screen::Auth && self.auth_key.is_none() => {
                if let Some(row) = self.auth.get(self.selected).cloned() {
                    if row.oauth_capable {
                        return vec![UiAction::AuthStart(row.id)];
                    }
                    self.auth_key = Some(row.id);
                }
            }
            NavStep::AuthFor(provider) => {
                self.auth_target = Some(provider);
                let mut actions = self.apply(NavStep::Open(Screen::Auth));
                if let Some(resolved) = self.resolve_auth_target() {
                    actions.extend(resolved);
                }
                return actions;
            }
            NavStep::OpenToolList(allow) => {
                let label = if allow { SETTING_ALLOW_LABEL } else { SETTING_DENY_LABEL };
                let actions = self.apply(NavStep::Open(Screen::Settings));
                if let Some(index) = self.settings.iter().position(|row| row.label == label) {
                    self.selected = index;
                }
                return actions;
            }
            NavStep::ActivateRow(index) if self.top() == Screen::Auth => {
                self.selected = index.min(self.auth.len().saturating_sub(1));
                return self.apply(NavStep::Activate);
            }
            NavStep::AuthCancelKey | NavStep::AuthKeyDone => self.auth_key = None,
            NavStep::OpenPicker(kind) => {
                self.push(Screen::Picker);
                self.picker = Some(kind);
                let now = self.picker_now();
                self.selected = picker_rows(kind, &now).iter().position(|r| r.current).unwrap_or(0);
                if kind == PickerKind::Agent {
                    return vec![UiAction::ListPersonas];
                }
            }
            NavStep::PickRow(index) if self.top() == Screen::Picker => {
                let rows = self.visible_picker();
                let picked = rows.get(index).filter(|r| r.enabled).and_then(|r| r.action.clone());
                if let Some(action) = picked {
                    // The picker closes first: the screen beneath it (Chat or
                    // Settings) is on top when the choice takes effect.
                    self.stack.pop();
                    self.picker = None;
                    self.filter.clear();
                    self.selected = 0;
                    return match action {
                        UiAction::Nav(step) => self.apply(step),
                        other => vec![other],
                    };
                }
            }
            NavStep::SetTheme(choice) => self.change_theme(choice, true),
            NavStep::AskSettingRemove(index) if self.top() == Screen::Settings => {
                let found = self
                    .settings
                    .get(index)
                    .and_then(|row| row.remove.clone().map(|action| (row.line(), action)));
                if let Some((line, action)) = found {
                    self.selected = index;
                    self.pending =
                        Some(Pending { text: remove_confirm_text(&line), actions: vec![action] });
                }
            }
            NavStep::HelpCycle(delta) if self.top() == Screen::Help => {
                self.help_tab = self.help_tab.step(delta);
                self.help_scroll = 0;
            }
            NavStep::HelpTo(tab) if self.top() == Screen::Help => {
                self.help_tab = tab;
                self.help_scroll = 0;
            }
            NavStep::HelpScroll(delta) if self.top() == Screen::Help => {
                let next = self.help_scroll as isize + delta;
                self.help_scroll = next.clamp(0, self.help_max_scroll() as isize) as usize;
            }
            NavStep::HelpEdge(top) if self.top() == Screen::Help => {
                self.help_scroll = if top { 0 } else { self.help_max_scroll() };
            }
            NavStep::Confirm => {
                return self.pending.take().map(|p| p.actions).unwrap_or_default();
            }
            NavStep::Cancel => self.pending = None,
            NavStep::SlashMove(delta) => {
                let len = self.slash_items.len() as isize;
                if len > 0 {
                    self.slash_sel = (self.slash_sel as isize + delta).rem_euclid(len) as usize;
                }
            }
            NavStep::SlashHide => self.slash_hidden = true,
            NavStep::CompleteSelected => {
                if let Some(name) = self.slash_items.get(self.slash_sel).cloned() {
                    self.complete(&name);
                }
            }
            NavStep::Complete(name) => self.complete(&name),
            _ => {}
        }
        Vec::new()
    }

    /// The picker rows that match the filter (the label), as the view shows
    /// them. Empty when no picker is open.
    fn visible_picker(&self) -> Vec<PickerRow> {
        let Some(kind) = self.picker else {
            return Vec::new();
        };
        let needle = self.filter.to_lowercase();
        picker_rows(kind, &self.picker_now())
            .into_iter()
            .filter(|row| needle.is_empty() || row.label.to_lowercase().contains(&needle))
            .collect()
    }

    /// The values the pickers mark as current.
    fn picker_now(&self) -> PickerNow {
        PickerNow {
            mode: self.mode_label.clone(),
            sandbox: self.sandbox_label.clone(),
            effort: self.effort_label.clone(),
            theme: self.theme,
            personas: self.personas.clone(),
        }
    }

    /// Whether `d` may ask to remove the selected Settings row.
    pub fn settings_removable(&self) -> bool {
        self.top() == Screen::Settings
            && self.settings.get(self.selected).is_some_and(|row| row.remove.is_some())
    }

    /// The last help scroll position that still shows the tab's last row.
    fn help_max_scroll(&self) -> usize {
        help::tab_lines(self.help_tab).len().saturating_sub(HELP_VISIBLE)
    }

    /// Records a theme change: the view applies it once (see [`ThemeChange`]),
    /// the Settings row and the picker's marker follow it.
    fn change_theme(&mut self, choice: Choice, remember: bool) {
        self.theme = choice;
        self.theme_change = ThemeChange { seq: self.theme_change.seq + 1, choice, remember };
        self.set_theme_row();
    }

    /// Shows the theme in use at startup, without saving it. The view draws
    /// it on its next change (the first one arrives with the first view).
    pub fn restore_theme(&mut self, choice: Choice) {
        self.change_theme(choice, false);
    }

    fn set_theme_row(&mut self) {
        for row in self.settings.iter_mut().filter(|row| row.label == SETTING_THEME_LABEL) {
            row.value = self.theme.name().to_string();
        }
    }

    /// The state the view renders.
    pub fn view(&self) -> NavView {
        let agent_open = self.top() == Screen::Agent;
        NavView {
            screen: self.top(),
            filter: self.filter.clone(),
            selected: self.selected,
            total: self.all.len(),
            sessions_note: if self.all.is_empty() { self.last_error.clone() } else { None },
            sessions: self.visible().into_iter().cloned().collect(),
            settings: self.settings.clone(),
            models: self.visible_models().into_iter().cloned().collect(),
            models_total: self.models.len(),
            save_default: self.save_default_action(),
            agents: self.agents.clone(),
            agent: if agent_open { self.agent_row.clone() } else { None },
            agent_live: self.agent_live(),
            confirm: self.pending.as_ref().map(|p| p.text.clone()),
            slash_hidden: self.slash_hidden,
            slash_selected: self.slash_sel,
            tool_panel: None,
            completion: self.completion.clone(),
            scroll_seq: 0,
            scroll_offset: 0,
            new_below: false,
            portal: self.visible_portal().into_iter().cloned().collect(),
            portal_total: self.portal.len(),
            signin_error: self.signin_error.clone(),
            share: self.share_active().then(|| ShareView {
                invites: self.invites.clone(),
                created: self.created.clone(),
                note: self.share_note.clone(),
            }),
            auth: self.auth.clone(),
            auth_key: self.auth_key.clone(),
            auth_progress: self.auth_progress.clone(),
            automations: self
                .visible_automations()
                .into_iter()
                .map(|(a, text)| AutomationRow { name: a.name.clone(), text })
                .collect(),
            automations_total: self.automations.len(),
            tasks: self.visible_tasks().into_iter().cloned().collect(),
            tasks_total: self.tasks.len(),
            note: self.last_note.clone(),
            picker_title: self
                .picker
                .map(|kind| picker_title(kind).to_string())
                .unwrap_or_default(),
            picker_note: if self.top() == Screen::Picker && self.picker == Some(PickerKind::Agent) {
                PERSONA_CLEAR_NOTE.to_string()
            } else {
                String::new()
            },
            picker: if self.top() == Screen::Picker { self.visible_picker() } else { Vec::new() },
            theme: self.theme,
            theme_change: self.theme_change,
            help_tab: self.help_tab,
            help_rows: help::tab_lines(self.help_tab),
            help_scroll: self.help_scroll.min(self.help_max_scroll()),
            form: self.form.as_ref().map(|f| FormView {
                kind: f.kind,
                labels: f.kind.labels(),
                values: f.values.clone(),
                active: f.active,
                error: self.form_error.clone(),
            }),
        }
    }
}

fn row(label: &str, value: String, action: Option<UiAction>) -> SettingRow {
    SettingRow { label: label.to_string(), value, action, remove: None }
}

/// A removable row: its shared label and value, and the action a confirmed
/// removal sends.
fn removable(label: &str, value: String, remove: UiAction) -> SettingRow {
    SettingRow { label: label.to_string(), value, action: None, remove: Some(remove) }
}

/// The model picker's rows. The server's catalog (`models`, from
/// `models_req`) when it has arrived: one row per entry, the core's text,
/// unavailable ones dim. Until then, the router-defaults rows: the current
/// model, the recommended cheap and strong models and the fallbacks, each
/// model once.
pub fn model_rows(s: &ViewModelSnapshot) -> Vec<ModelRow> {
    if !s.models.is_empty() {
        return s
            .models
            .iter()
            .map(|m| {
                let current = m.current;
                let text = if current {
                    format!("{} ({MODEL_ROLE_CURRENT})", m.text)
                } else {
                    m.text.clone()
                };
                ModelRow {
                    model: m.id.clone(),
                    provider: m.provider.clone(),
                    text,
                    available: m.available,
                    current,
                }
            })
            .collect();
    }
    let mut rows: Vec<ModelRow> = Vec::new();
    let mut push = |model: &str, role: &str| {
        if !model.is_empty() && !rows.iter().any(|r| r.model == model) {
            rows.push(ModelRow {
                model: model.to_string(),
                provider: if model == s.model { s.provider.clone() } else { String::new() },
                text: format!("{model} ({role})"),
                available: true,
                current: model == s.model,
            });
        }
    };
    push(&s.model, MODEL_ROLE_CURRENT);
    if let Some(d) = &s.router_defaults {
        if let Some(m) = &d.recommended_cheap {
            push(m, MODEL_ROLE_CHEAP);
        }
        if let Some(m) = &d.recommended_strong {
            push(m, MODEL_ROLE_STRONG);
        }
        for m in &d.cheap_fallback {
            push(m, MODEL_ROLE_FALLBACK);
        }
        for m in &d.strong_fallback {
            push(m, MODEL_ROLE_FALLBACK);
        }
    }
    rows
}

/// The settings rows, in screen order. Values and labels come from the
/// core's snapshot. Enter on approval mode, effort, sandbox, theme and agent
/// opens its picker (the old terminal's bare commands opened the same menus);
/// phase cycles, compact runs, auth opens the credentials screen. Permission
/// rules and the allow and deny entries follow, each removable with `d`.
pub fn settings_rows(s: &ViewModelSnapshot) -> Vec<SettingRow> {
    let effort = if s.effort_label.is_empty() {
        EFFORT_UNSET_LABEL.to_string()
    } else {
        s.effort_label.clone()
    };
    let mut rows = vec![
        row(
            SETTING_MODEL_LABEL,
            s.model.clone(),
            Some(UiAction::Nav(NavStep::Open(Screen::Models))),
        ),
        row(SETTING_PROVIDER_LABEL, s.provider.clone(), None),
        row(
            SETTING_APPROVAL_LABEL,
            s.approval_mode_label.clone(),
            Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Mode))),
        ),
        row(
            SETTING_PHASE_LABEL,
            s.phase_label.clone(),
            Some(UiAction::SetPhase(phase_next(s.phase))),
        ),
        row(
            SETTING_EFFORT_LABEL,
            effort,
            Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Effort))),
        ),
        row(
            SETTING_SANDBOX_LABEL,
            s.sandbox.clone(),
            Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Sandbox))),
        ),
        row(SETTING_CONTEXT_LABEL, s.context_label.clone(), None),
        row(SETTING_COMPACT_LABEL, String::new(), Some(UiAction::Compact)),
        row(SETTING_AUTH_LABEL, String::new(), Some(UiAction::Nav(NavStep::Open(Screen::Auth)))),
        row(
            SETTING_THEME_LABEL,
            Choice::default().name().to_string(),
            Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Theme))),
        ),
        row(
            SETTING_AGENT_LABEL,
            String::new(),
            Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Agent))),
        ),
    ];
    if s.rules.is_empty() {
        rows.push(row(SETTING_RULES_LABEL, RULES_NONE_LABEL.to_string(), None));
    } else {
        rows.extend(s.rules.iter().enumerate().map(|(index, rule)| {
            removable(SETTING_RULES_LABEL, rule.clone(), UiAction::RemoveRule(index))
        }));
    }
    for (label, list, allow) in
        [(SETTING_ALLOW_LABEL, &s.allow_tools, true), (SETTING_DENY_LABEL, &s.deny_tools, false)]
    {
        if list.is_empty() {
            rows.push(row(label, RULES_NONE_LABEL.to_string(), None));
        } else {
            rows.extend(list.iter().map(|tool| {
                removable(
                    label,
                    tool.clone(),
                    UiAction::RemoveToolRule { allow, tool: tool.clone() },
                )
            }));
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use modron_mobile_core::{ApprovalModeWire, SessionMetaWire};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn snapshot() -> ViewModelSnapshot {
        let meta = |n: u128, title: &str| SessionMetaWire {
            id: id(n),
            title: title.into(),
            created_at: String::new(),
            updated_at: String::new(),
            message_count: 1,
            task_branch: None,
            task_state: None,
            working_dir: None,
        };
        let sessions = vec![meta(1, "Router fix"), meta(2, "docs"), meta(3, "router tests")];
        ViewModelSnapshot {
            session_rows: sessions.iter().map(|m| m.title.clone()).collect(),
            sessions,
            model: "claude-opus-5".into(),
            approval_mode: Some(ApprovalModeWire::Ask),
            approval_mode_label: "ask".into(),
            ..Default::default()
        }
    }

    fn open_sessions(nav: &mut Nav, snap: &ViewModelSnapshot) {
        nav.observe(snap);
        assert_eq!(nav.apply(NavStep::Open(Screen::Sessions)), vec![UiAction::ListSessions]);
    }

    #[test]
    fn opening_sessions_refreshes_the_list_and_stacks_over_chat() {
        let mut nav = Nav::default();
        open_sessions(&mut nav, &snapshot());
        assert_eq!(nav.top(), Screen::Sessions);
        assert_eq!(nav.view().sessions.len(), 3);
        assert!(nav.apply(NavStep::Open(Screen::Sessions)).is_empty());
    }

    #[test]
    fn the_filter_is_a_case_insensitive_substring_of_the_row_text() {
        let mut nav = Nav::default();
        open_sessions(&mut nav, &snapshot());
        for c in "ROUTER".chars() {
            nav.apply(NavStep::Filter(c));
        }
        let texts: Vec<String> = nav.view().sessions.into_iter().map(|r| r.text).collect();
        assert_eq!(texts, vec!["Router fix", "router tests"]);
        nav.apply(NavStep::FilterBack);
        assert_eq!(nav.filter(), "ROUTE");
    }

    #[test]
    fn selection_moves_within_the_visible_rows_and_attaches_the_one_chosen() {
        let mut nav = Nav::default();
        open_sessions(&mut nav, &snapshot());
        nav.apply(NavStep::Move(5));
        assert_eq!(nav.selected_session(), Some(id(3)));
        nav.apply(NavStep::Move(-9));
        assert_eq!(nav.selected_session(), Some(id(1)));
        nav.apply(NavStep::Filter('d'));
        assert_eq!(nav.selected_session(), Some(id(2)));
    }

    #[test]
    fn attach_returns_to_chat_and_clears_the_filter() {
        let mut nav = Nav::default();
        open_sessions(&mut nav, &snapshot());
        nav.apply(NavStep::Filter('x'));
        let actions = nav.apply(NavStep::Attach(id(2)));
        assert_eq!(actions, vec![UiAction::Attach(id(2))]);
        assert_eq!(nav.top(), Screen::Chat);
        assert_eq!(nav.filter(), "");
    }

    #[test]
    fn new_session_goes_to_chat() {
        let mut nav = Nav::default();
        open_sessions(&mut nav, &snapshot());
        assert_eq!(nav.apply(NavStep::NewSession), vec![UiAction::NewSession]);
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn back_pops_one_screen_and_never_past_chat() {
        let mut nav = Nav::default();
        nav.observe(&snapshot());
        nav.apply(NavStep::Open(Screen::Settings));
        nav.apply(NavStep::Open(Screen::Sessions));
        nav.apply(NavStep::Back);
        assert_eq!(nav.top(), Screen::Settings);
        nav.apply(NavStep::Back);
        nav.apply(NavStep::Back);
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn settings_rows_carry_the_core_labels_and_open_their_pickers() {
        let rows = settings_rows(&snapshot());
        let lines: Vec<String> = rows.iter().map(SettingRow::line).collect();
        assert_eq!(lines[0], "model: claude-opus-5");
        assert_eq!(lines[2], "approval mode: ask");
        assert_eq!(lines[4], "effort: unset");
        assert_eq!(lines[9], "theme: system");
        assert_eq!(lines[lines.len() - 3..], ["rules: none", "allow: none", "deny: none"]);
        assert_eq!(rows[2].action, Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Mode))));
        assert_eq!(rows[4].action, Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Effort))));
        assert_eq!(rows[5].action, Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Sandbox))));
        assert_eq!(rows[7].action, Some(UiAction::Compact));
        assert_eq!(rows[9].action, Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Theme))));
        // The model row opens the picker (Packet C3f); provider stays read-only.
        assert_eq!(rows[0].action, Some(UiAction::Nav(NavStep::Open(Screen::Models))));
        assert_eq!(rows[1].action, None);
        assert!(rows.iter().all(|r| r.remove.is_none()), "no removable rows without rules");
    }

    #[test]
    fn enter_setting_follows_the_selected_row_on_settings_only() {
        let mut nav = Nav::default();
        nav.observe(&snapshot());
        assert_eq!(nav.enter_setting(), None);
        nav.apply(NavStep::Open(Screen::Settings));
        nav.apply(NavStep::Move(2));
        assert_eq!(nav.enter_setting(), Some(UiAction::Nav(NavStep::OpenPicker(PickerKind::Mode))));
    }
}

#[cfg(test)]
mod portal_tests {
    use super::*;
    use crate::chat::{route_key, Key, KeyFacts, Routed};
    use crate::portal::parse_sessions;
    use modron_mobile_core::view::ModelRowView;

    const ROWS: &str = r#"[
        {"id":"a","title":"fix router","host_label":"laptop","online":true,
         "updated_at":"2026-11-15T10:20:30Z","open_url":"/app/?url=ws%3A%2F%2Fh%2Fws%2Fa"},
        {"id":"b","title":"docs","host_label":"cloud","online":false,
         "updated_at":"2026-11-14T08:00:00Z","open_url":"/app/?url=ws%3A%2F%2Fh%2Fws%2Fb"},
        {"id":"c","title":"router tests","host_label":"laptop","online":true,
         "updated_at":"","open_url":"/app/?url=ws%3A%2F%2Fh%2Fws%2Fc"}
    ]"#;

    fn portal_nav() -> Nav {
        let mut nav = Nav::default();
        nav.set_root(Screen::Portal);
        nav.set_portal(&parse_sessions(ROWS).unwrap());
        nav
    }

    #[test]
    fn the_portal_list_filters_and_opens_only_online_rows() {
        let mut nav = portal_nav();
        assert!(nav.is_portal());
        assert_eq!(nav.view().portal.len(), 3);
        assert_eq!(nav.apply(NavStep::OpenPortal(1)), Vec::new());
        assert_eq!(
            nav.apply(NavStep::OpenPortal(0)),
            vec![UiAction::OpenUrl("/app/?url=ws%3A%2F%2Fh%2Fws%2Fa".into())]
        );
        for c in "ROUTER".chars() {
            nav.apply(NavStep::Filter(c));
        }
        let view = nav.view();
        assert_eq!(view.portal.len(), 2);
        assert_eq!(view.portal_total, 3);
        assert_eq!(
            nav.apply(NavStep::OpenPortal(1)),
            vec![UiAction::OpenUrl("/app/?url=ws%3A%2F%2Fh%2Fws%2Fc".into())]
        );
    }

    #[test]
    fn enter_opens_the_selected_portal_row_and_esc_clears_the_filter_first() {
        let facts = KeyFacts { screen: Screen::Portal, selected_index: 2, ..Default::default() };
        assert_eq!(
            route_key(Key::Enter, false, false, facts.clone(), ""),
            Routed::Nav(NavStep::OpenPortal(2))
        );
        let filtering = KeyFacts { filter_active: true, ..facts };
        assert_eq!(
            route_key(Key::Escape, false, false, filtering, ""),
            Routed::Nav(NavStep::ClearFilter)
        );
    }

    #[test]
    fn the_sign_in_screen_sends_keys_to_its_input_and_ignores_esc() {
        let mut nav = Nav::default();
        nav.set_root(Screen::SignIn);
        nav.set_signin_error(Some("wrong token".into()));
        let view = nav.view();
        assert!(view.is_portal());
        assert_eq!(view.screen, Screen::SignIn);
        assert_eq!(view.signin_error.as_deref(), Some("wrong token"));
        let facts = KeyFacts { screen: Screen::SignIn, ..Default::default() };
        assert_eq!(route_key(Key::Char('x'), false, false, facts.clone(), ""), Routed::Composer);
        assert_eq!(route_key(Key::Enter, false, false, facts.clone(), ""), Routed::Composer);
        assert_eq!(route_key(Key::Escape, false, false, facts, ""), Routed::Ignored);
    }

    #[test]
    fn the_share_section_needs_an_owner_and_a_session() {
        let mut nav = Nav::default();
        let snapshot =
            ViewModelSnapshot { session_id: Some(Uuid::from_u128(7)), ..Default::default() };
        nav.observe(&snapshot);
        assert!(nav.view().share.is_none());
        nav.set_share_enabled(true);
        nav.set_invites(vec![InviteLine {
            id: "i1".into(),
            text: "viewer \u{b7} expires x".into(),
        }]);
        assert_eq!(nav.apply(NavStep::Open(Screen::Settings)), vec![UiAction::ListInvites]);
        let share = nav.view().share.expect("share section");
        assert_eq!(share.invites.len(), 1);
        nav.set_share_enabled(false);
        assert!(nav.view().share.is_none());
    }

    #[test]
    fn the_model_picker_lists_the_catalog_and_skips_unavailable_rows() {
        let snapshot = ViewModelSnapshot {
            model: "claude-opus-5".into(),
            models: vec![
                ModelRowView {
                    id: "claude-opus-5".into(),
                    provider: "anthropic".into(),
                    text: "claude-opus-5 - anthropic".into(),
                    available: true,
                    current: true,
                },
                ModelRowView {
                    id: "gpt-x".into(),
                    provider: "openai".into(),
                    text: "gpt-x - openai".into(),
                    available: false,
                    current: false,
                },
            ],
            ..Default::default()
        };
        let rows = model_rows(&snapshot);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, "claude-opus-5 - anthropic (current)");
        assert!(!rows[1].available);
        let mut nav = Nav::default();
        nav.observe(&snapshot);
        nav.apply(NavStep::Open(Screen::Models));
        nav.apply(NavStep::Move(1));
        assert_eq!(nav.selected_model(), None);
        nav.apply(NavStep::Move(-1));
        assert_eq!(nav.selected_model().as_deref(), Some("claude-opus-5"));
    }

    #[test]
    fn the_model_picker_falls_back_to_the_router_defaults_while_the_catalog_is_empty() {
        let snapshot = ViewModelSnapshot { model: "claude-opus-5".into(), ..Default::default() };
        let rows = model_rows(&snapshot);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "claude-opus-5 (current)");
        assert!(rows[0].available);
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;
    use modron_protocol::AuthProviderWire;

    fn provider(id: &str, label: &str, status: &str, oauth: bool) -> AuthProviderWire {
        AuthProviderWire {
            id: id.into(),
            label: label.into(),
            status: status.into(),
            oauth_capable: oauth,
        }
    }

    fn snapshot(providers: Vec<AuthProviderWire>) -> ViewModelSnapshot {
        ViewModelSnapshot { providers, ..Default::default() }
    }

    fn auth_nav(snap: &ViewModelSnapshot) -> Nav {
        let mut nav = Nav::default();
        nav.observe(snap);
        nav.apply(NavStep::Open(Screen::Auth));
        nav
    }

    #[test]
    fn the_rows_show_the_core_label_and_status_verbatim() {
        let snap = snapshot(vec![
            provider("anthropic", "Anthropic", "subscription (oauth)", true),
            provider("openai", "OpenAI", "not configured", false),
        ]);
        let nav = auth_nav(&snap);
        let view = nav.view();
        assert_eq!(view.screen, Screen::Auth);
        let lines: Vec<String> = view.auth.iter().map(AuthRow::line).collect();
        assert_eq!(lines, vec!["Anthropic: subscription (oauth)", "OpenAI: not configured"]);
    }

    #[test]
    fn opening_the_screen_asks_for_the_provider_rows_and_settings_opens_it() {
        let snap = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        let mut nav = Nav::default();
        nav.observe(&snap);
        assert_eq!(nav.apply(NavStep::Open(Screen::Auth)), vec![UiAction::ListAuth]);
        let settings = settings_rows(&snap);
        let auth_row = settings.iter().find(|r| r.label == "auth").expect("auth row");
        assert_eq!(auth_row.action, Some(UiAction::Nav(NavStep::Open(Screen::Auth))));
    }

    #[test]
    fn enter_on_an_oauth_row_starts_the_sign_in_and_keeps_no_entry() {
        let snap = snapshot(vec![provider("anthropic", "Anthropic", "not configured", true)]);
        let mut nav = auth_nav(&snap);
        assert_eq!(nav.apply(NavStep::Activate), vec![UiAction::AuthStart("anthropic".into())]);
        assert_eq!(nav.view().auth_key, None);
    }

    #[test]
    fn enter_on_a_key_row_opens_the_entry_and_the_submit_or_escape_closes_it() {
        let snap = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        let mut nav = auth_nav(&snap);
        assert!(nav.apply(NavStep::Activate).is_empty());
        assert_eq!(nav.view().auth_key.as_deref(), Some("openai"));
        assert!(nav.auth_keying());
        // Enter again does not reopen or act while the entry is open.
        assert!(nav.apply(NavStep::Activate).is_empty());
        nav.apply(NavStep::AuthCancelKey);
        assert_eq!(nav.view().auth_key, None);
        nav.apply(NavStep::Activate);
        nav.apply(NavStep::AuthKeyDone);
        assert!(!nav.auth_keying());
    }

    #[test]
    fn a_tap_selects_the_row_then_acts_on_it() {
        let snap = snapshot(vec![
            provider("anthropic", "Anthropic", "not configured", true),
            provider("openai", "OpenAI", "not configured", false),
        ]);
        let mut nav = auth_nav(&snap);
        assert!(nav.apply(NavStep::ActivateRow(1)).is_empty());
        assert_eq!(nav.view().auth_key.as_deref(), Some("openai"));
        assert_eq!(nav.selected(), 1);
    }

    #[test]
    fn back_drops_an_open_entry_with_the_screen() {
        let snap = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        let mut nav = auth_nav(&snap);
        nav.apply(NavStep::Activate);
        nav.apply(NavStep::Back);
        assert_eq!(nav.top(), Screen::Chat);
        assert_eq!(nav.view().auth_key, None);
    }

    #[test]
    fn the_progress_block_carries_the_snapshot_fields() {
        let mut snap = snapshot(vec![provider("openai", "OpenAI", "not configured", true)]);
        snap.oauth = Some(OAuthProgressView {
            provider: "openai".into(),
            stage: "waiting".into(),
            message: "enter the code".into(),
            url: Some("https://auth.openai.com".into()),
            user_code: Some("ABCD-1234".into()),
        });
        let nav = auth_nav(&snap);
        let progress = nav.view().auth_progress.expect("progress");
        assert_eq!(progress.stage, "waiting");
        assert_eq!(progress.user_code.as_deref(), Some("ABCD-1234"));
    }

    #[test]
    fn auth_for_a_provider_waits_for_its_row_then_starts_key_entry_or_sign_in() {
        let mut nav = Nav::default();
        assert_eq!(nav.apply(NavStep::AuthFor("openai".into())), vec![UiAction::ListAuth]);
        assert_eq!(nav.top(), Screen::Auth);
        let rows = snapshot(vec![
            provider("anthropic", "Anthropic", "not configured", false),
            provider("openai", "OpenAI", "not configured", false),
        ]);
        nav.observe(&rows);
        assert!(nav.first_run(&rows).is_empty());
        assert_eq!(nav.selected(), 1, "the provider's row is selected");
        assert_eq!(nav.view().auth_key, Some("openai".into()), "key entry starts");
    }

    #[test]
    fn auth_for_an_oauth_provider_starts_sign_in_and_an_unknown_one_does_nothing() {
        let mut nav = Nav::default();
        nav.apply(NavStep::AuthFor("openai".into()));
        let rows = snapshot(vec![provider("openai", "OpenAI", "not configured", true)]);
        nav.observe(&rows);
        assert_eq!(nav.first_run(&rows), vec![UiAction::AuthStart("openai".into())]);

        let mut nav = Nav::default();
        nav.apply(NavStep::AuthFor("nope".into()));
        nav.observe(&rows);
        assert!(nav.first_run(&rows).is_empty());
        assert_eq!(nav.selected(), 0);
        assert_eq!(nav.view().auth_key, None);
    }

    #[test]
    fn first_run_asks_once_and_opens_the_screen_only_when_nothing_is_signed_in() {
        let mut nav = Nav::default();
        let empty = ViewModelSnapshot::default();
        assert!(nav.first_run(&empty).is_empty(), "rows not in yet");
        let mut none = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        none.provider = "openai".into();
        assert_eq!(nav.first_run(&none), vec![UiAction::ListAuth]);
        assert_eq!(nav.top(), Screen::Auth);
        assert!(nav.first_run(&none).is_empty());
    }

    #[test]
    fn first_run_waits_for_the_sessions_provider() {
        // The provider rows can arrive before the session's snapshot: the
        // check waits for the provider rather than judging an unknown one.
        let mut nav = Nav::default();
        let mut known = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        assert!(nav.first_run(&known).is_empty(), "provider not known yet");
        known.provider = "openai".into();
        assert_eq!(nav.first_run(&known), vec![UiAction::ListAuth]);
    }

    #[test]
    fn an_empty_sessions_list_shows_the_servers_refusal() {
        let mut nav = Nav::default();
        let refused = ViewModelSnapshot {
            last_error: Some(
                "this request is not available through a remote-control tunnel".into(),
            ),
            ..Default::default()
        };
        nav.observe(&refused);
        assert_eq!(
            nav.view().sessions_note.as_deref(),
            Some("this request is not available through a remote-control tunnel")
        );
    }

    #[test]
    fn first_run_stays_quiet_on_a_session_that_is_running() {
        let mut nav = Nav::default();
        let mut busy = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        busy.running = true;
        assert!(nav.first_run(&busy).is_empty());
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn first_run_stays_quiet_when_the_session_runs_on_a_provider_without_rows() {
        // The mock (and any local provider) has no credentials row: a blank
        // session on it is ready to use, so the screen must not open.
        let mut nav = Nav::default();
        let mut mock = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        mock.provider = "mock".into();
        assert!(nav.first_run(&mock).is_empty());
        assert_eq!(nav.top(), Screen::Chat);
        // A provider that has a row and no credentials still gets the screen.
        let mut unsigned = snapshot(vec![provider("openai", "OpenAI", "not configured", false)]);
        unsigned.provider = "openai".into();
        let mut nav = Nav::default();
        assert_eq!(nav.first_run(&unsigned), vec![UiAction::ListAuth]);
    }

    #[test]
    fn first_run_stays_quiet_when_a_provider_is_signed_in() {
        let mut nav = Nav::default();
        let signed = snapshot(vec![
            provider("openai", "OpenAI", "not configured", false),
            provider("anthropic", "Anthropic", "api key", false),
        ]);
        assert!(nav.first_run(&signed).is_empty());
        assert_eq!(nav.top(), Screen::Chat);
    }
}

#[cfg(test)]
mod model_mark_tests {
    use super::*;
    use modron_mobile_core::view::model_row;
    use modron_protocol::ModelWire;

    /// A catalog row as the core builds it: `current` is the session's model.
    fn model(id: &str, available: bool, current: &str) -> modron_mobile_core::view::ModelRowView {
        let wire = ModelWire {
            id: id.into(),
            provider: "anthropic".into(),
            label: id.into(),
            context_window: None,
            available,
            tier: None,
        };
        model_row(&wire, current, "")
    }

    #[test]
    fn the_current_model_is_marked_and_preselected_when_the_picker_opens() {
        let snap = ViewModelSnapshot {
            model: "beta".into(),
            models: vec![
                model("alpha", true, "beta"),
                model("beta", true, "beta"),
                model("gamma", false, "beta"),
            ],
            ..Default::default()
        };
        let mut nav = Nav::default();
        nav.observe(&snap);
        nav.apply(NavStep::Open(Screen::Models));
        let view = nav.view();
        assert_eq!(view.selected, 1);
        assert!(view.models[1].text.ends_with("(current)"), "{}", view.models[1].text);
        assert!(!view.models[0].text.contains("current"));
        assert!(!view.models[2].available, "gamma stays dim");
    }

    #[test]
    fn the_picker_starts_on_the_first_row_when_no_current_model_is_listed() {
        let snap = ViewModelSnapshot {
            model: "zeta".into(),
            models: vec![model("alpha", true, "zeta"), model("beta", true, "zeta")],
            ..Default::default()
        };
        let mut nav = Nav::default();
        nav.observe(&snap);
        nav.apply(NavStep::Open(Screen::Models));
        assert_eq!(nav.view().selected, 0);
    }
}

#[cfg(test)]
mod list_screen_tests {
    use super::*;
    use modron_mobile_core::view::{merge_confirm_text, TASK_TITLE_NEEDED};
    use modron_mobile_core::{SessionMetaWire, ViewModelSnapshot};
    use modron_protocol::AutomationWire;

    fn meta(n: u128, title: &str, branch: Option<&str>, state: Option<&str>) -> SessionMetaWire {
        SessionMetaWire {
            id: Uuid::from_u128(n),
            title: title.into(),
            created_at: String::new(),
            updated_at: String::new(),
            message_count: 0,
            task_branch: branch.map(String::from),
            task_state: state.map(String::from),
            working_dir: None,
        }
    }

    fn snapshot() -> ViewModelSnapshot {
        ViewModelSnapshot {
            sessions: vec![
                meta(1, "fix parser", Some("task/fix"), Some("running")),
                meta(2, "plain chat", None, None),
            ],
            session_rows: vec!["fix parser".into(), "plain chat".into()],
            task_rows: vec![modron_mobile_core::view::TaskRowView {
                session_id: Uuid::from_u128(1),
                text: "fix parser \u{b7} task/fix \u{b7} running".into(),
                summary: String::new(),
            }],
            automations: vec![AutomationWire {
                name: "nightly".into(),
                prompt: "check the build".into(),
                interval_minutes: 30,
                last_run: None,
            }],
            ..Default::default()
        }
    }

    fn nav_on(screen: Screen) -> Nav {
        let mut nav = Nav::default();
        nav.observe(&snapshot());
        nav.apply(NavStep::Open(screen));
        nav
    }

    fn type_text(nav: &mut Nav, text: &str) {
        for c in text.chars() {
            nav.apply(NavStep::FormChar(c));
        }
    }

    #[test]
    fn opening_automations_asks_for_the_list_and_shows_the_shared_rows() {
        let mut nav = Nav::default();
        nav.observe(&snapshot());
        assert_eq!(nav.apply(NavStep::Open(Screen::Automations)), vec![UiAction::ListAutomations]);
        let view = nav.view();
        assert_eq!(view.automations_total, 1);
        assert_eq!(view.automations[0].name, "nightly");
        assert_eq!(view.automations[0].text, "nightly \u{b7} every 30 min \u{b7} last run never");
    }

    #[test]
    fn the_automation_form_refuses_an_interval_below_the_floor_and_keeps_its_text() {
        let mut nav = nav_on(Screen::Automations);
        assert!(nav.apply(NavStep::OpenForm(FormKind::Automation)).is_empty());
        type_text(&mut nav, "backup");
        nav.apply(NavStep::FormEnter);
        type_text(&mut nav, "run it");
        nav.apply(NavStep::FormEnter);
        type_text(&mut nav, "4");
        assert!(nav.apply(NavStep::FormEnter).is_empty());
        let form = nav.view().form.expect("form stays open");
        assert_eq!(form.error.as_deref(), Some("interval must be at least 5 minutes"));
        assert_eq!(form.values, vec!["backup", "run it", "4"]);
        assert_eq!(form.active, 2);
    }

    #[test]
    fn a_valid_automation_form_sends_the_add_then_a_refresh_and_closes() {
        let mut nav = nav_on(Screen::Automations);
        nav.apply(NavStep::OpenForm(FormKind::Automation));
        type_text(&mut nav, "backup");
        nav.apply(NavStep::FormNext);
        type_text(&mut nav, "run it");
        nav.apply(NavStep::FormNext);
        type_text(&mut nav, "9");
        nav.apply(NavStep::FormBack);
        type_text(&mut nav, "15");
        assert_eq!(
            nav.apply(NavStep::FormSubmit),
            vec![
                UiAction::AddAutomation {
                    name: "backup".into(),
                    prompt: "run it".into(),
                    interval_minutes: 15,
                },
                UiAction::ListAutomations,
            ]
        );
        assert!(nav.view().form.is_none());
    }

    #[test]
    fn escape_closes_the_form_and_a_second_escape_leaves_the_screen() {
        let mut nav = nav_on(Screen::Automations);
        nav.apply(NavStep::OpenForm(FormKind::Automation));
        nav.apply(NavStep::FormCancel);
        assert!(nav.view().form.is_none());
        assert_eq!(nav.top(), Screen::Automations);
        nav.apply(NavStep::Back);
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn remove_asks_first_and_the_confirmation_sends_the_remove_and_a_refresh() {
        let mut nav = nav_on(Screen::Automations);
        nav.apply(NavStep::AskRemove(0));
        assert_eq!(nav.view().confirm.as_deref(), Some("delete nightly? y / n"));
        assert_eq!(
            nav.apply(NavStep::Confirm),
            vec![UiAction::RemoveAutomation("nightly".into()), UiAction::ListAutomations]
        );
    }

    #[test]
    fn the_tasks_screen_lists_only_sessions_with_a_branch_and_asks_for_their_status() {
        let mut nav = Nav::default();
        nav.observe(&snapshot());
        let opened = nav.apply(NavStep::Open(Screen::Tasks));
        assert_eq!(
            opened,
            vec![
                UiAction::ListSessions,
                UiAction::RefreshTaskStatuses,
                UiAction::TaskStatusOf(Uuid::from_u128(1)),
            ]
        );
        let view = nav.view();
        assert_eq!(view.tasks_total, 1);
        assert_eq!(view.tasks[0].text, "fix parser \u{b7} task/fix \u{b7} running");
        assert_eq!(nav.selected_task(), Some(Uuid::from_u128(1)));
    }

    #[test]
    fn a_task_needs_a_title_and_creating_one_goes_back_to_chat() {
        let mut nav = nav_on(Screen::Tasks);
        nav.apply(NavStep::OpenForm(FormKind::Task));
        assert!(nav.apply(NavStep::FormSubmit).is_empty());
        assert_eq!(nav.view().form.and_then(|f| f.error).as_deref(), Some(TASK_TITLE_NEEDED));
        type_text(&mut nav, "  add tests ");
        assert_eq!(nav.apply(NavStep::FormSubmit), vec![UiAction::CreateTask("add tests".into())]);
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn merge_asks_first_then_sends_the_merge_and_a_refresh() {
        let mut nav = nav_on(Screen::Tasks);
        nav.apply(NavStep::AskMerge(0));
        assert_eq!(nav.view().confirm, Some(merge_confirm_text("task/fix")));
        assert_eq!(
            nav.apply(NavStep::Confirm),
            vec![UiAction::MergeTask(Uuid::from_u128(1)), UiAction::ListSessions]
        );
    }

    #[test]
    fn enter_attaches_the_selected_task_and_the_filter_types() {
        let mut nav = nav_on(Screen::Tasks);
        nav.apply(NavStep::Filter('p'));
        assert_eq!(nav.view().tasks.len(), 1);
        assert_eq!(
            nav.apply(NavStep::Attach(Uuid::from_u128(1))),
            vec![UiAction::Attach(Uuid::from_u128(1))]
        );
        assert_eq!(nav.top(), Screen::Chat);
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;
    use modron_mobile_core::ViewModelSnapshot;

    fn base() -> ViewModelSnapshot {
        ViewModelSnapshot {
            approval_mode_label: "ask".into(),
            sandbox: "off".into(),
            model: "claude-opus-5".into(),
            provider: "anthropic".into(),
            phase_label: "build".into(),
            ..Default::default()
        }
    }

    fn labels(nav: &Nav) -> Vec<String> {
        nav.view().picker.iter().map(|r| r.label.clone()).collect()
    }

    fn settings_nav(s: &ViewModelSnapshot) -> Nav {
        let mut nav = Nav::default();
        nav.observe(s);
        nav.apply(NavStep::Open(Screen::Settings));
        nav
    }

    #[test]
    fn the_mode_picker_marks_the_current_mode_and_enter_applies_and_closes() {
        let mut nav = Nav::default();
        nav.observe(&base());
        nav.apply(NavStep::OpenPicker(PickerKind::Mode));
        let view = nav.view();
        assert_eq!(view.screen, Screen::Picker);
        assert_eq!(view.picker_title, "approval mode");
        assert_eq!(labels(&nav), ["ask", "yolo", "deny"]);
        assert!(view.picker[0].current && !view.picker[1].current);
        assert_eq!(view.selected, 0);
        nav.apply(NavStep::Move(1));
        let sent = nav.apply(NavStep::PickRow(1));
        assert_eq!(sent, vec![UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll)]);
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn a_picker_opened_from_settings_closes_back_to_settings() {
        let mut nav = settings_nav(&base());
        nav.apply(NavStep::OpenPicker(PickerKind::Sandbox));
        assert_eq!(labels(&nav), ["off", "seatbelt", "bwrap"]);
        let sent = nav.apply(NavStep::PickRow(2));
        assert_eq!(sent, vec![UiAction::SetSandbox(modron_types::SandboxPolicy::Bubblewrap)]);
        assert_eq!(nav.top(), Screen::Settings);
    }

    #[test]
    fn typing_filters_and_esc_clears_the_filter_before_the_picker_closes() {
        let mut nav = Nav::default();
        nav.observe(&base());
        nav.apply(NavStep::OpenPicker(PickerKind::Effort));
        assert_eq!(labels(&nav), ["unset", "low", "medium", "high", "xhigh", "max"]);
        nav.apply(NavStep::Filter('x'));
        nav.apply(NavStep::Filter('h'));
        nav.apply(NavStep::Filter('i'));
        assert_eq!(labels(&nav), ["xhigh"]);
        nav.apply(NavStep::ClearFilter);
        assert_eq!(nav.top(), Screen::Picker);
        assert_eq!(labels(&nav).len(), 6);
        nav.apply(NavStep::Back);
        assert_eq!(nav.top(), Screen::Chat);
    }

    #[test]
    fn the_effort_picker_marks_the_set_level_and_unset_when_empty() {
        let mut s = base();
        s.effort_label = "high".into();
        let mut nav = Nav::default();
        nav.observe(&s);
        nav.apply(NavStep::OpenPicker(PickerKind::Effort));
        let marked: Vec<String> =
            nav.view().picker.iter().filter(|r| r.current).map(|r| r.label.clone()).collect();
        assert_eq!(marked, ["high"]);
        assert_eq!(nav.view().selected, 3, "the current row is preselected");
    }

    #[test]
    fn the_agent_picker_lists_none_and_enter_on_it_does_nothing() {
        let mut nav = Nav::default();
        nav.observe(&base());
        nav.apply(NavStep::OpenPicker(PickerKind::Agent));
        let view = nav.view();
        assert_eq!(view.picker.len(), 1);
        assert_eq!(view.picker[0].label, PICKER_NONE_LABEL);
        assert!(!view.picker[0].enabled);
        assert!(nav.apply(NavStep::PickRow(0)).is_empty());
        assert_eq!(nav.top(), Screen::Picker);
    }

    #[test]
    fn the_theme_picker_applies_once_and_restore_is_not_remembered() {
        let mut nav = settings_nav(&base());
        nav.apply(NavStep::OpenPicker(PickerKind::Theme));
        assert_eq!(labels(&nav), ["oculus dark", "oculus light", "system"]);
        assert!(nav.view().picker[2].current, "the theme in use is marked");
        assert!(nav.apply(NavStep::PickRow(0)).is_empty());
        let change = nav.view().theme_change;
        assert_eq!(change, ThemeChange { seq: 1, choice: Choice::Dark, remember: true });
        assert_eq!(nav.top(), Screen::Settings);
        let view = nav.view();
        assert!(view.settings.iter().any(|r| r.line() == "theme: oculus dark"));
        nav.restore_theme(Choice::Light);
        assert_eq!(
            nav.view().theme_change,
            ThemeChange { seq: 2, choice: Choice::Light, remember: false }
        );
    }

    #[test]
    fn d_asks_before_removing_a_rule_and_the_confirmation_sends_the_remove() {
        let mut s = base();
        s.rules = vec!["allow read".into(), "deny bash(rm -rf *)".into()];
        let mut nav = settings_nav(&s);
        let row = nav.settings.iter().position(|r| r.line() == "rules: deny bash(rm -rf *)");
        let row = row.expect("the rule row");
        nav.apply(NavStep::Move(row as isize - nav.selected() as isize));
        assert!(nav.settings_removable());
        nav.apply(NavStep::AskSettingRemove(row));
        assert_eq!(nav.view().confirm.as_deref(), Some("remove rules: deny bash(rm -rf *)? y / n"));
        assert_eq!(nav.apply(NavStep::Confirm), vec![UiAction::RemoveRule(1)]);
        assert!(!nav.confirming());
    }

    #[test]
    fn bare_allow_and_deny_open_settings_on_their_list_and_d_removes_from_there() {
        let mut s = base();
        s.allow_tools = vec!["bash".into()];
        s.deny_tools = vec!["webfetch".into()];
        let mut nav = Nav::default();
        nav.observe(&s);
        nav.apply(NavStep::OpenToolList(false));
        assert_eq!(nav.top(), Screen::Settings);
        assert_eq!(nav.settings[nav.selected()].line(), "deny: webfetch");
        nav.apply(NavStep::AskSettingRemove(nav.selected()));
        assert!(nav.view().confirm.is_some(), "the removal asks first");
        nav.apply(NavStep::OpenToolList(true));
        assert_eq!(nav.settings[nav.selected()].line(), "allow: bash");
    }

    #[test]
    fn allow_and_deny_entries_are_removable_through_the_tool_lists() {
        let mut s = base();
        s.allow_tools = vec!["bash".into()];
        s.deny_tools = vec!["webfetch".into()];
        let nav = settings_nav(&s);
        let allow = nav.settings.iter().position(|r| r.line() == "allow: bash").expect("allow row");
        let deny =
            nav.settings.iter().position(|r| r.line() == "deny: webfetch").expect("deny row");
        assert_eq!(
            nav.settings[allow].remove,
            Some(UiAction::RemoveToolRule { allow: true, tool: "bash".into() })
        );
        assert_eq!(
            nav.settings[deny].remove,
            Some(UiAction::RemoveToolRule { allow: false, tool: "webfetch".into() })
        );
        assert!(nav.settings.iter().all(|r| r.label != "allow" || r.remove.is_some()));
    }

    #[test]
    fn the_help_tabs_switch_and_the_rows_scroll_within_the_tab() {
        let mut nav = Nav::default();
        nav.apply(NavStep::Open(Screen::Help));
        assert_eq!(nav.view().help_tab, HelpTab::General);
        nav.apply(NavStep::HelpCycle(1));
        assert_eq!(nav.view().help_tab, HelpTab::Commands);
        let total = nav.view().help_rows.len();
        assert!(total > HELP_VISIBLE, "the commands tab scrolls");
        nav.apply(NavStep::HelpScroll(1000));
        assert_eq!(nav.view().help_scroll, total - HELP_VISIBLE, "scroll stops at the last row");
        assert_eq!(nav.view().help_rows.len(), total);
        nav.apply(NavStep::HelpEdge(true));
        assert_eq!(nav.view().help_scroll, 0);
        nav.apply(NavStep::HelpEdge(false));
        nav.apply(NavStep::HelpTo(HelpTab::Shortcuts));
        assert_eq!(nav.view().help_scroll, 0, "a new tab starts at its top");
        nav.apply(NavStep::HelpCycle(-1));
        assert_eq!(nav.view().help_tab, HelpTab::Commands);
        nav.apply(NavStep::Back);
        assert_eq!(nav.top(), Screen::Chat);
    }
}

#[cfg(test)]
mod persona_task_auth_tests {
    use super::*;
    use modron_mobile_core::view::{TaskRowView, AUTH_NOT_CONFIGURED};
    use modron_protocol::{AuthProviderWire, SessionMetaWire};

    fn persona(name: &str, text: &str, current: bool) -> PersonaRowView {
        PersonaRowView { name: name.into(), text: text.into(), current }
    }

    #[allow(clippy::field_reassign_with_default)]
    fn task_snapshot(count: u128) -> ViewModelSnapshot {
        let mut s = ViewModelSnapshot::default();
        s.sessions = (1..=count)
            .map(|n| SessionMetaWire {
                id: Uuid::from_u128(n),
                title: "t".into(),
                created_at: String::new(),
                updated_at: String::new(),
                message_count: 0,
                working_dir: None,
                task_branch: Some(format!("task/{n}")),
                task_state: Some("idle".into()),
            })
            .collect();
        s.task_rows = (1..=count)
            .map(|n| TaskRowView {
                session_id: Uuid::from_u128(n),
                text: format!("task {n} - task/{n} - idle"),
                summary: String::new(),
            })
            .collect();
        s
    }

    #[allow(clippy::field_reassign_with_default)]
    fn providers(status: &str) -> ViewModelSnapshot {
        let mut s = ViewModelSnapshot::default();
        s.providers = vec![AuthProviderWire {
            id: "anthropic".into(),
            label: "Anthropic".into(),
            status: status.into(),
            oauth_capable: true,
        }];
        s
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn the_agent_picker_asks_for_personas_and_lists_their_text_with_the_current_marked() {
        let mut nav = Nav::default();
        let mut s = ViewModelSnapshot::default();
        s.personas = vec![
            persona("planner", "planner - plans the work", false),
            persona("reviewer", "reviewer", true),
        ];
        nav.observe(&s);
        assert_eq!(nav.apply(NavStep::OpenPicker(PickerKind::Agent)), vec![UiAction::ListPersonas]);
        let view = nav.view();
        let labels: Vec<&str> = view.picker.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, ["planner - plans the work", "reviewer"]);
        assert!(!view.picker[0].current && view.picker[1].current);
        assert_eq!(nav.apply(NavStep::PickRow(0)), vec![UiAction::SetPersona("planner".into())]);
    }

    #[test]
    fn the_agent_picker_shows_none_until_the_personas_arrive() {
        let mut nav = Nav::default();
        nav.observe(&ViewModelSnapshot::default());
        nav.apply(NavStep::OpenPicker(PickerKind::Agent));
        let labels: Vec<String> = nav.view().picker.iter().map(|r| r.label.clone()).collect();
        assert_eq!(labels, vec![PICKER_NONE_LABEL.to_string()]);
        assert!(nav.apply(NavStep::PickRow(0)).is_empty());
    }

    #[test]
    fn the_agent_picker_says_it_cannot_clear_the_persona_and_other_pickers_say_nothing() {
        let mut nav = Nav::default();
        nav.observe(&ViewModelSnapshot::default());
        nav.apply(NavStep::OpenPicker(PickerKind::Agent));
        assert_eq!(nav.view().picker_note, PERSONA_CLEAR_NOTE);
        nav.apply(NavStep::Back);
        nav.apply(NavStep::OpenPicker(PickerKind::Mode));
        assert_eq!(nav.view().picker_note, "");
    }

    #[test]
    fn the_tasks_screen_shows_the_core_rows_and_their_summaries() {
        let mut s = task_snapshot(1);
        s.task_rows[0].summary = "ahead 3 - 1 file (+120 -14)".into();
        let mut nav = Nav::default();
        nav.observe(&s);
        nav.apply(NavStep::Open(Screen::Tasks));
        let view = nav.view();
        assert_eq!(view.tasks[0].text, "task 1 - task/1 - idle");
        assert_eq!(view.tasks[0].summary, "ahead 3 - 1 file (+120 -14)");
    }

    #[test]
    fn the_refresh_timer_waits_five_seconds_and_runs_only_on_the_tasks_screen() {
        let mut nav = Nav::default();
        nav.observe(&task_snapshot(2));
        assert!(nav.apply(NavStep::Open(Screen::Tasks)).contains(&UiAction::RefreshTaskStatuses));
        let base = Instant::now();
        assert!(!nav.task_refresh_due(base + Duration::from_secs(4)), "the open just asked");
        assert!(nav.task_refresh_due(base + Duration::from_secs(6)));
        assert!(!nav.task_refresh_due(base + Duration::from_secs(7)));
        assert!(nav.task_refresh_due(base + Duration::from_secs(12)));
    }

    #[test]
    fn no_refresh_is_issued_while_another_screen_is_showing() {
        let mut nav = Nav::default();
        nav.observe(&task_snapshot(2));
        let base = Instant::now();
        assert!(!nav.task_refresh_due(base + Duration::from_secs(60)));
        nav.apply(NavStep::Open(Screen::Sessions));
        assert!(!nav.task_refresh_due(base + Duration::from_secs(120)));
    }

    #[test]
    fn a_move_past_the_refreshed_batch_asks_for_the_next_batch() {
        let mut nav = Nav::default();
        nav.observe(&task_snapshot(12));
        nav.apply(NavStep::Open(Screen::Tasks));
        assert!(!nav.apply(NavStep::Move(5)).contains(&UiAction::RefreshTaskStatuses));
        assert!(nav.apply(NavStep::Move(5)).contains(&UiAction::RefreshTaskStatuses));
    }

    #[test]
    fn d_on_a_signed_in_credential_asks_then_removes_it() {
        let mut nav = Nav::default();
        nav.observe(&providers("configured"));
        nav.apply(NavStep::Open(Screen::Auth));
        nav.apply(NavStep::AskAuthRemove(0));
        assert_eq!(nav.view().confirm.as_deref(), Some("remove Anthropic credentials? y / n"));
        assert_eq!(nav.apply(NavStep::Confirm), vec![UiAction::AuthRemove("anthropic".into())]);
        assert!(!nav.confirming());
    }

    #[test]
    fn a_row_that_is_not_signed_in_offers_no_removal() {
        let mut nav = Nav::default();
        nav.observe(&providers(AUTH_NOT_CONFIGURED));
        nav.apply(NavStep::Open(Screen::Auth));
        nav.apply(NavStep::AskAuthRemove(0));
        assert_eq!(nav.view().confirm, None);
    }
}
