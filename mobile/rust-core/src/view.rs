//! The mobile view-model: one serde-friendly projection of a session that
//! a native shell (nsh TSX) or a future Swift/Kotlin bridge renders by
//! polling. All strings a screen shows are produced here, from the same
//! modron-view primitives the TUI and desktop use — per the AGENTS.md
//! parity contract, no client formats anything locally.
//!
//! Folding rules (one defined effect per protocol event):
//! - `Snapshot` resets: `replay(snapshot.events)` replaces history, the
//!   live half seeds streaming buffers, the tool tracker, the pending
//!   approval, and cumulative usage.
//! - `Delta::Text` appends `streaming_text`; `Delta::Reasoning` appends
//!   `streaming_reasoning` only when `show_reasoning` is on (invariant 5:
//!   reasoning streams only when the user opted in). Buffers commit to
//!   history at `TurnDone` and clear (uncommitted) on abort/failure —
//!   TUI streaming parity.
//! - `Event` frames append committed rows by folding through
//!   [`modron_view::ReplayFolder`] one push at a time (see `FoldState`).
//! - `Tool` frames feed [`modron_view::ActivityTracker`] (the live strip).
//! - `ApprovalRequested` joins a FIFO queue; the view shows the front.
//!   `ApprovalResolved` removes its request id wherever it sits in the
//!   queue; answering removes the front. A `Snapshot` replaces the queue
//!   with the snapshot's own pending approval; a disconnect clears it.
//!   Config changes are label updates for future turns only.
//! - `Steered` queues a pending steer echo; the persisted `Task` event with
//!   the same text removes it; turn end and a Snapshot clear the rest.
//! - `QueueChanged` replaces the queue; `ContextChanged` sets the context
//!   gauge; `PlanProposed` opens the plan block (lines from
//!   `modron_view::plan_lines`), `PlanDecided` clears it. A Snapshot sets
//!   the plan block from `LiveState.plan_pending` (no lines on attach).
//! - `ConfigChanged` `phase` sets the phase label, `rules` replaces the
//!   rule list; a Snapshot sets all of them from `LiveState`.
//!
//! Usage accounting: cumulative usage grows from persisted `Usage` events
//! (`usage_totals` arithmetic) and resets to the snapshot's live value on
//! attach, so a reconnect never double-counts. `TurnDone` summaries do
//! not add usage (the server's `Usage` events already covered the turn).

use modron_protocol::{
    ApprovalModeWire, ContextGaugeWire, Decision, LiveState, ModelWire, PendingApproval,
    PersonaWire, PlanDecision, RuleWire, SessionMetaWire, TaskFileStat, WireRunSummary,
};
use modron_types::{Effort, Phase, PlanItem, Role, SessionEvent, ToolActivity, ToolCall, Usage};
use modron_view::{
    compact_tokens, replay, ActivityTracker, ReplayFolder, SwarmRow, ToolEvent, TranscriptEntry,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Parity constants — the exact shared words (AGENTS.md UI/UX parity contract)
// ---------------------------------------------------------------------------

/// Decision labels for the approval sheet, in protocol order. The shared
/// words (AGENTS.md invariant 1, `allow once / allow all / deny`), same
/// as the desktop's `decision_label`; the TUI's strip adds key hints
/// (`y = allow once · a = allow for this run · n/Esc = deny`) which are
/// TUI presentation only — keys differ per the divergence table.
pub const DECISION_LABELS: [(Decision, &str); 3] = [
    (Decision::AllowOnce, Decision::AllowOnce.label()),
    (Decision::AllowAll, Decision::AllowAll.label()),
    (Decision::Deny, Decision::Deny.label()),
];

/// The plan decision labels for the plan decision block, in protocol
/// order. Shared words for every client (the plan block is the approval
/// surface's sibling: `approve` flips to build, `revise` plans again).
pub const PLAN_DECISION_LABELS: [(PlanDecision, &str); 2] = [
    (PlanDecision::Approve, "approve"),
    (PlanDecision::Revise, "revise"),
];

/// The reasoning-effort label for the status line (`effort high`): empty
/// when unset, so the shell shows nothing for the provider default.
pub fn effort_label(effort: Option<Effort>) -> String {
    effort.map(|e| e.label().to_string()).unwrap_or_default()
}

/// The run-phase label for the status line and settings (`plan` / `build`).
pub fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Plan => "plan",
        Phase::Build => "build",
    }
}

/// The status text while a turn runs (the old TUI's input box title
/// "working (esc to interrupt)", crates/modron-tui/src/lib.rs:3771).
pub const RUNNING_HINT: &str = "esc to interrupt";

/// The running spinner frames: the old TUI's calculus spinner
/// (crates/modron-tui/src/lib.rs:904, SPINNER_WORKING: partial, nabla,
/// delta, nabla). Escapes keep the source ASCII.
pub const SPINNER_FRAMES: [&str; 4] = ["\u{2202}", "\u{2207}", "\u{2206}", "\u{2207}"];

/// Clock ticks each idle hint holds before the next one shows (the old TUI
/// rotated on `spinner_tick / 8`, crates/modron-tui/src/lib.rs:3832).
pub const IDLE_HINT_TICKS: u64 = 8;

/// The idle hints, shown in the empty composer and rotated (the old TUI's
/// HINTS, crates/modron-tui/src/lib.rs:3821-3830; same words).
pub const IDLE_HINTS: [&str; 8] = [
    "type a task (or / for commands)\u{2026}",
    "try /model \u{2014} switch models mid-session",
    "try /auth \u{2014} sign in or reuse existing logins",
    "try @file \u{2014} attach a file to the task",
    "try !command \u{2014} run shell passthrough",
    "try /tasks \u{2014} check background subagents",
    "try /fork \u{2014} branch the session to explore",
    "try ctrl+t \u{2014} expand the tool activity panel",
];

/// The spinner frame for a clock tick.
pub fn spinner_frame(tick: u64) -> &'static str {
    SPINNER_FRAMES[(tick % SPINNER_FRAMES.len() as u64) as usize]
}

/// The idle hint for a clock tick.
pub fn idle_hint(tick: u64) -> &'static str {
    IDLE_HINTS[((tick / IDLE_HINT_TICKS) % IDLE_HINTS.len() as u64) as usize]
}

/// The welcome banner's tagline (the old TUI's banner, crates/modron-tui/src/run.rs:541-575).
pub const WELCOME_TAGLINE: &str = "a token-efficient Rust coding agent";

/// The welcome banner's key hints. The app's keys: Shift+Enter adds a line
/// and Alt+Enter queues (the old banner said alt+enter newline).
pub const WELCOME_KEYS: &str =
    "enter send \u{b7} shift+enter newline \u{b7} pgup/pgdn scroll \u{b7} esc interrupt";

/// The welcome banner rows for a blank session: the app name and version,
/// the tagline, and the model with the key hints. The app passes its own
/// version.
pub fn welcome_lines(model: &str, version: &str) -> Vec<String> {
    vec![
        String::new(),
        format!("\u{2726} modron v{version}"),
        format!("  {WELCOME_TAGLINE}"),
        String::new(),
        format!("  model {model}  \u{b7}  {WELCOME_KEYS}"),
        String::new(),
    ]
}

/// The theme names the app offers (`/theme`, the Settings theme row and the
/// picker). The Oculus palette ships dark and light; "system" follows the
/// terminal's or browser's appearance. Custom themes from the old TUI
/// (crates/modron-tui/src/theme.rs) are not carried over.
pub const THEME_DARK_NAME: &str = "oculus dark";
pub const THEME_LIGHT_NAME: &str = "oculus light";
pub const THEME_SYSTEM_NAME: &str = "system";

/// The percentage at which the context gauge warns: the old TUI's compaction
/// trigger (crates/modron-tui/src/lib.rs, `status_spans`: `pct >= 80`).
pub const CONTEXT_WARNING_PCT: u64 = 80;

/// The marker the gauge appends to its percentage once it reaches
/// [`CONTEXT_WARNING_PCT`] (`ctx 85%!`), the old status line's `!`
/// (crates/modron-tui/src/lib.rs, `status_spans`). Shown on every client's
/// status line and `/context` note for as long as the gauge stays there.
pub const CONTEXT_WARNING_MARK: &str = "!";

/// The context meter label (`ctx 41% (74k/180k)`). Empty when the gauge
/// is absent or its window is zero (no meaningful percentage), so the
/// shell hides the meter. At or above the warning percentage the marker
/// follows the percentage (`ctx 85%! (153k/180k)`).
pub fn context_label(gauge: Option<ContextGaugeWire>) -> String {
    match gauge {
        Some(ContextGaugeWire { used, window }) if window > 0 => {
            // Rounded percentage, computed in integers.
            let pct = used.saturating_mul(100).saturating_add(window / 2) / window;
            let mark = if pct >= CONTEXT_WARNING_PCT {
                CONTEXT_WARNING_MARK
            } else {
                ""
            };
            format!(
                "ctx {pct}%{mark} ({}/{})",
                compact_tokens(used),
                compact_tokens(window)
            )
        }
        _ => String::new(),
    }
}

/// The longest goal the status area shows, in characters (cut with `short`).
pub const GOAL_MAX_CHARS: usize = 60;

/// The standing goal for the status area (`[goal: ship it]`, the old TUI's
/// dim row). Empty when no goal is set, so the shell hides the row.
pub fn goal_label(goal: &str) -> String {
    let goal = goal.trim();
    if goal.is_empty() {
        String::new()
    } else {
        format!("[goal: {}]", modron_view::short(goal, GOAL_MAX_CHARS))
    }
}

/// The `/context` note: the context gauge (`ctx 41% (74k/180k)`, empty when
/// unknown), the session's cumulative tokens and cache hit rate, worded as
/// the status line words them.
pub fn context_note(context: &str, tokens: u64, cache_pct: Option<f64>) -> String {
    let gauge = if context.is_empty() {
        "ctx window unknown"
    } else {
        context
    };
    let mut note = format!("context: {gauge} \u{b7} {tokens} tok");
    if let Some(pct) = cache_pct {
        note.push_str(&format!(" \u{b7} cache {pct:.0}%"));
    }
    note
}

/// The approval mode label for the status line (`ask`/`yolo`/`deny`,
/// default `ask`), delegating to the shared protocol label.
pub fn approval_mode_label(mode: Option<ApprovalModeWire>) -> &'static str {
    mode.unwrap_or(ApprovalModeWire::Ask).label()
}

/// The sandbox label for the settings views (`off` / `seatbelt` /
/// `bwrap`), delegating to the shared modron-view label.
pub fn sandbox_label(policy: modron_types::SandboxPolicy) -> &'static str {
    modron_view::sandbox_label(policy)
}

/// Connection-state words for the mobile header.
pub fn connection_label(state: &ConnectionState) -> &'static str {
    match state {
        ConnectionState::Connected => "connected",
        ConnectionState::Connecting => "connecting",
        ConnectionState::Disconnected => "disconnected",
    }
}

// ---------------------------------------------------------------------------
// Sessions and settings copy (the NSH app's screens and any later shell)
// ---------------------------------------------------------------------------

/// Navigation and screen words.
pub const SESSIONS_LABEL: &str = "sessions";
pub const SETTINGS_LABEL: &str = "settings";
pub const NEW_SESSION_LABEL: &str = "new session";
pub const SESSIONS_EMPTY_LABEL: &str = "no sessions yet";
pub const SESSIONS_NO_MATCH_LABEL: &str = "no sessions match";
pub const FILTER_LABEL: &str = "filter";
/// The chat bar's title (the app's name), the send control's word and the
/// session id's word in the bar's details row (web, UX-2).
pub const CHAT_TITLE_LABEL: &str = "modron";
pub const SEND_LABEL: &str = "send";
pub const SESSION_DETAIL_LABEL: &str = "session";
/// The running/idle words, for a session row and the status surface.
pub const RUNNING_LABEL: &str = "running";
pub const IDLE_LABEL: &str = "idle";
/// Lineage words on a forked session's row (Program V4): "forked from
/// <short id> at turn <n>". Shared by the app targets and `modron sessions`.
pub const FORKED_FROM_LABEL: &str = "forked from";
pub const FORK_AT_TURN_LABEL: &str = "at turn";
/// Settings row labels, in screen order.
pub const SETTING_MODEL_LABEL: &str = "model";
pub const SETTING_PROVIDER_LABEL: &str = "provider";
pub const SETTING_APPROVAL_LABEL: &str = "approval mode";
pub const SETTING_PHASE_LABEL: &str = "phase";
pub const SETTING_EFFORT_LABEL: &str = "effort";
pub const SETTING_SANDBOX_LABEL: &str = "sandbox";
pub const SETTING_CONTEXT_LABEL: &str = "context";
pub const SETTING_RULES_LABEL: &str = "rules";
pub const SETTING_COMPACT_LABEL: &str = "compact";
/// Shown as the value of an unset effort, and of an empty rule list.
pub const EFFORT_UNSET_LABEL: &str = "unset";
pub const RULES_NONE_LABEL: &str = "none";
/// Settings rows added with the pickers and the removable lists.
pub const SETTING_THEME_LABEL: &str = "theme";
pub const SETTING_AGENT_LABEL: &str = "agent";
pub const SETTING_ALLOW_LABEL: &str = "allow";
pub const SETTING_DENY_LABEL: &str = "deny";
/// The picker row shown when the server lists no personas (the shared word
/// for an empty list, as the rules row uses).
pub const PICKER_NONE_LABEL: &str = "none";
/// The agent picker's first row: it clears the persona (`set_config` with an
/// empty name), back to the default prompt. Marked current when no persona is
/// set.
pub const PERSONA_CLEAR_LABEL: &str = "no persona";
/// The web control that removes the row it sits on.
pub const REMOVE_LABEL: &str = "remove";

/// The inline confirmation before a settings row is removed. `line` is the
/// row's shared text (`rules: allow read`, `allow: bash`).
pub fn remove_confirm_text(line: &str) -> String {
    format!("remove {line}? {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}")
}

/// The permission rules without the one at `index`, as the wire rules a
/// `ConfigPatch.rules` takes (it replaces the whole set). Each snapshot line
/// is `effect spec`: the first space splits the two. None when `index` is not
/// a row.
pub fn rules_without(lines: &[String], index: usize) -> Option<Vec<RuleWire>> {
    if index >= lines.len() {
        return None;
    }
    Some(
        lines
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != index)
            .map(|(_, line)| {
                let (effect, spec) = line.split_once(' ').unwrap_or((line.as_str(), ""));
                RuleWire {
                    effect: effect.to_string(),
                    spec: spec.to_string(),
                }
            })
            .collect(),
    )
}

/// A tool allow or deny list without the exact entry `tool`, in its order.
pub fn tools_without(list: &[String], tool: &str) -> Vec<String> {
    list.iter()
        .filter(|entry| entry.as_str() != tool)
        .cloned()
        .collect()
}

// Packet C3f: the shared words of the slash commands, help, the model
// picker, confirmations and the sub-agent screen. Every client shows these.

