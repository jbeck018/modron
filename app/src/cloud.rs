//! The web Cloud screen and the owner vault (PLAN 10.9 item 3, 10.10 "App"),
//! target-neutral: the bridge's rows, the request URLs and bodies, the run
//! row's actions, the repo picker's pause, and the vault's key list. The
//! HTTP calls and the page's timers live in `web.rs` (wasm32 only). The
//! words come from the client core (`view.rs`).

use modron_mobile_core::view::{account_line, repo_line, run_line};
use serde::Deserialize;
use serde_json::json;

/// The repo picker sends its query once the text has not changed this long.
pub const QUERY_PAUSE_MS: f64 = 300.0;
/// The runs list refreshes this often while it shows.
pub const RUNS_REFRESH_MS: f64 = 5_000.0;

/// One `GET /api/github/accounts` row.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Account {
    pub id: String,
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub kind: String,
}

/// The accounts of a `GET /api/github/accounts` body (`{ accounts: [...] }`).
pub fn parse_accounts(body: &str) -> Option<Vec<Account>> {
    #[derive(Deserialize)]
    struct Body {
        accounts: Vec<Account>,
    }
    serde_json::from_str::<Body>(body).ok().map(|b| b.accounts)
}

/// One `GET /api/github/repos` row.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Repo {
    pub full_name: String,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub default_branch: String,
    #[serde(default)]
    pub pushed_at: String,
}

/// One page of repos and the page that follows it (`None` at the end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoPage {
    pub repos: Vec<Repo>,
    pub next_page: Option<u32>,
}

/// The page of a `GET /api/github/repos` body, or `None` when it is not one.
pub fn parse_repos(body: &str) -> Option<RepoPage> {
    #[derive(Deserialize)]
    struct Body {
        repos: Vec<Repo>,
        #[serde(default)]
        next_page: Option<u32>,
    }
    serde_json::from_str::<Body>(body)
        .ok()
        .map(|b| RepoPage { repos: b.repos, next_page: b.next_page })
}

/// One run row (`GET /api/runs`, or the `POST` answer). `reason` is set for a
/// failed run.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Run {
    pub id: String,
    #[serde(default)]
    pub repo: String,
    #[serde(default)]
    pub account: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub session_url: String,
}

/// The runs of a `GET /api/runs` body (`{ runs: [...] }`).
pub fn parse_runs(body: &str) -> Option<Vec<Run>> {
    #[derive(Deserialize)]
    struct Body {
        runs: Vec<Run>,
    }
    serde_json::from_str::<Body>(body).ok().map(|b| b.runs)
}

/// The runs list's row as the screen shows it: the shared text, the
/// actions it offers, and the session it opens.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RunRow {
    pub id: String,
    pub text: String,
    /// The state word the bridge sent (for the run's actions).
    pub state: String,
    pub session_url: String,
    pub stop: bool,
    pub wake: bool,
}

/// The runs list rows of a parsed listing, in the server's order.
pub fn run_rows(runs: &[Run]) -> Vec<RunRow> {
    runs.iter()
        .map(|run| {
            let actions = run_actions(&run.state);
            RunRow {
                id: run.id.clone(),
                text: run_line(&run.repo, &run.branch, &run.state, &run.reason, &run.created_at),
                state: run.state.clone(),
                session_url: run.session_url.clone(),
                stop: actions.stop,
                wake: actions.wake,
            }
        })
        .collect()
}

/// The account rows of a parsed listing (the shared line per account).
pub fn account_rows(accounts: &[Account]) -> Vec<(String, String, String)> {
    accounts
        .iter()
        .map(|a| (a.id.clone(), a.login.clone(), account_line(&a.login, &a.kind)))
        .collect()
}

/// The repo rows of a page (the shared line; the branch a pick fills in).
pub fn repo_rows(repos: &[Repo]) -> Vec<(String, String, String)> {
    repos
        .iter()
        .map(|r| {
            (
                r.full_name.clone(),
                r.default_branch.clone(),
                repo_line(&r.full_name, r.private, &r.pushed_at),
            )
        })
        .collect()
}

