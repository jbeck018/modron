//! The web host: the same link, driver and demo as the terminal host, clocked
//! by the page's own timer instead of a thread. The client core connects on
//! the browser event loop (`spawn_local`), so nothing here blocks.
//!
//! The page address selects the mode: `?url=<ws url>[&token=<t>][&session=<uuid>]`
//! attaches to a live server. With no `url`, the page asks its own origin
//! (`GET /api/me`, the owner cookie included): 200 is the owner's portal (the
//! session list), 401 the sign-in screen, anything else the built-in demo.
//! The owner portal's calls are same-origin `fetch`es, answered on the page's
//! timer through a channel; the token is sent once and never stored.

use std::{cell::RefCell, rc::Rc};

use modron_mobile_core::view::{
    invite_row, token_refused_line, CLOUD_FAILED_LABEL, CLOUD_NOT_CONFIGURED_LABEL, COPIED_LABEL,
    PUSH_BLOCKED_LABEL, PUSH_FAILED_LABEL, PUSH_NOT_ALLOWED_LABEL, VAULT_SAVE_FAILED_LABEL,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::{spawn_local, JsFuture};

use crate::{
    chat::{host_key, link, Client, Host, UiAction},
    cloud::{self, AccountRow, RepoRow, QUERY_PAUSE_MS, RUNS_REFRESH_MS},
    demo::Player,
    nav::{InviteLine, Screen},
    portal::{self, Mode},
    push::{self, Permission},
    session::{Driver, Live, LiveOptions, TICK},
};

/// How often the portal list refreshes while it shows.
const PORTAL_REFRESH_MS: f64 = 15_000.0;
/// The lifetime of a share link the owner creates here.
const INVITE_TTL_HOURS: u32 = 24;

/// The answers of the page's HTTP calls. A status of 0 means no answer
/// (the request failed).
enum Reply {
    Me(u16),
    Owner(u16),
    Sessions(u16, String),
    SignedIn(u16),
    SignedOut,
    Invites(u16, String),
    Created(u16, String),
    Changed,
    Copied,
    /// The page's push facts at load (see `probe_push`).
    PushProbe(Probe),
    /// A Settings push tap finished: the endpoint now subscribed (None when
    /// off), or the shared reason it did not change.
    PushDone(Result<Option<String>, &'static str>),
    /// The owner's accounts (the load-time probe, and each refresh).
    CloudAccounts(u16, String),
    /// The runs listing (the timer, and each refresh).
    CloudRuns(u16, String),
    /// A token was sent for a new account (its refusal shows GitHub's status).
    CloudAdded(u16, String),
    /// A remove, start, stop or wake answered with this status.
    CloudChanged(u16),
    /// One page of repos for the query and page asked for.
    CloudRepos {
        q: String,
        page: u32,
        status: u16,
        body: String,
    },
    /// The vault's key list (the load-time probe, and each refresh).
    Vault(u16, String),
    /// A key was saved or removed: its status.
    VaultChanged(u16),
}

/// The two feeds the page's timer advances, and the portal's states.
enum Feed {
    Live {
        live: Live,
        driver: Option<Driver>,
    },
    Demo {
        player: Player,
        started: f64,
    },
    /// No `url`: waiting for `/api/me` to choose the mode.
    Probing,
    /// The portal (the sign-in or the list, by the host's root). `refreshed`
    /// is when the list last loaded, in milliseconds.
    Portal {
        refreshed: f64,
    },
}

/// Everything the timer owns. Single-threaded: no locking is needed.
struct Web {
    host: Host,
    feed: Feed,
    replies: UnboundedReceiver<Reply>,
    reply_tx: UnboundedSender<Reply>,
    push: PushState,
    cloud: CloudState,
}

fn now() -> f64 {
    js_sys::Date::now()
}

impl Web {
    fn tick(&mut self) {
        self.host.tick_now();
        if let Some(h) =
            web_sys::window().and_then(|w| w.inner_height().ok()).and_then(|v| v.as_f64())
        {
            crate::chat::set_window_height(h as f32);
        }
        while let Ok(reply) = self.replies.try_recv() {
            self.apply_reply(reply);
        }
        match &mut self.feed {
            Feed::Live { live, driver } => {
                live.pump(&mut self.host);
                if let Some(active) = driver {
                    if !active.step() {
                        *driver = None;
                    }
                }
            }
            Feed::Demo { player, started } => {
                while self.host.try_action().is_some() {}
                let elapsed = (now() - *started).max(0.0) as u64;
                player.pump(&mut self.host, elapsed);
            }
            Feed::Probing | Feed::Portal { .. } => while self.host.try_action().is_some() {},
        }
        while let Some(action) = self.host.take_portal() {
            self.portal_action(action);
        }
        self.tick_cloud();
        self.refresh_if_due();
    }

    /// Applies one answer to the host's state.
    fn apply_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Me(status) => {
                if matches!(self.feed, Feed::Probing) {
                    match portal::mode_without_url(Some(status)) {
                        Mode::Live => {}
                        Mode::Demo => {
                            self.feed = Feed::Demo { player: Player::default(), started: now() }
                        }
                        Mode::SignIn => self.show_sign_in(),
                        Mode::Portal => self.show_list(),
                    }
                }
            }
            Reply::Owner(status) => self.host.set_share_enabled(status == 200),
            Reply::Sessions(status, body) => match status {
                200 => {
                    if let Some(rows) = portal::parse_sessions(&body) {
                        self.host.set_portal(&rows);
                    }
                }
                401 => self.show_sign_in(),
                _ => {}
            },
            Reply::SignedIn(status) => match portal::sign_in_error(status) {
                None => self.show_list(),
                Some(message) => self.host.set_signin_error(Some(message.to_string())),
            },
            Reply::SignedOut => {}
            Reply::Invites(status, body) => match status {
                200 => {
                    if let Some(rows) = portal::parse_invites(&body) {
                        let lines = rows
                            .iter()
                            .map(|r| InviteLine {
                                id: r.id.clone(),
                                text: invite_row(&r.role, &r.expires_at),
                            })
                            .collect();
                        self.host.set_invites(lines);
                    }
                }
                401 => self.host.set_share_enabled(false),
                _ => {}
            },
            Reply::Created(status, body) => {
                if status == 201 || status == 200 {
                    let url = serde_json::from_str::<serde_json::Value>(&body)
                        .ok()
                        .and_then(|v| v["url"].as_str().map(str::to_string));
                    self.host.set_created(url);
                    self.list_invites();
                }
            }
            Reply::Changed => self.list_invites(),
            Reply::Copied => self.host.set_share_note(Some(COPIED_LABEL.to_string())),
            Reply::PushProbe(probe) => {
                self.push.key_status = probe.key_status;
                self.push.key = probe.key;
                self.push.endpoint = probe.endpoint;
                self.push.browser = Some(probe.browser);
                self.refresh_push();
            }
            Reply::CloudAccounts(status, body) => match status {
                200 => {
                    if let Some(accounts) = cloud::parse_accounts(&body) {
                        self.host.set_cloud_available(true);
                        let rows = cloud::account_rows(&accounts)
                            .into_iter()
                            .map(|(id, login, text)| AccountRow { id, login, text })
                            .collect();
                        self.host.set_cloud_accounts(rows);
                    }
                }
                401 | 403 | 503 => self.host.set_cloud_available(false),
                _ => {}
            },
            Reply::CloudRuns(status, body) => match status {
                200 => {
                    if let Some(runs) = cloud::parse_runs(&body) {
                        self.host.set_cloud_note(None);
                        self.host.set_cloud_runs(cloud::run_rows(&runs));
                    }
                }
                503 => {
                    self.host.set_cloud_runs(Vec::new());
                    self.host.set_cloud_note(Some(CLOUD_NOT_CONFIGURED_LABEL.to_string()));
                }
                401 | 403 => self.host.set_cloud_available(false),
                _ => {}
            },
            Reply::CloudAdded(status, body) => match status {
                200 | 201 => {
                    self.host.set_cloud_note(None);
                    self.list_accounts();
                }
                400 => {
                    let note = token_refused_line(cloud::refused_status(&body));
                    self.host.set_cloud_note(Some(note));
                }
                401 | 403 => self.host.set_cloud_available(false),
                503 => self.host.set_cloud_note(Some(CLOUD_NOT_CONFIGURED_LABEL.to_string())),
                _ => self.host.set_cloud_note(Some(CLOUD_FAILED_LABEL.to_string())),
            },
            Reply::CloudChanged(status) => match status {
                200 | 201 | 204 => {
                    self.host.set_cloud_note(None);
                    self.list_accounts();
                    self.list_runs();
                }
                401 | 403 => self.host.set_cloud_available(false),
                503 => self.host.set_cloud_note(Some(CLOUD_NOT_CONFIGURED_LABEL.to_string())),
                _ => self.host.set_cloud_note(Some(CLOUD_FAILED_LABEL.to_string())),
            },
            Reply::CloudRepos { q, page, status, body } => {
                // Only the page the feed still waits for counts; a page of an
                // older query is dropped.
                let current = match self.cloud.feed.as_mut() {
                    Some(feed) if feed.q == q && feed.page == page => {
                        feed.loading = false;
                        true
                    }
                    _ => false,
                };
                if current {
                    match status {
                        200 => {
                            if let Some(parsed) = cloud::parse_repos(&body) {
                                let rows = cloud::repo_rows(&parsed.repos)
                                    .into_iter()
                                    .map(|(full_name, default_branch, text)| RepoRow {
                                        full_name,
                                        default_branch,
                                        text,
                                    })
                                    .collect();
                                self.host.set_cloud_repos(rows, parsed.next_page, page > 1);
                            }
                        }
                        401 | 403 => self.host.set_cloud_available(false),
                        _ => self.host.set_cloud_note(Some(CLOUD_FAILED_LABEL.to_string())),
                    }
                }
            }
            Reply::Vault(status, body) => {
                let rows = (status == 200).then(|| cloud::credential_rows(&body)).flatten();
                self.host.set_vault(rows);
            }
            Reply::VaultChanged(status) => {
                let saved = matches!(status, 200 | 201 | 204);
                self.host.set_vault_note((!saved).then(|| VAULT_SAVE_FAILED_LABEL.to_string()));
                self.list_vault();
            }
            Reply::PushDone(result) => {
                self.push.busy = false;
                match result {
                    Ok(endpoint) => {
                        self.push.endpoint = endpoint;
                        self.push.error = None;
                    }
                    Err(reason) => self.push.error = Some(reason.to_string()),
                }
                self.refresh_push();
            }
        }
    }

    /// The portal's sign-in screen (root), with no error.
    fn show_sign_in(&mut self) {
        self.host.set_signin_error(None);
        self.host.set_root(Screen::SignIn);
        self.feed = Feed::Portal { refreshed: 0.0 };
    }

    /// The portal's session list (root), loaded now.
    fn show_list(&mut self) {
        self.host.set_signin_error(None);
        self.host.set_root(Screen::Portal);
        self.feed = Feed::Portal { refreshed: now() };
        self.list_sessions();
    }

    fn list_sessions(&self) {
        request("GET", "/api/sessions".into(), None, self.reply_tx.clone(), |s, b| {
            Reply::Sessions(s, b)
        });
    }

    fn list_invites(&self) {
        if let Some(session) = self.host.session() {
            let url = format!("/api/invites?session_id={session}");
            request("GET", url, None, self.reply_tx.clone(), |s, b| Reply::Invites(s, b));
        }
    }

    /// The list refreshes every [`PORTAL_REFRESH_MS`] while it shows.
    fn refresh_if_due(&mut self) {
        let shows = self.host.screen() == Screen::Portal;
        if let Feed::Portal { refreshed } = &mut self.feed {
            if shows && now() - *refreshed >= PORTAL_REFRESH_MS {
                *refreshed = now();
                self.list_sessions();
            }
        }
    }

    /// The owner portal's actions (from the views taps and keys).
    fn portal_action(&mut self, action: UiAction) {
        let tx = self.reply_tx.clone();
        match action {
            UiAction::SignIn(token) => {
                let body = serde_json::json!({ "token": token }).to_string();
                request("POST", "/auth".into(), Some(body), tx, |s, _| Reply::SignedIn(s));
            }
            UiAction::SignOut => {
                request("POST", "/auth/logout".into(), None, tx, |_, _| Reply::SignedOut);
                self.show_sign_in();
            }
            UiAction::RefreshPortal => self.list_sessions(),
            UiAction::OpenUrl(url) => {
                if let Some(window) = web_sys::window() {
                    let _ = window.location().set_href(&url);
                }
            }
            UiAction::ListInvites => self.list_invites(),
            UiAction::CreateInvite(role) => {
                if let Some(session) = self.host.session() {
                    let body = serde_json::json!({
                        "session_id": session.to_string(),
                        "role": role,
                        "ttl_hours": INVITE_TTL_HOURS,
                    })
                    .to_string();
                    request("POST", "/api/invites".into(), Some(body), tx, |s, b| {
                        Reply::Created(s, b)
                    });
                }
            }
            UiAction::RevokeInvite(id) => {
                let url = format!("/api/invites/{id}");
                request("DELETE", url, None, tx, |_, _| Reply::Changed);
            }
            UiAction::CopyInvite(url) => copy_text(url, tx),
            UiAction::TogglePush => self.toggle_push(),
            UiAction::CloudRefresh => {
                self.cloud.runs_at = now();
                self.list_accounts();
                self.list_runs();
            }
            UiAction::AddAccount(secret) => {
                let body = cloud::add_account_body(&secret.key);
                request("POST", cloud::accounts_url(), Some(body), tx, |s, b| {
                    Reply::CloudAdded(s, b)
                });
            }
            UiAction::RemoveAccount(id) => {
                request("DELETE", cloud::account_url(&id), None, tx, |s, _| Reply::CloudChanged(s));
            }
            UiAction::StartRun { account, repo, branch } => {
                let body = cloud::start_run_body(&account, &repo, &branch);
                request("POST", cloud::runs_url(), Some(body), tx, |s, _| Reply::CloudChanged(s));
            }
            UiAction::StopRun(id) => {
                request("POST", cloud::run_action_url(&id, "stop"), None, tx, |s, _| {
                    Reply::CloudChanged(s)
                });
            }
            UiAction::WakeRun(id) => {
                request("POST", cloud::run_action_url(&id, "wake"), None, tx, |s, _| {
                    Reply::CloudChanged(s)
                });
            }
            // The vault (PLAN 10.9 item 3): while the host has one, a key
            // entry or removal goes over HTTPS to the bridge, not the session.
            UiAction::SetKey(secret) => {
                let body = cloud::credential_body(&secret.key);
                let url = cloud::credential_url(&secret.provider);
                request("PUT", url, Some(body), tx, |s, _| Reply::VaultChanged(s));
            }
            UiAction::AuthRemove(provider) => {
                let url = cloud::credential_url(&provider);
                request("DELETE", url, None, tx, |s, _| Reply::VaultChanged(s));
            }
            _ => {}
        }
    }
}

