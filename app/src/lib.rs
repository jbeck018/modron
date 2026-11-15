//! modron-app: the unified client app. NSH renders the chat screen over the
//! shared client core (`modron-mobile-core`); the host feeds snapshots in and
//! takes actions out through [`chat::link`].

pub mod chat;
pub mod demo;

use std::sync::Mutex;

use chat::{Client, Model};
use nsh_core::prelude::*;
use tokio::sync::mpsc::unbounded_channel;

/// The view's end of the link. The host sets it before the view loads; `App`
/// takes it on mount.
pub static CLIENT: Mutex<Option<Client>> = Mutex::new(None);

#[component]
pub fn App() -> impl IntoElement {
    let model = signal(Model::default());
    let draft = signal(String::new());
    let (actions, snapshots) = match CLIENT.lock().ok().and_then(|mut slot| slot.take()) {
        Some(client) => (client.actions, Some(client.snapshots)),
        None => (unbounded_channel().0, None),
    };
    if let Some(mut rx) = snapshots {
        // Runs on the engine thread under this view's owner; the channel's
        // waker wakes the engine when the host pushes a snapshot.
        spawn(async move {
            while let Some(snapshot) = rx.recv().await {
                model.set(Model::from_snapshot(&snapshot));
            }
        });
    }
    chat::screen(model, draft, actions)
}
