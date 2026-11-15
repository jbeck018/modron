//! Independent reviewer pass (H4a), pure half: the reviewer prompt, the
//! model-facing review request, a tolerant verdict parser and the feedback
//! text fed back to the main model. No I/O here; the engine owns the
//! provider call and the git diff.

use modron_types::{Finding, ReviewVerdict, Severity};
use std::fmt::Write as _;

/// System prompt for the reviewer call. Kept short on purpose: the reviewer
/// is an independent second opinion, not a style checker.
pub const REVIEWER_SYSTEM: &str = "You are an independent code reviewer. You did not write \
this change. Given the task and the diff, report only real defects the diff introduces: \
correctness bugs, security problems, or a missed requirement of the task. Do not report \
style, naming, speculation, or refactoring ideas. Output format, exactly: the first line is \
`VERDICT: PASS` or `VERDICT: NEEDS_FIX`. Then zero or more finding lines, each \
`P<0-3> <path>:<line> - <text>` (path and line optional; P0 is most severe). No other prose.";

/// The user message for the reviewer call: the task and the diff, nothing else.
pub fn build_review_prompt(task: &str, diff: &str) -> String {
    format!(
        "TASK:\n{task}\n\nDIFF:\n{diff}\n\nReview the diff against the task. \
         Reply in the required format."
    )
}

/// Parse the reviewer's output. Tolerant by design: the verdict is matched
/// case-insensitively, markdown bullets, numbering, bold markers and
/// backticks are ignored, and lines that are neither a verdict nor a
/// finding are skipped.
///
/// - No verdict line at all -> `Skipped` with no findings.
/// - `PASS` -> `Pass`; findings (if any) are kept as notes.
/// - `NEEDS_FIX` -> `NeedsFix`, even with zero parsable findings.
pub fn parse_review(output: &str) -> (ReviewVerdict, Vec<Finding>) {
    let mut verdict = None;
    let mut findings = Vec::new();
    for raw in output.lines() {
        let line = normalize(raw);
        if verdict.is_none() {
            if let Some(found) = parse_verdict(&line) {
                verdict = Some(found);
                continue;
            }
        }
        if let Some(finding) = parse_finding(&line) {
            findings.push(finding);
        }
    }
    match verdict {
        Some(verdict) => (verdict, findings),
        None => (ReviewVerdict::Skipped, Vec::new()),
    }
}

/// The feedback message for a `NeedsFix` round: the findings verbatim, as a
/// user message the main model must act on before finishing again.
pub fn feedback_message(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "An independent review judged the changes as needing fixes but listed no \
                details. Re-check the diff against the task, fix any real defect, then finish."
            .to_string();
    }
    let mut out = String::from(
        "An independent review of your changes found these issues. Fix them, then finish; \
         the review runs again:\n",
    );
    for finding in findings {
        let _ = write!(out, "- {} ", finding.severity.code());
        if let Some(path) = &finding.path {
            out.push_str(path);
            if let Some(line) = finding.line {
                let _ = write!(out, ":{line}");
            }
            out.push_str(" - ");
        }
        out.push_str(&finding.text);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Strip backticks, list bullets / numbering and surrounding markdown
/// emphasis from one output line.
fn normalize(raw: &str) -> String {
    let stripped: String = raw.chars().filter(|c| *c != '`').collect();
    let mut s = stripped.trim();
    for bullet in ["- ", "* ", "+ ", "\u{2022} "] {
        if let Some(rest) = s.strip_prefix(bullet) {
            s = rest.trim_start();
            break;
        }
    }
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &s[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            s = rest.trim_start();
        }
    }
    s.trim_matches(|c: char| c == '*' || c.is_whitespace())
        .to_string()
}

/// `VERDICT: PASS` / `VERDICT: NEEDS_FIX` (case-insensitive; bold markers
/// and `NEEDS FIX` / `NEEDS-FIX` spellings tolerated).
fn parse_verdict(line: &str) -> Option<ReviewVerdict> {
    let cleaned: String = line.chars().filter(|c| *c != '*' && *c != '`').collect();
    let (key, value) = cleaned.split_once(':')?;
    if !key.trim().eq_ignore_ascii_case("VERDICT") {
        return None;
    }
    let value = value.trim().to_ascii_uppercase().replace([' ', '-'], "_");
    match value.as_str() {
        "PASS" => Some(ReviewVerdict::Pass),
        "NEEDS_FIX" => Some(ReviewVerdict::NeedsFix),
        _ => None,
    }
}

/// `P<0-3> [<path>:<line>] - <text>`; the location part is optional. Lines
/// with an unknown severity or empty text are not findings.
fn parse_finding(line: &str) -> Option<Finding> {
    let mut chars = line.chars();
    if !chars.next()?.eq_ignore_ascii_case(&'p') {
        return None;
    }
    let severity = match chars.next()?.to_digit(10)? {
        0 => Severity::P0,
        1 => Severity::P1,
        2 => Severity::P2,
        3 => Severity::P3,
        _ => return None,
    };
    let rest = chars.as_str();
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim();
    let (location, text) = if let Some(text) = rest.strip_prefix("- ") {
        ("", text)
    } else if let Some((location, text)) = rest.split_once(" - ") {
        (location.trim(), text)
    } else {
        ("", rest)
    };
    // A location is a single token; anything with spaces is prose.
    let (location, text) = if location.contains(char::is_whitespace) {
        ("", rest)
    } else {
        (location, text)
    };
    let text = text.trim();
    if text.is_empty() || text == "-" {
        return None;
    }
    let (path, line) = parse_location(location);
    Some(Finding {
        severity,
        path,
        line,
        text: text.to_string(),
    })
}

