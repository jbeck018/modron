//! Permission rules: a pure matcher for `allow`, `ask` and `deny` rules.
//!
//! Rule text is `tool` (every call of that tool) or `tool(pattern)`, e.g.
//! `bash(cargo test*)`, `edit(src/**)`, `read(*.md)`. A call is a tool name
//! plus JSON args. The subject is `command` for `bash`, otherwise the first
//! string arg among `path`, `file_path`, `pattern`. Globs: `*` (no `/` for path
//! tools, any char for bash), `**` (anything), `?` (one char), else literal.
//! A leading `./` is stripped from paths. Allow patterns never match a bash
//! command with a compound operator; deny and ask patterns also match each
//! `;` / `&&` / `||` / `|` segment. Precedence: any deny, else any ask, else
//! any allow, else no verdict.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleEffect {
    Allow,
    Ask,
    Deny,
}

impl RuleEffect {
    fn as_str(self) -> &'static str {
        match self {
            RuleEffect::Allow => "allow",
            RuleEffect::Ask => "ask",
            RuleEffect::Deny => "deny",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub effect: RuleEffect,
    pub tool: String,
    pub pattern: Option<String>,
}

impl Rule {
    pub fn parse(effect: RuleEffect, spec: &str) -> Result<Rule, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("empty rule".to_string());
        }
        let (tool, pattern) = match spec.find('(') {
            None => {
                if spec.contains(')') {
                    return Err(format!("unbalanced parentheses in `{spec}`"));
                }
                (spec, None)
            }
            Some(open) => {
                let tool = &spec[..open];
                let Some(inner) = spec[open + 1..].strip_suffix(')') else {
                    return Err(format!("unbalanced parentheses in `{spec}`"));
                };
                if !parens_balanced(inner) {
                    return Err(format!("unbalanced parentheses in `{spec}`"));
                }
                let pattern = inner.trim();
                if pattern.is_empty() {
                    return Err(format!("empty pattern in `{spec}`"));
                }
                (tool, Some(pattern.to_string()))
            }
        };
        let tool = tool.trim();
        if tool.is_empty()
            || !tool
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!("invalid tool name `{tool}` in `{spec}`"));
        }
        Ok(Rule {
            effect,
            tool: tool.to_string(),
            pattern,
        })
    }

    /// The rule as text, e.g. `deny bash(rm -rf*)` or `allow read`.
    pub fn text(&self) -> String {
        match &self.pattern {
            None => format!("{} {}", self.effect.as_str(), self.tool),
            Some(p) => format!("{} {}({})", self.effect.as_str(), self.tool, p),
        }
    }
}

fn parens_balanced(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleSet {
    rules: Vec<Rule>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub effect: RuleEffect,
    /// The matching rule as text (see `Rule::text`).
    pub rule: String,
}

impl RuleSet {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules }
    }

    pub fn evaluate(&self, tool: &str, args: &Value) -> Option<Verdict> {
        let matched: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|r| matches(r, tool, args).is_ok())
            .collect();
        for effect in [RuleEffect::Deny, RuleEffect::Ask, RuleEffect::Allow] {
            if let Some(rule) = matched.iter().find(|r| r.effect == effect) {
                return Some(Verdict {
                    effect,
                    rule: rule.text(),
                });
            }
        }
        None
    }

    pub fn explain(&self, tool: &str, args: &Value) -> Vec<String> {
        let mut lines: Vec<String> = self
            .rules
            .iter()
            .map(|rule| match matches(rule, tool, args) {
                Ok(()) => format!("{}: matched", rule.text()),
                Err(reason) => format!("{}: no match ({reason})", rule.text()),
            })
            .collect();
        let result = self
            .evaluate(tool, args)
            .map_or("no rule", |v| v.effect.as_str());
        lines.push(format!("result: {result}"));
        lines
    }
}