/// Shown for a slash command the table does not know.
pub const SLASH_UNKNOWN_LABEL: &str = "unknown command - try /help";
/// The popup's about line for a project command (`.claude/commands/<name>.md`,
/// listed by the server's welcome).
pub const PROJECT_COMMAND_ABOUT: &str = "project command";
/// Shown for `/remote-control` with no bridge URL; the usage follows.
pub const SLASH_NEEDS_ARG_PREFIX: &str = "use ";
/// The title of the help screen and its two section headings.
pub const HELP_TITLE: &str = "help";
pub const HELP_KEYS_HEADING: &str = "keys";
pub const HELP_COMMANDS_HEADING: &str = "slash commands";
/// The model picker: row roles and the empty state.
pub const MODELS_EMPTY_LABEL: &str = "no models listed yet";
pub const MODEL_ROLE_CURRENT: &str = "current";
pub const MODEL_ROLE_CHEAP: &str = "recommended cheap";
pub const MODEL_ROLE_STRONG: &str = "recommended strong";
pub const MODEL_ROLE_FALLBACK: &str = "fallback";
/// Confirmation answers (the inline `y / n` line).
pub const CONFIRM_YES_LABEL: &str = "y";
pub const CONFIRM_NO_LABEL: &str = "n";
/// The sub-agent screen: its controls and states.
pub const AGENT_KILL_LABEL: &str = "kill";
pub const AGENT_STEER_PLACEHOLDER: &str = "steer this agent";
pub const AGENT_ENDED_LABEL: &str = "ended";
pub const AGENT_NONE_LABEL: &str = "no agent";
/// Shown when a session call runs with no session attached.
pub const NO_SESSION_LABEL: &str = "no session attached";
/// The sessions screen's delete control (Packet C3f).
pub const DELETE_LABEL: &str = "delete";

/// The app's own server, in the connection line (`started by modron-app`).
pub const OWN_SERVER_NOTE: &str = "started by modron-app";
/// Owner sign-in and the portal's session list (Packet C4g).
pub const SIGN_IN_LABEL: &str = "sign in";
pub const SIGN_OUT_LABEL: &str = "sign out";
pub const SIGN_IN_PLACEHOLDER: &str = "owner token";
/// The sign-in screen's one explanatory line (web): where the token comes from.
pub const SIGN_IN_HINT: &str = "the owner token is the OWNER_TOKEN set on the bridge.";
/// The web header's back control (the same action as Esc).
pub const BACK_LABEL: &str = "back";
pub const WRONG_TOKEN_LABEL: &str = "wrong token";
pub const TOO_MANY_ATTEMPTS_LABEL: &str = "too many attempts";
pub const SIGN_IN_FAILED_LABEL: &str = "sign-in failed";
pub const REFRESH_LABEL: &str = "refresh";
pub const ONLINE_LABEL: &str = "online";
pub const OFFLINE_LABEL: &str = "offline";
pub const UNTITLED_LABEL: &str = "untitled";
/// Invite links for the connected session (owner only).
pub const SHARE_LABEL: &str = "share";
pub const VIEWER_LABEL: &str = "viewer";
pub const APPROVER_LABEL: &str = "approver";
pub const COPY_LABEL: &str = "copy";
pub const COPIED_LABEL: &str = "copied";
pub const REVOKE_LABEL: &str = "revoke";
pub const NO_INVITES_LABEL: &str = "no invites";

// Web push (the Settings row of the web host; the terminal never shows it).
pub const PUSH_LABEL: &str = "notify this device";
pub const PUSH_ON_LABEL: &str = "on";
pub const PUSH_OFF_LABEL: &str = "off";
pub const PUSH_UNAVAILABLE_LABEL: &str = "unavailable";
pub const PUSH_HOME_SCREEN_HINT: &str = "on iPhone, add this page to the Home Screen first";
pub const PUSH_BLOCKED_LABEL: &str = "notifications are blocked for this site";
pub const PUSH_NOT_ALLOWED_LABEL: &str = "notifications were not allowed";
pub const PUSH_FAILED_LABEL: &str = "could not change notifications";

/// Parity D2: the credentials screen (`/auth`, and the Settings auth row).
/// The row label and the first-run hint are the old TUI's words
/// (crates/modron-tui/src/lib.rs, the auth manager and the idle hints).
pub const SETTING_AUTH_LABEL: &str = "auth";
pub const AUTH_TITLE_LABEL: &str = "auth";
/// The provider status the core reports for a provider with no credentials
/// (crates/modron-protocol AuthProviderWire.status).
pub const AUTH_NOT_CONFIGURED: &str = "not configured";
/// Shown on the first run when no provider is signed in (the old TUI's
/// idle hint, lib.rs HINTS, with an ASCII dash).
pub const AUTH_FIRST_RUN_HINT: &str = "try /auth: sign in or reuse existing logins";
pub const AUTH_NO_PROVIDERS_LABEL: &str = "no providers listed yet";
/// Row hints: an OAuth-capable provider signs in through the browser on the
/// server; a key-only provider takes a pasted key (old TUI AuthMethod labels).
pub const AUTH_SIGN_IN_HINT: &str = "sign in (browser)";
pub const AUTH_KEY_HINT: &str = "paste an API key";
pub const AUTH_KEY_SAVE_HINT: &str = "enter save - esc back";
pub const AUTH_KEY_PLACEHOLDER: &str = "paste an API key";
pub const AUTH_WAITING_LABEL: &str = "waiting for the sign-in... esc backs out";
pub const AUTH_CODE_LABEL: &str = "code";
pub const AUTH_VISIT_LABEL: &str = "open";

/// Usage lines for the task commands (copy, not prompt text).
pub const BTW_USAGE: &str = "use /btw <quick question>";
pub const PLAN_USAGE: &str = "use /plan <task>";
pub const AGENT_USAGE: &str = "use /agent <name>";
/// The canned task text (REVIEW_TASK, the BTW and PLAN suffixes, btw_task,
/// plan_task, learn_task, skill_slug) has one home in modron-view's slash
/// module. Re-exported under the same names so the app keeps compiling.
pub use modron_view::slash::{
    btw_task, learn_task, plan_task, skill_slug, BTW_SUFFIX, PLAN_SUFFIX, REVIEW_TASK,
};

/// Parity D2: app copy moved here so every client shares it. The Ctrl+T
/// note with no tool rows, the scrolled-up marker, and the second-Ctrl+C
/// hint of the terminal host (crates/modron-tui shows the same words).
pub const NO_TOOLS_NOTE: &str = "no tool activity yet";
pub const NEW_OUTPUT_BELOW_LABEL: &str = "new output below - end to follow";
pub const CTRL_C_QUIT_HINT: &str = "press ctrl+c again to quit";

/// The scrollback hint while the transcript is scrolled up, from the number
/// of lines above the view (0 is the top of the session). The terminal
/// (crates/modron-tui/src/lib.rs:3325-3329) and the app share these words.
pub fn scrolled_up_hint(lines_above: usize) -> String {
    if lines_above == 0 {
        "\u{2191} top of session \u{00b7} pgdn scroll \u{00b7} esc live".to_string()
    } else {
        format!("\u{2191} {lines_above} lines above \u{00b7} pgup/pgdn scroll \u{00b7} esc live")
    }
}

/// One credentials row: the core's label and status, verbatim, joined by
/// `: ` (the same shape as the settings rows).
pub fn auth_row_line(label: &str, status: &str) -> String {
    format!("{label}: {status}")
}

/// Whether a provider status counts as signed in (anything but the core's
/// not-configured word).
pub fn auth_is_signed_in(status: &str) -> bool {
    status != AUTH_NOT_CONFIGURED
}

/// The inline confirmation for deleting a session.
pub fn delete_confirm_text(title: &str) -> String {
    format!("delete {title}? {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}")
}

/// The inline confirmation before a provider's saved credentials are removed
/// (`auth_remove`). `label` is the provider's shared label.
pub fn credential_remove_confirm_text(label: &str) -> String {
    format!("remove {label} credentials? {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}")
}

/// The inline confirmation for killing a sub-agent.
pub fn kill_confirm_text(label: &str) -> String {
    format!("{AGENT_KILL_LABEL} {label}? {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}")
}

/// The smallest interval an automation may fire at, in minutes. Mirrors
/// `MIN_INTERVAL_MINUTES` in `crates/modron-server/src/automations.rs`: the
/// server clamps on load and refuses a smaller interval on add. Clients check
/// the form against this before sending; the server stays the authority, and
/// its refusal arrives as the snapshot's last error.
pub const AUTOMATION_MIN_INTERVAL_MINUTES: u32 = 5;
pub const AUTOMATIONS_TITLE_LABEL: &str = "automations";
pub const AUTOMATIONS_EMPTY_LABEL: &str = "no automations yet";
pub const AUTOMATIONS_NO_MATCH_LABEL: &str = "no automations match";
pub const AUTOMATION_ADD_LABEL: &str = "add";
pub const AUTOMATION_NAME_LABEL: &str = "name";
pub const AUTOMATION_PROMPT_LABEL: &str = "prompt";
pub const AUTOMATION_INTERVAL_LABEL: &str = "every (minutes)";
pub const AUTOMATION_NEVER_RUN_LABEL: &str = "never";
pub const AUTOMATION_FORM_HINT: &str = "tab next field - enter save - esc cancel";
pub const AUTOMATION_FIELDS_NEEDED: &str = "name, prompt and interval are needed";
pub const AUTOMATION_INTERVAL_NOT_A_NUMBER: &str = "interval must be a whole number of minutes";

/// One automation row: `name - every N min - last run TIME` (`never` before
/// its first run). The time is [`short_time`] of the server's stamp.
pub fn automation_row(automation: &modron_protocol::AutomationWire) -> String {
    let last = automation
        .last_run
        .as_deref()
        .map(short_time)
        .unwrap_or_else(|| AUTOMATION_NEVER_RUN_LABEL.to_string());
    format!(
        "{} \u{b7} every {} min \u{b7} last run {last}",
        automation.name, automation.interval_minutes
    )
}

/// The form's interval check: a whole number of minutes at or above
/// [`AUTOMATION_MIN_INTERVAL_MINUTES`]. Returns the refusal copy, None when
/// the text is acceptable.
pub fn automation_interval_error(text: &str) -> Option<String> {
    match text.trim().parse::<u32>() {
        Err(_) => Some(AUTOMATION_INTERVAL_NOT_A_NUMBER.to_string()),
        Ok(minutes) if minutes < AUTOMATION_MIN_INTERVAL_MINUTES => Some(format!(
            "interval must be at least {AUTOMATION_MIN_INTERVAL_MINUTES} minutes"
        )),
        Ok(_) => None,
    }
}

pub const TASKS_TITLE_LABEL: &str = "tasks";
pub const TASKS_EMPTY_LABEL: &str = "no tasks yet";
pub const TASKS_NO_MATCH_LABEL: &str = "no tasks match";
pub const TASK_NEW_LABEL: &str = "new task";
pub const TASK_TITLE_LABEL: &str = "title";
pub const TASK_MERGE_LABEL: &str = "merge";
pub const TASK_FORM_HINT: &str = "enter create - esc cancel";
pub const TASK_NO_BRANCH_LABEL: &str = "no branch";
pub const TASK_TITLE_NEEDED: &str = "a task needs a title";

/// One task row: `title - branch - running|idle`. A task is a session with
/// a branch; `running` is the session's task state (the caller reads it).
pub fn task_row(meta: &SessionMetaWire, running: bool) -> String {
    let title = if meta.title.trim().is_empty() {
        UNTITLED_LABEL.to_string()
    } else {
        meta.title.clone()
    };
    let branch = meta
        .task_branch
        .as_deref()
        .filter(|b| !b.is_empty())
        .unwrap_or(TASK_NO_BRANCH_LABEL);
    let state = if running { RUNNING_LABEL } else { IDLE_LABEL };
    format!("{title} \u{b7} {branch} \u{b7} {state}")
}

/// The latest `task_status` answer kept per task session: what the task
/// summary line needs (the branch and running state stay on the meta row).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskStatusRecord {
    pub ahead: u64,
    pub files: Vec<TaskFileStat>,
}

/// One task row for the tasks screen: the [`task_row`] text plus the
/// summary from the task's latest status (empty until one arrives).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRowView {
    pub session_id: Uuid,
    /// `title - branch - running|idle` ([`task_row`]).
    pub text: String,
    /// `ahead 3 - 5 files (+120 -14)` ([`task_status_summary`]), or empty
    /// when no `task_status` answer has arrived for this task.
    pub summary: String,
}

/// The task summary line: commits ahead of the base, the changed-file
/// count and the summed added/deleted lines. Only what the status carries.
pub fn task_status_summary(ahead: u64, files: &[TaskFileStat]) -> String {
    let added: u64 = files.iter().fold(0, |sum, f| sum.saturating_add(f.added));
    let deleted: u64 = files.iter().fold(0, |sum, f| sum.saturating_add(f.deleted));
    let count = if files.len() == 1 {
        "1 file".to_string()
    } else {
        format!("{} files", files.len())
    };
    format!("ahead {ahead} - {count} (+{added} -{deleted})")
}

/// The persona picker's one-line text: `name - summary`, or the name alone
/// when the persona has no prompt summary.
pub fn persona_row_text(persona: &PersonaWire) -> String {
    if persona.summary.is_empty() {
        persona.name.clone()
    } else {
        format!("{} - {}", persona.name, persona.summary)
    }
}

/// One persona picker row. `current` is the session's persona name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersonaRowView {
    pub name: String,
    /// `name - summary` ([`persona_row_text`]).
    pub text: String,
    pub current: bool,
}

/// One persona picker row for `persona`, marked current when its name is
/// the session's current persona.
pub fn persona_row(persona: &PersonaWire, current: Option<&str>) -> PersonaRowView {
    PersonaRowView {
        name: persona.name.clone(),
        text: persona_row_text(persona),
        current: current == Some(persona.name.as_str()),
    }
}

/// The inline confirmation for merging a task's branch into its base.
pub fn merge_confirm_text(branch: &str) -> String {
    format!("merge {branch} into its base? {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}")
}

/// The save-as-default copy (the model picker's web control and the
/// terminal's hint line). Shared so every client words it the same way.
pub const SAVE_DEFAULT_LABEL: &str = "save as default";
pub const SAVE_DEFAULT_KEY: &str = "shift+s";
pub const SAVE_DEFAULT_HINT: &str = "shift+s: save the highlighted model as the default";

/// A composer queue chip: `[n] text`, n counted from 1 (the next to run).
pub fn queue_chip(position: usize, text: &str) -> String {
    format!("[{position}] {text}")
}

/// A steer echo: the dim `> text` row for a steer accepted but not yet
/// persisted as a task.
pub fn steer_echo(text: &str) -> String {
    format!("> {text}")
}

/// The connection line: the state word, the address, and the note (when
/// set), joined with the middle dot.
pub fn connection_address_label(state: &str, address: &str, note: &str) -> String {
    if note.is_empty() {
        format!("{state} \u{b7} {address}")
    } else {
        format!("{state} \u{b7} {address} \u{b7} {note}")
    }
}

/// The start-up failure when `--continue` finds no session on the server.
pub fn no_session_to_continue(address: &str) -> String {
    format!("no session to continue on {address}: start one with modron-app-tui --new")
}

