//! The NSH app: a transcript (virtual list of text rows), a one-line input
//! and a status row. Rows come from NSH_SPIKE_ROWS (initial transcript size).

use nsh_core::element::input;
use nsh_core::prelude::*;

pub fn initial_rows() -> Vec<String> {
    let n: usize = std::env::var("NSH_SPIKE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    (0..n).map(|i| format!("row {i:05} lorem ipsum dolor sit amet")).collect()
}

#[component]
pub fn Chat() -> impl IntoElement {
    let rows = signal(initial_rows());
    let draft = signal(String::new());
    let tokens = signal(0usize);
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
        .child(
            input()
                .auto_focus(true)
                .placeholder("Type and press Enter")
                .value(move || draft.get())
                .on_change(move |s: &str| draft.set(s.to_string()))
                .on_submit(move |s: &str| {
                    rows.update(|r| r.push(format!("> {s}")));
                    draft.set(String::new());
                })
                .on_paste(move |p: &PasteEvent| {
                    tokens.update(|t| *t += 1);
                    rows.update(|r| match r.last_mut() {
                        Some(last) => last.push_str(&p.text),
                        None => r.push(p.text.to_string()),
                    });
                }),
        )
}
