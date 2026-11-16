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

/// Untracked files shown in full: at most this many, in path order.
pub const NEW_FILE_LIMIT: usize = 40;

/// Untracked files larger than this are listed as not shown.
pub const NEW_FILE_MAX_BYTES: u64 = 20 * 1024;

/// An untracked file as the engine found it on disk (H4b). `bytes` is the
/// on-disk size; `content` is `None` when the engine did not read it (not a
/// regular file, outside the working dir, or over the size limit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UntrackedFile {
    pub path: String,
    pub bytes: u64,
    pub content: Option<Vec<u8>>,
}

/// Sort the untracked paths and split off the ones the reviewer may see:
/// the first [`NEW_FILE_LIMIT`] in path order, plus how many were left out.
pub fn select_untracked(mut paths: Vec<String>) -> (Vec<String>, usize) {
    paths.sort();
    let left_out = paths.len().saturating_sub(NEW_FILE_LIMIT);
    paths.truncate(NEW_FILE_LIMIT);
    (paths, left_out)
}

/// Render untracked files as unified new-file diffs, in the order given.
/// A file is shown only when it is at most [`NEW_FILE_MAX_BYTES`], was read,
/// is valid UTF-8 and has no NUL byte; every other file gets one
/// `new file (not shown): <path> (<bytes> bytes)` line.
pub fn render_new_files(files: &[UntrackedFile], left_out: usize) -> String {
    let mut out = String::new();
    for file in files {
        match shown_text(file) {
            Some(text) => out.push_str(&new_file_diff(&file.path, text)),
            None => {
                let _ = writeln!(
                    out,
                    "new file (not shown): {} ({} bytes)",
                    file.path, file.bytes
                );
            }
        }
    }
    if left_out > 0 {
        let _ = writeln!(out, "[{left_out} more untracked files not shown]");
    }
    out
}

fn shown_text(file: &UntrackedFile) -> Option<&str> {
    if file.bytes > NEW_FILE_MAX_BYTES {
        return None;
    }
    let content = file.content.as_deref()?;
    if content.contains(&0) {
        return None;
    }
    std::str::from_utf8(content).ok()
}

/// One standard unified new-file diff: header, `/dev/null` source and a
/// single `@@ -0,0 +1,N @@` hunk with every line prefixed `+`.
fn new_file_diff(path: &str, text: &str) -> String {
    let mut out = format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n"
    );
    if text.is_empty() {
        return out;
    }
    let lines: Vec<&str> = text.lines().collect();
    let _ = writeln!(out, "@@ -0,0 +1,{} @@", lines.len());
    for line in &lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    if !text.ends_with('\n') {
        out.push_str("\\ No newline at end of file\n");
    }
    out
}

/// What the engine saw during one run, for the claim checks (H4b). Gathered
/// from tool calls the engine already dispatches; never persisted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunFacts {
    /// Bash command strings that actually executed this run.
    pub commands: Vec<String>,
    /// Whether a `verify` run happened (model-issued or the finish gate's).
    pub verify_ran: bool,
    /// Outcome of the last verify run; `None` when none ran.
    pub verify_passed: Option<bool>,
}

/// Command substrings that count as a test run.
const TEST_COMMAND_MARKERS: [&str; 11] = [
    "cargo test",
    "cargo nextest",
    "pytest",
    "npm test",
    "pnpm test",
    "yarn test",
    "go test",
    "jest",
    "vitest",
    "make test",
    "ctest",
];

/// Command substrings that count as a build or lint run.
const BUILD_COMMAND_MARKERS: [&str; 6] = ["build", "check", "clippy", "tsc", "lint", "compile"];

/// Words that, within the four words before a claim, make it a condition, a
/// plan or a request rather than a statement of fact ("if the tests pass",
/// "I will make sure all tests pass"). Conservative: a claim near one of
/// these is not reported.
const CLAIM_GUARDS: [&str; 15] = [
    "if", "whether", "when", "once", "until", "unless", "to", "should", "will", "must", "may",
    "might", "would", "could", "need",
];