/// Which controls a run offers: stop while it may still run, wake when it
/// is not running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunActions {
    pub stop: bool,
    pub wake: bool,
}

pub fn run_actions(state: &str) -> RunActions {
    RunActions {
        stop: matches!(state, "starting" | "running" | "sleeping"),
        wake: matches!(state, "stopped" | "sleeping" | "failed"),
    }
}

/// The URL a run's open control goes to: only a same-origin path (the
/// bridge's `/app/?url=...`), as the portal list's rows do.
pub fn open_target(session_url: &str) -> Option<String> {
    (session_url.starts_with('/') && !session_url.starts_with("//") && !session_url.is_empty())
        .then(|| session_url.to_string())
}

/// Percent-encodes everything but the RFC 3986 unreserved characters.
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

pub fn accounts_url() -> String {
    "/api/github/accounts".to_string()
}

pub fn account_url(id: &str) -> String {
    format!("/api/github/accounts/{}", percent_encode(id))
}

/// `GET /api/github/repos?account=<id>&q=<text>&page=<n>`.
pub fn repos_url(account: &str, query: &str, page: u32) -> String {
    format!(
        "/api/github/repos?account={}&q={}&page={page}",
        percent_encode(account),
        percent_encode(query)
    )
}

pub fn runs_url() -> String {
    "/api/runs".to_string()
}

pub fn run_action_url(id: &str, action: &str) -> String {
    format!("/api/runs/{}/{action}", percent_encode(id))
}

pub fn credentials_url() -> String {
    "/api/credentials".to_string()
}

pub fn credential_url(provider: &str) -> String {
    format!("/api/credentials/{}", percent_encode(provider))
}

/// The POST body of `/api/github/accounts`: the token, nothing else.
pub fn add_account_body(token: &str) -> String {
    json!({ "token": token }).to_string()
}

/// The POST body of `/api/runs`: the account, the repo and the branch.
pub fn start_run_body(account: &str, repo: &str, branch: &str) -> String {
    json!({ "account": account, "repo": repo, "branch": branch }).to_string()
}

/// The PUT body of `/api/credentials/<id>`: the key, nothing else.
pub fn credential_body(key: &str) -> String {
    json!({ "key": key }).to_string()
}

/// The GitHub status a refused token's `400` carries (`github_status`), or
/// `None`. The bridge never sends the token back.
pub fn refused_status(body: &str) -> Option<u16> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value["github_status"].as_u64().and_then(|s| u16::try_from(s).ok())
}

/// The vault's key list: `(provider id, set)` per provider. Accepts the
/// `{ credentials: [...] }` or `{ providers: [...] }` object, or a bare array
/// of `{ id, set }` rows.
pub fn credential_rows(body: &str) -> Option<Vec<(String, bool)>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let list = match &value {
        serde_json::Value::Array(rows) => rows,
        other => other.get("credentials").or_else(|| other.get("providers"))?.as_array()?,
    };
    Some(
        list.iter()
            .filter_map(|row| {
                // The bridge names the field `provider`; `id` is accepted too.
                let id = row["provider"].as_str().or_else(|| row["id"].as_str())?.to_string();
                Some((id, row["set"].as_bool().unwrap_or(false)))
            })
            .collect(),
    )
}

/// One linked account row on the Cloud screen.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AccountRow {
    pub id: String,
    pub login: String,
    pub text: String,
}

/// One repo row of the picker.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RepoRow {
    pub full_name: String,
    pub default_branch: String,
    pub text: String,
}