/// Ok when the rule matches the call; Err carries the reason it does not.
fn matches(rule: &Rule, tool: &str, args: &Value) -> Result<(), String> {
    if rule.tool != tool {
        return Err("different tool".to_string());
    }
    let Some(pattern) = rule.pattern.as_deref() else {
        return Ok(());
    };
    let is_bash = tool == "bash";
    let Some(raw) = subject(tool, args) else {
        return Err("no subject argument".to_string());
    };

    let (pattern, candidates): (&str, Vec<&str>) = if is_bash {
        if rule.effect == RuleEffect::Allow {
            if has_compound(raw) {
                return Err("compound command never matches an allow pattern".to_string());
            }
            (pattern, vec![raw.trim()])
        } else {
            let mut c = vec![raw.trim()];
            c.extend(split_segments(raw));
            (pattern, c)
        }
    } else {
        // `src/**` must not allow `src/../secret`: a path with a parent
        // component never matches an allow pattern.
        if rule.effect == RuleEffect::Allow && raw.split(['/', '\\']).any(|part| part == "..") {
            return Err("path with `..` never matches an allow pattern".to_string());
        }
        (
            pattern.strip_prefix("./").unwrap_or(pattern),
            vec![raw.strip_prefix("./").unwrap_or(raw)],
        )
    };

    if candidates.iter().any(|c| glob_match(pattern, c, !is_bash)) {
        Ok(())
    } else {
        Err(format!("pattern does not match `{raw}`"))
    }
}

fn subject<'a>(tool: &str, args: &'a Value) -> Option<&'a str> {
    if tool == "bash" {
        args.get("command")?.as_str()
    } else {
        ["path", "file_path", "pattern"]
            .iter()
            .find_map(|k| args.get(*k).and_then(Value::as_str))
    }
}

fn has_compound(cmd: &str) -> bool {
    // A lone `&` (background) chains commands too, so any `&` counts.
    [";", "&", "|", "`", "$(", ">", "<", "\n"]
        .iter()
        .any(|t| cmd.contains(t))
}

