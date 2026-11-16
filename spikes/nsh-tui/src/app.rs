//! The NSH app: a transcript (virtual list of text rows), a one-line input
//! and a status row. Rows come from NSH_SPIKE_ROWS (initial transcript size).
//!
//! S2b additions:
//! - `EXTERNAL`: a tokio channel receiver handed in by the host. The component
//!   takes it on mount and runs a `nsh_core::element::spawn` task that appends
//!   each received token to the last transcript row (no polling, the task waker
//!   from the producer thread wakes the engine).
//! - NSH_SPIKE_FOCUS: `blur` (default, the spike's first behavior), `submit`
//!   (`submit_behavior(SubmitBehavior::Submit)`), or `ref` (`element_ref().focus()`
//!   from `on_submit`).

use std::sync::Mutex;

use nsh_core::element::{element_ref, input, spawn};
use nsh_core::prelude::*;

/// The host's token channel. Taken once, by the first `Chat` that mounts.
pub static EXTERNAL: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>> = Mutex::new(None);

pub fn initial_rows() -> Vec<String> {
    let n: usize = std::env::var("NSH_SPIKE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    (0..n).map(|i| format!("row {i:05} lorem ipsum dolor sit amet")).collect()
}

fn focus_mode() -> String {
    std::env::var("NSH_SPIKE_FOCUS").unwrap_or_else(|_| "blur".to_string())
}

#[component]
pub fn Chat() -> impl IntoElement {
    let rows = signal(initial_rows());
    let draft = signal(String::new());
    let tokens = signal(0usize);
    let field = element_ref();
    let mode = focus_mode();

    let external = EXTERNAL.lock().ok().and_then(|mut slot| slot.take());
    if let Some(mut rx) = external {
        // Runs on the engine thread under this view's owner; ends with the view.
        let _ = spawn(async move {
            while let Some(token) = rx.recv().await {
                tokens.update(|t| *t += 1);
                rows.update(|r| match r.last_mut() {
                    Some(last) => last.push_str(&token),
                    None => r.push(token),
                });
            }
        });
    }

    let mut field_input = input()
        .auto_focus(true)
        .placeholder("Type and press Enter")
        .value(move || draft.get())
        .on_change(move |s: &str| draft.set(s.to_string()))
        .on_submit(move |s: &str| {
            rows.update(|r| r.push(format!("> {s}")));
            draft.set(String::new());
            if mode == "ref" {
                field.focus();
            }
        })
        .on_paste(move |p: &PasteEvent| {
            tokens.update(|t| *t += 1);
            rows.update(|r| match r.last_mut() {
                Some(last) => last.push_str(&p.text),
                None => r.push(p.text.to_string()),
            });
        });
    if focus_mode() == "submit" {
        field_input = field_input.submit_behavior(SubmitBehavior::Submit);
    }
    if focus_mode() == "ref" {
        field_input = field_input.element_ref(&field);
    }

    view()
        .flex_col()
        .size_full()
        .child(
            // Read the position inside the binding: cells are recycled, so a
            // captured index would keep showing the row the cell was built for.
            virtual_list(move || rows.with(|r| r.len()), move |at: Position| {
                text(move || rows.with(|r| r.get(at.index()).cloned().unwrap_or_default()))
            })
            .w_full()
            .h(px(600.0)),
        )
        .child(text(move || {
            format!(
                "status: rows {} | tokens {} | ask",
                rows.with(|r| r.len()),
                tokens.get()
            )
        }))
        .child(field_input)
}