/// An ISO 8601 timestamp as `YYYY-MM-DD HH:MM` (UTC as the server sent it);
/// anything else is returned unchanged.
pub fn short_time(iso: &str) -> String {
    match (iso.get(..10), iso.get(11..16)) {
        (Some(day), Some(time)) if iso.as_bytes().get(10) == Some(&b'T') => {
            format!("{day} {time}")
        }
        _ => iso.to_string(),
    }
}

/// One portal sessions row: the title (`untitled` when blank), the host
/// label, the online or offline word, and the last heartbeat time.
pub fn portal_session_row(title: &str, host_label: &str, online: bool, updated_at: &str) -> String {
    let (title, subtitle) = portal_session_parts(title, host_label, online, updated_at);
    format!("{title} \u{b7} {subtitle}")
}

/// The two parts of a portal row: the title, and the subtitle (host, online
/// or offline word, last heartbeat time). `portal_session_row` joins them with
/// the middle dot, so the terminal's line and the web's two lines share words.
pub fn portal_session_parts(
    title: &str,
    host_label: &str,
    online: bool,
    updated_at: &str,
) -> (String, String) {
    let title = if title.trim().is_empty() {
        UNTITLED_LABEL
    } else {
        title
    };
    let state = if online { ONLINE_LABEL } else { OFFLINE_LABEL };
    let mut subtitle = format!("{host_label} \u{b7} {state}");
    if !updated_at.is_empty() {
        subtitle.push_str(" \u{b7} ");
        subtitle.push_str(&short_time(updated_at));
    }
    (title.to_string(), subtitle)
}

/// Cloud runs (web, owner only; PLAN 10.10): the Cloud screen's words.
pub const CLOUD_LABEL: &str = "cloud";
pub const CLOUD_ACCOUNTS_LABEL: &str = "github accounts";
pub const CLOUD_ACCOUNTS_EMPTY_LABEL: &str = "no github accounts yet";
pub const CLOUD_ACCOUNT_ADD_LABEL: &str = "add account";
pub const CLOUD_TOKEN_PLACEHOLDER: &str = "paste a GitHub access token";
pub const CLOUD_TOKEN_HINT: &str = "github token";
pub const CLOUD_RUN_LABEL: &str = "new run";
pub const CLOUD_ACCOUNT_PICK_LABEL: &str = "account";
pub const CLOUD_NO_ACCOUNT_LABEL: &str = "add a GitHub account first";
pub const CLOUD_REPO_PICK_LABEL: &str = "choose a repo";
pub const CLOUD_BRANCH_LABEL: &str = "branch";
pub const CLOUD_START_LABEL: &str = "start run";
pub const CLOUD_RUNS_LABEL: &str = "runs";
pub const CLOUD_RUNS_EMPTY_LABEL: &str = "no runs yet";
pub const CLOUD_STOP_LABEL: &str = "stop";
pub const CLOUD_WAKE_LABEL: &str = "wake";
pub const CLOUD_OPEN_LABEL: &str = "open";
pub const CLOUD_NOT_CONFIGURED_LABEL: &str = "the bridge has no cloud runner yet";
pub const CLOUD_FAILED_LABEL: &str = "cloud request failed";
pub const CLOUD_REPOS_EMPTY_LABEL: &str = "no repos match";
pub const CLOUD_PRIVATE_LABEL: &str = "private";
pub const CLOUD_PUBLIC_LABEL: &str = "public";
pub const RUN_STARTING_LABEL: &str = "starting";
pub const RUN_RUNNING_LABEL: &str = "running";
pub const RUN_SLEEPING_LABEL: &str = "sleeping";
pub const RUN_STOPPED_LABEL: &str = "stopped";
pub const RUN_FAILED_LABEL: &str = "failed";
/// The credentials rows whose key is in the owner's vault (web, owner).
pub const VAULT_SAVED_LABEL: &str = "saved in the vault";
pub const VAULT_NOTE_LABEL: &str = "the key is stored for your cloud hosts";
pub const VAULT_SAVE_FAILED_LABEL: &str = "could not save the key";

/// The inline confirmation for removing a linked GitHub account.
pub fn cloud_account_remove_text(login: &str) -> String {
    format!("remove {login}? {CONFIRM_YES_LABEL} / {CONFIRM_NO_LABEL}")
}

/// The words for a run's state from the bridge's state word. An unknown
/// state shows as the bridge sent it.
pub fn run_state_label(state: &str) -> String {
    match state {
        "starting" => RUN_STARTING_LABEL,
        "running" => RUN_RUNNING_LABEL,
        "sleeping" => RUN_SLEEPING_LABEL,
        "stopped" => RUN_STOPPED_LABEL,
        "failed" => RUN_FAILED_LABEL,
        other => return other.to_string(),
    }
    .to_string()
}

/// A refused GitHub token: GitHub's status when the bridge sent one. The
/// token itself is never shown.
pub fn token_refused_line(github_status: Option<u16>) -> String {
    match github_status {
        Some(status) => format!("GitHub refused the token (status {status})"),
        None => "GitHub refused the token".to_string(),
    }
}

/// One linked account row: the login and its kind (`token`, `oauth`).
pub fn account_line(login: &str, kind: &str) -> String {
    if kind.is_empty() {
        login.to_string()
    } else {
        format!("{login} \u{b7} {kind}")
    }
}

/// One repo picker row: the full name, private or public, and the last push.
pub fn repo_line(full_name: &str, private: bool, pushed_at: &str) -> String {
    let visibility = if private {
        CLOUD_PRIVATE_LABEL
    } else {
        CLOUD_PUBLIC_LABEL
    };
    if pushed_at.is_empty() {
        format!("{full_name} \u{b7} {visibility}")
    } else {
        format!(
            "{full_name} \u{b7} {visibility} \u{b7} {}",
            short_time(pushed_at)
        )
    }
}

/// One run row: `repo · branch · state (· reason) · started`.
pub fn run_line(repo: &str, branch: &str, state: &str, reason: &str, created_at: &str) -> String {
    let mut row = format!("{repo} \u{b7} {branch} \u{b7} {}", run_state_label(state));
    if !reason.trim().is_empty() {
        row.push_str(" \u{b7} ");
        row.push_str(reason.trim());
    }
    if !created_at.is_empty() {
        row.push_str(" \u{b7} ");
        row.push_str(&short_time(created_at));
    }
    row
}

/// One invite row: its role and when it expires.
pub fn invite_row(role: &str, expires_at: &str) -> String {
    format!("{role} \u{b7} expires {}", short_time(expires_at))
}

/// The web push row's line: the label and its value (`notify this device: on`).
pub fn push_line(value: &str) -> String {
    format!("{PUSH_LABEL}: {value}")
}

/// One sessions-list row: the title (the short id when untitled), the
/// running/idle word and the message count. Shells show it verbatim.
/// One model-picker row (the `models` answer, as the picker shows it).
/// `current` marks the session's model; `available` gates selection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRowView {
    pub id: String,
    pub provider: String,
    /// `id - provider - 1000k ctx` (the window is omitted when unknown).
    pub text: String,
    pub available: bool,
    pub current: bool,
}

/// The picker's one-line text for a catalog model. The context window
/// goes through the shared token formatter (`compact_tokens`).
pub fn model_row_text(model: &ModelWire) -> String {
    match model.context_window {
        Some(window) => format!(
            "{} - {} - {} ctx",
            model.id,
            model.provider,
            compact_tokens(window)
        ),
        None => format!("{} - {}", model.id, model.provider),
    }
}

/// One picker row. `current` is the session's model and provider (an
/// empty provider matches on the model name alone).
pub fn model_row(model: &ModelWire, current_model: &str, current_provider: &str) -> ModelRowView {
    ModelRowView {
        id: model.id.clone(),
        provider: model.provider.clone(),
        text: model_row_text(model),
        available: model.available,
        current: model.id == current_model
            && (current_provider.is_empty() || model.provider == current_provider),
    }
}

pub fn session_row(meta: &SessionMetaWire, running: bool) -> String {
    let title = if meta.title.trim().is_empty() {
        meta.id.to_string().chars().take(8).collect()
    } else {
        meta.title.clone()
    };
    let state = if running { RUNNING_LABEL } else { IDLE_LABEL };
    let mut row = format!(
        "{title} \u{b7} {state} \u{b7} {} messages",
        meta.message_count
    );
    if let Some(origin) = &meta.forked_from {
        row.push_str(" \u{b7} ");
        row.push_str(&fork_origin_text(origin.session_id, origin.turn));
    }
    row
}

/// "forked from <short id> at turn <n>": the lineage line for a forked
/// session. The short id is the first 8 characters, as an untitled row shows.
/// Takes the parent id and the turn so the CLI can call it from core types.
pub fn fork_origin_text(parent: Uuid, turn: u32) -> String {
    let short: String = parent.to_string().chars().take(8).collect();
    format!("{FORKED_FROM_LABEL} {short} {FORK_AT_TURN_LABEL} {turn}")
}

/// The next approval mode in the settings cycle: ask, yolo, deny, ask.
pub fn approval_mode_next(mode: Option<ApprovalModeWire>) -> ApprovalModeWire {
    match mode.unwrap_or(ApprovalModeWire::Ask) {
        ApprovalModeWire::Ask => ApprovalModeWire::ApproveAll,
        ApprovalModeWire::ApproveAll => ApprovalModeWire::Deny,
        ApprovalModeWire::Deny => ApprovalModeWire::Ask,
    }
}

/// The next reasoning effort: unset, low, medium, high, xhigh, max, unset.
pub fn effort_next(effort: Option<Effort>) -> Option<Effort> {
    match effort {
        None => Some(Effort::Low),
        Some(Effort::Low) => Some(Effort::Medium),
        Some(Effort::Medium) => Some(Effort::High),
        Some(Effort::High) => Some(Effort::Xhigh),
        Some(Effort::Xhigh) => Some(Effort::Max),
        Some(Effort::Max) => None,
    }
}

/// The other run phase: build and plan swap.
pub fn phase_next(phase: Phase) -> Phase {
    match phase {
        Phase::Build => Phase::Plan,
        Phase::Plan => Phase::Build,
    }
}

/// The next sandbox policy: off, seatbelt, bwrap, off.
pub fn sandbox_next(policy: modron_types::SandboxPolicy) -> modron_types::SandboxPolicy {
    use modron_types::SandboxPolicy as S;
    match policy {
        S::Off => S::Seatbelt,
        S::Seatbelt => S::Bubblewrap,
        S::Bubblewrap => S::Off,
    }
}

/// One pre-rendered transcript row for shells that cannot call Rust
/// helpers: the shared copy verbatim.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderedRow {
    /// The `TranscriptEntry` this row renders (typed consumers re-derive).
    pub entry: TranscriptEntry,
    /// The shared-format strings to display (usually one line).
    pub lines: Vec<String>,
}

/// Render one committed transcript row with the shared words: task echo
/// (`❯` prefix), assistant/user text, tool summary lines, swarm rows, the
/// TUI's token/cache usage arithmetic, model and compaction notes.
pub fn entry_lines(entry: &TranscriptEntry) -> Vec<String> {
    match entry {
        TranscriptEntry::Task { text } => vec![format!("❯ {text}")],
        TranscriptEntry::Note { text } => vec![text.clone()],
        TranscriptEntry::User { text } => vec![text.clone()],
        TranscriptEntry::Assistant { text } => vec![text.clone()],
        TranscriptEntry::ToolSummary { line, .. } => vec![line.clone()],
        TranscriptEntry::Swarm { lines } => lines.clone(),
        // `$ <command>` then the output rows, through the shared formatter.
        TranscriptEntry::Shell {
            command,
            lines,
            private,
            ..
        } => modron_view::shell_rows(command, lines, *private),
        TranscriptEntry::Model { model, reason } => vec![format!("model: {model} ({reason})")],
        TranscriptEntry::Usage { usage, .. } => {
            // Same arithmetic and words as the TUI's per-turn usage note
            // (`{total} tok · cache {pct}%`); the turn count lives in the
            // status line on mobile instead. The cache percentage is the
            // shared `cache_hit_pct` (the canonical arithmetic).
            let cache = match modron_view::cache_hit_pct(usage) {
                Some(pct) => format!("cache {pct:.0}%"),
                None => "cache —".to_string(),
            };
            vec![format!("{} tok · {}", usage.total(), cache)]
        }
        TranscriptEntry::Compaction {
            summary,
            summarized_events,
        } => vec![format!("compacted {summarized_events} events: {summary}")],
        TranscriptEntry::GenerativeUi { ui } => modron_view::render_ascii(ui),
        TranscriptEntry::Plan { items } => modron_view::plan_lines(items),
        // Reviewer verdict: the shared one-line summary, then the shared
        // finding rows (same words as every client).
        TranscriptEntry::Review {
            summary, findings, ..
        } => std::iter::once(summary.clone())
            .chain(findings.iter().map(|f| format!("  {f}")))
            .collect(),
        // Generative widgets render as ASCII on text surfaces (same
        // projection the TUI uses).
    }
}

// ---------------------------------------------------------------------------
// Pending approval view
// ---------------------------------------------------------------------------

/// The approval awaiting a decision, pre-summarized for the sheet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingApprovalView {
    pub request_id: String,
    /// Tool name.
    pub tool: String,
    /// One-line summary: `modron_view::approval_summary` (essential
    /// argument first, `[+N chars]` when truncated), shared with the TUI
    /// and desktop.
    pub args_summary: String,
    /// Full parsed arguments for the detail sheet.
    pub arguments: serde_json::Value,
    /// The full call as sanitized compact JSON, for the sheet's expandable
    /// block (never truncated; bidi/control characters replaced).
    pub arguments_text: String,
    /// The three decisions with their shared labels, protocol order.
    pub options: Vec<DecisionOption>,
}

/// One selectable decision on the approval sheet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub decision: Decision,
    /// Shared label (`allow once` / `allow all` / `deny`).
    pub label: String,
}

impl PendingApprovalView {
    pub fn new(request_id: String, call: &ToolCall) -> Self {
        Self::for_agent(request_id, call, None)
    }

    /// H3b: a delegated child's ask shows as `[agent] <summary>`; the
    /// top-level run's own calls (`agent: None`) keep the bare summary.
    pub fn for_agent(request_id: String, call: &ToolCall, agent: Option<&str>) -> Self {
        let full = serde_json::to_string(&call.arguments).unwrap_or_default();
        let summary = modron_view::approval_summary(
            &call.name,
            &call.arguments,
            modron_view::APPROVAL_SUMMARY_MAX,
        );
        Self {
            request_id,
            tool: call.name.clone(),
            args_summary: match agent {
                Some(agent) => format!("[{agent}] {summary}"),
                None => summary,
            },
            arguments: call.arguments.clone(),
            arguments_text: modron_view::sanitize_display(&full),
            options: DECISION_LABELS
                .iter()
                .map(|(d, label)| DecisionOption {
                    decision: *d,
                    label: (*label).to_string(),
                })
                .collect(),
        }
    }
}

/// One selectable decision on the plan block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDecisionOption {
    pub decision: PlanDecision,
    /// Shared label (`approve` / `revise`).
    pub label: String,
}

/// A proposed plan awaiting a decision (`PlanProposed`), pre-rendered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPlanView {
    /// The plan as checkbox rows (`modron_view::plan_lines`). Empty when
    /// the plan came from a Snapshot: its `LiveState` carries only the
    /// pending flag, not the items.
    pub lines: Vec<String>,
    /// The two decisions with their shared labels, protocol order.
    pub options: [PlanDecisionOption; 2],
}

