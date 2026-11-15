//! Screen navigation, shared by both hosts. The host owns the state: it
//! routes keys before the view sees them (`chat::route_key`), so the screen
//! stack, the sessions filter and selection, and the settings rows live in
//! [`Nav`]. The view renders the [`NavView`] the host sends whenever it
//! changes. Presentation only: every word comes from the core.

use modron_mobile_core::{
    approval_mode_next, effort_next, phase_next, sandbox_next, ViewModelSnapshot,
    EFFORT_UNSET_LABEL, RULES_NONE_LABEL, SETTING_APPROVAL_LABEL, SETTING_COMPACT_LABEL,
    SETTING_CONTEXT_LABEL, SETTING_EFFORT_LABEL, SETTING_MODEL_LABEL, SETTING_PHASE_LABEL,
    SETTING_PROVIDER_LABEL, SETTING_RULES_LABEL, SETTING_SANDBOX_LABEL,
};
use uuid::Uuid;

use crate::chat::UiAction;

/// The screens. Chat is the root; Sessions and Settings open over it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Chat,
    Sessions,
    Settings,
}

/// One navigation step. Keys and taps both become steps; [`Nav::apply`]
/// performs them and returns the core calls they imply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavStep {
    /// Open a screen on top of the stack (Sessions also refreshes the list).
    Open(Screen),
    /// Esc with no filter text: back one screen (never past Chat).
    Back,
    /// Esc with filter text: clear the filter first.
    ClearFilter,
    /// A typed character appended to the sessions filter.
    Filter(char),
    /// Backspace in the sessions filter.
    FilterBack,
    /// Up / Down, or j / k with an empty filter. Negative moves up.
    Move(isize),
    /// Enter or a tap on a session row: attach it and go to Chat.
    Attach(Uuid),
    /// `n` with an empty filter, or the new-session control: create one
    /// and go to Chat.
    NewSession,
}

/// One sessions row: the id to attach and the core's row text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRow {
    pub id: Uuid,
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

/// What the view renders.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct NavView {
    pub screen: Screen,
    pub filter: String,
    pub selected: usize,
    /// Every session the core holds (0 shows the empty state).
    pub total: usize,
    /// The rows that match the filter, in the core's order.
    pub sessions: Vec<SessionRow>,
    pub settings: Vec<SettingRow>,
}

/// The navigation state the host owns.
#[derive(Debug, Default)]
pub struct Nav {
    /// Screens above Chat, oldest first. Empty means Chat.
    stack: Vec<Screen>,
    filter: String,
    selected: usize,
    all: Vec<SessionRow>,
    settings: Vec<SettingRow>,
}

impl Nav {
    /// The screen on top of the stack.
    pub fn top(&self) -> Screen {
        self.stack.last().copied().unwrap_or(Screen::Chat)
    }

    /// The sessions filter text.
    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Takes the latest snapshot's sessions and settings, then keeps the
    /// selection inside the list it now shows.
    pub fn observe(&mut self, snapshot: &ViewModelSnapshot) {
        self.all = snapshot
            .sessions
            .iter()
            .zip(snapshot.session_rows.iter())
            .map(|(meta, text)| SessionRow { id: meta.id, text: text.clone() })
            .collect();
        self.settings = settings_rows(snapshot);
        self.clamp();
    }

    /// The session rows that match the filter: a case-insensitive substring
    /// of the row text.
    fn visible(&self) -> Vec<&SessionRow> {
        let needle = self.filter.to_lowercase();
        self.all
            .iter()
            .filter(|row| needle.is_empty() || row.text.to_lowercase().contains(&needle))
            .collect()
    }

    /// The session Enter would attach (Sessions on top only).
    pub fn selected_session(&self) -> Option<Uuid> {
        if self.top() != Screen::Sessions {
            return None;
        }
        self.visible().get(self.selected).map(|row| row.id)
    }

    /// The action Enter would send for the selected settings row.
    pub fn enter_setting(&self) -> Option<UiAction> {
        if self.top() != Screen::Settings {
            return None;
        }
        self.settings.get(self.selected).and_then(|row| row.action.clone())
    }

    fn list_len(&self) -> usize {
        match self.top() {
            Screen::Chat => 0,
            Screen::Sessions => self.visible().len(),
            Screen::Settings => self.settings.len(),
        }
    }

    fn clamp(&mut self) {
        self.selected = self.selected.min(self.list_len().saturating_sub(1));
    }

    /// Performs one step and returns the core calls it implies. The host
    /// sends those and then sends the view its new [`NavView`].
    pub fn apply(&mut self, step: NavStep) -> Vec<UiAction> {
        match step {
            NavStep::Open(screen) => {
                if self.top() == screen || screen == Screen::Chat {
                    return Vec::new();
                }
                self.stack.push(screen);
                self.filter.clear();
                self.selected = 0;
                if screen == Screen::Sessions {
                    return vec![UiAction::ListSessions];
                }
            }
            NavStep::Back => {
                self.stack.pop();
                self.filter.clear();
                self.selected = 0;
            }
            NavStep::ClearFilter if self.top() == Screen::Sessions => {
                self.filter.clear();
                self.selected = 0;
            }
            NavStep::Filter(c) if self.top() == Screen::Sessions => {
                self.filter.push(c);
                self.selected = 0;
            }
            NavStep::FilterBack if self.top() == Screen::Sessions => {
                self.filter.pop();
                self.selected = 0;
            }
            NavStep::Move(delta) => {
                let last = self.list_len().saturating_sub(1) as isize;
                self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
            }
            NavStep::Attach(id) => {
                self.leave_to_chat();
                return vec![UiAction::Attach(id)];
            }
            NavStep::NewSession => {
                self.leave_to_chat();
                return vec![UiAction::NewSession];
            }
            _ => {}
        }
        Vec::new()
    }

    fn leave_to_chat(&mut self) {
        self.stack.clear();
        self.filter.clear();
        self.selected = 0;
    }

    /// The state the view renders.
    pub fn view(&self) -> NavView {
        NavView {
            screen: self.top(),
            filter: self.filter.clone(),
            selected: self.selected,
            total: self.all.len(),
            sessions: self.visible().into_iter().cloned().collect(),
            settings: self.settings.clone(),
        }
    }
}

fn row(label: &str, value: String, action: Option<UiAction>) -> SettingRow {
    SettingRow { label: label.to_string(), value, action }
}

/// The settings rows, in screen order. Values and labels come from the
/// core's snapshot; each cycle action starts from the core's current value.
pub fn settings_rows(s: &ViewModelSnapshot) -> Vec<SettingRow> {
    let effort = if s.effort_label.is_empty() {
        EFFORT_UNSET_LABEL.to_string()
    } else {
        s.effort_label.clone()
    };
    let mut rows = vec![
        row(SETTING_MODEL_LABEL, s.model.clone(), None),
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
        assert_eq!(rows[0].action, None);
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
