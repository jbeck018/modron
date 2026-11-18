//! Team mode (V3): `/team [N] <task>` and `modron team`. One planning call
//! splits the task into up to N independent work items with file scopes. Each
//! item runs as a `delegate` child ([`crate::subagent::delegate_owned`], the
//! same path as every other subagent) in its own git worktree under
//! `.modron/worktrees/`, in parallel. The team works on its own branch
//! `team/<id>` (created from HEAD) in an integration worktree; the finished
//! item worktrees are committed and merged into it in item order, with
//! `--no-ff` merge commits. A merge that does not apply becomes a fix item.
//! The `[verify]` command and the reviewer pass then check the integration
//! tree, and every failure becomes an item for the next round.
//!
//! Landing: a clean run (see [`is_clean`]) fast-forwards the user's branch to
//! `team/<id>` and deletes the team branch. Any other run leaves the user's
//! branch untouched and keeps `team/<id>`, and the summary gives the
//! `git merge` command. The team's worktrees and branch are removed when the
//! run is dropped mid-flight ([`worktree::TeamGuard`]); a later run sweeps the
//! leftovers of a team whose process is gone ([`worktree::sweep_stale`]). A
//! dirty tracked tree at the start refuses the run.
//!
//! Limits: a round is clean when no item failed or conflicted and verify
//! passed (or no verify command is configured) and the reviewer did not ask
//! for a fix. A run stops at the first clean round, at the fix-round cap
//! (`max_rounds`, default 2; `until_verified` raises the default to 6), or
//! when the engine's token budget or spend cap is spent. Between rounds only:
//! a round in flight is never cut short. Verify that is not permitted in the
//! current approval mode stops the team at once, unverified. An interrupt
//! aborts the run task, which drops the team future and every child in it.
//!
//! Progress shows through the swarm rows the children already emit; the
//! final summary (items, rounds, verify result, what failed) is the answer.

use crate::review;
use crate::subagent::{self, ChildOwner};
use crate::worktree::{self, MergeOutcome};
use crate::{
    ActionKind, Engine, Error, Message, Result, RunSummary, SessionEvent, SessionId, ToolCtx,
    ToolOutcome, Usage,
};
use modron_types::ReviewVerdict;
use std::path::PathBuf;
use std::sync::Arc;

/// Workers when `/team` or `modron team` names no size.
pub const DEFAULT_WORKERS: u32 = 3;
/// The largest team (`/team` and `modron team` refuse more).
pub const MAX_WORKERS: u32 = 5;
/// Fix rounds after the first round, by default.
pub const DEFAULT_MAX_ROUNDS: u32 = 2;
/// Fix rounds after the first round with `until_verified`, by default.
pub const UNTIL_VERIFIED_MAX_ROUNDS: u32 = 6;
/// Token budget of one worker child (its own `delegate` budget).
pub const WORKER_BUDGET_TOKENS: u64 = 100_000;
/// The diff a fix item carries, and the reviewer sees, is cut at this size.
const DIFF_CAP_BYTES: usize = 60 * 1024;
/// Verify output kept in a fix item and in the summary.
const OUTPUT_CHARS: usize = 2_000;

/// Stage-note wording of team mode: every line a team run writes to the
/// transcript as a `Progress` event comes from here. Plain ASCII.
mod progress {
    use super::RoundLog;

    /// After planning: how many items the round will run.
    pub fn plan(items: usize) -> String {
        format!("team plan: {items} item(s)")
    }

    /// A round starts with `items` work items.
    pub fn round_started(round: u32, items: usize) -> String {
        format!("round {round} started: {items} item(s)")
    }

    /// Workers have finished; their results are merged in item order.
    pub fn merging(round: u32, items: usize) -> String {
        format!("round {round} merging {items} worker item(s)")
    }

    /// The `[verify]` command runs on the team branch.
    pub fn verifying(round: u32) -> String {
        format!("round {round} verifying")
    }

    /// The reviewer reads the team's change.
    pub fn reviewing(round: u32) -> String {
        format!("round {round} reviewing")
    }

    /// The round's outcome, in the words the summary uses.
    pub fn round_result(round: u32, log: &RoundLog) -> String {
        format!(
            "round {round} result: {} merged, {} conflict(s), {} failed; verify {}; review {}",
            log.merged,
            log.conflicts,
            log.failed,
            log.verify.label(),
            log.review.label()
        )
    }
}

/// Options of one team run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TeamOptions {
    /// Parallel workers, `1..=MAX_WORKERS`: the plan's item cap.
    pub workers: u32,
    /// Fix rounds after the first round (`None`: the default for the mode).
    pub max_rounds: Option<u32>,
    /// Keep going until verify passes (cap [`UNTIL_VERIFIED_MAX_ROUNDS`]
    /// unless `max_rounds` is given).
    pub until_verified: bool,
}

impl Default for TeamOptions {
    fn default() -> Self {
        Self {
            workers: DEFAULT_WORKERS,
            max_rounds: None,
            until_verified: false,
        }
    }
}

impl TeamOptions {
    /// The fix-round cap for this run.
    pub fn fix_round_cap(&self) -> u32 {
        self.max_rounds.unwrap_or(if self.until_verified {
            UNTIL_VERIFIED_MAX_ROUNDS
        } else {
            DEFAULT_MAX_ROUNDS
        })
    }

    /// Refuses a team size outside `1..=MAX_WORKERS`.
    pub fn check(&self) -> std::result::Result<(), String> {
        if (1..=MAX_WORKERS).contains(&self.workers) {
            Ok(())
        } else {
            Err(format!(
                "team size must be 1 to {MAX_WORKERS}, got {}",
                self.workers
            ))
        }
    }
}

/// Reads a `/team [N] <task>` line. `None` when the line is not a team
/// command; `Some(Err)` for a bad size or a missing task.
pub fn parse_command(task: &str) -> Option<std::result::Result<(TeamOptions, String), String>> {
    let body = task.trim_start().strip_prefix('/')?;
    let (word, rest) = match body.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (body, ""),
    };
    if word != "team" {
        return None;
    }
    let mut opts = TeamOptions::default();
    let (first, after) = match rest.split_once(char::is_whitespace) {
        Some((first, after)) => (first, after.trim()),
        None => (rest, ""),
    };
    let goal = if !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) {
        match first.parse::<u32>() {
            Ok(n) => opts.workers = n,
            Err(_) => return Some(Err(format!("team size `{first}` is out of range"))),
        }
        after
    } else {
        rest
    };
    if goal.is_empty() {
        return Some(Err("usage: /team [N] <task>".into()));
    }
    if let Err(message) = opts.check() {
        return Some(Err(message));
    }
    Some(Ok((opts, goal.to_string())))
}

