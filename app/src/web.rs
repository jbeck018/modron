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

use modron_mobile_core::view::{invite_row, COPIED_LABEL};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::{spawn_local, JsFuture};

use crate::{
    chat::{host_key, link, Client, Host, UiAction},
    demo::Player,
    nav::{InviteLine, Screen},
    portal::{self, Mode},
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
            _ => {}
        }
    }
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
    let (host, client) = link();
    let (reply_tx, replies) = unbounded_channel();
    let query = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
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
    let web = Rc::new(RefCell::new(Web { host, feed, replies, reply_tx }));
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