impl Web {
    /// The Settings push row's facts, once the probe has answered.
    fn push_facts(&self) -> Option<push::Facts> {
        let browser = self.push.browser?;
        Some(push::Facts {
            key_status: self.push.key_status,
            role: push::role_of(self.push.key_status, self.push.invite.is_some()),
            supported: browser.supported,
            ios_tab: browser.ios_tab,
            permission: browser.permission,
            subscribed: self.push.endpoint.is_some(),
        })
    }

    /// Sends the row to the host (`None` hides it).
    fn refresh_push(&mut self) {
        let row =
            self.push_facts().and_then(|facts| push::view(&facts, self.push.error.as_deref()));
        self.host.set_push(row);
    }

    /// A tap on the row: subscribe when it is off, unsubscribe when it is on.
    /// One at a time; any other tap (hidden, unavailable, busy) does nothing.
    fn toggle_push(&mut self) {
        if self.push.busy {
            return;
        }
        let Some(facts) = self.push_facts() else { return };
        let tx = self.reply_tx.clone();
        let invite = self.push.invite.clone();
        match (push::row_state(&facts), self.push.endpoint.clone(), self.push.key.clone()) {
            (push::RowState::On, Some(endpoint), _) => spawn_local(async move {
                let _ = tx.send(Reply::PushDone(push_off(endpoint, invite).await));
            }),
            (push::RowState::Off, _, Some(key)) => spawn_local(async move {
                let _ = tx.send(Reply::PushDone(push_on(key, invite).await));
            }),
            _ => return,
        }
        self.push.busy = true;
        self.push.error = None;
        self.refresh_push();
    }
}