/// Everything the Cloud screen and the repo picker show (the navigation
/// state holds one of these, and the host sets it from the bridge's answers).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct CloudView {
    /// The bridge answered the accounts route 200 for this owner.
    pub available: bool,
    pub accounts: Vec<AccountRow>,
    /// The account a new run uses (an index into `accounts`).
    pub account: usize,
    pub runs: Vec<RunRow>,
    /// The shared note line (not configured, a refused token, a failure).
    pub note: Option<String>,
    /// The masked token entry is open.
    pub adding: bool,
    /// The repo picker's rows, and the page that follows them.
    pub repos: Vec<RepoRow>,
    pub next_page: Option<u32>,
    /// The repo a new run uses, and its branch (the default branch until edited).
    pub repo: Option<RepoRow>,
    pub branch: String,
}

impl CloudView {
    /// The account a run uses, when there is one.
    pub fn chosen_account(&self) -> Option<&AccountRow> {
        self.accounts.get(self.account)
    }

    /// The start action's body parts: the account, the repo and a non-empty
    /// branch. None when any is missing (the start control is not offered).
    pub fn start_parts(&self) -> Option<(String, String, String)> {
        let account = self.chosen_account()?.id.clone();
        let repo = self.repo.as_ref()?;
        let branch = self.branch.trim().to_string();
        (!branch.is_empty()).then(|| (account, repo.full_name.clone(), branch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUNS: &str = r#"{"runs":[
        {"id":"r1","repo":"acme/app","account":"a1","branch":"main","state":"running",
         "created_at":"2026-11-15T10:20:30Z","session_url":"/app/?url=ws%3A%2F%2Fh%2Fws%2Fk"},
        {"id":"r2","repo":"acme/api","account":"a1","branch":"dev","state":"failed",
         "reason":"exit 2","created_at":"2026-11-14T08:00:00Z","session_url":""}
    ]}"#;

    #[test]
    fn accounts_repos_and_runs_parse_and_ignore_unknown_fields() {
        let accounts =
            parse_accounts(r#"{"accounts":[{"id":"a1","login":"octocat","kind":"token","x":1}]}"#)
                .unwrap();
        assert_eq!(accounts[0].login, "octocat");
        assert!(parse_accounts("[]").is_none());
        let page = parse_repos(
            r#"{"repos":[{"full_name":"acme/app","private":true,"default_branch":"main","pushed_at":"2026-11-14T08:00:00Z"}],"next_page":2}"#,
        )
        .unwrap();
        assert_eq!(page.next_page, Some(2));
        assert!(page.repos[0].private);
        let last = parse_repos(r#"{"repos":[],"next_page":null}"#).unwrap();
        assert_eq!(last.next_page, None);
        let runs = parse_runs(RUNS).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].reason, "exit 2");
    }

    #[test]
    fn run_rows_carry_the_shared_text_and_the_actions_for_their_state() {
        let rows = run_rows(&parse_runs(RUNS).unwrap());
        assert_eq!(rows[0].text, "acme/app \u{b7} main \u{b7} running \u{b7} 2026-11-15 10:20");
        assert!(rows[0].stop && !rows[0].wake);
        assert_eq!(
            rows[1].text,
            "acme/api \u{b7} dev \u{b7} failed \u{b7} exit 2 \u{b7} 2026-11-14 08:00"
        );
        assert!(!rows[1].stop && rows[1].wake);
    }

    #[test]
    fn run_states_offer_stop_or_wake_and_never_both_for_a_running_run() {
        assert_eq!(run_actions("starting"), RunActions { stop: true, wake: false });
        assert_eq!(run_actions("sleeping"), RunActions { stop: true, wake: true });
        assert_eq!(run_actions("stopped"), RunActions { stop: false, wake: true });
        assert_eq!(run_actions("failed"), RunActions { stop: false, wake: true });
    }

    #[test]
    fn open_targets_are_same_origin_paths_only() {
        assert_eq!(open_target("/app/?url=x").as_deref(), Some("/app/?url=x"));
        assert_eq!(open_target("//evil.example/"), None);
        assert_eq!(open_target("https://evil.example/"), None);
        assert_eq!(open_target(""), None);
    }

    #[test]
    fn urls_encode_their_parts() {
        assert_eq!(accounts_url(), "/api/github/accounts");
        assert_eq!(repos_url("a1", "app x", 3), "/api/github/repos?account=a1&q=app%20x&page=3");
        assert_eq!(runs_url(), "/api/runs");
        assert_eq!(run_action_url("r 1", "stop"), "/api/runs/r%201/stop");
        assert_eq!(account_url("a1"), "/api/github/accounts/a1");
        assert_eq!(credential_url("openai"), "/api/credentials/openai");
        assert_eq!(percent_encode("a/b?c=d&e"), "a%2Fb%3Fc%3Dd%26e");
    }

    #[test]
    fn request_bodies_carry_exactly_their_fields() {
        let value = |s: String| -> serde_json::Value { serde_json::from_str(&s).unwrap() };
        assert_eq!(value(add_account_body("ghp_x")), json!({ "token": "ghp_x" }));
        assert_eq!(
            value(start_run_body("a1", "acme/app", "main")),
            json!({ "account": "a1", "repo": "acme/app", "branch": "main" })
        );
        assert_eq!(value(credential_body("sk-1")), json!({ "key": "sk-1" }));
    }

    #[test]
    fn a_refused_token_reports_githubs_status_from_the_400() {
        assert_eq!(refused_status(r#"{"error":"refused","github_status":401}"#), Some(401));
        assert_eq!(refused_status(r#"{"error":"refused"}"#), None);
        assert_eq!(refused_status("not json"), None);
    }

    #[test]
    fn the_vault_list_reads_set_and_unset_per_provider() {
        let rows = credential_rows(
            r#"{"credentials":[{"id":"anthropic","set":true},{"id":"openai","set":false}]}"#,
        )
        .unwrap();
        assert_eq!(rows, vec![("anthropic".to_string(), true), ("openai".to_string(), false)]);
        let bridge = credential_rows(
            r#"{"credentials":[{"provider":"zai","env_var":"ZAI_API_KEY","set":true,"unreadable":false}]}"#,
        )
        .unwrap();
        assert_eq!(bridge, vec![("zai".to_string(), true)]);
        let bare = credential_rows(r#"[{"id":"openai","set":true}]"#).unwrap();
        assert_eq!(bare, vec![("openai".to_string(), true)]);
        assert!(credential_rows("{}").is_none());
    }

    #[test]
    fn the_start_parts_need_an_account_a_repo_and_a_branch() {
        let mut view = CloudView {
            accounts: vec![AccountRow {
                id: "a1".into(),
                login: "octocat".into(),
                text: String::new(),
            }],
            repo: Some(RepoRow {
                full_name: "acme/app".into(),
                default_branch: "main".into(),
                text: String::new(),
            }),
            branch: "main".into(),
            ..CloudView::default()
        };
        assert_eq!(view.start_parts(), Some(("a1".into(), "acme/app".into(), "main".into())));
        view.branch = "  ".into();
        assert_eq!(view.start_parts(), None, "a blank branch is not started");
        view.branch = "dev".into();
        view.repo = None;
        assert_eq!(view.start_parts(), None);
    }

    #[test]
    fn repo_and_account_rows_are_built_from_the_listing() {
        let page = parse_repos(r#"{"repos":[{"full_name":"acme/app","private":false,"default_branch":"main","pushed_at":""}]}"#)
            .unwrap();
        let rows = repo_rows(&page.repos);
        assert_eq!(rows, vec![("acme/app".into(), "main".into(), "acme/app \u{b7} public".into())]);
        let accounts =
            parse_accounts(r#"{"accounts":[{"id":"a1","login":"octocat","kind":"token"}]}"#)
                .unwrap();
        assert_eq!(
            account_rows(&accounts),
            vec![("a1".into(), "octocat".into(), "octocat \u{b7} token".into())]
        );
    }
}
