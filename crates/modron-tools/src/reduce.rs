//! Output reduction (LEDGER T2) — the single choke point every tool result
//! passes through before it enters model context.
//!
//! Pipeline: strip ANSI escape sequences → collapse blank-line runs → byte cap
//! with an explicit elision marker. Token efficiency is a product principle
//! (PLAN.md §4.6); nothing reaches the model un-reduced.

/// Maximum size of one elision marker line; trivially small, here for docs.
///
/// Reduces tool output for model consumption:
///
/// 1. Strips ANSI escape sequences (colors, cursor movement).
/// 2. Collapses runs of more than 3 consecutive blank lines to one blank line.
/// 3. If the result exceeds `max_bytes`, truncates at a UTF-8 char boundary and
///    appends a trailing marker: `… [N of M bytes elided]`.
///
/// Every tool in this crate routes both successful and error output through
/// this function before returning a [`modron_core::ToolOutcome`].
pub fn reduce_output(content: impl Into<String>, max_bytes: usize) -> String {
    let cleaned = collapse_blank_lines(&strip_ansi(&content.into()));
    if cleaned.len() <= max_bytes {
        return cleaned;
    }
    // Truncate at a UTF-8 char boundary at or below max_bytes.
    let mut cut = max_bytes;
    while cut > 0 && !cleaned.is_char_boundary(cut) {
        cut -= 1;
    }
    let elided = cleaned.len() - cut;
    format!(
        "{}\n… [{} of {} bytes elided]",
        &cleaned[..cut],
        elided,
        cleaned.len()
    )
}

/// Removes ANSI escape sequences. Regex-free state machine covering the
/// sequences that show up in command output: CSI (`ESC [ … final`), OSC
/// (`ESC ] … BEL` or `ESC ] … ESC \`), and two-byte escapes (`ESC M` etc.).
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                // CSI: parameter/intermediate bytes (0x20–0x3F) then a final byte (0x40–0x7E).
                chars.next();
                for n in chars.by_ref() {
                    if ('@'..='~').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                // OSC: terminated by BEL or ST (ESC \).
                chars.next();
                loop {
                    match chars.next() {
                        None | Some('\x07') => break,
                        Some('\x1b') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Some(_) => {
                // Two-byte escape (ESC M, ESC 7, …) or a stray ESC: drop the
                // next char; a trailing lone ESC is dropped by the peek/next.
                chars.next();
            }
            None => {}
        }
    }
    out
}

/// Collapses runs of more than 3 consecutive blank lines (whitespace-only
/// lines count as blank) to a single blank line.
fn collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blanks = 0usize;
    let flush = |out: &mut Vec<&str>, blanks: &mut usize| {
        if *blanks > 3 {
            out.push("");
        } else {
            for _ in 0..*blanks {
                out.push("");
            }
        }
        *blanks = 0;
    };
    for line in s.lines() {
        if line.trim().is_empty() {
            blanks += 1;
        } else {
            flush(&mut out, &mut blanks);
            out.push(line);
        }
    }
    flush(&mut out, &mut blanks);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_output_is_capped_with_marker() {
        let big = "x".repeat(10_000);
        let out = reduce_output(big, 1_000);
        assert!(
            out.len() < 1_100,
            "output should be near the cap, got {}",
            out.len()
        );
        assert!(out.contains("… ["), "missing elision marker: {out}");
        assert!(
            out.contains(" of 10000 bytes elided]"),
            "marker should report totals: {out}"
        );
        assert!(out.starts_with("xxxx"), "kept prefix should be preserved");
    }

    #[test]
    fn ansi_escape_sequences_are_stripped() {
        assert_eq!(reduce_output("\x1b[31mred\x1b[0m", 1_000), "red");
        assert_eq!(
            reduce_output("\x1b[1;32mgreen\x1b[m plain", 1_000),
            "green plain"
        );
        // OSC (window title) and a two-byte escape.
        assert_eq!(reduce_output("\x1b]0;title\x07body", 1_000), "body");
        assert_eq!(reduce_output("a\x1bMb", 1_000), "ab");
    }

    #[test]
    fn blank_line_runs_collapse() {
        let input = "a\n\n\n\n\n\nb"; // 5 blank lines between a and b
        assert_eq!(reduce_output(input, 10_000), "a\n\nb");
        // 3 blank lines are left untouched (only >3 collapses).
        assert_eq!(reduce_output("a\n\n\n\nb", 10_000), "a\n\n\n\nb");
    }

    #[test]
    fn under_cap_output_is_unchanged() {
        assert_eq!(reduce_output("hello\nworld", 1_000), "hello\nworld");
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        // 'é' is 2 bytes; a 5-byte cap cannot split it.
        let out = reduce_output("ééé", 5);
        assert!(out.starts_with("éé"), "cut must not split a char: {out}");
        assert!(out.contains("bytes elided]"));
    }
}
