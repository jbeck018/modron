//! markdown.rs — a small, dependency-free markdown renderer for the
//! desktop transcript. Covers the subset models actually emit in chat:
//! fenced code blocks, headings, bullet/numbered lists, bold/italic/
//! inline code, links, blockquotes, and horizontal rules. Blocks render
//! with the active theme (styled divs, monospace panels for code).

use gpui_kit::component::theme::Theme;
use gpui_kit::{div, px, IntoElement, ParentElement as _, SharedString, Styled as _};

const MONO: &str = "ui-monospace, SFMono-Regular, Menlo, monospace";

/// One rendered block.
#[derive(Clone, Debug, PartialEq)]
pub enum MdBlock {
    Heading { level: u8, text: String },
    Paragraph { spans: Vec<MdSpan> },
    CodeBlock { code: String },
    ListItem { spans: Vec<MdSpan>, marker: String },
    Quote { text: String },
    Rule,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MdSpan {
    Plain(String),
    Bold(String),
    Italic(String),
    Code(String),
    BoldItalic(String),
    Link { text: String, url: String },
}

/// Parse an answer into blocks (fenced code first, then line grouping
/// into headings / lists / quotes / paragraphs).
pub fn parse_blocks(text: &str) -> Vec<MdBlock> {
    let mut blocks = Vec::new();
    let mut in_code = false;
    let mut code_lines: Vec<String> = Vec::new();
    let mut para: Vec<String> = Vec::new();
    let mut list_items: Vec<String> = Vec::new();

    fn flush_para(blocks: &mut Vec<MdBlock>, para: &mut Vec<String>) {
        if !para.is_empty() {
            blocks.push(MdBlock::Paragraph {
                spans: parse_inline(&para.join("\n")),
            });
            para.clear();
        }
    }
    fn flush_list(blocks: &mut Vec<MdBlock>, items: &mut Vec<String>) {
        for item in items.drain(..) {
            let (marker, body) = item
                .split_once(' ')
                .map(|(m, b)| (m.to_string(), b.to_string()))
                .unwrap_or_else(|| (String::new(), item));
            blocks.push(MdBlock::ListItem {
                spans: parse_inline(&body),
                marker,
            });
        }
    }

    for line in text.lines() {
        let t = line.trim_start();

        if t.starts_with("```") {
            if in_code {
                blocks.push(MdBlock::CodeBlock {
                    code: code_lines.join("\n"),
                });
                code_lines.clear();
                in_code = false;
            } else {
                flush_para(&mut blocks, &mut para);
                flush_list(&mut blocks, &mut list_items);
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_lines.push(line.to_string());
            continue;
        }

        if t.trim_end().is_empty() {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut list_items);
            continue;
        }
        if t.trim_end() == "---" || t.trim_end() == "***" {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut list_items);
            blocks.push(MdBlock::Rule);
            continue;
        }
        let hashes = t.chars().take_while(|c| *c == '#').count();
        if (1..=4).contains(&hashes) && t[hashes..].starts_with(' ') {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut list_items);
            blocks.push(MdBlock::Heading {
                level: hashes as u8,
                text: t[hashes..].trim().to_string(),
            });
            continue;
        }
        if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            flush_para(&mut blocks, &mut para);
            list_items.push(format!("\u{2022} {rest}"));
            continue;
        }
        let mut num = String::new();
        for c in t.chars() {
            if c.is_ascii_digit() {
                num.push(c);
            } else {
                break;
            }
        }
        if !num.is_empty() && t[num.len()..].starts_with(". ") {
            flush_para(&mut blocks, &mut para);
            list_items.push(format!("{num}. {}", &t[num.len() + 2..]));
            continue;
        }
        if t.starts_with('>') {
            flush_para(&mut blocks, &mut para);
            flush_list(&mut blocks, &mut list_items);
            blocks.push(MdBlock::Quote {
                text: t.trim_start_matches('>').trim().to_string(),
            });
            continue;
        }
        if let Some(last) = list_items.last_mut() {
            // A continuation line of the previous list item.
            let is_new_item = t.starts_with("- ")
                || t.starts_with("* ")
                || (num.is_empty() && t.chars().next().is_some_and(|c| c.is_ascii_digit()));
            if !is_new_item && !t.is_empty() {
                last.push(' ');
                last.push_str(t.trim());
                continue;
            }
        }
        para.push(line.trim_end().to_string());
    }
    if in_code {
        blocks.push(MdBlock::CodeBlock {
            code: code_lines.join("\n"),
        });
    }
    flush_para(&mut blocks, &mut para);
    flush_list(&mut blocks, &mut list_items);
    blocks
}