impl PendingPlanView {
    pub fn new(items: &[PlanItem]) -> Self {
        Self {
            lines: modron_view::plan_lines(items),
            options: PLAN_DECISION_LABELS.map(|(decision, label)| PlanDecisionOption {
                decision,
                label: label.to_string(),
            }),
        }
    }
}

/// OAuth sign-in progress surfaced to remote clients.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthProgressView {
    pub provider: String,
    pub stage: String,
    pub message: String,
    pub url: Option<String>,
    pub user_code: Option<String>,
}

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

// ---------------------------------------------------------------------------
// View model + snapshot
// ---------------------------------------------------------------------------

/// The foldable session view behind the client's mutex. `history` is the
/// committed transcript; `new_entries` is the drain queue for polling UIs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SessionViewModel {
    pub session_id: Option<Uuid>,
    pub connection: ConnectionState,
    pub server_version: String,
    pub running: bool,
    pub streaming_text: String,
    pub streaming_reasoning: String,
    /// Whether streamed reasoning surfaces at all (AGENTS.md streaming
    /// invariant 5: reasoning streams only when the user opted in — the
    /// TUI's `/reasoning` and the desktop's reasoning chip, default off).
    /// While off, the fold drops reasoning deltas and the snapshot seeds
    /// an empty buffer.
    pub show_reasoning: bool,
    /// Live grouped tool strip (`ToolEvent::summary()` rows).
    pub turn_tools: Vec<ToolEvent>,
    /// Live swarm rows.
    pub subagents: Vec<SwarmRow>,
    /// The last committed tool group's events (the TUI's `tool_log` tail):
    /// what the tool detail panel shows once the live turn has no tools.
    /// Set by [`commit`] on every `ToolSummary`, replaced by a Snapshot.
    #[serde(default)]
    pub last_tool_turn: Vec<ToolEvent>,
    /// Approvals awaiting a decision, oldest first. The view shows the
    /// front; see the module docs for the queue rules.
    pub pending_approvals: VecDeque<PendingApprovalView>,
    /// Inbound frames this client could not decode (counted, not shown).
    pub undecodable_frames: u64,
    pub cumulative_usage: Usage,
    pub model: String,
    pub provider: String,
    pub goal: String,
    pub approval_mode: Option<ApprovalModeWire>,
    /// Sandbox for tool execution (the last config patch; the settings
    /// views cycle it).
    pub sandbox: modron_types::SandboxPolicy,
    /// Tool allow/deny lists (the last config patches).
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    /// Messages held for the next run, front first (`QueueChanged`
    /// replaces it; a Snapshot seeds it from `LiveState.queued`).
    pub queued: Vec<String>,
    /// Steered texts accepted but not yet persisted as a task echo. Each
    /// is removed when its persisted `Task` event arrives; turn end and
    /// a Snapshot clear the lot.
    pub pending_steers: Vec<String>,
    /// A proposed plan awaiting a decision (`PlanProposed` sets it,
    /// `PlanDecided` clears it).
    pub plan_pending: Option<PendingPlanView>,
    /// The run phase for future runs (`ConfigPatch.phase`, snapshot).
    /// Defaults to `build`.
    pub phase: Phase,
    /// The reasoning effort for future turns (`ConfigPatch.effort` /
    /// `clear_effort`, snapshot). `None` = provider default, no label.
    pub effort: Option<Effort>,
    /// The context occupancy (`ContextChanged`, snapshot); `None` hides
    /// the meter.
    pub context: Option<ContextGaugeWire>,
    /// Permission rules in evaluation order (`ConfigPatch.rules`,
    /// snapshot).
    pub rules: Vec<RuleWire>,
    /// Sessions list (answers to `list_sessions`).
    pub sessions: Vec<SessionMetaWire>,
    /// Scheduled automations (answers to `list_automations`; settings
    /// views render these).
    pub automations: Vec<modron_protocol::AutomationWire>,
    /// Provider auth rows (answers to `auth_providers`; the settings
    /// providers card renders these).
    pub providers: Vec<modron_protocol::AuthProviderWire>,
    /// The router's default-model picture (answers to
    /// `router_defaults_req`; the settings defaults card renders it).
    pub router_defaults: Option<modron_protocol::RouterDefaults>,
    /// The model catalog (answers to `models_req`), in the server's order.
    pub models: Vec<ModelWire>,
    /// The configured personas (answers to `personas_req`), in server order.
    #[serde(default)]
    pub personas: Vec<PersonaWire>,
    /// Project command names from the welcome (`.claude/commands`), sorted.
    /// Empty from servers that predate them: every unknown `/word` is then a
    /// local notice (see `modron_protocol::Payload::Welcome`).
    #[serde(default)]
    pub project_commands: Vec<String>,
    /// The session's current persona (`LiveState.persona`, `ConfigPatch`).
    #[serde(default)]
    pub persona: Option<String>,
    /// The latest `task_status` answer per task session.
    #[serde(default)]
    pub task_statuses: BTreeMap<Uuid, TaskStatusRecord>,
    /// Last seen server seq for this session (reconnect resumes past it).
    pub last_seq: u64,
    pub turns_completed: u32,
    pub tool_calls_total: u32,
    /// Last `Error` frame or disconnect cause (transient toast material).
    pub last_error: Option<String>,
    /// Last `Note` frame text (slash-command results).
    pub last_note: Option<String>,
    pub oauth: Option<OAuthProgressView>,
    /// Committed transcript, in order, pre-rendered: the shared strings
    /// are produced once at commit time ([`commit`]), so a snapshot
    /// ([`ViewModelSnapshot::from_vm`], polled 4x/sec by the shell)
    /// clones finished rows instead of re-rendering every entry.
    pub history: Vec<RenderedRow>,
    /// Entries not yet drained by `take_new_entries`.
    pub new_entries: Vec<TranscriptEntry>,
}

/// Committed-transcript bound: both `history` and the `new_entries`
/// drain queue drop their oldest rows past this (the poll reads the
/// whole snapshot and nothing else drains the queue, so an uncapped
/// transcript would grow without bound on a long session).
const HISTORY_CAP: usize = 500;

/// Drop the oldest rows past [`HISTORY_CAP`] (FIFO; the display is the
/// most recent transcript either way).
fn cap_rows<T>(rows: &mut Vec<T>) {
    if rows.len() > HISTORY_CAP {
        rows.drain(..rows.len() - HISTORY_CAP);
    }
}

/// Render one committed row once: the typed entry plus its shared
/// strings ([`entry_lines`]).
fn rendered_row(entry: &TranscriptEntry) -> RenderedRow {
    RenderedRow {
        entry: entry.clone(),
        lines: entry_lines(entry),
    }
}

/// The events the tool detail panel shows (TUI `latest_tool_events`):
/// the live turn's tools when there are any, else the last committed group.
pub(crate) fn tool_detail_events(vm: &SessionViewModel) -> &[ToolEvent] {
    if vm.turn_tools.is_empty() {
        &vm.last_tool_turn
    } else {
        &vm.turn_tools
    }
}

impl SessionViewModel {
    /// The collapsed turn-tool line (`⏺ 21 tools · glob ×9 · read ×8 …`),
    /// straight from modron-view.
    pub fn turn_summary_line(&self) -> Option<String> {
        (!self.turn_tools.is_empty()).then(|| modron_view::tool_summary_line(&self.turn_tools))
    }
}

/// Cheap clone of the view state plus every string a shell renders
/// verbatim (TSX displays `status_line`/`tool_lines`/rows as-is).
/// One sub-agent row as the screens need it (Packet C3f): the row's id
/// (what `steer_agent` / `kill_agent` name), its label, the shared summary
/// text (`SwarmRow::summary`), and its lifecycle state with the shared
/// state word. `running` is true while the agent can still take a steer or
/// a kill; a finished row (done, failed, killed) is final.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRowView {
    pub id: String,
    pub label: String,
    pub text: String,
    pub state: modron_types::AgentState,
    pub state_word: String,
    pub running: bool,
}

