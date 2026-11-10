//! gen_ui — generative UI: structured widgets the model emits inside a
//! fenced ```modron-ui block, rendered natively by every client.
//!
//! The contract is deliberately boring: JSON in a code fence. No protocol
//! or engine change (the block rides in the assistant text), so the same
//! answer works everywhere — the GUI renders real widgets (table, chart,
//! form), the TUI renders aligned ASCII, mobile renders its own
//! projection. Malformed blocks stay visible as plain text: the model
//! self-corrects on the next turn instead of silently dropping content.
//!
//! The `generative-ui` built-in skill teaches the model the schema.

use serde::{Deserialize, Serialize};

/// The fence tag that marks a generative-UI block: ```modron-ui
pub const FENCE_TAG: &str = "modron-ui";

/// One widget: a form the user fills (GUI submits the values as the next
/// message), a data table, or a chart.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GenUi {
    Form {
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submit_label: Option<String>,
        fields: Vec<GenField>,
    },
    Table {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Chart {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        kind: ChartKind,
        points: Vec<GenPoint>,
    },
}

/// One form field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenField {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub kind: GenFieldKind,
    /// Options for `select` fields.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenFieldKind {
    #[default]
    Text,
    Number,
    Select,
    Check,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChartKind {
    Bar,
    Line,
    Pie,
}

/// One labeled data point.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenPoint {
    pub label: String,
    pub value: f64,
}

/// A piece of an assistant answer: plain text or one parsed widget.
#[derive(Clone, Debug, PartialEq)]
pub enum GenSegment {
    Text(String),
    Ui(GenUi),
}

/// Split an assistant answer into text segments and parsed widgets.
/// Malformed `modron-ui` blocks stay as plain text (the model sees its own
/// output next turn and can fix the JSON).
pub fn split_generative_ui(text: &str) -> Vec<GenSegment> {
    let fence = format!("```{FENCE_TAG}");
    let mut segments = Vec::new();
    let mut rest = text;
    loop {
        let Some(start) = find_line(rest, &fence) else {
            let tail = rest.trim_start_matches('\n');
            if !tail.is_empty() {
                segments.push(GenSegment::Text(tail.to_string()));
            }
            break;
        };
        // Text before the fence (keep verbatim, trimmed of the trailing
        // newline that separated it from the block).
        let before = &rest[..start];
        let before = before.trim_end_matches('\n');
        if !before.is_empty() {
            segments.push(GenSegment::Text(before.to_string()));
        }
        let after_fence = &rest[start + fence.len()..];
        // Skip the newline after the fence line.
        let body_start = after_fence.strip_prefix('\n').unwrap_or(after_fence);
        let Some(close_rel) = find_line(body_start, "```") else {
            // Unterminated block: it is still streaming or malformed -
            // keep the whole tail as text.
            segments.push(GenSegment::Text(format!("{fence}\n{body_start}")));
            break;
        };
        let body = &body_start[..close_rel];
        match serde_json::from_str::<GenUi>(body.trim()) {
            Ok(ui) => segments.push(GenSegment::Ui(ui)),
            Err(_) => segments.push(GenSegment::Text(format!("{fence}\n{}\n```", body.trim()))),
        }
        rest = &body_start[close_rel + 3..];
        let rest_empty = rest.trim().is_empty();
        if rest_empty {
            break;
        }
    }
    segments
}

/// Byte offset of the first line in `text` that equals `needle` (after
/// trimming trailing whitespace on the line).
fn find_line(text: &str, needle: &str) -> Option<usize> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_end() == needle {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

/// ASCII rendering for text surfaces (the TUI): aligned tables, horizontal
/// bars, checkable field lists. One string per display line.
pub fn render_ascii(ui: &GenUi) -> Vec<String> {
    match ui {
        GenUi::Table {
            title,
            columns,
            rows,
        } => {
            let mut out = Vec::new();
            if let Some(title) = title {
                out.push(title.clone());
            }
            let widths: Vec<usize> = columns
                .iter()
                .enumerate()
                .map(|(i, col)| {
                    let widest = rows
                        .iter()
                        .map(|row| row.get(i).map(|c| c.chars().count()).unwrap_or(0))
                        .max()
                        .unwrap_or(0);
                    col.chars().count().max(widest).max(3)
                })
                .collect();
            let header: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, col)| pad(col, widths[i]))
                .collect();
            out.push(header.join(" | "));
            out.push(
                widths
                    .iter()
                    .map(|w| "-".repeat(*w))
                    .collect::<Vec<_>>()
                    .join("-+-"),
            );
            for row in rows {
                let cells: Vec<String> = columns
                    .iter()
                    .enumerate()
                    .map(|(i, _)| pad(row.get(i).map(String::as_str).unwrap_or(""), widths[i]))
                    .collect();
                out.push(cells.join(" | "));
            }
            out
        }
        GenUi::Chart {
            title,
            kind,
            points,
        } => {
            let mut out = Vec::new();
            if let Some(title) = title {
                out.push(title.clone());
            }
            let max = points
                .iter()
                .map(|p| p.value.abs())
                .fold(0.0_f64, f64::max)
                .max(1.0);
            let total: f64 = points.iter().map(|p| p.value).sum();
            for p in points {
                let n = ((p.value.abs() / max) * 32.0).round() as usize;
                let bar = "#".repeat(n.max(if p.value != 0.0 { 1 } else { 0 }));
                match kind {
                    ChartKind::Pie => {
                        let pct = if total > 0.0 {
                            format!(" {:.0}%", (p.value / total) * 100.0)
                        } else {
                            String::new()
                        };
                        out.push(format!("{:<12} | {bar}{pct}", p.label));
                    }
                    _ => out.push(format!("{:<12} | {bar} {}", p.label, p.value)),
                }
            }
            if kind == &ChartKind::Line {
                out.push("(line data drawn as bars)".into());
            }
            out
        }
        GenUi::Form { title, fields, .. } => {
            let mut out = Vec::new();
            out.push(title.clone());
            for field in fields {
                match field.kind {
                    GenFieldKind::Check => {
                        out.push(format!("[ ] {}", field_label(field)));
                    }
                    GenFieldKind::Select if !field.options.is_empty() => {
                        out.push(format!(
                            "[ ] {}: {}",
                            field_label(field),
                            field.options.join("/")
                        ));
                    }
                    _ => out.push(format!("[ ] {} ({:?})", field_label(field), field.kind)),
                }
            }
            out.push("(answer in chat with one `name: value` per field)".into());
            out
        }
    }
}

