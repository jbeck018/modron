//! modron-app: the unified client app. NSH renders the chat screen over the
//! shared client core (`modron-mobile-core`); the host feeds snapshots in and
//! takes actions out through [`chat::link`]. Two hosts drive the screen: the
//! terminal host (`session`, `demo`, the `modron-app-tui` binary) and the web
//! host ([`web`], wasm32 only).

pub mod chat;
pub mod demo;
#[cfg(not(target_arch = "wasm32"))]
pub mod files;
pub mod help;
pub mod nav;
pub mod portal;
pub mod screens;
pub mod session;
pub mod slash;
#[cfg(not(target_arch = "wasm32"))]
pub mod startup;
pub mod suggest;
#[cfg(target_arch = "wasm32")]
pub mod web;

use std::sync::Mutex;

use chat::{Client, Model};
use nav::NavView;
use nsh_core::element::{AdaptiveColor, Tokens};
use nsh_core::prelude::*;
use tokio::sync::mpsc::unbounded_channel;

/// The Oculus theme file (`theme/oculus.json`). The web build reads it from
/// `nsh.json` (`"theme"`) through the packager; the terminal has no packager.
pub const THEME_JSON: &str = include_str!("../theme/oculus.json");

/// Installs [`THEME_JSON`] as the app's design tokens (light and dark
/// values, as the theme file gives them). `App` calls this on the terminal,
/// so the semantic token names resolve there too; the terminal then draws
/// them through its own palette.
pub fn install_theme() {
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(THEME_JSON) else {
        return;
    };
    let (Some(light), Some(dark)) =
        (doc["cssVars"]["light"].as_object(), doc["cssVars"]["dark"].as_object())
    else {
        return;
    };
    let mut tokens = Tokens::default();
    for (name, value) in light {
        let Some(light) = value.as_str().and_then(parse_hex) else {
            continue;
        };
        let dark = dark.get(name).and_then(|v| v.as_str()).and_then(parse_hex).unwrap_or(light);
        tokens.set_color(name, AdaptiveColor { light, dark });
    }
    set_tokens(tokens);
}

fn parse_hex(hex: &str) -> Option<nsh_core::color::Hsla> {
    let rgba = nsh_core::color::Rgba::try_from(hex).ok()?;
    Some(AdaptiveColor::from(rgba).light)
}

/// The view's end of the link. The host sets it before the view loads; `App`
/// takes it on mount. A web page with no host set up starts its own (see
/// [`default_client`]).
pub static CLIENT: Mutex<Option<Client>> = Mutex::new(None);

/// The app's UI set (`nsh.json` `"rust"`): the native module the NSH CLI's
/// describe step requires, with the root component registered.
#[native_module("modron-app/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

/// The link for a view nobody set up: the web host starts its own driver and
/// demo on the page; the terminal host always sets [`CLIENT`] first.
#[cfg(target_arch = "wasm32")]
fn default_client() -> Option<Client> {
    Some(web::start())
}

#[cfg(not(target_arch = "wasm32"))]
fn default_client() -> Option<Client> {
    None
}

#[component]
pub fn App() -> impl IntoElement {
    // The terminal has no packager to read `nsh.json`'s theme: the app
    // installs the Oculus tokens itself, on the engine thread.
    #[cfg(not(target_arch = "wasm32"))]
    install_theme();
    let model = signal(Model::default());
    let window_h = signal(chat::window_height_default());
    // The page's inner height, applied on the engine thread.
    let mut window_rx = chat::window_height_feed();
    spawn(async move {
        while let Some(h) = window_rx.recv().await {
            window_h.set(h);
        }
    });
    let draft = signal(String::new());
    let nav = signal(NavView::default());
    let client = CLIENT.lock().ok().and_then(|mut slot| slot.take()).or_else(default_client);
    let (actions, snapshots, navs) = match client {
        Some(client) => (client.actions, Some(client.snapshots), Some(client.nav)),
        None => (unbounded_channel().0, None, None),
    };
    if let Some(mut rx) = navs {
        // The host's navigation state: the screen, the sessions filter and
        // rows, the settings rows.
        spawn(async move {
            // The composer's completed text (Tab, Enter on a partial command,
            // a tap on a suggestion) is applied once per sequence number.
            let mut applied = 0u64;
            while let Some(view) = rx.recv().await {
                let (seq, text) = view.completion.clone();
                if seq != applied {
                    applied = seq;
                    chat::set_draft(&text);
                    draft.set(text);
                }
                nav.set(view);
            }
        });
    }
    if let Some(mut rx) = snapshots {
        // Runs on the engine thread under this view's owner; the channel's
        // waker wakes the engine when the host pushes a snapshot.
        spawn(async move {
            while let Some(snapshot) = rx.recv().await {
                chat::after_snapshot(&snapshot, draft);
                model.set(Model::from_snapshot(&snapshot));
            }
        });
    }
    screens::frame(model, draft, nav, window_h, actions)
}