impl AgentRowView {
    /// The view of one folded swarm row.
    pub fn from_row(row: &SwarmRow) -> Self {
        let state = row.effective_state();
        Self {
            id: row.id.clone(),
            label: row.label.clone(),
            text: row.summary(),
            state,
            state_word: modron_view::agent_state_word(state).to_string(),
            running: matches!(
                state,
                modron_types::AgentState::Running | modron_types::AgentState::WaitingApproval
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ViewModelSnapshot {
    pub connection: ConnectionState,
    pub connection_label: String,
    pub session_id: Option<Uuid>,
    pub server_version: String,
    /// The project command names the server listed in its welcome. The
    /// slash popup lists them after the built-ins, and a `/name` line is
    /// sent only for a listed name (the app's slash module).
    #[serde(default)]
    pub project_commands: Vec<String>,
    pub running: bool,
    /// `running` or `idle`.
    pub running_label: String,
    /// `<model> · <tok> tok · cache <pct>% · <mode>` — the TUI status
    /// strip words (spinner and key hints are TUI presentation only).
    pub status_line: String,
    pub streaming_text: String,
    pub streaming_reasoning: String,
    /// Whether reasoning streaming is opted in (default off; mirrored
    /// from the view model so shells can render the toggle's state).
    pub show_reasoning: bool,
    /// Per-event rows (`✓ read — 2.1 KB` / `● bash …`).
    pub tool_lines: Vec<String>,
    /// The collapsed per-turn line (`⏺ 21 tools · glob ×9 …`), if any.
    pub turn_summary_line: Option<String>,
    /// The tool detail panel rows (ctrl+t in the TUI), in order: every
    /// event of the live turn, or of the last committed tool group when
    /// the live turn has none (TUI `latest_tool_events`). Formatted by
    /// `ToolEvent::summary`, the TUI panel's own formatter.
    #[serde(default)]
    pub tool_detail: Vec<String>,
    /// The title over `tool_detail`: `tool_summary_line` of the same
    /// events. Empty when there is no tool activity to show.
    #[serde(default)]
    pub tool_detail_title: String,
    /// Live swarm rows (`⏵ [label] done · 3 turns · …`).
    pub subagent_rows: Vec<String>,
    /// The same sub-agents as structured rows (Packet C3f): ids for steer
    /// and kill, and the final state once an agent ends.
    pub agents: Vec<AgentRowView>,
    pub pending_approval: Option<PendingApprovalView>,
    pub approval_mode: Option<ApprovalModeWire>,
    /// `ask` / `yolo` / `deny`.
    pub approval_mode_label: String,
    /// `off` / `seatbelt` / `bwrap` (the desktop's sandbox words).
    pub sandbox: String,
    /// The sandbox policy itself (the settings cycle sends the next one).
    pub sandbox_policy: modron_types::SandboxPolicy,
    /// The run phase and the reasoning effort as values (the settings
    /// cycles start from them; the labels above are for display).
    pub phase: Phase,
    pub effort: Option<Effort>,
    /// One formatted row per `sessions` entry, in the same order
    /// ([`session_row`]). The sessions screen shows these verbatim.
    pub session_rows: Vec<String>,
    /// Tool allow/deny lists (editable in the settings views).
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
    /// Messages held for the next run, front first (the queue chips).
    pub queued: Vec<String>,
    /// Steers accepted but not yet echoed as persisted tasks (dim echoes).
    pub pending_steers: Vec<String>,
    /// The proposed plan awaiting a decision (the plan decision block).
    pub plan_pending: Option<PendingPlanView>,
    /// `plan` / `build`.
    pub phase_label: String,
    /// `high` etc., empty when the effort is unset (see `effort_label`).
    pub effort_label: String,
    /// `ctx 41% (74k/180k)`, empty when the context is unknown.
    pub context_label: String,
    /// Permission rules as `effect spec` lines (the settings view).
    pub rules: Vec<String>,
    pub cumulative_usage: Usage,
    pub cumulative_tokens: u64,
    /// Session-cumulative cache hit rate, `None` before any tokens.
    pub session_cache_pct: Option<f64>,
    pub turns_completed: u32,
    pub tool_calls_total: u32,
    pub model: String,
    pub provider: String,
    pub goal: String,
    pub sessions: Vec<SessionMetaWire>,
    /// Scheduled automations (settings views render these).
    pub automations: Vec<modron_protocol::AutomationWire>,
    /// Provider auth rows (the settings providers card renders these).
    pub providers: Vec<modron_protocol::AuthProviderWire>,
    /// The router's default-model picture (the settings defaults card).
    pub router_defaults: Option<modron_protocol::RouterDefaults>,
    /// The model picker rows, one per catalog entry, in the server's
    /// order ([`model_row`]). Unavailable rows are listed but not selectable.
    pub models: Vec<ModelRowView>,
    /// The persona picker rows, in server order; `current` marks the
    /// session's persona ([`persona_row`]). Empty when none are configured.
    #[serde(default)]
    pub personas: Vec<PersonaRowView>,
    /// The session's persona (`None` when none is set): the agent picker's
    /// clear row is marked current then ([`PERSONA_CLEAR_LABEL`]).
    #[serde(default)]
    pub persona: Option<String>,
    /// The tasks screen rows: every task session in list order, each with
    /// its summary ([`TaskRowView`]).
    #[serde(default)]
    pub task_rows: Vec<TaskRowView>,
    pub last_seq: u64,
    pub last_error: Option<String>,
    pub last_note: Option<String>,
    pub oauth: Option<OAuthProgressView>,
    /// Committed transcript, rendered (`RenderedRow.lines` is display-
    /// ready; `entry` is the typed row for rich shells).
    pub history: Vec<RenderedRow>,
}

impl ViewModelSnapshot {
    pub fn from_vm(vm: &SessionViewModel) -> Self {
        let detail = tool_detail_events(vm);
        // The shared cache-hit arithmetic (None before any tokens).
        let session_cache_pct = modron_view::cache_hit_pct(&vm.cumulative_usage);
        let mut status_line = if vm.model.is_empty() {
            "modron".to_string()
        } else {
            vm.model.clone()
        };
        // The effort sits next to the model, and only when set (H7a).
        if let Some(effort) = vm.effort {
            status_line.push_str(&format!(" · effort {}", effort.label()));
        }
        status_line.push_str(&format!(" · {} tok", vm.cumulative_usage.total()));
        if let Some(pct) = session_cache_pct {
            status_line.push_str(&format!(" · cache {pct:.0}%"));
        }
        status_line.push_str(&format!(" · {}", approval_mode_label(vm.approval_mode)));
        // Phase shows only in plan (build is the quiet default); the
        // context meter shows whenever the window is known.
        if vm.phase == Phase::Plan {
            status_line.push_str(&format!(" · {}", phase_label(vm.phase)));
        }
        let context = context_label(vm.context);
        if !context.is_empty() {
            status_line.push_str(&format!(" · {context}"));
        }
        Self {
            connection: vm.connection,
            connection_label: connection_label(&vm.connection).to_string(),
            session_id: vm.session_id,
            server_version: vm.server_version.clone(),
            running: vm.running,
            running_label: if vm.running { "running" } else { "idle" }.to_string(),
            status_line,
            streaming_text: vm.streaming_text.clone(),
            streaming_reasoning: vm.streaming_reasoning.clone(),
            show_reasoning: vm.show_reasoning,
            tool_lines: vm.turn_tools.iter().map(ToolEvent::summary).collect(),
            turn_summary_line: vm.turn_summary_line(),
            tool_detail: detail.iter().map(ToolEvent::summary).collect(),
            tool_detail_title: if detail.is_empty() {
                String::new()
            } else {
                modron_view::tool_summary_line(detail)
            },
            subagent_rows: vm.subagents.iter().map(SwarmRow::summary).collect(),
            agents: vm.subagents.iter().map(AgentRowView::from_row).collect(),
            pending_approval: vm.pending_approvals.front().cloned(),
            approval_mode: vm.approval_mode,
            approval_mode_label: approval_mode_label(vm.approval_mode).to_string(),
            sandbox: sandbox_label(vm.sandbox).to_string(),
            sandbox_policy: vm.sandbox,
            phase: vm.phase,
            effort: vm.effort,
            session_rows: vm
                .sessions
                .iter()
                .map(|meta| {
                    let running = meta.task_state.as_deref() == Some("running")
                        || (vm.running && vm.session_id == Some(meta.id));
                    session_row(meta, running)
                })
                .collect(),
            allow_tools: vm.allow_tools.clone(),
            deny_tools: vm.deny_tools.clone(),
            queued: vm.queued.clone(),
            pending_steers: vm.pending_steers.clone(),
            plan_pending: vm.plan_pending.clone(),
            phase_label: phase_label(vm.phase).to_string(),
            effort_label: effort_label(vm.effort),
            context_label: context_label(vm.context),
            rules: vm
                .rules
                .iter()
                .map(|rule| format!("{} {}", rule.effect, rule.spec))
                .collect(),
            cumulative_usage: vm.cumulative_usage,
            cumulative_tokens: vm.cumulative_usage.total(),
            session_cache_pct,
            turns_completed: vm.turns_completed,
            tool_calls_total: vm.tool_calls_total,
            model: vm.model.clone(),
            provider: vm.provider.clone(),
            goal: vm.goal.clone(),
            sessions: vm.sessions.clone(),
            automations: vm.automations.clone(),
            providers: vm.providers.clone(),
            router_defaults: vm.router_defaults.clone(),
            models: vm
                .models
                .iter()
                .map(|model| model_row(model, &vm.model, &vm.provider))
                .collect(),
            project_commands: vm.project_commands.clone(),
            personas: vm
                .personas
                .iter()
                .map(|persona| persona_row(persona, vm.persona.as_deref()))
                .collect(),
            persona: vm.persona.clone(),
            task_rows: vm
                .sessions
                .iter()
                .filter(|meta| meta.task_branch.as_deref().is_some_and(|b| !b.is_empty()))
                .map(|meta| {
                    let running = meta.task_state.as_deref() == Some("running")
                        || (vm.running && vm.session_id == Some(meta.id));
                    TaskRowView {
                        session_id: meta.id,
                        text: task_row(meta, running),
                        summary: vm
                            .task_statuses
                            .get(&meta.id)
                            .map(|status| task_status_summary(status.ahead, &status.files))
                            .unwrap_or_default(),
                    }
                })
                .collect(),
            last_seq: vm.last_seq,
            last_error: vm.last_error.clone(),
            last_note: vm.last_note.clone(),
            oauth: vm.oauth.clone(),
            // Rows are rendered at commit time (`commit`/`fold_snapshot`);
            // this is a plain clone per poll, never a re-render. (The one
            // commit outside the per-event flow is the held ToolSummary
            // tail at the turn boundary — `FoldState::flush_tools` — and
            // it renders exactly the same once.)
            history: vm.history.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Incremental folding state (Event frames through ReplayFolder)
// ---------------------------------------------------------------------------

/// Bookkeeping for folding live `Event` frames: a
/// [`modron_view::ReplayFolder`] IS the replay algorithm, so `fold_event`
/// is a single `folder.push` (O(new rows), never a re-replay of the log)
/// and a live fold groups tools exactly like a whole-log `replay` by
/// construction. The mobile-only bits (streaming-buffer clears, usage
/// accumulation, the committed-tools guard) stay here, fed from the
/// entries each push returns.
///
/// The folder holds a trailing run of `ToolResult` events open (no
/// summary row yet) until the next boundary event — replay's own
/// end-of-log behavior. `flush_tools` mirrors that flush at the turn
/// boundary: the still-open group is re-folded through `replay` (the
/// naming prefix rides along so call ids recover their tool names), the
/// held summary commits like any other row, and the folder resets to the
/// prefix (the old `reset_to_prefix`) so the next turn starts clean.
#[derive(Default)]
pub(crate) struct FoldState {
    pub tracker: ActivityTracker,
    folder: ReplayFolder,
    /// The naming prefix: the most recent assistant `Message` (it carries
    /// `tool_calls`); replayed into a fresh folder after every reset.
    prefix: Vec<SessionEvent>,
    /// `ToolResult` events of the still-open tool group (the folder
    /// flushed it already once a boundary arrived). Empty at every
    /// boundary; the turn-end flush commits what it holds.
    open_group: Vec<SessionEvent>,
    /// Whether this turn already committed a tool summary (via the
    /// Event/ToolResult path); guards the live-strip commit at turn end
    /// against servers that send both.
    pub committed_tools: bool,
}

impl FoldState {
    /// Commit what the folder still holds open (end of the current tool
    /// group), then reset it to the naming prefix.
    pub fn flush_tools(&mut self, vm: &mut SessionViewModel) {
        if !self.open_group.is_empty() {
            let mut log = Vec::with_capacity(self.prefix.len() + self.open_group.len());
            log.extend_from_slice(&self.prefix);
            log.extend_from_slice(&self.open_group);
            // The prefix's own rows committed when it was folded live;
            // only the group's held summary is new.
            let entries = replay(&log);
            let skip = replay(&self.prefix).len().min(entries.len());
            for entry in &entries[skip..] {
                if matches!(entry, TranscriptEntry::ToolSummary { .. }) {
                    self.committed_tools = true;
                }
                commit(vm, entry.clone());
            }
        }
        self.reset_to_prefix();
    }

    /// Reset the folder to the naming prefix: the group's rows are all
    /// committed by now, so later events fold from the prefix forward
    /// (a fresh tool run, named by the prefix's `tool_calls`).
    fn reset_to_prefix(&mut self) {
        self.folder.reset();
        for event in &self.prefix {
            // The prefix's own rows committed when it was folded live;
            // only the folder's naming state (call ids) matters here.
            let _ = self.folder.push(event);
        }
        self.open_group.clear();
    }

    /// Fold one live `SessionEvent` into committed rows.
    pub fn fold_event(&mut self, vm: &mut SessionViewModel, event: SessionEvent) {
        // A persisted task echo settles its pending steer (the server
        // injects a steer as a `Task` event, same text): the pending echo
        // gives way to the committed row below, in transcript order.
        if let SessionEvent::Task { text } = &event {
            if let Some(index) = vm.pending_steers.iter().position(|steer| steer == text) {
                vm.pending_steers.remove(index);
            }
        }
        // A persisted assistant message supersedes the streamed buffer
        // (the replayed row carries the authoritative text).
        if let SessionEvent::Message { message, .. } = &event {
            if message.role == Role::Assistant {
                vm.streaming_text.clear();
                vm.streaming_reasoning.clear();
            }
        }
        // The single incremental push: the folder returns exactly the
        // rows this event lands (a boundary event flushes the open tool
        // group first, a ToolResult joins it). A trailing ToolSummary
        // stays open inside the folder until the next boundary — the
        // old `held` bookkeeping, done by the shared fold.
        for entry in self.folder.push(&event) {
            if matches!(entry, TranscriptEntry::ToolSummary { .. }) {
                self.committed_tools = true;
            }
            commit(vm, entry.clone());
        }
        // Usage accounting: persisted Usage events are ground truth.
        if let SessionEvent::Usage { usage, .. } = event {
            vm.cumulative_usage.input_tokens += usage.input_tokens;
            vm.cumulative_usage.output_tokens += usage.output_tokens;
            vm.cumulative_usage.cached_input_tokens += usage.cached_input_tokens;
        }
        // Track the open group for the turn-end flush (`flush_tools`):
        // the naming prefix rolls forward on every persisted assistant
        // message, and ToolResults accumulate until the next boundary
        // clears them.
        match &event {
            SessionEvent::ToolResult { .. } => self.open_group.push(event),
            SessionEvent::Message { message, .. } if message.role == Role::Assistant => {
                self.prefix = vec![event];
                self.open_group.clear();
            }
            _ => self.open_group.clear(),
        }
    }

    /// Reset everything from a snapshot's live half.
    pub fn seed_from_live(&mut self, vm: &mut SessionViewModel, live: &LiveState) {
        self.tracker = ActivityTracker {
            turn_tools: live.turn_tools.clone(),
            subagents: live
                .subagents
                .iter()
                .map(|row| SwarmRow {
                    id: row.id.clone(),
                    label: row.label.clone(),
                    turns: row.turns,
                    tokens: row.tokens,
                    last_tool: row.last_tool.clone(),
                    done: row.done,
                    started_ms: None,
                    state: row.state,
                })
                .collect(),
        };
        vm.turn_tools = self.tracker.turn_tools.clone();
        vm.subagents = self.tracker.subagents.clone();
        self.folder.reset();
        self.prefix.clear();
        self.open_group.clear();
        self.committed_tools = false;
    }

    /// Fold one live `Tool` frame through the canonical tracker.
    pub fn fold_tool(&mut self, vm: &mut SessionViewModel, activity: &ToolActivity) {
        self.tracker.on_tool(activity);
        vm.turn_tools = self.tracker.turn_tools.clone();
        vm.subagents = self.tracker.subagents.clone();
    }
}

/// Append a committed row to history and the drain queue, rendering the
/// shared strings ONCE here (a poll clones the finished rows; it never
/// re-runs `entry_lines`). Both stores drop their oldest rows past
/// [`HISTORY_CAP`].
pub(crate) fn commit(vm: &mut SessionViewModel, entry: TranscriptEntry) {
    if let TranscriptEntry::ToolSummary { events, .. } = &entry {
        vm.last_tool_turn = events.clone();
    }
    vm.history.push(rendered_row(&entry));
    cap_rows(&mut vm.history);
    vm.new_entries.push(entry);
    cap_rows(&mut vm.new_entries);
}

/// Fold a snapshot: `replay(events)` replaces the committed history in
/// full, the live half seeds the in-flight view, and the naming prefix
/// rolls forward from the log. Usage resets to the snapshot's cumulative
/// value so a reconnect never double-counts.
pub(crate) fn fold_snapshot(
    vm: &mut SessionViewModel,
    fold: &mut FoldState,
    session_id: Uuid,
    last_seq: u64,
    events: &[SessionEvent],
    live: &LiveState,
) {
    vm.session_id = Some(session_id);
    vm.last_seq = vm.last_seq.max(last_seq);
    // Snapshot replay becomes the committed history: rendered once here,
    // never again per poll. (The `new_entries` batch carries the typed
    // entries for incremental shells.)
    let entries = replay(events);
    let rows: Vec<RenderedRow> = entries.iter().map(rendered_row).collect();
    // Same rule as `commit`: the newest replayed tool group, or none.
    vm.last_tool_turn = entries
        .iter()
        .rev()
        .find_map(|entry| match entry {
            TranscriptEntry::ToolSummary { events, .. } => Some(events.clone()),
            _ => None,
        })
        .unwrap_or_default();
    vm.history = rows;
    vm.new_entries = entries;
    cap_rows(&mut vm.history);
    cap_rows(&mut vm.new_entries);
    fold.prefix = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event,
                SessionEvent::Message {
                    message,
                    ..
                } if message.role == Role::Assistant
            )
        })
        .cloned()
        .into_iter()
        .collect();
    fold.reset_to_prefix();
    vm.streaming_text = live.streaming_text.clone();
    // Reasoning is opt-in (invariant 5): a snapshot's live reasoning
    // buffer only seeds when the user asked for it, same as the fold's
    // delta gate. `streaming_text` always seeds — text streaming is on
    // by definition.
    vm.streaming_reasoning = if vm.show_reasoning {
        live.streaming_reasoning.clone()
    } else {
        String::new()
    };
    vm.running = live.running;
    vm.model = live.model.clone();
    vm.provider = live.provider.clone();
    vm.goal = live.goal.clone();
    vm.persona = live.persona.clone();
    // H1-H9b: queue, steer echoes, phase, plan, context and rules are the
    // snapshot's whole truth (a reattach never keeps stale values).
    vm.queued = live.queued.clone();
    vm.pending_steers.clear();
    vm.phase = live.phase.unwrap_or_default();
    vm.effort = live.effort;
    // The Snapshot carries only the pending flag, so the plan's lines stay
    // empty until a live `PlanProposed` arrives.
    vm.plan_pending = live
        .plan_pending
        .then(|| PendingPlanView::new(&[] as &[PlanItem]));
    vm.context = live.context;
    vm.rules = live.rules.clone();
    // C5: the server's real approval mode and sandbox (an absent field
    // keeps the client's default), and the server's allow/deny lists, so
    // the tool editor starts from what the server holds.
    if let Some(mode) = live.approval_mode {
        vm.approval_mode = Some(mode);
    }
    if let Some(policy) = live.sandbox {
        vm.sandbox = policy;
    }
    vm.allow_tools = live.allow.clone();
    vm.deny_tools = live.deny.clone();
    vm.cumulative_usage = live.cumulative_usage;
    // The snapshot is the whole truth for approvals: its one pending
    // request (if any) replaces whatever was queued before the reattach.
    vm.pending_approvals.clear();
    if let Some(PendingApproval {
        request_id,
        call,
        agent,
    }) = &live.pending_approval
    {
        enqueue_approval(vm, request_id.clone(), call, agent.as_deref());
    }
    // Counters are re-derived from the log, not accumulated onto the old
    // values (a reattach would otherwise double-count).
    let (turns, tool_calls) = count_log(events);
    vm.turns_completed = turns;
    vm.tool_calls_total = tool_calls;
    fold.seed_from_live(vm, live);
}

/// Session counters from a persisted log, the same units the live
/// `TurnDone` summary reports: one model turn per per-turn `TurnTrace`
/// record (verifier records and the terminal stop record are not turns),
/// and one tool call per `ToolResult` (the engine appends one per executed
/// call).
pub(crate) fn count_log(events: &[SessionEvent]) -> (u32, u32) {
    let mut turns = 0u32;
    let mut tool_calls = 0u32;
    for event in events {
        match event {
            SessionEvent::TurnTrace {
                verifier: None,
                failure_label: None,
                ..
            } => turns += 1,
            SessionEvent::ToolResult { .. } => tool_calls += 1,
            _ => {}
        }
    }
    (turns, tool_calls)
}

/// Queue an approval request. A repeat of an id already queued replaces
/// that entry in place (no double prompt for one request).
pub(crate) fn enqueue_approval(
    vm: &mut SessionViewModel,
    request_id: String,
    call: &ToolCall,
    agent: Option<&str>,
) {
    let view = PendingApprovalView::for_agent(request_id, call, agent);
    match vm
        .pending_approvals
        .iter()
        .position(|p| p.request_id == view.request_id)
    {
        Some(index) => vm.pending_approvals[index] = view,
        None => vm.pending_approvals.push_back(view),
    }
}

/// Remove one request from the queue wherever it sits. Unknown ids are a
/// no-op (the server echo of an answer the client already removed).
pub(crate) fn resolve_approval(vm: &mut SessionViewModel, request_id: &str) {
    vm.pending_approvals.retain(|p| p.request_id != request_id);
}

/// `TurnDone`: held tool rows commit, the streaming buffer commits as the
/// assistant turn (or the summary answer when the server never streamed),
/// the swarm block commits, and the live view goes idle.
pub(crate) fn fold_turn_done(
    vm: &mut SessionViewModel,
    fold: &mut FoldState,
    summary: &WireRunSummary,
) {
    fold.flush_tools(vm);
    // TUI parity: the turn's live tool strip lands as ONE grouped summary
    // line, committed before the answer — unless the Event path already
    // committed one.
    if !fold.committed_tools && !fold.tracker.turn_tools.is_empty() {
        commit(
            vm,
            TranscriptEntry::ToolSummary {
                line: modron_view::tool_summary_line(&fold.tracker.turn_tools),
                events: fold.tracker.turn_tools.clone(),
            },
        );
    }
    fold.committed_tools = false;
    let mut text = std::mem::take(&mut vm.streaming_text);
    if text.is_empty() {
        text = summary.answer.clone();
    }
    // Servers that persist the assistant message AND send TurnDone would
    // double-commit; the identical last assistant row suppresses that.
    let last_assistant = vm.history.iter().rev().find_map(|row| match &row.entry {
        TranscriptEntry::Assistant { text } => Some(text.as_str()),
        _ => None,
    });
    let already_committed = last_assistant == Some(text.as_str());
    if !text.is_empty() && !already_committed {
        commit(vm, TranscriptEntry::Assistant { text });
    }
    vm.streaming_reasoning.clear();
    let lines = fold.tracker.take_swarm_summary();
    if !lines.is_empty() {
        commit(vm, TranscriptEntry::Swarm { lines });
    }
    // The tracker is the source `fold_tool` re-syncs from: clear it with
    // the view, or the next turn's first tool event re-adds this turn's rows.
    fold.tracker.clear_turn();
    vm.turn_tools.clear();
    vm.subagents.clear();
    vm.pending_steers.clear();
    vm.running = false;
    vm.pending_approvals.clear();
    vm.turns_completed += summary.turns;
    vm.tool_calls_total += summary.tool_calls;
}

/// `TurnFailed`/`TurnAborted`: persisted rows commit (they already
/// happened), streaming buffers clear uncommitted (TUI abort parity),
/// and the live view goes idle. `error` surfaces as `last_error`.
pub(crate) fn fold_turn_unfinished(
    vm: &mut SessionViewModel,
    fold: &mut FoldState,
    error: Option<String>,
) {
    let _ = fold.tracker.take_swarm_summary();
    fold.flush_tools(vm);
    // TUI interrupt parity: the partial strip commits as a summary line.
    if !fold.committed_tools && !fold.tracker.turn_tools.is_empty() {
        commit(
            vm,
            TranscriptEntry::ToolSummary {
                line: modron_view::tool_summary_line(&fold.tracker.turn_tools),
                events: fold.tracker.turn_tools.clone(),
            },
        );
    }
    fold.committed_tools = false;
    vm.streaming_text.clear();
    vm.streaming_reasoning.clear();
    // The tracker is the source `fold_tool` re-syncs from: clear it with
    // the view, or the next turn's first tool event re-adds this turn's rows.
    fold.tracker.clear_turn();
    vm.turn_tools.clear();
    vm.subagents.clear();
    vm.pending_steers.clear();
    vm.running = false;
    vm.pending_approvals.clear();
    if let Some(error) = error {
        vm.last_error = Some(error);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn goal_label_hides_an_empty_goal_and_cuts_a_long_one() {
        use super::{goal_label, GOAL_MAX_CHARS};
        assert_eq!(goal_label(""), "");
        assert_eq!(goal_label("   "), "");
        assert_eq!(goal_label(" ship it "), "[goal: ship it]");
        let long = "x".repeat(GOAL_MAX_CHARS + 5);
        assert_eq!(
            goal_label(&long),
            format!("[goal: {}\u{2026}]", "x".repeat(GOAL_MAX_CHARS))
        );
    }

    #[test]
    fn context_note_words_the_gauge_tokens_and_cache() {
        use super::context_note;
        assert_eq!(
            context_note("ctx 41% (74k/180k)", 1234, Some(33.0)),
            "context: ctx 41% (74k/180k) \u{b7} 1234 tok \u{b7} cache 33%"
        );
        assert_eq!(
            context_note("", 0, None),
            "context: ctx window unknown \u{b7} 0 tok"
        );
    }

    use super::*;
    use modron_types::{Message, ToolOutcome};

    fn assistant_with_calls(content: &str, call_id: &str) -> SessionEvent {
        let mut message = Message::assistant(content);
        message.tool_calls = vec![ToolCall {
            id: call_id.into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        }];
        SessionEvent::Message {
            parent_id: None,
            message,
        }
    }

    #[test]
    fn pending_approval_summary_is_the_shared_string() {
        let long = "p".repeat(200);
        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": long.clone() }),
        };
        let view = PendingApprovalView::new("r".into(), &call);
        assert_eq!(
            view.args_summary,
            modron_view::approval_summary(
                "bash",
                &call.arguments,
                modron_view::APPROVAL_SUMMARY_MAX
            )
        );
        assert!(
            view.args_summary.ends_with(" [+120 chars]"),
            "{}",
            view.args_summary
        );
        // The sheet's expandable block carries the whole call, untruncated.
        assert!(view.arguments_text.contains(&long));

        // Bidi overrides cannot reorder the full-call text either.
        let call = ToolCall {
            id: "c".into(),
            name: "write".into(),
            arguments: serde_json::json!({ "path": "a\u{202e}b" }),
        };
        let view = PendingApprovalView::new("r".into(), &call);
        assert_eq!(view.args_summary, r#"a?b"#);
        assert_eq!(view.arguments_text, r#"{"path":"a?b"}"#);
    }

    /// H3b: a child's ask reads `[agent] <summary>`; the run's own call
    /// keeps the shared summary unchanged.
    #[test]
    fn child_approval_summary_is_prefixed_with_its_agent() {
        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "ls" }),
        };
        let own = PendingApprovalView::new("r".into(), &call);
        assert_eq!(
            own.args_summary,
            PendingApprovalView::for_agent("r".into(), &call, None).args_summary
        );
        let child = PendingApprovalView::for_agent("r".into(), &call, Some("delegate"));
        assert_eq!(
            child.args_summary,
            format!("[delegate] {}", own.args_summary)
        );
        assert_eq!(child.tool, "bash");
    }

    #[test]
    fn decision_labels_match_the_shared_words() {
        assert_eq!(DECISION_LABELS[0], (Decision::AllowOnce, "allow once"));
        assert_eq!(DECISION_LABELS[1], (Decision::AllowAll, "allow all"));
        assert_eq!(DECISION_LABELS[2], (Decision::Deny, "deny"));
        for (i, decision) in Decision::ALL.into_iter().enumerate() {
            assert_eq!(DECISION_LABELS[i].0, decision);
            assert_eq!(DECISION_LABELS[i].1, decision.label());
        }
        assert_eq!(approval_mode_label(None), "ask");
        assert_eq!(
            approval_mode_label(Some(ApprovalModeWire::ApproveAll)),
            "yolo"
        );
    }

    #[test]
    fn snapshot_carries_mode_sandbox_and_lists() {
        // C5: attach to a yolo session: the label is yolo, the sandbox and
        // the allow/deny copy come from the server.
        let live = LiveState {
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            sandbox: Some(modron_types::SandboxPolicy::Bubblewrap),
            allow: vec!["diagnostics".into()],
            deny: vec!["bash".into()],
            ..LiveState::default()
        };
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        fold_snapshot(&mut vm, &mut fold, Uuid::nil(), 1, &[], &live);
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.approval_mode_label, "yolo");
        assert_eq!(snap.sandbox, "bwrap");
        assert_eq!(snap.allow_tools, vec!["diagnostics".to_string()]);
        assert_eq!(snap.deny_tools, vec!["bash".to_string()]);
        assert!(
            snap.status_line.ends_with(" · yolo"),
            "{}",
            snap.status_line
        );

        // An old snapshot without the fields: the label stays at its
        // default (ask) and the lists are empty.
        let mut fresh = SessionViewModel::default();
        fold_snapshot(
            &mut fresh,
            &mut fold,
            Uuid::nil(),
            1,
            &[],
            &LiveState::default(),
        );
        let snap = ViewModelSnapshot::from_vm(&fresh);
        assert_eq!(snap.approval_mode_label, "ask");
        assert!(snap.allow_tools.is_empty() && snap.deny_tools.is_empty());
    }

    #[test]
    fn snapshot_seeds_reasoning_only_when_opted_in() {
        // Invariant 5: reasoning streams only when opted in — also at the
        // snapshot boundary (a reconnect must not surface a live
        // reasoning buffer the user never asked to see).
        let session = Uuid::nil();
        let live = LiveState {
            streaming_reasoning: "thinking".into(),
            ..LiveState::default()
        };
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        fold_snapshot(&mut vm, &mut fold, session, 1, &[], &live);
        assert!(!vm.show_reasoning, "show_reasoning defaults off");
        assert_eq!(vm.streaming_reasoning, "", "not seeded while off");
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert!(!snap.show_reasoning);
        assert_eq!(snap.streaming_reasoning, "");
        // Opted in: the snapshot's live buffer seeds (mid-run attach).
        vm.show_reasoning = true;
        fold_snapshot(&mut vm, &mut fold, session, 1, &[], &live);
        assert_eq!(vm.streaming_reasoning, "thinking");
        assert!(ViewModelSnapshot::from_vm(&vm).show_reasoning);
    }

    #[test]
    fn event_stream_groups_tools_like_a_whole_log_replay() {
        // Folding event-by-event must produce the same rows as replaying
        // the entire log at once (the parity contract).
        let events = vec![
            SessionEvent::Task {
                text: "read a.rs".into(),
            },
            assistant_with_calls("looking at the file", "call-1"),
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("contents"),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::err("nope"),
            },
            SessionEvent::Message {
                parent_id: None,
                message: Message::assistant("done"),
            },
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 4,
                },
            },
        ];
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        for event in &events {
            fold.fold_event(&mut vm, event.clone());
        }
        fold.flush_tools(&mut vm);
        let incremental: Vec<TranscriptEntry> =
            vm.history.iter().map(|row| row.entry.clone()).collect();
        let whole = replay(&events);
        assert_eq!(incremental, whole, "incremental fold == one-shot replay");
    }

    #[test]
    fn turn_end_flush_commits_an_open_tool_group() {
        // A log ending in a run of ToolResults: replay commits the group
        // at end-of-log; the live fold holds it open inside the folder
        // and the turn boundary (`flush_tools`) commits the same row.
        let events = vec![
            SessionEvent::Task {
                text: "edit a.rs".into(),
            },
            assistant_with_calls("making the change", "call-1"),
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("written"),
            },
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::err("boom"),
            },
        ];
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        for event in &events {
            fold.fold_event(&mut vm, event.clone());
        }
        assert!(
            vm.history
                .iter()
                .all(|row| !matches!(row.entry, TranscriptEntry::ToolSummary { .. })),
            "the trailing group stays open before the flush"
        );
        fold.flush_tools(&mut vm);
        let incremental: Vec<TranscriptEntry> =
            vm.history.iter().map(|row| row.entry.clone()).collect();
        assert_eq!(incremental, replay(&events));
        assert!(fold.committed_tools, "the held summary marks the commit");
        // The next turn folds from the prefix forward: another result of
        // the same call lands in a FRESH group (one summary), and the
        // committed_tools guard resets with it.
        fold.committed_tools = false;
        fold.fold_event(
            &mut vm,
            SessionEvent::ToolResult {
                call_id: "call-1".into(),
                outcome: ToolOutcome::ok("more"),
            },
        );
        fold.flush_tools(&mut vm);
        let summaries = vm
            .history
            .iter()
            .filter(|row| matches!(row.entry, TranscriptEntry::ToolSummary { .. }))
            .count();
        assert_eq!(summaries, 2, "each turn's group lands as one summary");
    }

    #[test]
    fn fold_event_accumulates_usage() {
        let mut vm = SessionViewModel::default();
        let mut fold = FoldState::default();
        fold.fold_event(
            &mut vm,
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    cached_input_tokens: 0,
                },
            },
        );
        fold.fold_event(
            &mut vm,
            SessionEvent::Usage {
                model: "m".into(),
                usage: Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 3,
                },
            },
        );
        assert_eq!(vm.cumulative_usage.total(), 14);
    }

    #[test]
    fn snapshot_status_line_matches_tui_words() {
        let vm = SessionViewModel {
            model: "gpt-5.6-sol".into(),
            cumulative_usage: Usage {
                input_tokens: 300,
                output_tokens: 100,
                cached_input_tokens: 100,
            },
            approval_mode: Some(ApprovalModeWire::ApproveAll),
            ..SessionViewModel::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "gpt-5.6-sol · 400 tok · cache 33% · yolo");
        assert_eq!(snap.running_label, "idle");
    }

    #[test]
    fn shell_entries_render_through_the_shared_rows() {
        let rows = entry_lines(&TranscriptEntry::Shell {
            command: "ls".into(),
            lines: vec!["a.rs".into()],
            is_error: false,
            private: false,
        });
        assert_eq!(rows, vec!["$ ls", "a.rs"]);
        let rows = entry_lines(&TranscriptEntry::Shell {
            command: "env".into(),
            lines: vec![],
            is_error: false,
            private: true,
        });
        assert_eq!(rows, vec!["$ env [private]"]);
    }

    #[test]
    fn entry_lines_render_shared_copy() {
        let rows = entry_lines(&TranscriptEntry::Task {
            text: "fix it".into(),
        });
        assert_eq!(rows, vec!["❯ fix it"]);
        let rows = entry_lines(&TranscriptEntry::ToolSummary {
            line: "⏺ 1 tool · read".into(),
            events: vec![],
        });
        assert_eq!(rows, vec!["⏺ 1 tool · read"]);
        let rows = entry_lines(&TranscriptEntry::Usage {
            model: "m".into(),
            usage: Usage {
                input_tokens: 75,
                output_tokens: 25,
                cached_input_tokens: 25,
            },
        });
        assert_eq!(rows, vec!["100 tok · cache 33%"]);
    }

    fn steer_vm(texts: &[&str]) -> SessionViewModel {
        SessionViewModel {
            running: true,
            pending_steers: texts.iter().map(|t| t.to_string()).collect(),
            ..SessionViewModel::default()
        }
    }

    #[test]
    fn steer_echo_is_removed_by_its_persisted_task_event() {
        let mut vm = steer_vm(&["also check B", "and C"]);
        let mut fold = FoldState::default();
        fold.fold_event(
            &mut vm,
            SessionEvent::Task {
                text: "also check B".into(),
            },
        );
        assert_eq!(vm.pending_steers, vec!["and C".to_string()]);
        // The committed task row lands in history as usual.
        assert!(vm
            .history
            .iter()
            .any(|row| row.lines == vec!["❯ also check B".to_string()]));
        // An unrelated task leaves the pending echo alone.
        fold.fold_event(
            &mut vm,
            SessionEvent::Task {
                text: "something else".into(),
            },
        );
        assert_eq!(vm.pending_steers, vec!["and C".to_string()]);
    }

    #[test]
    fn steer_echoes_clear_on_abort_failure_and_turn_done() {
        let mut fold = FoldState::default();
        let mut vm = steer_vm(&["one"]);
        fold_turn_unfinished(&mut vm, &mut fold, None);
        assert!(vm.pending_steers.is_empty(), "abort clears steers");

        let mut vm = steer_vm(&["two"]);
        fold_turn_unfinished(&mut vm, &mut fold, Some("boom".into()));
        assert!(vm.pending_steers.is_empty(), "failure clears steers");

        let mut vm = steer_vm(&["three"]);
        let summary = WireRunSummary {
            session_id: Uuid::nil(),
            answer: String::new(),
            turns: 0,
            tool_calls: 0,
            usage: Usage::default(),
            models_used: Vec::new(),
        };
        fold_turn_done(&mut vm, &mut fold, &summary);
        assert!(vm.pending_steers.is_empty(), "turn done clears steers");
    }

    #[test]
    fn snapshot_resets_queue_steers_phase_context_and_rules() {
        let session = Uuid::new_v4();
        let mut vm = SessionViewModel {
            queued: vec!["stale".into()],
            pending_steers: vec!["stale steer".into()],
            plan_pending: Some(PendingPlanView::new(&[])),
            phase: Phase::Plan,
            context: Some(ContextGaugeWire { used: 1, window: 2 }),
            ..SessionViewModel::default()
        };
        let mut fold = FoldState::default();
        let live = LiveState {
            queued: vec!["next".into()],
            phase: Some(Phase::Build),
            context: Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000,
            }),
            rules: vec![RuleWire {
                effect: "deny".into(),
                spec: "bash(rm -rf*)".into(),
            }],
            ..LiveState::default()
        };
        fold_snapshot(&mut vm, &mut fold, session, 3, &[], &live);
        assert_eq!(vm.queued, vec!["next".to_string()]);
        assert!(vm.pending_steers.is_empty());
        assert!(vm.plan_pending.is_none(), "snapshot says not pending");
        assert_eq!(vm.phase, Phase::Build);
        assert_eq!(
            vm.context,
            Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000
            })
        );
        assert_eq!(vm.rules.len(), 1);

        // A snapshot that says a plan awaits keeps the block with no lines.
        let live = LiveState {
            plan_pending: true,
            ..LiveState::default()
        };
        fold_snapshot(&mut vm, &mut fold, session, 4, &[], &live);
        let plan = vm.plan_pending.clone().expect("pending plan on attach");
        assert!(plan.lines.is_empty(), "attach carries no items");
        assert_eq!(plan.options[0].label, "approve");
        assert_eq!(plan.options[1].label, "revise");
        // Absent context and phase reset to the defaults.
        assert_eq!(vm.phase, Phase::Build);
        assert!(vm.context.is_none());
    }

    #[test]
    fn plan_block_renders_items_and_the_shared_option_labels() {
        let items = vec![PlanItem {
            text: "Edit the parser".into(),
            done_criteria: "parser tests pass".into(),
            status: "pending".into(),
        }];
        let plan = PendingPlanView::new(&items);
        assert_eq!(plan.lines, vec!["○ Edit the parser - parser tests pass"]);
        assert_eq!(plan.options[0].decision, PlanDecision::Approve);
        assert_eq!(plan.options[0].label, "approve");
        assert_eq!(plan.options[1].decision, PlanDecision::Revise);
        assert_eq!(plan.options[1].label, "revise");
        assert_eq!(
            PLAN_DECISION_LABELS.map(|(_, label)| label),
            ["approve", "revise"]
        );
    }

    #[test]
    fn effort_shows_next_to_the_model_only_when_set() {
        let mut vm = SessionViewModel {
            model: "m".into(),
            ..SessionViewModel::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "m · 0 tok · ask");
        assert_eq!(snap.effort_label, "");

        vm.effort = Some(Effort::Xhigh);
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "m · effort xhigh · 0 tok · ask");
        assert_eq!(snap.effort_label, "xhigh");
        assert_eq!(effort_label(None), "");
    }

    #[test]
    fn the_welcome_banner_names_the_app_the_model_and_the_keys() {
        let lines = welcome_lines("claude-opus-5", "0.2.0");
        assert_eq!(lines[1], "\u{2726} modron v0.2.0");
        assert_eq!(lines[2], "  a token-efficient Rust coding agent");
        assert!(
            lines[4].starts_with("  model claude-opus-5  "),
            "{}",
            lines[4]
        );
        assert!(lines[4].contains("enter send") && lines[4].contains("esc interrupt"));
    }

    #[test]
    fn running_spinner_and_rotating_idle_hints_follow_the_clock_tick() {
        assert_eq!(RUNNING_HINT, "esc to interrupt");
        let frames: Vec<&str> = (0..5).map(spinner_frame).collect();
        assert_eq!(frames[0], SPINNER_FRAMES[0]);
        assert_eq!(frames[4], frames[0], "the spinner wraps after four frames");
        assert_ne!(frames[0], frames[1]);
        // Each hint holds for IDLE_HINT_TICKS ticks, then the next shows.
        assert_eq!(idle_hint(0), IDLE_HINTS[0]);
        assert_eq!(idle_hint(IDLE_HINT_TICKS - 1), IDLE_HINTS[0]);
        assert_eq!(idle_hint(IDLE_HINT_TICKS), IDLE_HINTS[1]);
        let wrap = IDLE_HINT_TICKS * IDLE_HINTS.len() as u64;
        assert_eq!(idle_hint(wrap), IDLE_HINTS[0]);
        assert!(IDLE_HINTS.iter().all(|h| !h.is_empty()));
    }

    #[test]
    fn context_label_warns_from_eighty_percent_with_the_old_marker() {
        let gauge = |used| Some(ContextGaugeWire { used, window: 180 });
        // 79.4% rounds to 79: no marker. 80% exactly: the marker follows it.
        assert_eq!(context_label(gauge(143)), "ctx 79% (143/180)");
        assert_eq!(context_label(gauge(144)), "ctx 80%! (144/180)");
        assert_eq!(context_label(gauge(180)), "ctx 100%! (180/180)");
        assert_eq!(CONTEXT_WARNING_PCT, 80);
        assert_eq!(CONTEXT_WARNING_MARK, "!");
    }

    #[test]
    fn context_label_formats_the_meter_and_hides_when_unknown() {
        assert_eq!(context_label(None), "");
        // A zero window has no percentage: the meter stays hidden.
        assert_eq!(
            context_label(Some(ContextGaugeWire { used: 5, window: 0 })),
            ""
        );
        assert_eq!(
            context_label(Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000
            })),
            "ctx 41% (74k/180k)"
        );
        assert_eq!(
            context_label(Some(ContextGaugeWire {
                used: 0,
                window: 200
            })),
            "ctx 0% (0/200)"
        );
    }

    #[test]
    fn status_line_shows_plan_phase_and_context_only_when_relevant() {
        let mut vm = SessionViewModel {
            model: "m".into(),
            context: Some(ContextGaugeWire {
                used: 74_000,
                window: 180_000,
            }),
            ..SessionViewModel::default()
        };
        // Build: no phase word; the context meter still shows.
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.status_line, "m · 0 tok · ask · ctx 41% (74k/180k)");
        assert_eq!(snap.phase_label, "build");
        assert_eq!(snap.context_label, "ctx 41% (74k/180k)");

        vm.phase = Phase::Plan;
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(
            snap.status_line,
            "m · 0 tok · ask · plan · ctx 41% (74k/180k)"
        );
        assert_eq!(snap.phase_label, "plan");
    }

    #[test]
    fn rules_and_queue_project_into_the_snapshot() {
        let vm = SessionViewModel {
            queued: vec!["a".into(), "b".into()],
            rules: vec![RuleWire {
                effect: "allow".into(),
                spec: "read".into(),
            }],
            ..SessionViewModel::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.queued, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(snap.rules, vec!["allow read".to_string()]);
        assert!(snap.context_label.is_empty());
        assert!(snap.plan_pending.is_none());
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use modron_types::SandboxPolicy;

    fn meta(id: &str, title: &str, count: u32, state: Option<&str>) -> SessionMetaWire {
        SessionMetaWire {
            id: Uuid::parse_str(id).expect("uuid"),
            title: title.into(),
            created_at: String::new(),
            updated_at: String::new(),
            message_count: count,
            task_branch: None,
            task_state: state.map(String::from),
            working_dir: None,
            forked_from: None,
        }
    }

    const ID_A: &str = "abcdef01-2345-6789-abcd-ef0123456789";
    const ID_B: &str = "11111111-2222-3333-4444-555555555555";

    #[test]
    fn model_rows_format_and_mark_only_the_current_model() {
        let wire = |id: &str, provider: &str, window: Option<u64>| ModelWire {
            id: id.into(),
            provider: provider.into(),
            label: id.into(),
            context_window: window,
            available: true,
            tier: None,
        };
        assert_eq!(
            model_row_text(&wire("claude-opus-5-5", "anthropic", Some(1_000_000))),
            "claude-opus-5-5 - anthropic - 1000k ctx"
        );
        assert_eq!(
            model_row_text(&wire("mini", "ollama", None)),
            "mini - ollama"
        );
        // Same name under another provider is not the current model.
        let other = model_row(&wire("shared", "openrouter", None), "shared", "anthropic");
        assert!(!other.current);
        let same = model_row(&wire("shared", "anthropic", None), "shared", "anthropic");
        assert!(same.current);
        // An empty provider matches on the name alone.
        assert!(model_row(&wire("shared", "openrouter", None), "shared", "").current);
    }

    #[test]
    fn session_row_shows_title_state_and_count() {
        let m = meta(ID_A, "router fix", 3, None);
        assert_eq!(
            session_row(&m, false),
            "router fix \u{b7} idle \u{b7} 3 messages"
        );
        assert_eq!(
            session_row(&m, true),
            "router fix \u{b7} running \u{b7} 3 messages"
        );
    }

    #[test]
    fn a_forked_session_row_names_its_parent_and_turn() {
        let mut m = meta(ID_A, "retry", 4, None);
        m.forked_from = Some(modron_protocol::ForkOriginWire {
            session_id: Uuid::parse_str(ID_B).expect("uuid"),
            turn: 2,
        });
        assert_eq!(
            session_row(&m, false),
            "retry \u{b7} idle \u{b7} 4 messages \u{b7} forked from 11111111 at turn 2"
        );
        // A session that was not forked keeps the plain row.
        m.forked_from = None;
        assert_eq!(
            session_row(&m, false),
            "retry \u{b7} idle \u{b7} 4 messages"
        );
    }

    #[test]
    fn an_untitled_session_shows_its_short_id() {
        let m = meta(ID_A, "  ", 0, None);
        assert_eq!(
            session_row(&m, false),
            "abcdef01 \u{b7} idle \u{b7} 0 messages"
        );
    }

    #[test]
    fn approval_cycle_is_ask_yolo_deny() {
        assert_eq!(
            approval_mode_next(Some(ApprovalModeWire::Ask)),
            ApprovalModeWire::ApproveAll
        );
        assert_eq!(
            approval_mode_next(Some(ApprovalModeWire::ApproveAll)),
            ApprovalModeWire::Deny
        );
        assert_eq!(
            approval_mode_next(Some(ApprovalModeWire::Deny)),
            ApprovalModeWire::Ask
        );
        assert_eq!(approval_mode_next(None), ApprovalModeWire::ApproveAll);
    }

    #[test]
    fn effort_cycle_runs_unset_through_max_and_back() {
        let mut effort = None;
        let mut seen = Vec::new();
        for _ in 0..7 {
            effort = effort_next(effort);
            seen.push(effort.map(|e| e.label()).unwrap_or(EFFORT_UNSET_LABEL));
        }
        assert_eq!(
            seen,
            ["low", "medium", "high", "xhigh", "max", "unset", "low"]
        );
    }

    #[test]
    fn phase_and_sandbox_cycles() {
        assert_eq!(phase_next(Phase::Build), Phase::Plan);
        assert_eq!(phase_next(Phase::Plan), Phase::Build);
        let s = SandboxPolicy::Off;
        assert_eq!(sandbox_next(s), SandboxPolicy::Seatbelt);
        assert_eq!(sandbox_next(sandbox_next(s)), SandboxPolicy::Bubblewrap);
        assert_eq!(
            sandbox_next(sandbox_next(sandbox_next(s))),
            SandboxPolicy::Off
        );
    }

    #[test]
    fn snapshot_rows_follow_the_session_list_and_running_state() {
        let a = Uuid::parse_str(ID_A).expect("uuid");
        let vm = SessionViewModel {
            session_id: Some(a),
            running: true,
            sessions: vec![
                meta(ID_A, "attached", 2, None),
                meta(ID_B, "other", 5, None),
            ],
            ..Default::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(snap.session_rows.len(), snap.sessions.len());
        assert_eq!(
            snap.session_rows[0],
            "attached \u{b7} running \u{b7} 2 messages"
        );
        assert_eq!(snap.session_rows[1], "other \u{b7} idle \u{b7} 5 messages");
    }

    #[test]
    fn a_task_marked_running_on_the_server_shows_running() {
        let vm = SessionViewModel {
            sessions: vec![meta(ID_B, "task", 1, Some("running"))],
            ..Default::default()
        };
        let snap = ViewModelSnapshot::from_vm(&vm);
        assert_eq!(
            snap.session_rows,
            vec!["task \u{b7} running \u{b7} 1 messages".to_string()]
        );
    }
}

#[cfg(test)]
mod c3f_tests {
    use super::*;
    use modron_types::AgentState;

    fn row(state: AgentState, done: Option<bool>) -> SwarmRow {
        SwarmRow {
            id: "a1".into(),
            label: "reviewer".into(),
            turns: 3,
            tokens: 1200,
            last_tool: Some("read".into()),
            done,
            started_ms: None,
            state,
        }
    }

    #[test]
    fn agent_rows_carry_id_text_and_state_word() {
        let view = AgentRowView::from_row(&row(AgentState::Running, None));
        assert_eq!(view.id, "a1");
        assert_eq!(view.text, row(AgentState::Running, None).summary());
        assert_eq!(view.state_word, "running");
        assert!(view.running);
    }

    #[test]
    fn finished_agents_are_not_running_and_keep_their_word() {
        let killed = AgentRowView::from_row(&row(AgentState::Killed, Some(false)));
        assert_eq!(killed.state_word, "killed");
        assert!(!killed.running);
        // An older peer's row (no state) reads done from its done flag.
        let old = AgentRowView::from_row(&row(AgentState::Running, Some(true)));
        assert_eq!(old.state_word, "done");
        assert!(!old.running);
    }

    #[test]
    fn confirmation_copy_is_the_shared_wording() {
        assert_eq!(
            delete_confirm_text("fix router"),
            "delete fix router? y / n"
        );
        assert_eq!(kill_confirm_text("reviewer"), "kill reviewer? y / n");
    }
}

#[cfg(test)]
mod portal_copy_tests {
    use super::*;

    #[test]
    fn connection_line_joins_state_address_and_note() {
        assert_eq!(
            connection_address_label("connected", "ws://h/ws", ""),
            "connected \u{b7} ws://h/ws"
        );
        assert_eq!(
            connection_address_label("connected", "ws://h/ws", OWN_SERVER_NOTE),
            "connected \u{b7} ws://h/ws \u{b7} started by modron-app"
        );
    }

    #[test]
    fn no_session_message_names_the_address_and_the_fix() {
        assert_eq!(
            no_session_to_continue("ws://x"),
            "no session to continue on ws://x: start one with modron-app-tui --new"
        );
    }

    #[test]
    fn iso_times_shorten_to_minutes_and_other_text_passes_through() {
        assert_eq!(short_time("2026-11-15T10:20:30Z"), "2026-11-15 10:20");
        assert_eq!(short_time("yesterday"), "yesterday");
        assert_eq!(short_time(""), "");
        assert_eq!(short_time("2026-11-15 10:20"), "2026-11-15 10:20");
    }

    #[test]
    fn sign_in_hint_names_the_bridge_token() {
        assert!(SIGN_IN_HINT.contains("OWNER_TOKEN"));
        assert!(SIGN_IN_HINT.is_ascii());
    }

    #[test]
    fn portal_parts_join_to_the_row_line() {
        let (title, subtitle) =
            portal_session_parts("fix router", "laptop", true, "2026-11-15T10:20:30Z");
        assert_eq!(title, "fix router");
        assert_eq!(
            format!("{title} \u{b7} {subtitle}"),
            portal_session_row("fix router", "laptop", true, "2026-11-15T10:20:30Z")
        );
        let (blank, _) = portal_session_parts("  ", "cloud", false, "");
        assert_eq!(blank, UNTITLED_LABEL);
    }

    #[test]
    fn portal_row_shows_title_host_state_and_time() {
        assert_eq!(
            portal_session_row("fix router", "laptop", true, "2026-11-15T10:20:30Z"),
            "fix router \u{b7} laptop \u{b7} online \u{b7} 2026-11-15 10:20"
        );
        assert_eq!(
            portal_session_row("  ", "cloud", false, ""),
            "untitled \u{b7} cloud \u{b7} offline"
        );
    }

    #[test]
    fn invite_row_names_role_and_expiry() {
        assert_eq!(
            invite_row("viewer", "2026-11-16T10:20:30Z"),
            "viewer \u{b7} expires 2026-11-16 10:20"
        );
    }

    #[test]
    fn push_line_is_the_label_and_the_value() {
        assert_eq!(push_line(PUSH_ON_LABEL), "notify this device: on");
        assert_eq!(
            push_line(PUSH_UNAVAILABLE_LABEL),
            "notify this device: unavailable"
        );
    }
}

#[cfg(test)]
mod task_copy_tests {
    use super::*;

    #[test]
    fn btw_and_plan_wrap_the_task_with_the_old_tui_suffix() {
        assert_eq!(btw_task("why?"), format!("why?\n\n{BTW_SUFFIX}"));
        assert_eq!(
            plan_task("add tests"),
            format!("add tests\n\n{PLAN_SUFFIX}")
        );
    }

    #[test]
    fn learn_names_the_skill_file_with_the_slug() {
        let text = learn_task("Fix the Parser!");
        assert!(text.contains(".modron/skills/fix-the-parser/SKILL.md"));
        assert!(text.contains("(name: fix-the-parser, description: one line)"));
        assert!(learn_task("").contains(".modron/skills/session-skill/SKILL.md"));
    }

    #[test]
    fn the_slug_collapses_separators_and_is_capped() {
        assert_eq!(skill_slug("  a -- b  "), "a-b");
        assert_eq!(skill_slug(&"x".repeat(80)).len(), 48);
    }

    #[test]
    fn the_persona_picker_clears_with_the_shared_label() {
        assert_eq!(PERSONA_CLEAR_LABEL, "no persona");
        assert!(PERSONA_CLEAR_LABEL.is_ascii());
    }

    #[test]
    fn the_scrolled_up_hint_counts_the_lines_above_and_says_top_at_zero() {
        assert_eq!(
            scrolled_up_hint(0),
            "\u{2191} top of session \u{00b7} pgdn scroll \u{00b7} esc live"
        );
        assert_eq!(
            scrolled_up_hint(42),
            "\u{2191} 42 lines above \u{00b7} pgup/pgdn scroll \u{00b7} esc live"
        );
        assert!(!scrolled_up_hint(1).is_ascii());
    }

    #[test]
    fn the_app_copy_words_are_shared_constants() {
        assert_eq!(NO_TOOLS_NOTE, "no tool activity yet");
        assert!(NEW_OUTPUT_BELOW_LABEL.starts_with("new output below"));
        assert_eq!(CTRL_C_QUIT_HINT, "press ctrl+c again to quit");
    }

    #[test]
    fn the_review_task_is_the_old_tui_text() {
        assert!(REVIEW_TASK.starts_with("Review the working tree: run `git diff`"));
        assert!(REVIEW_TASK.ends_with("Do not fix anything."));
    }
}

#[cfg(test)]
mod auth_copy_tests {
    use super::*;

    #[test]
    fn auth_rows_join_the_core_label_and_status_with_a_colon() {
        assert_eq!(
            auth_row_line("Anthropic", "subscription (oauth)"),
            "Anthropic: subscription (oauth)"
        );
    }

    #[test]
    fn only_the_not_configured_status_counts_as_unsigned() {
        assert!(!auth_is_signed_in(AUTH_NOT_CONFIGURED));
        assert!(auth_is_signed_in("api key"));
        assert!(auth_is_signed_in("subscription (oauth)"));
    }
}

#[cfg(test)]
mod app4_shared_copy_tests {
    use super::*;
    use modron_protocol::AutomationWire;

    fn automation(last_run: Option<&str>) -> AutomationWire {
        AutomationWire {
            name: "nightly".into(),
            prompt: "check the build".into(),
            interval_minutes: 30,
            last_run: last_run.map(String::from),
        }
    }

    /// The task summary is `ahead N - F files (+added -deleted)`, with the
    /// singular form for one file and zeros when nothing changed.
    #[test]
    fn task_status_summary_formats_ahead_files_and_lines() {
        let file = |added, deleted| TaskFileStat {
            path: "p".into(),
            added,
            deleted,
        };
        assert_eq!(
            task_status_summary(3, &[file(100, 10), file(20, 4)]),
            "ahead 3 - 2 files (+120 -14)"
        );
        assert_eq!(
            task_status_summary(1, &[file(5, 0)]),
            "ahead 1 - 1 file (+5 -0)"
        );
        assert_eq!(task_status_summary(0, &[]), "ahead 0 - 0 files (+0 -0)");
    }

    /// A persona row shows `name - summary`, the name alone without a
    /// summary, and marks only the current persona.
    #[test]
    fn persona_rows_show_the_summary_and_mark_the_current_one() {
        let with = PersonaWire {
            name: "reviewer".into(),
            model: None,
            summary: "Review diffs.".into(),
        };
        let bare = PersonaWire {
            name: "writer".into(),
            model: None,
            summary: String::new(),
        };
        assert_eq!(persona_row_text(&with), "reviewer - Review diffs.");
        assert_eq!(persona_row_text(&bare), "writer");
        assert!(persona_row(&with, Some("reviewer")).current);
        assert!(!persona_row(&bare, Some("reviewer")).current);
        assert!(!persona_row(&with, None).current);
    }

    fn task_meta(title: &str, branch: Option<&str>) -> SessionMetaWire {
        SessionMetaWire {
            forked_from: None,
            id: Uuid::from_u128(7),
            title: title.into(),
            created_at: String::new(),
            updated_at: String::new(),
            message_count: 0,
            task_branch: branch.map(String::from),
            task_state: None,
            working_dir: None,
        }
    }

    #[test]
    fn automation_row_shows_name_interval_and_last_run() {
        assert_eq!(
            automation_row(&automation(None)),
            "nightly \u{b7} every 30 min \u{b7} last run never"
        );
        assert_eq!(
            automation_row(&automation(Some("2026-11-14T09:30:00Z"))),
            "nightly \u{b7} every 30 min \u{b7} last run 2026-11-14 09:30"
        );
    }

    #[test]
    fn the_interval_floor_is_the_servers_five_minutes() {
        // Pinned to MIN_INTERVAL_MINUTES in crates/modron-server/src/automations.rs.
        assert_eq!(AUTOMATION_MIN_INTERVAL_MINUTES, 5);
    }

    #[test]
    fn the_interval_check_refuses_text_and_values_below_the_floor() {
        assert_eq!(automation_interval_error(" 5 "), None);
        assert_eq!(automation_interval_error("60"), None);
        assert_eq!(
            automation_interval_error("4").as_deref(),
            Some("interval must be at least 5 minutes")
        );
        assert_eq!(
            automation_interval_error("0").as_deref(),
            Some("interval must be at least 5 minutes")
        );
        assert_eq!(
            automation_interval_error("soon").as_deref(),
            Some(AUTOMATION_INTERVAL_NOT_A_NUMBER)
        );
        assert_eq!(
            automation_interval_error("-5").as_deref(),
            Some(AUTOMATION_INTERVAL_NOT_A_NUMBER)
        );
        assert_eq!(
            automation_interval_error("").as_deref(),
            Some(AUTOMATION_INTERVAL_NOT_A_NUMBER)
        );
    }

    #[test]
    fn task_row_names_the_title_branch_and_state() {
        assert_eq!(
            task_row(&task_meta("fix parser", Some("task/fix-parser")), true),
            "fix parser \u{b7} task/fix-parser \u{b7} running"
        );
        assert_eq!(
            task_row(&task_meta("", None), false),
            format!("{UNTITLED_LABEL} \u{b7} {TASK_NO_BRANCH_LABEL} \u{b7} idle")
        );
    }

    #[test]
    fn merge_confirmation_names_the_branch_and_both_answers() {
        assert_eq!(
            merge_confirm_text("task/fix-parser"),
            "merge task/fix-parser into its base? y / n"
        );
    }

    #[test]
    fn queue_chip_and_steer_echo_keep_their_shapes() {
        assert_eq!(queue_chip(2, "next"), "[2] next");
        assert_eq!(steer_echo("also check tests"), "> also check tests");
    }

    #[test]
    fn save_default_copy_leads_with_its_key() {
        assert!(SAVE_DEFAULT_HINT.starts_with(SAVE_DEFAULT_KEY));
        assert_eq!(SAVE_DEFAULT_LABEL, "save as default");
    }
}

#[cfg(test)]
mod removal_tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn removing_a_rule_sends_the_rest_as_wire_rules_in_order() {
        let rules = lines(&["allow read", "ask bash(git push*)", "deny bash(rm -rf *)"]);
        let kept = rules_without(&rules, 1).expect("row 1");
        assert_eq!(
            kept,
            vec![
                RuleWire {
                    effect: "allow".into(),
                    spec: "read".into()
                },
                RuleWire {
                    effect: "deny".into(),
                    spec: "bash(rm -rf *)".into()
                },
            ]
        );
    }

    #[test]
    fn removing_the_last_rule_sends_an_empty_set_and_a_bad_index_sends_nothing() {
        let rules = lines(&["allow read"]);
        assert_eq!(rules_without(&rules, 0), Some(Vec::new()));
        assert_eq!(rules_without(&rules, 1), None);
        assert_eq!(rules_without(&[], 0), None);
    }

    #[test]
    fn a_rule_line_without_a_spec_keeps_an_empty_spec() {
        let rules = lines(&["allow", "deny read"]);
        let kept = rules_without(&rules, 1).expect("row 1");
        assert_eq!(
            kept,
            vec![RuleWire {
                effect: "allow".into(),
                spec: String::new()
            }]
        );
    }

    #[test]
    fn tool_lists_drop_the_exact_entry_only() {
        let list = lines(&["bash", "bashful", "read"]);
        assert_eq!(tools_without(&list, "bash"), lines(&["bashful", "read"]));
        assert_eq!(tools_without(&list, "ba"), list);
        assert!(tools_without(&[], "bash").is_empty());
    }

    #[test]
    fn the_removal_confirmation_names_the_row_and_the_keys() {
        assert_eq!(
            remove_confirm_text("allow bash"),
            "remove allow bash? y / n"
        );
    }

    #[test]
    fn the_credential_removal_confirmation_names_the_provider_and_the_keys() {
        assert_eq!(
            credential_remove_confirm_text("Anthropic"),
            "remove Anthropic credentials? y / n"
        );
        assert!(auth_is_signed_in("configured"));
        assert!(!auth_is_signed_in(AUTH_NOT_CONFIGURED));
    }

    #[test]
    fn the_theme_names_and_picker_words_are_shared_strings() {
        assert_eq!(
            [THEME_DARK_NAME, THEME_LIGHT_NAME, THEME_SYSTEM_NAME],
            ["oculus dark", "oculus light", "system"]
        );
        assert_eq!(PICKER_NONE_LABEL, RULES_NONE_LABEL);
        assert_eq!(REMOVE_LABEL, "remove");
    }
}

#[cfg(test)]
mod cloud_view_tests {
    use super::*;

    #[test]
    fn run_states_map_to_the_shared_words_and_unknown_passes_through() {
        assert_eq!(run_state_label("starting"), "starting");
        assert_eq!(run_state_label("running"), "running");
        assert_eq!(run_state_label("sleeping"), "sleeping");
        assert_eq!(run_state_label("stopped"), "stopped");
        assert_eq!(run_state_label("failed"), "failed");
        assert_eq!(run_state_label("paused"), "paused");
    }

    #[test]
    fn run_rows_show_repo_branch_state_reason_and_time() {
        assert_eq!(
            run_line(
                "acme/app",
                "main",
                "failed",
                "out of memory",
                "2026-11-15T10:20:30Z"
            ),
            "acme/app \u{b7} main \u{b7} failed \u{b7} out of memory \u{b7} 2026-11-15 10:20"
        );
        assert_eq!(
            run_line("a/b", "dev", "running", "  ", ""),
            "a/b \u{b7} dev \u{b7} running"
        );
    }

    #[test]
    fn repo_and_account_rows_format_their_fields() {
        assert_eq!(
            repo_line("acme/app", true, "2026-11-14T08:00:00Z"),
            "acme/app \u{b7} private \u{b7} 2026-11-14 08:00"
        );
        assert_eq!(repo_line("acme/site", false, ""), "acme/site \u{b7} public");
        assert_eq!(account_line("octocat", "token"), "octocat \u{b7} token");
        assert_eq!(account_line("octocat", ""), "octocat");
    }

    #[test]
    fn a_refused_token_names_githubs_status_only() {
        assert_eq!(
            token_refused_line(Some(401)),
            "GitHub refused the token (status 401)"
        );
        assert_eq!(token_refused_line(None), "GitHub refused the token");
        assert!(!token_refused_line(Some(401)).contains("ghp_"));
    }

    #[test]
    fn the_account_removal_confirmation_uses_the_shared_yes_and_no() {
        assert_eq!(
            cloud_account_remove_text("octocat"),
            "remove octocat? y / n"
        );
    }
}