/// System prompt of the planning call. It starts with a fixed phrase the
/// tests use to recognise the plan turn.
pub fn plan_system(max_items: u32) -> String {
    format!(
        "You split a coding task into independent work items for parallel workers. Every \
         worker edits the same git repository in its own worktree and the results are merged \
         afterwards, so items must not edit the same files. Reply with JSON only: \
         {{\"items\":[{{\"task\":\"<self-contained instruction>\",\"scope\":[\"<file or directory \
         the item edits>\"]}}]}}. Use at most {max_items} items. If the task does not split into \
         independent parts, reply with one item that holds the whole task."
    )
}

/// One work item: what a worker does, and the paths it may edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkItem {
    pub task: String,
    pub scope: Vec<String>,
}

/// Reads the planning call's JSON. Items with an empty task are skipped and
/// the list is cut at `max_items`. `None` when no usable item is there, so
/// the caller runs the whole task as one item.
pub fn parse_plan(text: &str, max_items: u32) -> Option<Vec<WorkItem>> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let items = value.get("items")?.as_array()?;
    let parsed: Vec<WorkItem> = items
        .iter()
        .filter_map(|item| {
            let task = item.get("task")?.as_str()?.trim();
            if task.is_empty() {
                return None;
            }
            let scope = item
                .get("scope")
                .and_then(|v| v.as_array())
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(|p| p.as_str())
                        .map(str::trim)
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(WorkItem {
                task: task.to_string(),
                scope,
            })
        })
        .take(max_items as usize)
        .collect();
    (!parsed.is_empty()).then_some(parsed)
}

/// The task a worker child runs: the item, its scope, and the overall goal
/// for context.
fn worker_task(goal: &str, item: &WorkItem) -> String {
    let scope = if item.scope.is_empty() {
        "not stated".to_string()
    } else {
        item.scope.join(", ")
    };
    format!(
        "{}\n\nScope: {scope}. Other workers edit other parts of this repository at the same \
         time; keep your edits inside your scope.\nOverall goal (context only): {goal}",
        item.task.trim()
    )
}

/// A fix item for an item whose worker failed.
fn redo_item(item: &WorkItem, reason: &str) -> WorkItem {
    WorkItem {
        task: format!(
            "A worker failed on this item ({}). Complete the item on the current tree.\nItem: {}",
            first_line(reason),
            item.task.trim()
        ),
        scope: item.scope.clone(),
    }
}

/// A fix item for a worker change that does not merge cleanly.
fn conflict_item(item: &WorkItem, diff: &str, report: &str) -> WorkItem {
    WorkItem {
        task: format!(
            "A parallel worker's change conflicts with the code already merged ({}). Re-apply \
             that change on top of the current tree, resolving the conflict so both the merged \
             work and this change hold.\nThe worker's task was: {}\nIts diff against the \
             round's base:\n{diff}",
            first_line(report),
            item.task.trim()
        ),
        scope: item.scope.clone(),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .chars()
        .take(200)
        .collect()
}

/// The verify outcome of one round.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum VerifyState {
    /// No `verify` tool is registered (no `[verify]` command).
    #[default]
    NotConfigured,
    Passed,
    /// The command failed; its output is attached.
    Failed(String),
    /// The approval mode does not let the harness run verify.
    NotPermitted,
}

impl VerifyState {
    fn label(&self) -> &'static str {
        match self {
            Self::NotConfigured => "not configured",
            Self::Passed => "passed",
            Self::Failed(_) => "failed",
            Self::NotPermitted => "not permitted",
        }
    }
}

/// The reviewer outcome of one round.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum ReviewState {
    #[default]
    NotConfigured,
    /// The team has not changed the tree, so there is nothing to review.
    NoChange,
    Passed,
    NeedsFix(usize),
    /// The reviewer could not run (no resolvable model, or the call failed).
    Unavailable,
}

impl ReviewState {
    fn label(&self) -> String {
        match self {
            Self::NotConfigured => "not configured".into(),
            Self::NoChange => "no change to review".into(),
            Self::Passed => "passed".into(),
            Self::NeedsFix(n) => format!("needs fix ({n} finding(s))"),
            Self::Unavailable => "unavailable (skipped)".into(),
        }
    }
}

/// Why the team stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Verified,
    Clean,
    Cap,
    Budget,
    NotPermitted,
}

impl Stop {
    fn label(self, cap: u32) -> String {
        match self {
            Self::Verified => "verify passed".into(),
            Self::Clean => "the round was clean and no [verify] command is configured".into(),
            Self::Cap => {
                format!("fix-round cap reached ({cap}); the changes above may be incomplete")
            }
            Self::Budget => "token budget or spend cap reached".into(),
            Self::NotPermitted => {
                "verify is not permitted in this approval mode; the merged change is not verified"
                    .into()
            }
        }
    }
}

/// One round, as the summary reports it.
#[derive(Clone, Debug, Default)]
struct RoundLog {
    items: usize,
    merged: usize,
    conflicts: usize,
    failed: usize,
    verify: VerifyState,
    review: ReviewState,
    /// Worktrees kept for inspection (not merged).
    kept: Vec<PathBuf>,
}

/// Everything a run accumulates across rounds.
#[derive(Default)]
struct TeamReport {
    rounds: Vec<RoundLog>,
    usage: Usage,
    spend_usd: f64,
    notes: Vec<String>,
    /// Items the last round left open (fix items not yet run).
    open_items: usize,
    /// The last verify failure's output, when verify failed.
    last_verify_output: Option<String>,
}

impl TeamReport {
    fn add_spend(&mut self, usage: Usage, spend_usd: f64) {
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.usage.cached_input_tokens = self
            .usage
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens);
        self.spend_usd += spend_usd;
    }
}

/// One worker's result, before its merge.
struct WorkerRun {
    label: String,
    item: WorkItem,
    ok: bool,
    brief: String,
    worktree: Option<PathBuf>,
    usage: Usage,
    spend_usd: f64,
}

/// One round's result: its log and the fix items for the next round.
struct RoundResult {
    log: RoundLog,
    fixes: Vec<WorkItem>,
}

/// How a finished team run left the user's branch.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Landing {
    /// The run was clean: the user's branch fast-forwarded to the team's work
    /// and the team branch was deleted.
    FastForwarded,
    /// The user's branch is untouched; the team branch is kept, with why.
    Kept { branch: String, reason: String },
}

/// A team run in progress: the engine it runs on and its fixed context.
struct Team<'a> {
    engine: &'a Engine,
    session: SessionId,
    goal: &'a str,
    /// The user's checkout (the engine's working dir). It is never edited
    /// during the run; a clean run fast-forwards its branch at the end.
    main: PathBuf,
    /// The checkout's HEAD when the team started.
    base: String,
    /// The team id (8 hex digits): names the team's worktrees and branch.
    id: String,
    /// The team branch `team/<id>`, created from `base`.
    branch: String,
    /// The integration worktree on `branch`: items merge here, verify and
    /// review run here.
    work: PathBuf,
    /// Notes known before the first round (for example a stale sweep).
    pre_notes: Vec<String>,
}

