//! The web host: the same link, driver and demo as the terminal host, clocked
//! by the page's own timer instead of a thread. The client core connects on
//! the browser event loop (`spawn_local`), so nothing here blocks.
//!
//! The page address selects the mode: `?url=<ws url>[&token=<t>][&session=<uuid>]`
//! attaches to a live server; with no `url` the built-in demo plays.

use std::{cell::RefCell, rc::Rc};

use wasm_bindgen::{prelude::*, JsCast};

use crate::{
    chat::{host_key, link, Client, Host},
    demo::Player,
    session::{Driver, Live, LiveOptions, TICK},
};

/// The two feeds the page's timer advances.
enum Feed {
    Live { live: Live, driver: Option<Driver> },
    Demo { player: Player, started: f64 },
}

/// Everything the timer owns. Single-threaded: no locking is needed.
struct Web {
    host: Host,
    feed: Feed,
}

impl Web {
    fn tick(&mut self) {
        if let Some(h) =
            web_sys::window().and_then(|w| w.inner_height().ok()).and_then(|v| v.as_f64())
        {
            crate::chat::set_window_height(h as f32);
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
                let elapsed = (js_sys::Date::now() - *started).max(0.0) as u64;
                player.pump(&mut self.host, elapsed);
            }
        }
    }
}

/// Starts the page's feed and returns the view's end of the link.
pub fn start() -> Client {
    let (host, client) = link();
    let query = web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default();
    let feed = match LiveOptions::from_query(&query) {
        Ok(Some(options)) => {
            let (live, actions, out) = Live::channels();
            let driver = Driver::start(options, live.status.clone(), actions, out);
            Feed::Live { live, driver }
        }
        Ok(None) => Feed::Demo { player: Player::default(), started: js_sys::Date::now() },
        Err(message) => {
            web_sys::console::error_1(&JsValue::from_str(&format!("modron: {message}")));
            Feed::Demo { player: Player::default(), started: js_sys::Date::now() }
        }
    };
    let web = Rc::new(RefCell::new(Web { host, feed }));
    listen_keys(web.clone());
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
fn listen_keys(web: Rc<RefCell<Web>>) {
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let on_key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |event: web_sys::KeyboardEvent| {
            let key = host_key(&event.key());
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
