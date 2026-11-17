//! `modron mcp add <preset>`: writes a built-in MCP server entry into the user
//! config (`config.toml`, see `config::config_path`).
//!
//! A new entry is appended as a text block, so the user's comments and layout
//! stay as they were. The result is parsed again before anything is written:
//! a layout the appended block would break (for example an inline `mcp = {...}`
//! table) is refused and the file is left alone. An existing entry is never
//! replaced without `--force`; with `--force` the file is re-serialized, which
//! drops comments.

use anyhow::anyhow;
use std::path::Path;
use toml::{Table, Value};

/// A built-in server entry: the command and args written to config.
#[derive(Debug, PartialEq, Eq)]
pub struct Preset {
    pub name: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub about: &'static str,
}

/// Every preset `modron mcp add` knows.
pub const PRESETS: &[Preset] = &[Preset {
    name: "playwright",
    command: "npx",
    args: &["-y", "@playwright/mcp@latest"],
    about: "browser automation through the Playwright MCP server (needs Node.js; npx fetches it on first run)",
}];

/// The preset named `name`, if any.
pub fn find(name: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.name == name)
}

/// What [`with_preset`] changed.
#[derive(Debug, PartialEq, Eq)]
pub enum Written {
    Added,
    Replaced,
}

/// Returns the config text with the preset's server entry in place. Pure:
/// the caller reads and writes the file. `Err` means nothing should be
/// written.
pub fn with_preset(
    existing: &str,
    preset: &Preset,
    force: bool,
) -> Result<(String, Written), String> {
    let table: Table = if existing.trim().is_empty() {
        Table::new()
    } else {
        existing
            .parse()
            .map_err(|e| format!("config.toml is not valid TOML, so it was not changed: {e}"))?
    };
    let exists = table
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(|s| s.get(preset.name))
        .is_some();
    if exists && !force {
        return Err(format!(
            "`mcp.servers.{}` already exists in config.toml; pass --force to replace it",
            preset.name
        ));
    }

    if !exists {
        let mut text = existing.trim_end().to_string();
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&preset_block(preset));
        verify(&text, preset)?;
        return Ok((text, Written::Added));
    }

    let mut table = table;
    let servers = table
        .get_mut("mcp")
        .and_then(Value::as_table_mut)
        .and_then(|m| m.get_mut("servers"))
        .and_then(Value::as_table_mut)
        .ok_or_else(|| "`mcp.servers` is not a table in config.toml".to_string())?;
    servers.insert(preset.name.to_string(), Value::Table(preset_table(preset)));
    let text =
        toml::to_string_pretty(&table).map_err(|e| format!("cannot serialize config.toml: {e}"))?;
    verify(&text, preset)?;
    Ok((text, Written::Replaced))
}

/// The text block appended for a new entry.
fn preset_block(preset: &Preset) -> String {
    let args: Vec<String> = preset.args.iter().map(|a| format!("\"{a}\"")).collect();
    format!(
        "[mcp.servers.{}]\ncommand = \"{}\"\nargs = [{}]\n",
        preset.name,
        preset.command,
        args.join(", ")
    )
}

fn preset_table(preset: &Preset) -> Table {
    let mut entry = Table::new();
    entry.insert("command".into(), Value::String(preset.command.into()));
    entry.insert(
        "args".into(),
        Value::Array(
            preset
                .args
                .iter()
                .map(|a| Value::String((*a).into()))
                .collect(),
        ),
    );
    entry
}

/// The rendered text must parse and carry the entry exactly as the preset
/// names it. Anything else means the layout was not safe to write to.
fn verify(text: &str, preset: &Preset) -> Result<(), String> {
    let table: Table = text
        .parse()
        .map_err(|e| format!("the new config would not parse, so it was not written: {e}"))?;
    let entry = table
        .get("mcp")
        .and_then(|m| m.get("servers"))
        .and_then(|s| s.get(preset.name))
        .and_then(Value::as_table)
        .ok_or_else(|| {
            "the new config is missing the server entry, so it was not written".to_string()
        })?;
    let command = entry.get("command").and_then(Value::as_str);
    let args: Option<Vec<&str>> = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect());
    if command != Some(preset.command) || args.as_deref() != Some(preset.args) {
        return Err("the new config entry does not match the preset, so it was not written".into());
    }
    Ok(())
}