impl Engine {
    /// Team mode (V3) in a fresh session: `modron team` and `/team`.
    pub async fn run_team(&self, task: &str, opts: TeamOptions) -> Result<RunSummary> {
        self.run_team_in(None, task, opts).await
    }

    /// Team mode in `session` (a resumed session) or a fresh one.
    pub(crate) async fn run_team_in(
        &self,
        session: Option<SessionId>,
        goal: &str,
        opts: TeamOptions,
    ) -> Result<RunSummary> {
        let goal = goal.trim();
        if goal.is_empty() {
            return Err(Error::Config(
                "empty task".into(),
                "pass a task for team mode".into(),
            ));
        }
        opts.check()
            .map_err(|message| Error::Config(message, "use 1 to 5 workers".into()))?;
        let main = self.config.working_dir.clone();
        let Some(base) = worktree::head_sha(&main).await else {
            return Err(Error::Config(
                "team mode needs a git repository with at least one commit".into(),
                "run `git init` and commit first, or run the task without /team".into(),
            ));
        };
        // The clean run fast-forwards the user's branch at the end, which git
        // refuses over tracked edits; refuse up front with the same reason.
        match worktree::is_dirty(&main).await {
            Ok(false) => {}
            Ok(true) => {
                return Err(Error::Config(
                    "team mode needs a clean working tree: uncommitted changes to tracked files \
                     would block the fast-forward that lands the team's work"
                        .into(),
                    "commit or stash your changes, then run the team again".into(),
                ));
            }
            Err(reason) => {
                return Err(Error::Config(
                    format!("cannot read git status: {reason}"),
                    "team mode needs a git repository".into(),
                ));
            }
        }
        // Leftovers of earlier runs that died (their marker's pid is gone).
        let swept = worktree::sweep_stale(&main);
        let pre_notes = if swept.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "removed {} team workspace(s) left by an earlier run that did not finish",
                swept.len()
            )]
        };
        let id = subagent::new_worktree_id();
        let branch = worktree::team_branch(&id);
        let work = worktree::team_dir(&main, &id);
        // The marker comes first, so a crash after this point is swept later.
        worktree::write_marker(&main, &id).map_err(|reason| {
            Error::Config(
                format!("team mode could not write its marker: {reason}"),
                "check that .modron/worktrees is writable".into(),
            )
        })?;
        let mut guard = worktree::TeamGuard::new(&main, &id);
        if let Err(reason) = worktree::create_on_branch(&main, &work, &branch).await {
            return Err(Error::Config(
                format!("team mode could not create its branch: {reason}"),
                "check the repository state (git worktree list), then run the team again".into(),
            ));
        }
        let session_id = match session {
            Some(id) => id,
            None => {
                let title: String = goal.chars().take(60).collect();
                self.store
                    .create_session(&format!("team: {title}"))
                    .await?
                    .id
            }
        };
        self.store
            .append(
                session_id,
                &SessionEvent::Task {
                    text: goal.to_string(),
                },
            )
            .await?;
        // Live for the whole team: child asks reach the approver only while
        // this run is live, and an interrupt ends it with the run task.
        let _run = crate::run_scope().begin(session_id);
        let team = Team {
            engine: self,
            session: session_id,
            goal,
            main,
            base,
            id,
            branch,
            work,
            pre_notes,
        };
        let (report, stop) = team.run(&opts).await;
        let landing = team.land(stop, &report, &mut guard).await;
        let answer = render(&opts, &report, stop, &landing);
        self.store
            .append(
                session_id,
                &SessionEvent::Message {
                    parent_id: None,
                    message: Message::assistant(answer.clone()),
                },
            )
            .await?;
        Ok(RunSummary {
            session_id,
            answer,
            turns: report.rounds.len() as u32,
            tool_calls: report.rounds.iter().map(|r| r.items).sum::<usize>() as u32,
            usage: report.usage,
            models_used: Vec::new(),
        })
    }
}