/// A claim phrase as a sequence of word slots; each slot lists the words it
/// accepts. Matching is on whole words, so there is no substring leakage.
type Phrase = &'static [&'static [&'static str]];

/// Test-success claims: "all tests pass", "3 tests passed", "cargo test
/// passes", "test suite is green".
const TEST_CLAIMS: [Phrase; 5] = [
    &[&["tests", "test"], &["pass", "passes", "passed"]],
    &[
        &["cargo", "go", "npm", "pnpm", "yarn", "make"],
        &["test"],
        &["passes", "passed"],
    ],
    &[&["pytest"], &["passes", "passed"]],
    &[&["test"], &["suite"], &["green"]],
    &[&["test"], &["suite"], &["is"], &["green"]],
];

/// Clean build or lint claims: "builds cleanly", "compiles", "clippy clean",
/// "no lint errors", "typechecks".
const BUILD_CLAIMS: [Phrase; 6] = [
    &[&["build", "builds"], &["cleanly"]],
    &[&["compiles"]],
    &[&["clippy"], &["clean"]],
    &[&["no"], &["lint"], &["errors", "warnings", "issues"]],
    &[&["typechecks"]],
    &[&["type"], &["checks"], &["cleanly"]],
];

/// Lowercase word tokens: maximal runs of ASCII alphanumerics.
fn tokenize(text: &str) -> Vec<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether `phrases` matches somewhere in `tokens` outside a guard context.
fn claims(tokens: &[String], phrases: &[Phrase]) -> bool {
    phrases.iter().any(|phrase| {
        (0..tokens.len()).any(|start| {
            let fits = start + phrase.len() <= tokens.len()
                && phrase
                    .iter()
                    .enumerate()
                    .all(|(i, slot)| slot.contains(&tokens[start + i].as_str()));
            let guarded = tokens[start.saturating_sub(4)..start]
                .iter()
                .any(|word| CLAIM_GUARDS.contains(&word.as_str()));
            fits && !guarded
        })
    })
}

fn ran_command_with(commands: &[String], markers: &[&str]) -> bool {
    commands.iter().any(|command| {
        let lower = command.to_ascii_lowercase();
        markers.iter().any(|marker| lower.contains(marker))
    })
}

/// Cheap, non-LLM checks on the final answer against what the run did (H4b).
/// Every finding is P2, except P1 when verify ran and failed while the answer
/// claims success. Advisory: the engine never extends a run on these.
///
/// - tests claimed, no test command ran and verify did not pass -> P2
/// - clean build or lint claimed, no build/lint command ran and verify did
///   not pass -> P2
/// - verify failed and the answer claims either success -> P1 (one finding)
pub fn claim_findings(final_answer: &str, ran: &RunFacts) -> Vec<Finding> {
    let tokens = tokenize(final_answer);
    let claims_tests = claims(&tokens, &TEST_CLAIMS);
    let claims_build = claims(&tokens, &BUILD_CLAIMS);
    let verify_failed = ran.verify_ran && ran.verify_passed == Some(false);
    let verify_ok = ran.verify_passed == Some(true);
    let finding = |severity, text: &str| Finding {
        severity,
        path: None,
        line: None,
        text: text.to_string(),
    };
    if verify_failed && (claims_tests || claims_build) {
        return vec![finding(
            Severity::P1,
            "claims success, but the verify command failed",
        )];
    }
    let mut out = Vec::new();
    if claims_tests && !verify_ok && !ran_command_with(&ran.commands, &TEST_COMMAND_MARKERS) {
        out.push(finding(
            Severity::P2,
            "claims tests pass, but no test command ran in this run",
        ));
    }
    if claims_build && !verify_ok && !ran_command_with(&ran.commands, &BUILD_COMMAND_MARKERS) {
        out.push(finding(
            Severity::P2,
            "claims a clean build or lint, but no such command ran",
        ));
    }
    out
}

/// Fold claim findings into a review result (H4b). Claims are appended to
/// the findings; a `Pass` with any P1 claim becomes `NeedsFix`. Other
/// verdicts are left as they are.
pub fn merge_claims(
    verdict: ReviewVerdict,
    mut findings: Vec<Finding>,
    claims: Vec<Finding>,
) -> (ReviewVerdict, Vec<Finding>) {
    let p1 = claims.iter().any(|f| f.severity == Severity::P1);
    findings.extend(claims);
    let verdict = match verdict {
        ReviewVerdict::Pass if p1 => ReviewVerdict::NeedsFix,
        other => other,
    };
    (verdict, findings)
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

    fn file(path: &str, content: &[u8]) -> UntrackedFile {
        UntrackedFile {
            path: path.into(),
            bytes: content.len() as u64,
            content: Some(content.to_vec()),
        }
    }

    #[test]
    fn new_file_renders_as_a_unified_new_file_diff() {
        let out = render_new_files(&[file("src/new.rs", b"fn a() {}\nfn b() {}\n")], 0);
        assert_eq!(
            out,
            "diff --git a/src/new.rs b/src/new.rs\nnew file mode 100644\n--- /dev/null\n\
             +++ b/src/new.rs\n@@ -0,0 +1,2 @@\n+fn a() {}\n+fn b() {}\n"
        );
    }

    #[test]
    fn new_file_without_trailing_newline_gets_the_marker() {
        let out = render_new_files(&[file("a.txt", b"one\ntwo")], 0);
        assert!(out.contains("@@ -0,0 +1,2 @@\n+one\n+two\n\\ No newline at end of file\n"));
    }

    #[test]
    fn empty_new_file_has_header_and_no_hunk() {
        let out = render_new_files(&[file("empty.txt", b"")], 0);
        assert!(out.ends_with("+++ b/empty.txt\n"), "{out}");
        assert!(!out.contains("@@"));
    }

    #[test]
    fn selection_is_path_ordered_and_capped_at_forty() {
        let paths: Vec<String> = (0..45).rev().map(|i| format!("f{i:02}.rs")).collect();
        let (selected, left_out) = select_untracked(paths);
        assert_eq!(selected.len(), NEW_FILE_LIMIT);
        assert_eq!(left_out, 5);
        assert_eq!(selected[0], "f00.rs");
        assert_eq!(selected[39], "f39.rs");
        let files: Vec<UntrackedFile> = selected.iter().map(|p| file(p, b"x\n")).collect();
        let out = render_new_files(&files, left_out);
        assert!(out.contains("[5 more untracked files not shown]"), "{out}");
        assert!(!out.contains("f40.rs"));
    }

    #[test]
    fn files_over_twenty_kb_are_listed_not_shown() {
        let big = UntrackedFile {
            path: "big.bin".into(),
            bytes: NEW_FILE_MAX_BYTES + 1,
            content: Some(vec![b'a'; NEW_FILE_MAX_BYTES as usize + 1]),
        };
        let exact = UntrackedFile {
            path: "edge.txt".into(),
            bytes: NEW_FILE_MAX_BYTES,
            content: Some(vec![b'a'; NEW_FILE_MAX_BYTES as usize]),
        };
        let out = render_new_files(&[big, exact], 0);
        assert!(
            out.contains("new file (not shown): big.bin (20481 bytes)\n"),
            "{out}"
        );
        assert!(out.contains("+++ b/edge.txt\n"), "exactly 20 KB is shown");
    }

    #[test]
    fn binary_and_non_utf8_files_are_listed_not_shown() {
        let nul = file("a.dat", b"ab\0cd");
        let bad_utf8 = file("b.dat", &[0xff, 0xfe, b'x']);
        let unread = UntrackedFile {
            path: "link".into(),
            bytes: 9,
            content: None,
        };
        let out = render_new_files(&[nul, bad_utf8, unread], 0);
        assert!(
            out.contains("new file (not shown): a.dat (5 bytes)\n"),
            "{out}"
        );
        assert!(
            out.contains("new file (not shown): b.dat (3 bytes)\n"),
            "{out}"
        );
        assert!(
            out.contains("new file (not shown): link (9 bytes)\n"),
            "{out}"
        );
        assert!(!out.contains("diff --git"));
    }

    fn facts(commands: &[&str], verify: Option<bool>) -> RunFacts {
        RunFacts {
            commands: commands.iter().map(|c| c.to_string()).collect(),
            verify_ran: verify.is_some(),
            verify_passed: verify,
        }
    }

    fn texts(findings: &[Finding]) -> Vec<&str> {
        findings.iter().map(|f| f.text.as_str()).collect()
    }

    const NO_TEST: &str = "claims tests pass, but no test command ran in this run";
    const NO_BUILD: &str = "claims a clean build or lint, but no such command ran";
    const VERIFY_FAILED: &str = "claims success, but the verify command failed";

    #[test]
    fn tests_claimed_without_a_test_command_is_p2() {
        for answer in [
            "All tests pass.",
            "3 tests passed.",
            "The test suite is green.",
            "cargo test passes now",
        ] {
            let f = claim_findings(answer, &facts(&["cargo build"], None));
            assert_eq!(texts(&f), vec![NO_TEST], "answer: {answer}");
            assert_eq!(f[0].severity, Severity::P2);
            assert_eq!(f[0].path, None);
        }
    }

    #[test]
    fn test_command_or_passing_verify_silences_the_tests_claim() {
        let ran_tests = claim_findings("All tests pass.", &facts(&["cargo nextest run"], None));
        assert!(ran_tests.is_empty());
        let verified = claim_findings("All tests pass.", &facts(&[], Some(true)));
        assert!(verified.is_empty());
    }

    #[test]
    fn build_claimed_without_a_build_command_is_p2() {
        for answer in [
            "It builds cleanly.",
            "The code compiles.",
            "clippy clean",
            "no lint errors",
            "it typechecks",
        ] {
            let f = claim_findings(answer, &facts(&["ls"], None));
            assert_eq!(texts(&f), vec![NO_BUILD], "answer: {answer}");
            assert_eq!(f[0].severity, Severity::P2);
        }
    }

    #[test]
    fn build_command_silences_the_build_claim() {
        let f = claim_findings(
            "It compiles.",
            &facts(&["cargo clippy --all-targets"], None),
        );
        assert!(f.is_empty());
    }

    #[test]
    fn failed_verify_with_a_success_claim_is_one_p1() {
        let f = claim_findings("All tests pass and it compiles.", &facts(&[], Some(false)));
        assert_eq!(texts(&f), vec![VERIFY_FAILED]);
        assert_eq!(f[0].severity, Severity::P1);
    }

    #[test]
    fn failed_verify_without_a_claim_is_silent() {
        let f = claim_findings("I changed the parser.", &facts(&["ls"], Some(false)));
        assert!(f.is_empty());
    }

    #[test]
    fn conditional_plans_and_requests_are_not_claims() {
        let answers = [
            "Tests should pass after this change.",
            "Please run the tests.",
            "If the tests pass, you can commit.",
            "I will make sure all tests pass before merging.",
            "The tests did not pass.",
            "I could not compile the crate.",
            "Run cargo build to compile.",
        ];
        for answer in answers {
            let f = claim_findings(answer, &facts(&[], Some(false)));
            assert!(
                f.is_empty(),
                "answer should make no claim: {answer} -> {:?}",
                texts(&f)
            );
        }
    }

    #[test]
    fn no_claim_means_no_findings() {
        let f = claim_findings("Updated the README.", &facts(&[], None));
        assert!(f.is_empty());
    }

    #[test]
    fn merge_appends_claims_and_flips_pass_only_on_p1() {
        let p2 = claim_findings("All tests pass.", &facts(&[], None));
        let (verdict, findings) = merge_claims(ReviewVerdict::Pass, Vec::new(), p2);
        assert_eq!(verdict, ReviewVerdict::Pass);
        assert_eq!(findings.len(), 1);
        let p1 = claim_findings("All tests pass.", &facts(&[], Some(false)));
        let (verdict, _) = merge_claims(ReviewVerdict::Pass, Vec::new(), p1.clone());
        assert_eq!(verdict, ReviewVerdict::NeedsFix);
        let (verdict, _) = merge_claims(ReviewVerdict::Skipped, Vec::new(), p1);
        assert_eq!(verdict, ReviewVerdict::Skipped);
    }
}