/// The Cloud screen's and the vault's page state (web host).
#[derive(Default)]
struct CloudState {
    /// When the runs list last refreshed (ms), for the screen's timer.
    runs_at: f64,
    /// The repo picker's paging, while the picker is on top.
    feed: Option<RepoFeed>,
}

/// The repo picker's paging: the query and page asked for, and whether a
/// page is in flight. The text the picker shows, and when it last changed,
/// is the pause before a new query goes out.
struct RepoFeed {
    q: String,
    page: u32,
    loading: bool,
    seen_q: String,
    seen_at: f64,
}

impl Web {
    fn list_accounts(&self) {
        request("GET", cloud::accounts_url(), None, self.reply_tx.clone(), |s, b| {
            Reply::CloudAccounts(s, b)
        });
    }

    fn list_runs(&self) {
        request("GET", cloud::runs_url(), None, self.reply_tx.clone(), |s, b| {
            Reply::CloudRuns(s, b)
        });
    }

    fn list_vault(&self) {
        request("GET", cloud::credentials_url(), None, self.reply_tx.clone(), |s, b| {
            Reply::Vault(s, b)
        });
    }

    /// The Cloud screen's timers: the runs list refreshes every
    /// [`RUNS_REFRESH_MS`] while it shows, and the repo picker runs its feed.
    fn tick_cloud(&mut self) {
        let at = now();
        match self.host.screen() {
            Screen::Cloud => {
                if at - self.cloud.runs_at >= RUNS_REFRESH_MS {
                    self.cloud.runs_at = at;
                    self.list_runs();
                }
            }
            Screen::CloudRepos => self.tick_repo_picker(at),
            _ => self.cloud.feed = None,
        }
    }