impl Team<'_> {
    /// Plan, then rounds until a stop. Returns the report and the stop.
    async fn run(&self, opts: &TeamOptions) -> (TeamReport, Stop) {
        let cap = opts.fix_round_cap();
        let mut report = TeamReport {
            notes: self.pre_notes.clone(),
            ..TeamReport::default()
        };
        let mut items = self.plan(opts, &mut report).await;
        self.note(progress::plan(items.len())).await;
        let stop = loop {
            let round = report.rounds.len() as u32 + 1;
            self.note(progress::round_started(round, items.len())).await;
            let result = self.round(items, round, &mut report).await;
            let verify = result.log.verify.clone();
            report.open_items = result.fixes.len();
            report.rounds.push(result.log);
            if verify == VerifyState::NotPermitted {
                break Stop::NotPermitted;
            }
            if result.fixes.is_empty() {
                break if verify == VerifyState::Passed {
                    Stop::Verified
                } else {
                    Stop::Clean
                };
            }
            // Fix rounds done so far: the rounds after the first.
            if round > cap {
                break Stop::Cap;
            }
            if self.over_budget(&report) {
                break Stop::Budget;
            }
            items = result.fixes;
        };
        if matches!(stop, Stop::Verified | Stop::Clean) {
            report.open_items = 0;
        }
        (report, stop)
    }

    /// Appends one stage note to the team session, so live clients see it as
    /// it is written. A failed append loses the note, not the run.
    async fn note(&self, text: String) {
        let _ = self
            .engine
            .store
            .append(self.session, &SessionEvent::Progress { text })
            .await;
    }

    /// Lands the run. A clean run fast-forwards the user's branch to the team
    /// branch and removes the team's state. Any other run leaves the user's
    /// branch where it was, removes the integration worktree, and keeps the
    /// team branch (and the worktrees kept on purpose) for the user to merge.
    async fn land(
        &self,
        stop: Stop,
        report: &TeamReport,
        guard: &mut worktree::TeamGuard,
    ) -> Landing {
        let reason = if !is_clean(stop, report) {
            "the run did not end clean".to_string()
        } else {
            match worktree::fast_forward(&self.main, &self.branch).await {
                Ok(()) => {
                    worktree::remove_team(&self.main, &self.id);
                    guard.disarm();
                    return Landing::FastForwarded;
                }
                Err(reason) => format!("fast-forward refused: {}", first_line(&reason)),
            }
        };
        worktree::remove_work(&self.main, &self.id);
        guard.disarm();
        Landing::Kept {
            branch: self.branch.clone(),
            reason,
        }
    }

    /// Stage 1: one planning call. A call that fails or returns no usable
    /// items runs the whole task as one item, and the summary says so.
    async fn plan(&self, opts: &TeamOptions, report: &mut TeamReport) -> Vec<WorkItem> {
        let whole = || {
            vec![WorkItem {
                task: self.goal.to_string(),
                scope: Vec::new(),
            }]
        };
        let turn = self
            .engine
            .team_turn(
                self.goal,
                ActionKind::Plan,
                "plan",
                self.engine.config.roles.plan.as_deref(),
                &plan_system(opts.workers),
                format!("Task: {}", self.goal),
            )
            .await;
        let Some((text, usage, model)) = turn else {
            report
                .notes
                .push("the plan call failed; the task runs as one item".into());
            return whole();
        };
        self.record(&model, usage, report).await;
        match parse_plan(&text, opts.workers) {
            Some(items) => items,
            None => {
                report
                    .notes
                    .push("the plan did not parse; the task runs as one item".into());
                whole()
            }
        }
    }

    /// Stages 2 to 5 for one round: run the items in parallel, merge them in
    /// order, verify the merged tree, review it, and collect the fix items.
    async fn round(
        &self,
        items: Vec<WorkItem>,
        round: u32,
        report: &mut TeamReport,
    ) -> RoundResult {
        let round_base = worktree::head_sha(&self.work)
            .await
            .unwrap_or_else(|| self.base.clone());
        let mut log = RoundLog {
            items: items.len(),
            ..RoundLog::default()
        };
        let runs = futures::future::join_all(
            items
                .into_iter()
                .enumerate()
                .map(|(index, item)| self.worker(format!("{round}.{}", index + 1), item)),
        )
        .await;

        self.note(progress::merging(round, runs.len())).await;
        let mut fixes = Vec::new();
        for run in runs {
            report.add_spend(run.usage, run.spend_usd);
            let Some(path) = run.worktree.clone() else {
                log.failed += 1;
                fixes.push(redo_item(&run.item, &run.brief));
                continue;
            };
            if !run.ok {
                log.failed += 1;
                log.kept.push(path);
                fixes.push(redo_item(&run.item, &run.brief));
                continue;
            }
            let sha = match worktree::commit_all(&path).await {
                Ok(sha) => sha,
                Err(reason) => {
                    log.failed += 1;
                    log.kept.push(path);
                    fixes.push(redo_item(&run.item, &reason));
                    continue;
                }
            };
            let diff = worktree::diff(&self.work, &round_base, &sha, DIFF_CAP_BYTES)
                .await
                .unwrap_or_default();
            let message = format!("team worker {}: {}", run.label, first_line(&run.item.task));
            let merged = worktree::merge_into(&self.work, &sha, &message).await;
            match merged {
                Ok(MergeOutcome::Merged) => {
                    log.merged += 1;
                    worktree::remove(&self.main, &path).await;
                }
                Ok(MergeOutcome::Conflict(report_text)) => {
                    log.conflicts += 1;
                    log.kept.push(path);
                    fixes.push(conflict_item(&run.item, &diff, &report_text));
                }
                Err(reason) => {
                    log.conflicts += 1;
                    log.kept.push(path);
                    fixes.push(conflict_item(&run.item, &diff, &reason));
                }
            }
        }

        log.verify = self.verify(round).await;
        match &log.verify {
            VerifyState::Failed(output) => {
                let output = clip(output);
                report.last_verify_output = Some(output.clone());
                fixes.push(WorkItem {
                    task: format!(
                        "The verify command failed on the merged tree:\n{output}\nFix the \
                         failures in the code on the current tree. Do not weaken or skip the \
                         checks."
                    ),
                    scope: Vec::new(),
                });
            }
            VerifyState::Passed => report.last_verify_output = None,
            _ => {}
        }
        let (review, fix) = self.review(round, report).await;
        log.review = review;
        fixes.extend(fix);
        self.note(progress::round_result(round, &log)).await;
        RoundResult { log, fixes }
    }

    /// One worker: a `delegate` child in a fresh worktree of the current HEAD.
    async fn worker(&self, label: String, item: WorkItem) -> WorkerRun {
        let path = worktree::item_dir(&self.main, &self.id, &label);
        if let Err(reason) = worktree::create(&self.work, &path).await {
            return WorkerRun {
                label,
                ok: false,
                brief: format!("worktree isolation failed: {reason}"),
                worktree: None,
                usage: Usage::default(),
                spend_usd: 0.0,
                item,
            };
        }
        let task = worker_task(self.goal, &item);
        let owner = ChildOwner::for_session(Some(self.session));
        let outcome = subagent::delegate_owned(
            owner,
            self.engine,
            &task,
            None,
            None,
            WORKER_BUDGET_TOKENS,
            Some(&path),
        )
        .await;
        match outcome {
            Ok(child) => WorkerRun {
                label,
                ok: !child.killed,
                brief: child.brief,
                worktree: Some(path),
                usage: child.usage,
                spend_usd: child.spend_usd,
                item,
            },
            Err(err) => WorkerRun {
                label,
                ok: false,
                brief: err.to_string(),
                worktree: Some(path),
                usage: Usage::default(),
                spend_usd: 0.0,
                item,
            },
        }
    }

    /// Stage 4: the `verify` tool on the merged main checkout, when the
    /// engine has one. Runs the way the finish gate runs it: the approval
    /// policy decides whether it may run at all.
    async fn verify(&self, round: u32) -> VerifyState {
        let Some(tool) = self.engine.tools.get("verify") else {
            return VerifyState::NotConfigured;
        };
        if !self.engine.finish_gate_may_verify() {
            return VerifyState::NotPermitted;
        }
        self.note(progress::verifying(round)).await;
        let ctx = ToolCtx {
            sandbox: self.engine.config.sandbox,
            working_dir: self.work.clone(),
            engine: Some(Arc::new(self.engine.clone())),
            session_id: None,
        };
        let outcome = tool
            .execute(serde_json::json!({}), &ctx)
            .await
            .unwrap_or_else(|err| ToolOutcome::err(err.to_string()));
        if outcome.is_error {
            VerifyState::Failed(outcome.content)
        } else {
            VerifyState::Passed
        }
    }

    /// Stage 4, reviewer half: one tool-less review of the whole team change
    /// (base to HEAD) on the reviewer model the engine would use.
    async fn review(&self, round: u32, report: &mut TeamReport) -> (ReviewState, Option<WorkItem>) {
        let Some(reviewer) = self
            .engine
            .config
            .reviewer
            .clone()
            .or_else(|| self.engine.critic_reviewer(self.goal))
        else {
            return (ReviewState::NotConfigured, None);
        };
        let diff = worktree::diff(&self.work, &self.base, "HEAD", DIFF_CAP_BYTES)
            .await
            .filter(|diff| !diff.trim().is_empty());
        let Some(diff) = diff else {
            return (ReviewState::NoChange, None);
        };
        self.note(progress::reviewing(round)).await;
        let turn = self
            .engine
            .team_turn(
                self.goal,
                ActionKind::Chat,
                "review",
                Some(&reviewer.model),
                review::REVIEWER_SYSTEM,
                review::build_review_prompt(self.goal, &diff),
            )
            .await;
        let Some((text, usage, model)) = turn else {
            return (ReviewState::Unavailable, None);
        };
        self.record(&model, usage, report).await;
        let (verdict, findings) = review::parse_review(&text);
        match verdict {
            ReviewVerdict::NeedsFix => (
                ReviewState::NeedsFix(findings.len()),
                Some(WorkItem {
                    task: format!(
                        "The reviewer found problems in the merged change:\n{}\nFix them on the \
                         current tree.",
                        review::feedback_message(&findings)
                    ),
                    scope: Vec::new(),
                }),
            ),
            ReviewVerdict::Pass => (ReviewState::Passed, None),
            ReviewVerdict::Skipped => (ReviewState::Unavailable, None),
        }
    }

    /// Books a team-level model call: its usage event in the team session,
    /// and its tokens and priced spend in the report.
    async fn record(&self, model: &str, usage: Usage, report: &mut TeamReport) {
        let _ = self
            .engine
            .store
            .append(
                self.session,
                &SessionEvent::Usage {
                    model: model.to_string(),
                    usage,
                },
            )
            .await;
        report.add_spend(usage, self.engine.turn_cost_usd(model, &usage));
    }

    /// True once the engine's token budget or spend cap is spent.
    fn over_budget(&self, report: &TeamReport) -> bool {
        let config = &self.engine.config;
        report.usage.total() > config.max_total_tokens
            || config
                .max_cost_usd
                .is_some_and(|cap| report.spend_usd > cap)
    }
}