/// Inline spans: bold, bold-italic, italic, inline code, links.
pub fn parse_inline(text: &str) -> Vec<MdSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;

    macro_rules! flush {
        () => {
            if !plain.is_empty() {
                spans.push(MdSpan::Plain(std::mem::take(&mut plain)));
            }
        };
    }

    while i < n {
        if chars[i] == '`' {
            if let Some(close) = (i + 1..n).find(|&j| chars[j] == '`') {
                if close > i + 1 {
                    flush!();
                    spans.push(MdSpan::Code(chars[i + 1..close].iter().collect()));
                    i = close + 1;
                    continue;
                }
            }
        }
        if chars[i] == '*' && i + 1 < n && chars[i + 1] == '*' {
            if let Some(close) = find_delim(&chars, i + 2, "**") {
                let inner: String = chars[i + 2..close].iter().collect();
                if !inner.trim().is_empty() {
                    flush!();
                    spans.push(MdSpan::BoldItalic(inner));
                    i = close + 2;
                    continue;
                }
            }
        }
        if chars[i] == '*' {
            if let Some(close) = find_delim(&chars, i + 1, "*") {
                let inner: String = chars[i + 1..close].iter().collect();
                if !inner.trim().is_empty() {
                    flush!();
                    spans.push(MdSpan::Italic(inner));
                    i = close + 1;
                    continue;
                }
            }
        }
        if chars[i] == '[' {
            if let Some(close_bracket) = (i + 1..n).find(|&j| chars[j] == ']') {
                if close_bracket + 1 < n
                    && chars[close_bracket + 1] == '('
                    && (close_bracket + 2..=n)
                        .find(|&j| j == n || chars[j] == ')')
                        .is_some_and(|close| close > close_bracket + 2 && close <= n)
                {
                    let close = (close_bracket + 2..=n)
                        .find(|&j| j == n || chars[j] == ')')
                        .unwrap();
                    flush!();
                    spans.push(MdSpan::Link {
                        text: chars[i + 1..close_bracket].iter().collect(),
                        url: chars[close_bracket + 2..close].iter().collect(),
                    });
                    i = close + usize::from(close < n);
                    continue;
                }
            }
        }
        plain.push(chars[i]);
        i += 1;
    }
    flush!();
    spans
}

fn find_delim(chars: &[char], from: usize, delim: &str) -> Option<usize> {
    let d: Vec<char> = delim.chars().collect();
    let m = d.len();
    if from + m > chars.len() {
        return None;
    }
    (from..=chars.len() - m).find(|&j| chars[j..j + m] == d[..])
}

/// Render an assistant answer as styled markdown elements.
pub fn render_markdown(text: &str, theme: &Theme) -> gpui_kit::AnyElement {
    render_blocks(&parse_blocks(text), theme)
}