    /// The picker's feed: the first page at once; a new query once the text
    /// has not changed for [`QUERY_PAUSE_MS`] (the list is then replaced);
    /// the next page when the last row is in view.
    fn tick_repo_picker(&mut self, at: f64) {
        let (Some(q), Some(account)) = (self.host.cloud_query(), self.host.cloud_account_id())
        else {
            return;
        };
        let next = {
            let feed = self.cloud.feed.get_or_insert_with(|| RepoFeed {
                q: String::new(),
                page: 0,
                loading: false,
                seen_q: q.clone(),
                seen_at: at,
            });
            if feed.seen_q != q {
                feed.seen_q = q.clone();
                feed.seen_at = at;
            }
            let settled = at - feed.seen_at >= QUERY_PAUSE_MS;
            if feed.page == 0 || (feed.q != q && settled) {
                let replace = feed.page != 0;
                feed.q = q.clone();
                feed.page = 1;
                feed.loading = true;
                Some((q, 1, replace))
            } else if !feed.loading {
                self.host.cloud_next_page_due().map(|page| {
                    feed.page = page;
                    feed.loading = true;
                    (feed.q.clone(), page, false)
                })
            } else {
                None
            }
        };
        if let Some((q, page, replace)) = next {
            if replace {
                self.host.set_cloud_repos(Vec::new(), None, false);
            }
            let url = cloud::repos_url(&account, &q, page);
            request_with("GET", url, None, self.reply_tx.clone(), move |status, body| {
                Reply::CloudRepos { q, page, status, body }
            });
        }
    }
}

/// The page's push state (the Settings row). The browser's own subscription
/// is the source of truth for `endpoint`; this copy is what the row shows.
#[derive(Default)]
struct PushState {
    /// `GET /api/push/key`: 200 when the page may subscribe, else the status.
    key_status: Option<u16>,
    /// The VAPID public key (base64url) from that answer.
    key: Option<String>,
    browser: Option<Browser>,
    /// The endpoint of this device's subscription, when there is one.
    endpoint: Option<String>,
    /// The last tap's shared reason, shown until the next tap.
    error: Option<String>,
    /// A tap is in flight; further taps wait for its answer.
    busy: bool,
    /// The invite the page was opened with (`?invite=`), if any.
    invite: Option<String>,
}

/// What the browser offers for push, read at load.
#[derive(Clone, Copy)]
struct Browser {
    /// `navigator.serviceWorker` and `PushManager` both exist.
    supported: bool,
    /// iOS outside a Home Screen web app.
    ios_tab: bool,
    permission: Permission,
}

