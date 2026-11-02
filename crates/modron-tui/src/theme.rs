//! Theme system (Phase 14, dirge pattern): JSON theme files at
//! `~/.config/modron/themes/<name>.json`, plus built-ins that always work.
//! Switch live in the TUI with `/theme <name>`; `/theme` lists what exists.
//!
//! Colors are hex strings (`#ff9f0a`) mapped to ratatui `Color::Rgb`;
//! invalid hex falls back to the built-in default for that slot — a bad
//! theme file must never break the TUI.

use ratatui::style::Color;

/// One theme: the six color slots the TUI uses.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Theme {
    pub name: String,
    pub accent: String,
    pub dim: String,
    pub tool: String,
    pub warning: String,
    pub error: String,
    pub success: String,
}

impl Default for Theme {
    fn default() -> Self {
        Self::builtin("modron")
    }
}

/// Resolved colors ready for rendering.
#[derive(Clone, Copy, Debug)]
pub struct Resolved {
    pub accent: Color,
    pub dim: Color,
    pub tool: Color,
    pub warning: Color,
    pub error: Color,
    pub success: Color,
}

impl Theme {
    /// The shipped themes (dirge-style: a warm default, a cool dark, a
    /// grayscale for terminals that hate color).
    pub fn builtin(name: &str) -> Theme {
        match name {
            "midnight" => Theme {
                name: "midnight".into(),
                accent: "#7aa2f7".into(),
                dim: "#565f89".into(),
                tool: "#9ece6a".into(),
                warning: "#e0af68".into(),
                error: "#f7768e".into(),
                success: "#9ece6a".into(),
            },
            "mono" => Theme {
                name: "mono".into(),
                accent: "#ffffff".into(),
                dim: "#888888".into(),
                tool: "#aaaaaa".into(),
                warning: "#cccccc".into(),
                error: "#ffffff".into(),
                success: "#aaaaaa".into(),
            },
            _ => Theme {
                name: "modron".into(),
                accent: "#ff9f0a".into(),
                dim: "#585858".into(),
                tool: "#22ccdd".into(),
                warning: "#e5c07b".into(),
                error: "#e06c75".into(),
                success: "#98c379".into(),
            },
        }
    }

    pub fn builtin_names() -> Vec<&'static str> {
        vec!["modron", "midnight", "mono"]
    }

    /// Resolve to ratatui colors; invalid hex falls back to the default
    /// theme's slot (never break rendering over cosmetics).
    pub fn resolved(&self) -> Resolved {
        let d = Theme::builtin("modron");
        Resolved {
            accent: parse_hex(&self.accent).unwrap_or_else(|| parse_hex(&d.accent).unwrap()),
            dim: parse_hex(&self.dim).unwrap_or_else(|| parse_hex(&d.dim).unwrap()),
            tool: parse_hex(&self.tool).unwrap_or_else(|| parse_hex(&d.tool).unwrap()),
            warning: parse_hex(&self.warning).unwrap_or_else(|| parse_hex(&d.warning).unwrap()),
            error: parse_hex(&self.error).unwrap_or_else(|| parse_hex(&d.error).unwrap()),
            success: parse_hex(&self.success).unwrap_or_else(|| parse_hex(&d.success).unwrap()),
        }
    }

    /// Load `<themes_dir>/<name>.json`, falling back to the builtin of the
    /// same name (unknown → None).
    pub fn load(themes_dir: &std::path::Path, name: &str) -> Option<Theme> {
        let path = themes_dir.join(format!("{name}.json"));
        let raw = std::fs::read_to_string(path).ok()?;
        let mut theme: Theme = serde_json::from_str(&raw).ok()?;
        theme.name = name.to_string();
        Some(theme)
    }

    /// Every available theme name: built-ins plus files in `themes_dir`.
    pub fn available(themes_dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = Self::builtin_names()
            .into_iter()
            .map(String::from)
            .collect();
        if let Ok(entries) = std::fs::read_dir(themes_dir) {
            for entry in entries.flatten() {
                if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                    if !names.iter().any(|n| n == stem) {
                        names.push(stem.to_string());
                    }
                }
            }
        }
        names.sort();
        names
    }
}

/// `#rrggbb` → `Color::Rgb`.
fn parse_hex(hex: &str) -> Option<Color> {
    let hex = hex.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_resolve_and_default_is_warm_orange() {
        let t = Theme::default();
        assert_eq!(t.name, "modron");
        let r = t.resolved();
        assert_eq!(r.accent, Color::Rgb(255, 159, 10));
        assert_eq!(
            Theme::builtin("midnight").resolved().accent,
            Color::Rgb(122, 162, 247)
        );
    }

    #[test]
    fn invalid_hex_falls_back_never_breaks() {
        let mut t = Theme::default();
        t.accent = "not-a-color".into();
        t.dim = "#123".into(); // wrong length
        let r = t.resolved();
        assert_eq!(r.accent, Color::Rgb(255, 159, 10), "bad accent → default");
        assert_eq!(r.dim, Color::Rgb(88, 88, 88), "bad dim → default");
    }

    #[test]
    fn loads_from_json_and_lists_available() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            dir.path().join("solar.json"),
            r##"{"accent":"#ffcc00","dim":"#666666","tool":"#00cc88","warning":"#ffaa00","error":"#ff4444","success":"#00ff88","name":"solar"}"##,
        )
        .unwrap();
        let t = Theme::load(dir.path(), "solar").unwrap();
        assert_eq!(t.resolved().accent, Color::Rgb(255, 204, 0));
        assert!(Theme::load(dir.path(), "nope").is_none());
        let names = Theme::available(dir.path());
        assert!(names.contains(&"solar".to_string()));
        assert!(names.contains(&"modron".to_string()), "builtins listed");
    }
}
