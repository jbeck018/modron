//! Spike: one transcript (2,000 rows), a text input, a status row and a
//! timer-driven stream that appends 2,000 tokens to the last row.
//! Typing "stream" and pressing Enter starts the stream.

use std::time::Duration;

use nsh_base::theme;
use nsh_core::element::{input, set_interval, virtual_list};
use nsh_core::prelude::*;

/// The app's UI set (nsh.json "rust"); required by the CLI describe step.
#[native_module("nsh-web-spike/native")]
fn native(registry: &mut Registry) {
    registry.register::<App>();
}

const ROWS: usize = 2000;
const TOKENS: usize = 2000;

#[component]
pub fn App() -> impl IntoElement {
    let rows = signal((0..ROWS).map(|i| format!("row {i:05} lorem ipsum dolor sit amet")).collect::<Vec<String>>());
    let draft = signal(String::new());
    let tokens = signal(0usize);
    let streaming = signal(false);

    let mut field = input()
        .auto_focus(true)
        .placeholder("Type and press Enter")
        .value(move || draft.get())
        .on_change(move |s: &str| draft.set(s.to_string()))
        .on_submit(move |s: &str| {
            let s = s.to_string();
            draft.set(String::new());
            if s == "stream" && !streaming.get() {
                streaming.set(true);
                rows.update(|r| r.push("assistant: ".to_string()));
                let n = std::cell::Cell::new(0usize);
                let _timer = set_interval(Duration::from_millis(4), move || {
                    if n.get() >= TOKENS {
                        streaming.set(false);
                        return;
                    }
                    n.set(n.get() + 1);
                    tokens.update(|t| *t += 1);
                    rows.update(|r| {
                        if let Some(last) = r.last_mut() {
                            last.push_str("tok ");
                        }
                    });
                });
            } else {
                rows.update(|r| r.push(format!("> {s}")));
            }
        });
    field = field.on_paste(move |p: &PasteEvent| {
        draft.update(|d| d.push_str(&p.text));
    });

    view()
        .flex_1()
        .bg(move || theme().background)
        .text_color(move || theme().foreground)
        .child(
            virtual_list(move || rows.with(|r| r.len()), move |at: Position| {
                text(move || rows.with(|r| r.get(at.index()).cloned().unwrap_or_default()))
            })
            .flex_1()
            .w_full(),
        )
        .child(
            text(move || {
                format!(
                    "status: rows {} | tokens {} | {}",
                    rows.with(|r| r.len()),
                    tokens.get(),
                    if streaming.get() { "streaming" } else { "ready" }
                )
            })
            .p_2(),
        )
        .child(field)
}