/// The load-time answers: the key route's status, the key, the browser, and
/// this device's existing subscription endpoint.
struct Probe {
    key_status: Option<u16>,
    key: Option<String>,
    browser: Browser,
    endpoint: Option<String>,
}

/// The browser's push support, permission and iOS tab state (no requests).
fn browser_push() -> Browser {
    let navigator = navigator_value().unwrap_or(JsValue::UNDEFINED);
    let window = window_value().unwrap_or(JsValue::UNDEFINED);
    let present = |target: &JsValue, name: &str| {
        field(target, name).map(|v| !v.is_undefined() && !v.is_null()).unwrap_or(false)
    };
    let supported = present(&navigator, "serviceWorker") && present(&window, "PushManager");
    let user_agent =
        field(&navigator, "userAgent").ok().and_then(|v| v.as_string()).unwrap_or_default();
    let standalone = field(&navigator, "standalone").ok().and_then(|v| v.as_bool()) == Some(true);
    let permission = field(&window, "Notification")
        .and_then(|notification| field(&notification, "permission"))
        .ok()
        .and_then(|v| v.as_string())
        .map(|text| Permission::parse(&text))
        .unwrap_or(Permission::Default);
    Browser { supported, ios_tab: push::is_ios(&user_agent) && !standalone, permission }
}

/// The load-time probe: `GET /api/push/key` (with the page's invite, if any),
/// then, when the key is there, this device's existing subscription. Sends
/// nothing and changes nothing.
async fn probe_push(browser: Browser, invite: Option<String>) -> Probe {
    let query = invite.map(|i| format!("?invite={i}")).unwrap_or_default();
    let (status, body) =
        fetch("GET", &format!("/api/push/key{query}"), None).await.unwrap_or((0, String::new()));
    let key = (status == 200)
        .then(|| serde_json::from_str::<serde_json::Value>(&body).ok())
        .flatten()
        .and_then(|v| v["key"].as_str().map(str::to_string));
    let key_status = (status != 0 && (status != 200 || key.is_some())).then_some(status);
    let endpoint = if key_status == Some(200) && browser.supported {
        current_subscription()
            .await
            .ok()
            .flatten()
            .and_then(|sub| field(&sub, "endpoint").ok())
            .and_then(|v| v.as_string())
    } else {
        None
    };
    Probe { key_status, key, browser, endpoint }
}

/// Subscribes this device: register `/app/sw.js` (scope `/app/`), ask for
/// permission, subscribe with the bridge's key, then POST the subscription.
/// A refused POST unsubscribes the browser again, so the two stay in step.
async fn push_on(key: String, invite: Option<String>) -> Result<Option<String>, &'static str> {
    let failed = |_: JsValue| PUSH_FAILED_LABEL;
    let registration = register_worker().await.map_err(failed)?;
    match ask_permission().await.map_err(failed)? {
        Permission::Granted => {}
        Permission::Denied => return Err(PUSH_BLOCKED_LABEL),
        Permission::Default => return Err(PUSH_NOT_ALLOWED_LABEL),
    }
    let manager = field(&registration, "pushManager").map_err(failed)?;
    let options = js_sys::Object::new();
    set(&options, "userVisibleOnly", &JsValue::TRUE).map_err(failed)?;
    let key_bytes = push::b64url_decode(&key).ok_or(PUSH_FAILED_LABEL)?;
    set(&options, "applicationServerKey", &js_sys::Uint8Array::from(key_bytes.as_slice()).into())
        .map_err(failed)?;
    let sub = settle(call(&manager, "subscribe", &[options.into()]).map_err(failed)?)
        .await
        .map_err(failed)?;
    let endpoint = field(&sub, "endpoint").map_err(failed)?.as_string().ok_or(PUSH_FAILED_LABEL)?;
    let p256dh = key_text(&sub, "p256dh").map_err(failed)?;
    let auth = key_text(&sub, "auth").map_err(failed)?;
    let body = push::subscribe_body(&endpoint, &p256dh, &auth, invite.as_deref());
    let (status, _) = fetch("POST", "/api/push/subscribe", Some(body)).await.map_err(failed)?;
    if status != 204 {
        if let Ok(done) = call(&sub, "unsubscribe", &[]) {
            let _ = settle(done).await;
        }
        return Err(PUSH_FAILED_LABEL);
    }
    Ok(Some(endpoint))
}

/// Unsubscribes this device (`subscription.unsubscribe()`), then DELETEs the
/// endpoint from the bridge. A failed DELETE leaves the row on, so a second
/// tap retries the DELETE.
async fn push_off(
    endpoint: String,
    invite: Option<String>,
) -> Result<Option<String>, &'static str> {
    let failed = |_: JsValue| PUSH_FAILED_LABEL;
    if let Some(sub) = current_subscription().await.map_err(failed)? {
        settle(call(&sub, "unsubscribe", &[]).map_err(failed)?).await.map_err(failed)?;
    }
    let body = push::unsubscribe_body(&endpoint, invite.as_deref());
    let (status, _) = fetch("DELETE", "/api/push/subscribe", Some(body)).await.map_err(failed)?;
    if status == 204 {
        Ok(None)
    } else {
        Err(PUSH_FAILED_LABEL)
    }
}