/// A run is clean when it stopped on a clean round or a verified one, no item
/// failed in any round, and the last round's verify and review are not
/// outstanding (passed, not configured, or nothing to review). Only a clean
/// run lands on the user's branch.
fn is_clean(stop: Stop, report: &TeamReport) -> bool {
    matches!(stop, Stop::Verified | Stop::Clean)
        && report.rounds.iter().all(|round| round.failed == 0)
        && report.rounds.last().is_some_and(|round| {
            matches!(
                round.verify,
                VerifyState::Passed | VerifyState::NotConfigured
            ) && matches!(
                round.review,
                ReviewState::Passed | ReviewState::NoChange | ReviewState::NotConfigured
            )
        })
}

/// The final summary: what ran, what merged, what failed, how it stopped,
/// and where the work landed.
fn render(opts: &TeamOptions, report: &TeamReport, stop: Stop, landing: &Landing) -> String {
    let items: usize = report.rounds.iter().map(|r| r.items).sum();
    let mut lines = vec![format!(
        "team: {items} worker item(s) in {} round(s), up to {} per round",
        report.rounds.len(),
        opts.workers
    )];
    for (index, round) in report.rounds.iter().enumerate() {
        lines.push(format!(
            "round {}: {} item(s), {} merged, {} conflict(s), {} failed; verify {}; review {}",
            index + 1,
            round.items,
            round.merged,
            round.conflicts,
            round.failed,
            round.verify.label(),
            round.review.label()
        ));
        for path in &round.kept {
            lines.push(format!("  kept worktree: {}", path.display()));
        }
    }
    for note in &report.notes {
        lines.push(format!("note: {note}"));
    }
    if report.open_items > 0 {
        lines.push(format!(
            "open: {} fix item(s) were not run",
            report.open_items
        ));
    }
    if let Some(output) = &report.last_verify_output {
        lines.push(format!("last verify output:\n{output}"));
    }
    lines.push(format!("stopped: {}", stop.label(opts.fix_round_cap())));
    match landing {
        Landing::FastForwarded => lines.push(
            "landed: your branch was fast-forwarded to the team's work; the team branch was \
             removed"
                .to_string(),
        ),
        Landing::Kept { branch, reason } => {
            lines.push(format!(
                "not landed ({reason}): your branch is unchanged and the team branch `{branch}` \
                 is kept"
            ));
            lines.push(format!("to merge it: git merge {branch}"));
        }
    }
    lines.push(format!(
        "tokens: {}, spend: ${:.4}",
        report.usage.total(),
        report.spend_usd
    ));
    lines.join("\n")
}