/// Adds `preset` to the config file at `path` and describes what was written.
pub fn add_preset(path: &Path, name: &str, force: bool) -> anyhow::Result<String> {
    let preset = find(name).ok_or_else(|| {
        let known: Vec<&str> = PRESETS.iter().map(|p| p.name).collect();
        anyhow!("unknown preset `{name}`; available: {}", known.join(", "))
    })?;
    let existing = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(anyhow!("cannot read {}: {e}", path.display())),
    };
    let (text, written) = with_preset(&existing, preset, force).map_err(|m| anyhow!(m))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &text)?;
    let verb = match written {
        Written::Added => "wrote",
        Written::Replaced => "replaced",
    };
    let mut out = format!(
        "{verb} [mcp.servers.{}] in {}\n  command: {} {}\n  {}\n",
        preset.name,
        path.display(),
        preset.command,
        preset.args.join(" "),
        preset.about
    );
    if written == Written::Replaced {
        out.push_str("  comments in config.toml were not kept\n");
    }
    out.push_str("restart modron to load it");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn playwright() -> &'static Preset {
        find("playwright").expect("preset")
    }

    #[test]
    fn empty_config_gets_the_entry_exactly() {
        let (text, written) = with_preset("", playwright(), false).expect("add");
        assert_eq!(written, Written::Added);
        assert_eq!(
            text,
            "[mcp.servers.playwright]\ncommand = \"npx\"\nargs = [\"-y\", \"@playwright/mcp@latest\"]\n"
        );
    }

    #[test]
    fn existing_content_and_comments_are_kept_on_add() {
        let existing = "# my settings\ndefault_provider = \"zai\"\n\n[mcp.servers.web]\nurl = \"https://example.test/mcp\"\n";
        let (text, written) = with_preset(existing, playwright(), false).expect("add");
        assert_eq!(written, Written::Added);
        assert!(text.starts_with(existing.trim_end()), "{text}");
        assert!(text.contains("# my settings"), "{text}");
        let parsed: Table = text.parse().expect("parses");
        assert_eq!(parsed["default_provider"].as_str(), Some("zai"));
        assert!(parsed["mcp"]["servers"].get("web").is_some());
        assert!(parsed["mcp"]["servers"].get("playwright").is_some());
    }

    #[test]
    fn existing_entry_is_refused_without_force() {
        let existing = "[mcp.servers.playwright]\ncommand = \"node\"\nargs = [\"x.js\"]\n";
        let err = with_preset(existing, playwright(), false).expect_err("refused");
        assert!(err.contains("--force"), "{err}");
    }

    #[test]
    fn force_replaces_the_entry_and_keeps_other_servers() {
        let existing = "[mcp.servers.playwright]\ncommand = \"node\"\nargs = [\"x.js\"]\n\n[mcp.servers.web]\nurl = \"https://example.test/mcp\"\n";
        let (text, written) = with_preset(existing, playwright(), true).expect("replace");
        assert_eq!(written, Written::Replaced);
        let parsed: Table = text.parse().expect("parses");
        let entry = &parsed["mcp"]["servers"]["playwright"];
        assert_eq!(entry["command"].as_str(), Some("npx"));
        assert_eq!(
            entry["args"].as_array().map(Vec::len),
            Some(2),
            "args replaced: {text}"
        );
        assert_eq!(
            parsed["mcp"]["servers"]["web"]["url"].as_str(),
            Some("https://example.test/mcp")
        );
    }

    #[test]
    fn malformed_config_is_refused() {
        let err = with_preset("[mcp.servers\nbroken", playwright(), false).expect_err("refused");
        assert!(err.contains("not valid TOML"), "{err}");
    }

    #[test]
    fn inline_mcp_table_is_refused_rather_than_corrupted() {
        let existing = "mcp = { servers = {} }\n";
        assert!(with_preset(existing, playwright(), false).is_err());
    }

    #[test]
    fn add_preset_writes_a_new_file_and_names_what_it_wrote() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("config.toml");
        let out = add_preset(&path, "playwright", false).expect("add");
        assert!(out.starts_with("wrote [mcp.servers.playwright]"), "{out}");
        assert!(out.contains("npx -y @playwright/mcp@latest"), "{out}");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("command = \"npx\""), "{text}");
    }

    #[test]
    fn add_preset_without_force_leaves_the_file_byte_identical() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let original = "[mcp.servers.playwright]\ncommand = \"my-own\"\n";
        std::fs::write(&path, original).expect("seed");
        let err = add_preset(&path, "playwright", false).expect_err("refused");
        assert!(err.to_string().contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), original);
    }

    #[test]
    fn unknown_preset_names_the_available_ones() {
        let dir = tempdir().expect("tempdir");
        let err = add_preset(&dir.path().join("c.toml"), "nope", false).expect_err("unknown");
        assert!(err.to_string().contains("available: playwright"), "{err}");
        assert!(!dir.path().join("c.toml").exists());
    }
}