/// The page's registration of `/app/sw.js` at scope `/app/`.
async fn register_worker() -> Result<JsValue, JsValue> {
    let options = js_sys::Object::new();
    set(&options, "scope", &JsValue::from_str("/app/"))?;
    settle(call(
        &service_worker()?,
        "register",
        &[JsValue::from_str("/app/sw.js"), options.into()],
    )?)
    .await
}

/// This device's subscription for the `/app/` scope, if there is one.
async fn current_subscription() -> Result<Option<JsValue>, JsValue> {
    let registration =
        settle(call(&service_worker()?, "getRegistration", &[JsValue::from_str("/app/")])?).await?;
    if registration.is_null() || registration.is_undefined() {
        return Ok(None);
    }
    let manager = field(&registration, "pushManager")?;
    let sub = settle(call(&manager, "getSubscription", &[])?).await?;
    Ok((!sub.is_null() && !sub.is_undefined()).then_some(sub))
}

/// `Notification.requestPermission()`: the browser's answer.
async fn ask_permission() -> Result<Permission, JsValue> {
    let notification = field(&window_value()?, "Notification")?;
    let answer = settle(call(&notification, "requestPermission", &[])?).await?;
    Ok(Permission::parse(&answer.as_string().unwrap_or_default()))
}

/// One subscription key (`p256dh` or `auth`) as unpadded base64url.
fn key_text(sub: &JsValue, name: &str) -> Result<String, JsValue> {
    let buffer = call(sub, "getKey", &[JsValue::from_str(name)])?;
    if buffer.is_null() || buffer.is_undefined() {
        return Err(JsValue::NULL);
    }
    Ok(push::b64url_encode(&js_sys::Uint8Array::new(&buffer).to_vec()))
}

fn window_value() -> Result<JsValue, JsValue> {
    Ok(web_sys::window().ok_or(JsValue::NULL)?.into())
}

fn navigator_value() -> Result<JsValue, JsValue> {
    Ok(web_sys::window().ok_or(JsValue::NULL)?.navigator().into())
}

fn service_worker() -> Result<JsValue, JsValue> {
    field(&navigator_value()?, "serviceWorker")
}

/// A property of a JS value (`Reflect.get`), by name.
fn field(target: &JsValue, name: &str) -> Result<JsValue, JsValue> {
    js_sys::Reflect::get(target, &JsValue::from_str(name))
}

fn set(target: &JsValue, name: &str, value: &JsValue) -> Result<bool, JsValue> {
    js_sys::Reflect::set(target, &JsValue::from_str(name), value)
}

/// A method call on a JS value, with `target` as `this`.
fn call(target: &JsValue, name: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
    let function: js_sys::Function = field(target, name)?.dyn_into()?;
    let list = js_sys::Array::new();
    for arg in args {
        list.push(arg);
    }
    function.apply(target, &list)
}

/// Waits for a promise (or passes a plain value through as a resolved one).
async fn settle(value: JsValue) -> Result<JsValue, JsValue> {
    JsFuture::from(js_sys::Promise::resolve(&value)).await
}

/// One same-origin request with the page's cookies. The answer (or a
/// status of 0 when it failed) goes to `tx` as `answer(status, body)`.
fn request(
    method: &str,
    url: String,
    body: Option<String>,
    tx: UnboundedSender<Reply>,
    answer: fn(u16, String) -> Reply,
) {
    let method = method.to_string();
    spawn_local(async move {
        let (status, text) = fetch(&method, &url, body).await.unwrap_or((0, String::new()));
        let _ = tx.send(answer(status, text));
    });
}

/// [`request`] with an answer that carries the call's own values (a page of
/// repos answers with its query and page). The closure lives in the request's
/// future only.
fn request_with<F>(
    method: &str,
    url: String,
    body: Option<String>,
    tx: UnboundedSender<Reply>,
    answer: F,
) where
    F: FnOnce(u16, String) -> Reply + 'static,
{
    let method = method.to_string();
    spawn_local(async move {
        let (status, text) = fetch(&method, &url, body).await.unwrap_or((0, String::new()));
        let _ = tx.send(answer(status, text));
    });
}

/// `fetch` with `credentials: include`. A relative `url` is same-origin.
async fn fetch(method: &str, url: &str, body: Option<String>) -> Result<(u16, String), JsValue> {
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    init.set_credentials(web_sys::RequestCredentials::Include);
    if let Some(body) = body {
        let headers = web_sys::Headers::new()?;
        headers.set("Content-Type", "application/json")?;
        init.set_headers(headers.as_ref());
        init.set_body(&JsValue::from_str(&body));
    }
    let window = web_sys::window().ok_or(JsValue::NULL)?;
    let response: web_sys::Response =
        JsFuture::from(window.fetch_with_str_and_init(url, &init)).await?.dyn_into()?;
    let status = response.status();
    let text = JsFuture::from(response.text()?).await?.as_string().unwrap_or_default();
    Ok((status, text))
}