fn parse_location(location: &str) -> (Option<String>, Option<u32>) {
    if location.is_empty() {
        return (None, None);
    }
    if let Some((path, line)) = location.rsplit_once(':') {
        if let Ok(line) = line.trim().parse::<u32>() {
            let path = path.trim();
            let path = (!path.is_empty()).then(|| path.to_string());
            return (path, Some(line));
        }
    }
    (Some(location.to_string()), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pass_with_no_findings() {
        let (verdict, findings) = parse_review("VERDICT: PASS");
        assert_eq!(verdict, ReviewVerdict::Pass);
        assert!(findings.is_empty());
    }

    #[test]
    fn needs_fix_with_located_findings() {
        let out = "VERDICT: NEEDS_FIX\nP1 src/lib.rs:42 - off by one in the loop bound\n\
                   P0 src/auth.rs:7 - token compared with ==";
        let (verdict, findings) = parse_review(out);
        assert_eq!(verdict, ReviewVerdict::NeedsFix);
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].severity, Severity::P1);
        assert_eq!(findings[0].path.as_deref(), Some("src/lib.rs"));
        assert_eq!(findings[0].line, Some(42));
        assert_eq!(findings[0].text, "off by one in the loop bound");
        assert_eq!(findings[1].severity, Severity::P0);
    }

    #[test]
    fn verdict_is_case_insensitive() {
        assert_eq!(parse_review("verdict: pass").0, ReviewVerdict::Pass);
        assert_eq!(
            parse_review("Verdict: needs_fix").0,
            ReviewVerdict::NeedsFix
        );
    }

    #[test]
    fn markdown_bullets_bold_and_backticks_are_ignored() {
        let out = "**VERDICT: NEEDS_FIX**\n- `P2` `src/x.rs:3` - unchecked `unwrap`\n\
                   * P3 - nit about naming";
        let (verdict, findings) = parse_review(out);
        assert_eq!(verdict, ReviewVerdict::NeedsFix);
        assert_eq!(findings.len(), 2, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::P2);
        assert_eq!(findings[0].path.as_deref(), Some("src/x.rs"));
        assert_eq!(findings[0].line, Some(3));
        assert_eq!(findings[0].text, "unchecked unwrap");
        assert_eq!(findings[1].severity, Severity::P3);
        assert_eq!(findings[1].text, "nit about naming");
    }

    #[test]
    fn missing_verdict_is_skipped_even_with_findings() {
        let (verdict, findings) = parse_review("P1 a.rs:1 - looks broken");
        assert_eq!(verdict, ReviewVerdict::Skipped);
        assert!(findings.is_empty());
    }

    #[test]
    fn empty_output_is_skipped() {
        assert_eq!(parse_review("").0, ReviewVerdict::Skipped);
        assert_eq!(parse_review("   \n\n").0, ReviewVerdict::Skipped);
    }

    #[test]
    fn needs_fix_with_zero_findings_stays_needs_fix() {
        let (verdict, findings) = parse_review("VERDICT: NEEDS_FIX\nSome prose only.");
        assert_eq!(verdict, ReviewVerdict::NeedsFix);
        assert!(findings.is_empty());
    }

    #[test]
    fn pass_keeps_findings_as_notes() {
        let (verdict, findings) = parse_review("VERDICT: PASS\nP3 b.rs:9 - minor note");
        assert_eq!(verdict, ReviewVerdict::Pass);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::P3);
    }

    #[test]
    fn unknown_lines_and_severities_are_skipped() {
        let out = "VERDICT: NEEDS_FIX\nP4 a.rs:1 - too severe a level\nP10 x\n\
                   Here is my review.\nP2 - no location here\nP1 - \n";
        let (_, findings) = parse_review(out);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].severity, Severity::P2);
        assert_eq!(findings[0].path, None);
        assert_eq!(findings[0].line, None);
        assert_eq!(findings[0].text, "no location here");
    }

    #[test]
    fn first_verdict_line_wins() {
        let (verdict, _) = parse_review("VERDICT: PASS\nVERDICT: NEEDS_FIX");
        assert_eq!(verdict, ReviewVerdict::Pass);
    }

    #[test]
    fn unknown_verdict_value_is_not_a_verdict() {
        let (verdict, _) = parse_review("VERDICT: MAYBE");
        assert_eq!(verdict, ReviewVerdict::Skipped);
    }

    #[test]
    fn feedback_lists_each_finding() {
        let findings = vec![
            Finding {
                severity: Severity::P1,
                path: Some("a.rs".into()),
                line: Some(5),
                text: "wrong default".into(),
            },
            Finding {
                severity: Severity::P2,
                path: None,
                line: None,
                text: "missing test".into(),
            },
        ];
        let msg = feedback_message(&findings);
        assert!(msg.contains("- P1 a.rs:5 - wrong default"), "{msg}");
        assert!(msg.contains("- P2 missing test"), "{msg}");
    }

    #[test]
    fn review_prompt_carries_task_and_diff() {
        let prompt = build_review_prompt("add a flag", "+fn x() {}");
        assert!(prompt.contains("add a flag"));
        assert!(prompt.contains("+fn x() {}"));
    }
}