fn field_label(field: &GenField) -> String {
    field.label.clone().unwrap_or_else(|| field.name.clone())
}

fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_string()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_form_table_chart() {
        let form: GenUi = serde_json::from_value(json!({
            "type": "form",
            "title": "Ship it?",
            "fields": [
                {"name": "env", "label": "Environment", "kind": "select", "options": ["dev", "prod"]},
                {"name": "note", "kind": "text"}
            ]
        }))
        .unwrap();
        assert!(matches!(form, GenUi::Form { ref title, .. } if title == "Ship it?"));

        let table: GenUi = serde_json::from_value(json!({
            "type": "table",
            "title": "deps",
            "columns": ["crate", "version"],
            "rows": [["serde", "1"]]
        }))
        .unwrap();
        assert!(matches!(table, GenUi::Table { .. }));

        let chart: GenUi = serde_json::from_value(json!({
            "type": "chart", "kind": "bar",
            "points": [{"label": "a", "value": 3.0}]
        }))
        .unwrap();
        assert!(matches!(
            chart,
            GenUi::Chart {
                kind: ChartKind::Bar,
                ..
            }
        ));
    }

    #[test]
    fn splits_text_and_blocks() {
        let text = "before\n```modron-ui\n{\"type\":\"table\",\"columns\":[\"a\"],\"rows\":[[\"1\"]]}\n```\nafter";
        let segments = split_generative_ui(text);
        assert_eq!(segments.len(), 3);
        assert!(matches!(&segments[0], GenSegment::Text(t) if t == "before"));
        assert!(matches!(&segments[1], GenSegment::Ui(GenUi::Table { .. })));
        assert!(matches!(&segments[2], GenSegment::Text(t) if t == "after"));
    }

    #[test]
    fn malformed_blocks_stay_visible_as_text() {
        let text = "```modron-ui\n{not json}\n```";
        let segments = split_generative_ui(text);
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], GenSegment::Text(t) if t.contains("{not json}")));
    }

    #[test]
    fn unterminated_block_keeps_as_text() {
        let segments = split_generative_ui("```modron-ui\n{\"type\":");
        assert_eq!(segments.len(), 1);
        assert!(matches!(&segments[0], GenSegment::Text(_)));
    }

    #[test]
    fn ascii_table_aligns_columns() {
        let ui = GenUi::Table {
            title: Some("t".into()),
            columns: vec!["crate".into(), "v".into()],
            rows: vec![vec!["serde".into(), "1.0".into()]],
        };
        let lines = render_ascii(&ui);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "t", "title first");
        assert!(lines[1].starts_with("crate | v  "));
        assert!(
            lines[2].contains("-+-") && lines[2].len() == 11,
            "separator: {:?}",
            lines[2]
        );
        assert!(lines[3].starts_with("serde | 1.0"));
    }

    #[test]
    fn ascii_chart_draws_bars_and_pie_percent() {
        let bar = GenUi::Chart {
            title: None,
            kind: ChartKind::Bar,
            points: vec![
                GenPoint {
                    label: "big".into(),
                    value: 8.0,
                },
                GenPoint {
                    label: "small".into(),
                    value: 2.0,
                },
            ],
        };
        let lines = render_ascii(&bar);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("########"));
        assert!(lines[1].contains("##"));

        let pie = GenUi::Chart {
            title: None,
            kind: ChartKind::Pie,
            points: vec![
                GenPoint {
                    label: "a".into(),
                    value: 1.0,
                },
                GenPoint {
                    label: "b".into(),
                    value: 1.0,
                },
            ],
        };
        let lines = render_ascii(&pie);
        assert!(lines[0].contains("50%"));
        assert!(lines[1].contains("50%"));
    }

    #[test]
    fn ascii_form_lists_fields() {
        let ui = GenUi::Form {
            title: "Deploy".into(),
            submit_label: Some("ship it".into()),
            fields: vec![
                GenField {
                    name: "env".into(),
                    label: Some("Env".into()),
                    kind: GenFieldKind::Select,
                    options: vec!["dev".into(), "prod".into()],
                    placeholder: None,
                },
                GenField {
                    name: "note".into(),
                    label: None,
                    kind: GenFieldKind::Text,
                    options: vec![],
                    placeholder: None,
                },
            ],
        };
        let lines = render_ascii(&ui);
        assert_eq!(lines[0], "Deploy");
        assert!(lines[1].contains("[ ] Env: dev/prod"));
        assert!(lines[2].contains("[ ] note (Text)"));
        assert!(lines[3].contains("answer in chat"));
    }
}