/// Copies `url` with `navigator.clipboard`; without it, or when the browser
/// refuses, the URL's text is selected so the owner can copy it by hand.
fn copy_text(url: String, tx: UnboundedSender<Reply>) {
    let Some(window) = web_sys::window() else { return };
    let navigator: JsValue = window.navigator().into();
    let clipboard = js_sys::Reflect::get(&navigator, &JsValue::from_str("clipboard"))
        .unwrap_or(JsValue::UNDEFINED);
    if clipboard.is_undefined() || clipboard.is_null() {
        select_text(&url);
        return;
    }
    let clipboard: web_sys::Clipboard = clipboard.unchecked_into();
    let promise = clipboard.write_text(&url);
    spawn_local(async move {
        match JsFuture::from(promise).await {
            Ok(_) => {
                let _ = tx.send(Reply::Copied);
            }
            Err(_) => select_text(&url),
        }
    });
}

/// Selects the text of the one leaf element whose text is `text`.
fn select_text(text: &str) {
    let Some(window) = web_sys::window() else { return };
    let Some(document) = window.document() else { return };
    let all = document.get_elements_by_tag_name("*");
    for i in 0..all.length() {
        let Some(element) = all.item(i) else { continue };
        if element.child_element_count() == 0 && element.text_content().as_deref() == Some(text) {
            if let (Ok(range), Ok(Some(selection))) =
                (document.create_range(), window.get_selection())
            {
                let _ = range.select_node_contents(&element);
                let _ = selection.remove_all_ranges();
                let _ = selection.add_range(&range);
            }
            return;
        }
    }
}

/// Starts the page's feed and returns the view's end of the link.
pub fn start() -> Client {
    let (mut host, client) = link();
    // The saved theme (localStorage) shows at startup; it is not saved again.
    crate::theme::install_store(std::sync::Arc::new(crate::theme::LocalStore));
    host.restore_theme(crate::theme::saved_installed());
    let (reply_tx, replies) = unbounded_channel();
    let query = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    // The Settings push row: probed only where the browser could show it.
    let invite = push::invite_from_query(&query);
    let browser = browser_push();
    if browser.supported || browser.ios_tab {
        let tx = reply_tx.clone();
        let probe_invite = invite.clone();
        spawn_local(async move {
            let _ = tx.send(Reply::PushProbe(probe_push(browser, probe_invite).await));
        });
    }
    // The owner's Cloud screen and vault (PLAN 10.9, 10.10): a 200 here shows
    // them; any other answer hides them.
    request("GET", cloud::accounts_url(), None, reply_tx.clone(), |s, b| {
        Reply::CloudAccounts(s, b)
    });
    request("GET", cloud::credentials_url(), None, reply_tx.clone(), |s, b| Reply::Vault(s, b));
    let feed = match LiveOptions::from_query(&query) {
        Ok(Some(options)) => {
            let (live, actions, out) = Live::channels();
            let driver = Driver::start(options, live.status.clone(), actions, out);
            // Owner or not: the share section in Settings only.
            request("GET", "/api/me".into(), None, reply_tx.clone(), |s, _| Reply::Owner(s));
            Feed::Live { live, driver }
        }
        Ok(None) => {
            request("GET", "/api/me".into(), None, reply_tx.clone(), |s, _| Reply::Me(s));
            Feed::Probing
        }
        Err(message) => {
            web_sys::console::error_1(&JsValue::from_str(&format!("modron: {message}")));
            Feed::Demo { player: Player::default(), started: now() }
        }
    };
    let push = PushState { invite, ..PushState::default() };
    let web = Rc::new(RefCell::new(Web {
        host,
        feed,
        replies,
        reply_tx,
        push,
        cloud: CloudState::default(),
    }));
    listen_keys(web.clone());
    listen_scroll(web.clone());
    if let Some(window) = web_sys::window() {
        let web = web.clone();
        let tick = Closure::<dyn FnMut()>::new(move || web.borrow_mut().tick());
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            tick.as_ref().unchecked_ref(),
            TICK.as_millis() as i32,
        );
        // The page keeps the timer for its whole life.
        tick.forget();
    }
    client
}

/// Routes the page's keys the terminal's way, before the composer sees them
/// (capture phase, so the key is decided first). A consumed key is
/// cancelled, so no character lands in the composer.
/// Wheel and touch scrolling of the transcript. The page itself does not
/// scroll (`html, body { overflow: hidden }`), so a wheel turn or a finger
/// drag over the page moves the transcript's offset by rows, as PageUp and
/// PageDown do (see `chat::rows_for_pixels`).
fn listen_scroll(web: Rc<RefCell<Web>>) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let on_wheel = {
        let web = web.clone();
        Closure::<dyn FnMut(web_sys::WheelEvent)>::new(move |event: web_sys::WheelEvent| {
            // Not cancelled: the document listener is passive in Chromium, and the
            // page does not scroll natively (html, body overflow: hidden).
            web.borrow_mut().host.wheel(event.delta_y());
        })
    };
    let _ = document.add_event_listener_with_callback_and_bool(
        "wheel",
        on_wheel.as_ref().unchecked_ref(),
        false,
    );
    on_wheel.forget();
    // Touch: the previous finger y (None between gestures) and each move's
    // delta. A finger moving down (clientY grows) scrolls toward older rows,
    // which is a negative pixel delta for the wheel mapping, so the sign flips.
    let last_y: Rc<RefCell<Option<f64>>> = Rc::new(RefCell::new(None));
    let on_touch_start = {
        let last_y = last_y.clone();
        Closure::<dyn FnMut(web_sys::TouchEvent)>::new(move |event: web_sys::TouchEvent| {
            *last_y.borrow_mut() = touch_y(&event);
        })
    };
    let on_touch_move = {
        let last_y = last_y.clone();
        Closure::<dyn FnMut(web_sys::TouchEvent)>::new(move |event: web_sys::TouchEvent| {
            let Some(now) = touch_y(&event) else { return };
            let previous = last_y.borrow_mut().replace(now);
            if let Some(previous) = previous {
                web.borrow_mut().host.wheel(previous - now);
            }
        })
    };
    let on_touch_end = {
        let last_y = last_y.clone();
        Closure::<dyn FnMut(web_sys::TouchEvent)>::new(move |_: web_sys::TouchEvent| {
            *last_y.borrow_mut() = None;
        })
    };
    for (name, closure) in [
        ("touchstart", on_touch_start.as_ref().unchecked_ref()),
        ("touchmove", on_touch_move.as_ref().unchecked_ref()),
        ("touchend", on_touch_end.as_ref().unchecked_ref()),
    ] {
        let _ = document.add_event_listener_with_callback_and_bool(name, closure, false);
    }
    on_touch_start.forget();
    on_touch_move.forget();
    on_touch_end.forget();
}

