//! Screen navigation, shared by both hosts. The host owns the state: it
//! routes keys before the view sees them (`chat::route_key`), so the screen
//! stack, the filters and selections, the settings rows, the model list, the
//! sub-agents, the inline confirmation and the composer's command list live
//! in [`Nav`]. The view renders the [`NavView`] the host sends whenever it
//! changes. Presentation only: every word comes from the core or the shared
//! table.

use modron_mobile_core::view::{
    auth_is_signed_in, auth_row_line, delete_confirm_text, kill_confirm_text, AgentRowView,
    OAuthProgressView, MODEL_ROLE_CHEAP, MODEL_ROLE_CURRENT, MODEL_ROLE_FALLBACK,
    MODEL_ROLE_STRONG, SETTING_AUTH_LABEL,
};
use modron_mobile_core::{
    approval_mode_next, effort_next, phase_next, sandbox_next, ViewModelSnapshot,
    EFFORT_UNSET_LABEL, RULES_NONE_LABEL, SETTING_APPROVAL_LABEL, SETTING_COMPACT_LABEL,
    SETTING_CONTEXT_LABEL, SETTING_EFFORT_LABEL, SETTING_MODEL_LABEL, SETTING_PHASE_LABEL,
    SETTING_PROVIDER_LABEL, SETTING_RULES_LABEL, SETTING_SANDBOX_LABEL,
};
use uuid::Uuid;

use crate::chat::UiAction;
use crate::portal::{self, PortalLine, PortalRow};
use crate::slash;

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
    /// The first-run check ran (see [`Nav::first_run`]).
    first_run_done: bool,
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
        self.auth = auth_rows(snapshot);
        self.auth_progress = snapshot.oauth.clone();
        self.models = model_rows(snapshot);
        self.agents = snapshot.agents.clone();
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

    /// True on the two screens with a filter line.
    fn filtering(&self) -> bool {
        matches!(self.top(), Screen::Sessions | Screen::Models | Screen::Portal)
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
    pub fn first_run(&mut self, snapshot: &ViewModelSnapshot) -> Vec<UiAction> {
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
    }

    fn leave_to_chat(&mut self) {
        self.stack.clear();
        self.filter.clear();
        self.selected = 0;
        self.pending = None;
        self.agent_id = None;
        self.agent_row = None;
        self.auth_key = None;
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
                    Screen::Auth => return vec![UiAction::ListAuth],
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
            }
            NavStep::FilterBack if self.filtering() => {
                self.filter.pop();
                self.selected = 0;
            }
            NavStep::Move(delta) => {
                let last = self.list_len().saturating_sub(1) as isize;
                self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
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
            NavStep::Activate if self.top() == Screen::Auth && self.auth_key.is_none() => {
                if let Some(row) = self.auth.get(self.selected).cloned() {
                    if row.oauth_capable {
                        return vec![UiAction::AuthStart(row.id)];
                    }
                    self.auth_key = Some(row.id);
                }
            }
            NavStep::ActivateRow(index) if self.top() == Screen::Auth => {
                self.selected = index.min(self.auth.len().saturating_sub(1));
                return self.apply(NavStep::Activate);
            }
            NavStep::AuthCancelKey | NavStep::AuthKeyDone => self.auth_key = None,
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
        }
    }
}

fn row(label: &str, value: String, action: Option<UiAction>) -> SettingRow {
    SettingRow { label: label.to_string(), value, action }
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
/// core's snapshot; each cycle action starts from the core's current value.
/// The model row opens the picker.
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
            Some(UiAction::SetApprovalMode(approval_mode_next(s.approval_mode))),
        ),
        row(
            SETTING_PHASE_LABEL,
            s.phase_label.clone(),
            Some(UiAction::SetPhase(phase_next(s.phase))),
        ),
        row(SETTING_EFFORT_LABEL, effort, Some(UiAction::SetEffort(effort_next(s.effort)))),
        row(
            SETTING_SANDBOX_LABEL,
            s.sandbox.clone(),
            Some(UiAction::SetSandbox(sandbox_next(s.sandbox_policy))),
        ),
        row(SETTING_CONTEXT_LABEL, s.context_label.clone(), None),
        row(SETTING_COMPACT_LABEL, String::new(), Some(UiAction::Compact)),
        row(SETTING_AUTH_LABEL, String::new(), Some(UiAction::Nav(NavStep::Open(Screen::Auth)))),
    ];
    if s.rules.is_empty() {
        rows.push(row(SETTING_RULES_LABEL, RULES_NONE_LABEL.to_string(), None));
    } else {
        rows.extend(s.rules.iter().map(|rule| row(SETTING_RULES_LABEL, rule.clone(), None)));
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
    fn settings_rows_carry_the_core_labels_and_cycle_actions() {
        let rows = settings_rows(&snapshot());
        let lines: Vec<String> = rows.iter().map(SettingRow::line).collect();
        assert_eq!(lines[0], "model: claude-opus-5");
        assert_eq!(lines[2], "approval mode: ask");
        assert_eq!(lines[4], "effort: unset");
        assert_eq!(lines.last().map(String::as_str), Some("rules: none"));
        assert_eq!(rows[2].action, Some(UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll)));
        assert_eq!(
            rows[4].action,
            Some(UiAction::SetEffort(Some(modron_mobile_core::Effort::Low)))
        );
        assert_eq!(rows[7].action, Some(UiAction::Compact));
        // The model row opens the picker (Packet C3f); provider stays read-only.
        assert_eq!(rows[0].action, Some(UiAction::Nav(NavStep::Open(Screen::Models))));
        assert_eq!(rows[1].action, None);
    }

    #[test]
    fn enter_setting_follows_the_selected_row_on_settings_only() {
        let mut nav = Nav::default();
        nav.observe(&snapshot());
        assert_eq!(nav.enter_setting(), None);
        nav.apply(NavStep::Open(Screen::Settings));
        nav.apply(NavStep::Move(2));
        assert_eq!(
            nav.enter_setting(),
            Some(UiAction::SetApprovalMode(ApprovalModeWire::ApproveAll))
        );
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