/// Render pre-parsed blocks. Split from [`render_markdown`] so callers can
/// cache the parse (completed messages never change; re-parsing them per
/// render frame was pure waste) and rebuild only the elements.
pub fn render_blocks(blocks: &[MdBlock], theme: &Theme) -> gpui_kit::AnyElement {
    let mut root = div().flex().flex_col().gap_1().w_full();

    for block in blocks {
        match block {
            MdBlock::Heading { level, text } => {
                let size = match level {
                    1 => 18.,
                    2 => 16.,
                    3 => 14.5,
                    _ => 13.5,
                };
                let label: SharedString = text.clone().into();
                root = root.child(
                    div()
                        .pt_1()
                        .text_size(px(size))
                        .text_color(theme.foreground)
                        .child(label),
                );
            }
            MdBlock::Paragraph { spans } => {
                root = root.child(render_spans(spans.clone(), theme, 13.5));
            }
            MdBlock::CodeBlock { code } => {
                let mut panel = div()
                    .w_full()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary)
                    .p_2()
                    .overflow_hidden();
                for line in code.lines() {
                    let label: SharedString = if line.is_empty() {
                        " ".into()
                    } else {
                        line.to_owned().into()
                    };
                    panel = panel.child(
                        div()
                            .font_family(MONO)
                            .text_size(px(crate::brand::text::SM))
                            .text_color(theme.foreground)
                            .child(label),
                    );
                }
                root = root.child(panel);
            }
            MdBlock::ListItem { spans, marker } => {
                let marker_label: SharedString = if marker.is_empty() {
                    "\u{2022}".into()
                } else {
                    marker.clone().into()
                };
                root = root.child(
                    div()
                        .flex()
                        .flex_row()
                        .gap_2()
                        .px_4()
                        .child(
                            div()
                                .text_size(px(crate::brand::text::BASE))
                                .text_color(theme.muted_foreground)
                                .child(marker_label),
                        )
                        .child(render_spans(spans.clone(), theme, 13.)),
                );
            }
            MdBlock::Quote { text } => {
                let label: SharedString = text.clone().into();
                root = root.child(
                    div()
                        .border_l_2()
                        .border_color(theme.muted_foreground)
                        .pl_3()
                        .text_color(theme.muted_foreground)
                        .italic()
                        .child(label),
                );
            }
            MdBlock::Rule => {
                root = root.child(div().h(px(1.)).bg(theme.border).my_1());
            }
        }
    }
    root.into_any_element()
}

fn render_spans(spans: Vec<MdSpan>, theme: &Theme, size: f32) -> gpui_kit::AnyElement {
    let mut row = div().flex().flex_row().flex_wrap().gap_x_1();
    for span in &spans {
        let element = match span {
            MdSpan::Plain(text)
            | MdSpan::Bold(text)
            | MdSpan::Italic(text)
            | MdSpan::BoldItalic(text) => {
                let mut el = div()
                    .text_color(theme.foreground)
                    .child(SharedString::from(text.clone()));
                if matches!(span, MdSpan::Bold(_) | MdSpan::BoldItalic(_)) {
                    el = el.font_weight(gpui_kit::FontWeight::BOLD);
                }
                if matches!(span, MdSpan::Italic(_) | MdSpan::BoldItalic(_)) {
                    el = el.italic();
                }
                el
            }
            MdSpan::Code(text) => div()
                .px_1()
                .rounded_sm()
                .bg(theme.secondary)
                .font_family(MONO)
                .text_size(px(size - 1.5))
                .text_color(theme.foreground)
                .child(SharedString::from(text.clone())),
            MdSpan::Link { text, url } => div()
                .text_color(theme.accent)
                .child(SharedString::from(format!("{} ({})", text, url))),
        };
        row = row.child(element.into_any_element());
    }
    row.into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_bold_italic_code() {
        let spans = parse_inline("plain **bold** and *it* plus `code`");
        assert_eq!(
            spans,
            vec![
                MdSpan::Plain("plain ".into()),
                MdSpan::BoldItalic("bold".into()),
                MdSpan::Plain(" and ".into()),
                MdSpan::Italic("it".into()),
                MdSpan::Plain(" plus ".into()),
                MdSpan::Code("code".into()),
            ]
        );
    }

    #[test]
    fn unterminated_markers_stay_plain() {
        let spans = parse_inline("a ** b ` c");
        assert_eq!(spans, vec![MdSpan::Plain("a ** b ` c".into())]);
    }

    #[test]
    fn blocks_split_code_and_headings() {
        let blocks = parse_blocks("# Title\n\npara\n```rust\ncode\n```\n- item");
        assert_eq!(
            blocks,
            vec![
                MdBlock::Heading {
                    level: 1,
                    text: "Title".into()
                },
                MdBlock::Paragraph {
                    spans: vec![MdSpan::Plain("para".into())]
                },
                MdBlock::CodeBlock {
                    code: "code".into()
                },
                MdBlock::ListItem {
                    spans: vec![MdSpan::Plain("item".into())],
                    marker: "\u{2022}".into(),
                },
            ]
        );
    }

    #[test]
    fn numbered_lists_keep_their_number() {
        let blocks = parse_blocks("1. first\n2. second");
        let markers: Vec<String> = blocks
            .iter()
            .filter_map(|b| match b {
                MdBlock::ListItem { marker, .. } => Some(marker.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(markers, vec!["1.", "2."]);
    }
}