/// The first touch point's vertical position, if any.
fn touch_y(event: &web_sys::TouchEvent) -> Option<f64> {
    event.touches().get(0).map(|touch| f64::from(touch.client_y()))
}

fn listen_keys(web: Rc<RefCell<Web>>) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let on_key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |event: web_sys::KeyboardEvent| {
            // Shift+Tab reaches the router as BackTab (the terminal host does the same).
            let name = if event.key() == "Tab" && event.shift_key() {
                "BackTab".to_string()
            } else {
                event.key()
            };
            let key = host_key(&name);
            let chord = event.ctrl_key() || event.meta_key() || event.alt_key();
            if web.borrow_mut().host.route(key, event.alt_key(), chord) {
                event.prevent_default();
            }
        });
    let _ = document.add_event_listener_with_callback_and_bool(
        "keydown",
        on_key.as_ref().unchecked_ref(),
        true,
    );
    // The page keeps the listener for its whole life.
    on_key.forget();
}

// ---- Attach control (APP-4 item 3) ----

thread_local! {
    /// The hidden file input (made on first use) and its change handler. The
    /// handler is kept here so the browser never holds a dropped closure.
    static PICKER: RefCell<Option<(web_sys::HtmlInputElement, Closure<dyn FnMut(web_sys::Event)>)>> =
        RefCell::new(None);
    /// The action sender the picked file goes to (the driver's channel).
    static PICK_TO: RefCell<Option<UnboundedSender<crate::chat::UiAction>>> = RefCell::new(None);
}

/// Opens the file chooser for an image (the attach control). The input is
/// hidden and made once; its change event reads the file and sends
/// `AttachFile`, which the driver stages as a chip, as Ctrl+V does.
pub fn open_file_picker(actions: UnboundedSender<crate::chat::UiAction>) {
    PICK_TO.with(|slot| *slot.borrow_mut() = Some(actions));
    let input = PICKER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = make_picker();
        }
        slot.as_ref().map(|(input, _)| input.clone())
    });
    if let Some(input) = input {
        input.set_value("");
        input.click();
    }
}

fn make_picker() -> Option<(web_sys::HtmlInputElement, Closure<dyn FnMut(web_sys::Event)>)> {
    let document = web_sys::window()?.document()?;
    let input =
        document.create_element("input").ok()?.dyn_into::<web_sys::HtmlInputElement>().ok()?;
    input.set_type("file");
    input.set_accept("image/*");
    input.set_hidden(true);
    document.body()?.append_child(&input).ok()?;
    let on_change = Closure::<dyn FnMut(web_sys::Event)>::new(|event: web_sys::Event| {
        let file = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok())
            .and_then(|input| input.files())
            .and_then(|files| files.get(0));
        let Some(file) = file else {
            return;
        };
        let Some(to) = PICK_TO.with(|slot| slot.borrow().clone()) else {
            return;
        };
        spawn_local(async move {
            let name = file.name();
            let mime = file.type_();
            let action = match read_file(&file).await {
                Ok(bytes) => crate::chat::UiAction::AttachFile { name, mime, bytes },
                Err(reason) => crate::chat::UiAction::Notice(reason),
            };
            let _ = to.send(action);
        });
    });
    input.add_event_listener_with_callback("change", on_change.as_ref().unchecked_ref()).ok()?;
    Some((input, on_change))
}

/// The file's bytes through `Blob.arrayBuffer()`, called by name so no
/// extra web-sys feature is needed.
async fn read_file(file: &web_sys::File) -> Result<Vec<u8>, String> {
    const REFUSED: &str = "the browser would not read that file";
    let refused = || REFUSED.to_string();
    let method =
        js_sys::Reflect::get(file, &JsValue::from_str("arrayBuffer")).map_err(|_| refused())?;
    let method: js_sys::Function = method.dyn_into().map_err(|_| refused())?;
    let promise: js_sys::Promise =
        method.call0(file).map_err(|_| refused())?.dyn_into().map_err(|_| refused())?;
    let buffer = JsFuture::from(promise).await.map_err(|_| refused())?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}