fn clip(text: &str) -> String {
    text.chars().take(OUTPUT_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{InMemoryStore, StaticSelector};
    use crate::{
        ApprovalMode, ApprovalPolicy, EngineConfig, ModelProvider, ReviewerConfig, SessionStore,
        StopReason, Tool, ToolRegistry, ToolSpec, TurnRequest, TurnResponse,
    };
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PLAN_PREFIX: &str = "You split a coding task";

    #[test]
    fn command_forms_and_refusals() {
        assert!(parse_command("fix the bug").is_none());
        assert!(parse_command("/teammate x").is_none());
        let (opts, goal) = parse_command("/team 4 refactor x").unwrap().unwrap();
        assert_eq!((opts.workers, goal.as_str()), (4, "refactor x"));
        let (opts, goal) = parse_command("/team build it").unwrap().unwrap();
        assert_eq!((opts.workers, goal.as_str()), (DEFAULT_WORKERS, "build it"));
        assert!(parse_command("/team").unwrap().is_err());
        assert!(
            parse_command("/team 3").unwrap().is_err(),
            "size without a task"
        );
        assert!(parse_command("/team 9 x").unwrap().is_err(), "over the cap");
        assert!(parse_command("/team 0 x").unwrap().is_err(), "zero workers");
    }

    #[test]
    fn plan_parses_capped_items_and_falls_back_on_garbage() {
        let text = r#"Here is the plan: {"items":[{"task":"a","scope":["x"]},{"task":" "},{"task":"b"},{"task":"c"}]}"#;
        let items = parse_plan(text, 2).expect("usable plan");
        assert_eq!(items.len(), 2, "empty tasks skipped, list cut at the cap");
        assert_eq!(items[0].scope, vec!["x".to_string()]);
        assert_eq!(items[1].task, "b");
        assert!(parse_plan("not json at all", 3).is_none());
        assert!(parse_plan(r#"{"items":[]}"#, 3).is_none());
        assert!(parse_plan(r#"{"items":[{"task":"  "}]}"#, 3).is_none());
    }

    /// A worker writes the `WRITE <path>=<content>` lines of its task, one
    /// `write` call each, then answers.
    fn worker_turn(req: &TurnRequest) -> TurnResponse {
        let task = req
            .messages
            .iter()
            .find(|m| m.role == crate::Role::User)
            .map(|m| m.content.as_str())
            .unwrap_or("");
        let mut writes: Vec<(String, String)> = task
            .lines()
            .filter_map(|line| line.strip_prefix("WRITE "))
            .filter_map(|rest| rest.split_once('='))
            .map(|(path, content)| (path.trim().to_string(), content.trim().to_string()))
            .collect();
        if task.contains("conflicts with the code already merged") {
            writes = vec![("shared.txt".into(), "resolved".into())];
        }
        let done = req
            .messages
            .iter()
            .filter(|m| m.role == crate::Role::Tool)
            .count();
        match writes.get(done) {
            Some((path, content)) => call(
                "write",
                serde_json::json!({"path": path, "content": content}),
            ),
            None => text("done"),
        }
    }

    fn text(content: &str) -> TurnResponse {
        TurnResponse {
            text: content.into(),
            tool_calls: Vec::new(),
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::EndTurn,
            provider_blocks: Vec::new(),
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> TurnResponse {
        TurnResponse {
            text: String::new(),
            tool_calls: vec![crate::ToolCall {
                id: format!("{name}-{}", uuid::Uuid::new_v4()),
                name: name.into(),
                arguments,
            }],
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
            },
            stop_reason: StopReason::ToolUse,
            provider_blocks: Vec::new(),
        }
    }

    type Respond = dyn Fn(&TurnRequest) -> TurnResponse + Send + Sync;

    /// The mock provider for team tests: the plan, reviewer and worker turns
    /// are told apart by their system prompt, so parallel workers cannot
    /// consume each other's scripted answers.
    struct Keyed {
        respond: Box<Respond>,
    }

    #[async_trait]
    impl ModelProvider for Keyed {
        fn id(&self) -> &str {
            "keyed"
        }

        async fn turn(&self, req: Arc<TurnRequest>) -> Result<TurnResponse> {
            Ok((self.respond)(&req))
        }
    }

    struct WriteTool;

    #[async_trait]
    impl Tool for WriteTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "write".into(),
                description: "write a file".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(&self, args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
            let path = args["path"].as_str().unwrap_or_default();
            let content = args["content"].as_str().unwrap_or_default();
            let full = ctx.working_dir.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
            Ok(ToolOutcome::ok(format!("wrote {path}")))
        }
    }

    /// Verify: the integration worktree (the merged tree) fails its first
    /// `fail_main` calls, then passes. Item and delegate worktrees always pass
    /// and do not count, so the finish gate inside a worker cannot skew the
    /// count.
    struct VerifyTool {
        fail_main: usize,
        main_calls: AtomicUsize,
    }

    #[async_trait]
    impl Tool for VerifyTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "verify".into(),
                description: "verify".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(&self, _args: serde_json::Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
            // The integration worktree is `team-<id>` (one dash); item and
            // delegate worktrees carry a second dash or an 8-hex name.
            let name = ctx
                .working_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let integration = name.starts_with("team-") && name.matches('-').count() == 1;
            if !integration {
                return Ok(ToolOutcome::ok("worker check ok"));
            }
            let n = self.main_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= self.fail_main {
                Ok(ToolOutcome::err("check failed: assertion 3 != 4"))
            } else {
                Ok(ToolOutcome::ok("all green"))
            }
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "team@test.invalid"]);
        git(dir.path(), &["config", "user.name", "team test"]);
        std::fs::write(dir.path().join("README.md"), "base\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-qm", "init"]);
        dir
    }

    struct Harness {
        engine: Engine,
        store: Arc<InMemoryStore>,
    }

    /// An engine over `dir` with the `keyed` provider, the write tool and
    /// (optionally) a verify tool.
    fn harness(
        dir: &Path,
        respond: impl Fn(&TurnRequest) -> TurnResponse + Send + Sync + 'static,
        verify: Option<VerifyTool>,
        tweak: impl FnOnce(&mut EngineConfig),
    ) -> Harness {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(WriteTool) as Arc<dyn Tool>);
        if let Some(verify) = verify {
            registry.register(Arc::new(verify) as Arc<dyn Tool>);
        }
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "keyed".into(),
            Arc::new(Keyed {
                respond: Box::new(respond),
            }),
        );
        let mut config = EngineConfig {
            working_dir: dir.to_path_buf(),
            default_provider: "keyed".into(),
            default_model: "m".into(),
            approvals: ApprovalPolicy {
                mode: ApprovalMode::ApproveAll,
                ..ApprovalPolicy::default()
            },
            ..EngineConfig::default()
        };
        tweak(&mut config);
        let store = Arc::new(InMemoryStore::default());
        let engine = Engine {
            selector: Arc::new(StaticSelector::new("m", "keyed")),
            providers,
            tools: Arc::new(registry),
            store: Arc::clone(&store) as Arc<dyn SessionStore>,
            config,
        };
        Harness { engine, store }
    }

    /// The plan, reviewer and worker turns, keyed by system prompt.
    fn plan_and_workers(plan: &'static str) -> impl Fn(&TurnRequest) -> TurnResponse {
        move |req: &TurnRequest| {
            if req.system.starts_with(PLAN_PREFIX) {
                text(plan)
            } else {
                worker_turn(req)
            }
        }
    }

    fn opts(workers: u32) -> TeamOptions {
        TeamOptions {
            workers,
            ..TeamOptions::default()
        }
    }

    const TWO_FILES: &str = r#"{"items":[{"task":"WRITE a.txt=alpha","scope":["a.txt"]},{"task":"WRITE b.txt=beta","scope":["b.txt"]}]}"#;

    #[tokio::test]
    async fn plan_splits_into_two_items_and_both_run() {
        let dir = repo();
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), None, |_| {});
        let summary = h.engine.run_team("make the files", opts(2)).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "beta"
        );
        assert_eq!(summary.turns, 1, "one round");
        assert_eq!(summary.tool_calls, 2, "two items");
        assert!(summary.answer.contains("2 merged"), "{}", summary.answer);
        assert!(
            summary.answer.contains("stopped: the round was clean"),
            "{}",
            summary.answer
        );
        assert!(summary.usage.total() > 0, "plan and worker usage counted");
        let events = h.store.load(summary.session_id).await.unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, SessionEvent::Message { .. })),
            "the summary is stored as the team session's answer"
        );
    }

    #[tokio::test]
    async fn slash_team_is_routed_by_run_task() {
        // The app and the server submit `/team N <task>` as a plain task; the
        // engine's task entry point must pick team mode, not one model turn.
        let dir = repo();
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), None, |_| {});
        let summary = h.engine.run_task("/team 2 make the files").await.unwrap();
        assert_eq!(
            summary.tool_calls, 2,
            "two team items ran: {}",
            summary.answer
        );
        assert!(dir.path().join("b.txt").exists());
        assert!(
            h.engine.run_task("/team 9 too many").await.is_err(),
            "size cap"
        );
    }

    #[tokio::test]
    async fn single_item_fallback_runs_the_whole_task() {
        let dir = repo();
        let h = harness(
            dir.path(),
            plan_and_workers("I would rather not split this."),
            None,
            |_| {},
        );
        let summary = h
            .engine
            .run_team("WRITE only.txt=solo", opts(3))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("only.txt")).unwrap(),
            "solo"
        );
        assert_eq!(summary.tool_calls, 1, "one item");
        assert!(
            summary.answer.contains("the plan did not parse"),
            "{}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn merge_conflict_produces_a_fix_round() {
        let dir = repo();
        let plan =
            r#"{"items":[{"task":"WRITE shared.txt=first"},{"task":"WRITE shared.txt=second"}]}"#;
        let h = harness(dir.path(), plan_and_workers(plan), None, |_| {});
        let summary = h.engine.run_team("share a file", opts(2)).await.unwrap();
        assert_eq!(
            summary.turns, 2,
            "one conflict, one fix round: {}",
            summary.answer
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("shared.txt")).unwrap(),
            "resolved",
            "the fix item re-applied the conflicting change on the merged tree"
        );
        assert!(
            summary.answer.contains("1 conflict(s)"),
            "{}",
            summary.answer
        );
        assert!(
            summary.answer.contains("stopped: the round was clean"),
            "{}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn verify_failure_triggers_rounds_and_stops_at_the_cap() {
        let dir = repo();
        let verify = VerifyTool {
            fail_main: usize::MAX,
            main_calls: AtomicUsize::new(0),
        };
        let h = harness(
            dir.path(),
            plan_and_workers(r#"{"items":[{"task":"WRITE x.txt=1"}]}"#),
            Some(verify),
            |_| {},
        );
        let mut team = opts(1);
        team.max_rounds = Some(2);
        let summary = h.engine.run_team("keep checking", team).await.unwrap();
        assert_eq!(
            summary.turns, 3,
            "first round plus two fix rounds: {}",
            summary.answer
        );
        assert!(
            summary.answer.contains("fix-round cap reached (2)"),
            "{}",
            summary.answer
        );
        assert!(
            summary.answer.contains("assertion 3 != 4"),
            "{}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn default_cap_is_two_fix_rounds_and_until_verified_raises_it() {
        // Verify fails four times, then passes.
        let mk = || VerifyTool {
            fail_main: 4,
            main_calls: AtomicUsize::new(0),
        };
        let dir = repo();
        let h = harness(
            dir.path(),
            plan_and_workers(r#"{"items":[{"task":"WRITE x.txt=1"}]}"#),
            Some(mk()),
            |_| {},
        );
        let capped = h.engine.run_team("g", opts(1)).await.unwrap();
        assert_eq!(capped.turns, 3, "default cap stops after two fix rounds");
        assert!(
            capped.answer.contains("fix-round cap reached"),
            "{}",
            capped.answer
        );

        let dir = repo();
        let h = harness(
            dir.path(),
            plan_and_workers(r#"{"items":[{"task":"WRITE x.txt=1"}]}"#),
            Some(mk()),
            |_| {},
        );
        let until = TeamOptions {
            until_verified: true,
            ..opts(1)
        };
        let verified = h.engine.run_team("g", until).await.unwrap();
        assert_eq!(
            verified.turns, 5,
            "four failures, then the pass: {}",
            verified.answer
        );
        assert!(
            verified.answer.contains("stopped: verify passed"),
            "{}",
            verified.answer
        );
    }

    #[tokio::test]
    async fn reviewer_needs_fix_triggers_a_round() {
        let dir = repo();
        let reviews = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&reviews);
        let h = harness(
            dir.path(),
            move |req: &TurnRequest| {
                if req.system.starts_with(PLAN_PREFIX) {
                    text(r#"{"items":[{"task":"WRITE r.txt=1"}]}"#)
                } else if req.system == review::REVIEWER_SYSTEM {
                    let n = seen.fetch_add(1, Ordering::SeqCst);
                    text(if n == 0 {
                        "VERDICT: NEEDS_FIX"
                    } else {
                        "VERDICT: PASS"
                    })
                } else {
                    worker_turn(req)
                }
            },
            None,
            |c| {
                c.reviewer = Some(ReviewerConfig {
                    model: "m".into(),
                    max_rounds: 1,
                })
            },
        );
        let summary = h.engine.run_team("review me", opts(1)).await.unwrap();
        assert_eq!(
            summary.turns, 2,
            "review failure becomes a fix round: {}",
            summary.answer
        );
        assert_eq!(reviews.load(Ordering::SeqCst), 2);
        assert!(summary.answer.contains("needs fix"), "{}", summary.answer);
    }

    #[tokio::test]
    async fn token_budget_stops_the_team_between_rounds() {
        let dir = repo();
        let verify = VerifyTool {
            fail_main: usize::MAX,
            main_calls: AtomicUsize::new(0),
        };
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), Some(verify), |c| {
            c.max_total_tokens = 1
        });
        let summary = h.engine.run_team("spend", opts(2)).await.unwrap();
        assert_eq!(summary.turns, 1, "no second round once the budget is spent");
        assert!(
            summary.answer.contains("token budget or spend cap"),
            "{}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn spend_cap_stops_the_team_between_rounds() {
        let dir = repo();
        let verify = VerifyTool {
            fail_main: usize::MAX,
            main_calls: AtomicUsize::new(0),
        };
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), Some(verify), |c| {
            c.max_cost_usd = Some(0.000_001);
            c.price_lookup = Some(Arc::new(|_: &str| Some((1.0, 1.0))));
        });
        let summary = h.engine.run_team("spend", opts(2)).await.unwrap();
        assert_eq!(summary.turns, 1, "{}", summary.answer);
        assert!(
            summary.answer.contains("token budget or spend cap"),
            "{}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn not_permitted_verify_stops_unverified() {
        let dir = repo();
        let verify = VerifyTool {
            fail_main: 0,
            main_calls: AtomicUsize::new(0),
        };
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), Some(verify), |c| {
            c.approvals.deny.push("verify".into())
        });
        let summary = h.engine.run_team("no verify", opts(2)).await.unwrap();
        assert_eq!(summary.turns, 1);
        assert!(
            summary.answer.contains("not permitted"),
            "{}",
            summary.answer
        );
    }

    #[tokio::test]
    async fn outside_a_git_repo_the_team_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), None, |_| {});
        let err = h.engine.run_team("nope", opts(2)).await.unwrap_err();
        assert!(err.to_string().contains("git repository"), "{err}");
    }

    /// Plans instantly, then blocks every worker turn, so the test can
    /// interrupt the team while a worker is in flight.
    struct Blocking {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ModelProvider for Blocking {
        fn id(&self) -> &str {
            "keyed"
        }

        async fn turn(&self, req: Arc<TurnRequest>) -> Result<TurnResponse> {
            if req.system.starts_with(PLAN_PREFIX) {
                return Ok(text(TWO_FILES));
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            Ok(text("never"))
        }
    }

    #[tokio::test]
    async fn interrupt_aborts_the_team_and_its_children() {
        let dir = repo();
        let h = harness(dir.path(), |_| text("unused"), None, |_| {});
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "keyed".into(),
            Arc::new(Blocking {
                calls: AtomicUsize::new(0),
            }),
        );
        let engine = Engine {
            providers,
            ..h.engine.clone()
        };
        let store = Arc::clone(&h.store);
        let run = tokio::spawn(async move { engine.run_team("interrupt me", opts(2)).await });

        // Wait until a worker child is live under the team's session.
        let mut session = None;
        for _ in 0..500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if session.is_none() {
                session = store.list().await.unwrap().first().map(|m| m.id);
            }
            if let Some(id) = session {
                if !crate::subagent::live_agents(id).is_empty() {
                    break;
                }
            }
        }
        let id = session.expect("the team opened a session");
        assert!(
            !crate::subagent::live_agents(id).is_empty(),
            "a worker is live"
        );

        // The interrupt: the run task is aborted, as the server does.
        run.abort();
        let _ = run.await;
        assert!(
            crate::subagent::live_agents(id).is_empty(),
            "no child outlives the interrupt"
        );
        assert!(
            crate::run_scope().token(id).is_none(),
            "the team run is no longer live"
        );
        assert_no_team_state(dir.path());
    }

    /// `git <args>` in `dir`, its stdout trimmed.
    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// No team worktree directory, marker or `team/` branch is left in `dir`.
    fn assert_no_team_state(dir: &Path) {
        let root = dir.join(".modron").join("worktrees");
        let leftovers: Vec<String> = std::fs::read_dir(&root)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("team-"))
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "team leftovers: {leftovers:?}");
        let branches = git_out(dir, &["branch", "--list", "team/*"]);
        assert!(branches.is_empty(), "team branch left: {branches}");
    }

    #[tokio::test]
    async fn clean_run_fast_forwards_the_user_branch_and_deletes_the_team_branch() {
        let dir = repo();
        let base = git_out(dir.path(), &["rev-parse", "HEAD"]);
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), None, |_| {});
        let summary = h.engine.run_team("make the files", opts(2)).await.unwrap();
        assert!(
            summary
                .answer
                .contains("landed: your branch was fast-forwarded"),
            "{}",
            summary.answer
        );
        assert!(dir.path().join("a.txt").exists() && dir.path().join("b.txt").exists());
        let head = git_out(dir.path(), &["rev-parse", "HEAD"]);
        assert_ne!(head, base, "the user's branch moved");
        git(dir.path(), &["merge-base", "--is-ancestor", &base, &head]);
        assert_no_team_state(dir.path());
    }

    #[tokio::test]
    async fn unclean_run_leaves_the_user_branch_and_keeps_the_team_branch() {
        let dir = repo();
        let base = git_out(dir.path(), &["rev-parse", "HEAD"]);
        let verify = VerifyTool {
            fail_main: usize::MAX,
            main_calls: AtomicUsize::new(0),
        };
        let h = harness(
            dir.path(),
            plan_and_workers(r#"{"items":[{"task":"WRITE x.txt=1"}]}"#),
            Some(verify),
            |_| {},
        );
        let team = TeamOptions {
            max_rounds: Some(0),
            ..opts(1)
        };
        let summary = h.engine.run_team("keep it", team).await.unwrap();
        assert!(
            summary.answer.contains("your branch is unchanged"),
            "{}",
            summary.answer
        );
        assert_eq!(git_out(dir.path(), &["rev-parse", "HEAD"]), base);
        assert!(!dir.path().join("x.txt").exists(), "nothing landed");
        let branches = git_out(dir.path(), &["branch", "--list", "team/*"]);
        let branch = branches.trim().trim_start_matches("* ").trim().to_string();
        assert!(branch.starts_with("team/"), "team branch kept: {branches}");
        let id = branch.trim_start_matches("team/");
        assert!(
            summary.answer.contains(&format!("git merge team/{id}")),
            "the merge command is named: {}",
            summary.answer
        );
        // The integration worktree is gone; the branch is what remains.
        assert!(!dir
            .path()
            .join(".modron/worktrees")
            .join(format!("team-{id}"))
            .exists());
    }

    #[tokio::test]
    async fn stage_notes_arrive_in_order_and_stay_out_of_the_model_request() {
        let dir = repo();
        let sent = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = Arc::clone(&sent);
        let plan = plan_and_workers(TWO_FILES);
        let verify = VerifyTool {
            fail_main: 0,
            main_calls: AtomicUsize::new(0),
        };
        let h = harness(
            dir.path(),
            move |req: &TurnRequest| {
                let mut log = seen.lock().unwrap();
                log.push(req.system.clone());
                log.extend(req.messages.iter().map(|m| m.content.clone()));
                drop(log);
                plan(req)
            },
            Some(verify),
            |_| {},
        );
        let summary = h.engine.run_team("make the files", opts(2)).await.unwrap();
        let events = h.store.load(summary.session_id).await.unwrap();
        let notes: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                SessionEvent::Progress { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            notes,
            vec![
                "team plan: 2 item(s)".to_string(),
                "round 1 started: 2 item(s)".to_string(),
                "round 1 merging 2 worker item(s)".to_string(),
                "round 1 verifying".to_string(),
                "round 1 result: 2 merged, 0 conflict(s), 0 failed; verify passed; review not configured"
                    .to_string(),
            ],
            "the stage notes, in the order the run wrote them"
        );
        let sent = sent.lock().unwrap();
        assert!(
            sent.iter()
                .all(|text| !text.contains("team plan:") && !text.contains("round 1 ")),
            "no stage note reaches a provider request"
        );
    }

    #[tokio::test]
    async fn dirty_tracked_tree_refuses_the_team_before_any_work() {
        let dir = repo();
        std::fs::write(dir.path().join("README.md"), "edited\n").unwrap();
        let h = harness(dir.path(), plan_and_workers(TWO_FILES), None, |_| {});
        let err = h.engine.run_team("nope", opts(2)).await.unwrap_err();
        assert!(err.to_string().contains("clean working tree"), "{err}");
        assert_no_team_state(dir.path());
        assert!(!dir.path().join("a.txt").exists());
    }
}