/// Splits on `;`, `&&`, `||` and `|`; segments are trimmed, empties dropped.
fn split_segments(cmd: &str) -> Vec<&str> {
    let bytes = cmd.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let sep = match bytes[i] {
            b';' | b'|' | b'&' | b'\n' => 1,
            _ => 0,
        };
        if sep > 0 {
            out.push(&cmd[start..i]);
            i += sep;
            start = i;
        } else {
            i += 1;
        }
    }
    out.push(&cmd[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tok {
    Lit(char),
    One,
    Star,
    Globstar,
}

/// Whole-string glob match. `slash_limits_star` is true for path tools,
/// where `*` may not cross `/`.
fn glob_match(pattern: &str, text: &str, slash_limits_star: bool) -> bool {
    let mut toks = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                toks.push(Tok::Globstar);
            }
            '*' => toks.push(Tok::Star),
            '?' => toks.push(Tok::One),
            other => toks.push(Tok::Lit(other)),
        }
    }
    let text: Vec<char> = text.chars().collect();
    let (p, n) = (toks.len(), text.len());
    // ok[i][j]: toks[i..] matches text[j..].
    let mut ok = vec![vec![false; n + 1]; p + 1];
    ok[p][n] = true;
    for i in (0..p).rev() {
        for j in (0..=n).rev() {
            ok[i][j] = match toks[i] {
                Tok::Lit(c) => j < n && text[j] == c && ok[i + 1][j + 1],
                Tok::One => j < n && (!slash_limits_star || text[j] != '/') && ok[i + 1][j + 1],
                Tok::Star => {
                    ok[i + 1][j]
                        || (j < n && (!slash_limits_star || text[j] != '/') && ok[i][j + 1])
                }
                Tok::Globstar => ok[i + 1][j] || (j < n && ok[i][j + 1]),
            };
        }
    }
    ok[0][0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(effect: RuleEffect, spec: &str) -> Rule {
        Rule::parse(effect, spec).unwrap()
    }

    fn set(rules: Vec<Rule>) -> RuleSet {
        RuleSet::new(rules)
    }

    #[test]
    fn parse_accepts_tool_only_and_pattern_forms() {
        let r = rule(RuleEffect::Allow, "read");
        assert_eq!(r.tool, "read");
        assert_eq!(r.pattern, None);
        let r = rule(RuleEffect::Deny, "bash(rm -rf*)");
        assert_eq!(r.tool, "bash");
        assert_eq!(r.pattern.as_deref(), Some("rm -rf*"));
        assert_eq!(
            rule(RuleEffect::Ask, "mcp__fs_read(*.md)").tool,
            "mcp__fs_read"
        );
        assert_eq!(
            rule(RuleEffect::Allow, "bash(echo (hi))")
                .pattern
                .as_deref(),
            Some("echo (hi)")
        );
    }

    #[test]
    fn parse_rejects_bad_specs() {
        for bad in [
            "",
            "   ",
            "bash tool",
            "bash.tool",
            "(x)",
            "bash(",
            "bash)",
            "bash()",
            "bash(   )",
            "bash(a))",
            "bash(a(b)",
            "bash(x) y",
        ] {
            assert!(
                Rule::parse(RuleEffect::Allow, bad).is_err(),
                "expected error for {bad:?}"
            );
        }
    }

    #[test]
    fn tool_only_rule_matches_any_args() {
        let rs = set(vec![rule(RuleEffect::Allow, "read")]);
        let v = rs.evaluate("read", &json!({"path": "anything/at/all.txt"}));
        assert_eq!(v.map(|v| v.effect), Some(RuleEffect::Allow));
        assert_eq!(rs.evaluate("write", &json!({"path": "x"})), None);
    }

    #[test]
    fn bash_prefix_glob() {
        let rs = set(vec![rule(RuleEffect::Allow, "bash(cargo test*)")]);
        assert!(rs
            .evaluate("bash", &json!({"command": "cargo test --workspace"}))
            .is_some());
        assert_eq!(
            rs.evaluate("bash", &json!({"command": "cargo build"})),
            None
        );
    }

    #[test]
    fn allow_pattern_refuses_compound_commands() {
        let allow_pat = set(vec![rule(RuleEffect::Allow, "bash(cargo*)")]);
        assert_eq!(
            allow_pat.evaluate("bash", &json!({"command": "cargo test && rm -rf x"})),
            None
        );
        assert_eq!(
            allow_pat.evaluate("bash", &json!({"command": "cargo test > out.txt"})),
            None
        );
        let allow_any = set(vec![rule(RuleEffect::Allow, "bash")]);
        assert!(allow_any
            .evaluate("bash", &json!({"command": "cargo test && rm -rf x"}))
            .is_some());
    }

    #[test]
    fn deny_catches_later_segment() {
        let rs = set(vec![rule(RuleEffect::Deny, "bash(rm -rf*)")]);
        let v = rs
            .evaluate("bash", &json!({"command": "ls && rm -rf /"}))
            .unwrap();
        assert_eq!(v.effect, RuleEffect::Deny);
        assert_eq!(v.rule, "deny bash(rm -rf*)");
        assert!(rs
            .evaluate("bash", &json!({"command": "ls | cat ; echo rm -rf"}))
            .is_none());
        assert!(rs
            .evaluate("bash", &json!({"command": "ls || rm -rf d"}))
            .is_some());
    }

    #[test]
    fn path_star_does_not_cross_slash() {
        let rs = set(vec![rule(RuleEffect::Allow, "read(src/*)")]);
        assert!(rs
            .evaluate("read", &json!({"path": "src/main.rs"}))
            .is_some());
        assert_eq!(rs.evaluate("read", &json!({"path": "src/a/b.rs"})), None);
    }

    #[test]
    fn globstar_crosses_slash() {
        let rs = set(vec![rule(RuleEffect::Allow, "edit(src/**)")]);
        assert!(rs
            .evaluate("edit", &json!({"path": "src/a/b/c.rs"}))
            .is_some());
        let md = set(vec![rule(RuleEffect::Allow, "read(*.md)")]);
        assert!(md.evaluate("read", &json!({"path": "README.md"})).is_some());
        assert_eq!(
            md.evaluate("read", &json!({"path": "docs/README.md"})),
            None
        );
    }

    #[test]
    fn dot_slash_prefix_is_stripped() {
        let rs = set(vec![rule(RuleEffect::Allow, "edit(src/**)")]);
        assert!(rs
            .evaluate("edit", &json!({"path": "./src/lib.rs"}))
            .is_some());
    }

    #[test]
    fn question_mark_matches_one_char() {
        let rs = set(vec![rule(RuleEffect::Allow, "bash(ls ?)")]);
        assert!(rs.evaluate("bash", &json!({"command": "ls x"})).is_some());
        assert_eq!(rs.evaluate("bash", &json!({"command": "ls xy"})), None);
    }

    #[test]
    fn precedence_deny_then_ask_then_allow() {
        let rs = set(vec![
            rule(RuleEffect::Allow, "bash"),
            rule(RuleEffect::Ask, "bash(git push*)"),
            rule(RuleEffect::Deny, "bash(git push --force*)"),
        ]);
        let eff = |c: &str| {
            rs.evaluate("bash", &json!({ "command": c }))
                .map(|v| v.effect)
        };
        assert_eq!(eff("git push --force origin"), Some(RuleEffect::Deny));
        assert_eq!(eff("git push origin"), Some(RuleEffect::Ask));
        assert_eq!(eff("ls"), Some(RuleEffect::Allow));
        assert_eq!(
            set(vec![]).evaluate("bash", &json!({"command": "ls"})),
            None
        );
    }

    #[test]
    fn no_subject_means_no_match() {
        let rs = set(vec![rule(RuleEffect::Allow, "read(*.md)")]);
        assert_eq!(rs.evaluate("read", &json!({})), None);
        let bash = set(vec![rule(RuleEffect::Deny, "bash(rm*)")]);
        assert_eq!(bash.evaluate("bash", &json!({"path": "rm"})), None);
    }

    #[test]
    fn subject_falls_back_through_path_keys() {
        let rs = set(vec![rule(RuleEffect::Allow, "read(*.md)")]);
        assert!(rs.evaluate("read", &json!({"file_path": "a.md"})).is_some());
        assert!(rs.evaluate("grep", &json!({"pattern": "x"})).is_none());
        let g = set(vec![rule(RuleEffect::Deny, "grep(x)")]);
        assert_eq!(
            g.evaluate("grep", &json!({"pattern": "x"}))
                .map(|v| v.effect),
            Some(RuleEffect::Deny)
        );
    }

    #[test]
    fn explain_ends_with_result_line() {
        let rs = set(vec![
            rule(RuleEffect::Allow, "bash"),
            rule(RuleEffect::Deny, "bash(rm -rf*)"),
        ]);
        let lines = rs.explain("bash", &json!({"command": "rm -rf /"}));
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "allow bash: matched");
        assert_eq!(lines[1], "deny bash(rm -rf*): matched");
        assert_eq!(lines[2], "result: deny");

        let lines = rs.explain("read", &json!({"path": "a"}));
        assert_eq!(lines[0], "allow bash: no match (different tool)");
        assert_eq!(lines.last().map(String::as_str), Some("result: no rule"));

        let lines = set(vec![rule(RuleEffect::Allow, "bash(cargo*)")])
            .explain("bash", &json!({"command": "cargo x && y"}));
        assert_eq!(
            lines[0],
            "allow bash(cargo*): no match (compound command never matches an allow pattern)"
        );
        assert_eq!(lines[1], "result: no rule");
    }

    #[test]
    fn lone_ampersand_is_compound_and_splits() {
        let allow = RuleSet::new(vec![
            Rule::parse(RuleEffect::Allow, "bash(cargo test*)").unwrap()
        ]);
        let args = serde_json::json!({"command": "cargo test & rm -rf x"});
        assert!(allow.evaluate("bash", &args).is_none());
        let deny = RuleSet::new(vec![Rule::parse(RuleEffect::Deny, "bash(rm -rf*)").unwrap()]);
        assert_eq!(
            deny.evaluate("bash", &args).unwrap().effect,
            RuleEffect::Deny
        );
    }

    #[test]
    fn parent_components_never_match_an_allow_pattern() {
        let rules = RuleSet::new(vec![Rule::parse(RuleEffect::Allow, "edit(src/**)").unwrap()]);
        let escape = serde_json::json!({"path": "src/../.env"});
        assert!(rules.evaluate("edit", &escape).is_none());
        let inside = serde_json::json!({"path": "src/a/b.rs"});
        assert_eq!(
            rules.evaluate("edit", &inside).unwrap().effect,
            RuleEffect::Allow
        );
    }

    #[test]
    fn mcp_tool_names_parse() {
        assert!(Rule::parse(RuleEffect::Ask, "mcp__Server-1__do_it").is_ok());
    }
}
