//! modron-app: the unified client app. NSH renders the chat screen over the
//! shared client core (`modron-mobile-core`); the host feeds snapshots in and
//! takes actions out through [`chat::link`]. Two hosts drive the screen: the
//! terminal host (`session`, `demo`, the `modron-app-tui` binary) and the web
//! host ([`web`], wasm32 only).

pub mod chat;
pub mod demo;
pub mod session;
#[cfg(target_arch = "wasm32")]
pub mod web;

use std::sync::Mutex;

use chat::{Client, Model};
use nsh_core::prelude::*;
use tokio::sync::mpsc::unbounded_channel;

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
    let model = signal(Model::default());
    let draft = signal(String::new());
    let client = CLIENT.lock().ok().and_then(|mut slot| slot.take()).or_else(default_client);
    let (actions, snapshots) = match client {
        Some(client) => (client.actions, Some(client.snapshots)),
        None => (unbounded_channel().0, None),
    };
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
    chat::screen(model, draft, actions)
}
