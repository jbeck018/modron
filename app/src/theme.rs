//! The app's theme choice: Oculus dark, Oculus light, or "system" (follow the
//! terminal's or browser's appearance). The Oculus tokens already carry light
//! and dark values (`theme/oculus.json`), so switching is one NSH call,
//! `set_color_scheme`. The choice is remembered through a [`Store`] the host
//! installs (the terminal's config file, the web's localStorage). With no
//! store installed nothing is saved, so tests touch no real config. A missing
//! or unreadable saved value means "system".

use std::sync::{Arc, Mutex};

use modron_mobile_core::view::{THEME_DARK_NAME, THEME_LIGHT_NAME, THEME_SYSTEM_NAME};
use nsh_core::prelude::{set_color_scheme, Appearance};

/// What the user picked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Choice {
    Dark,
    Light,
    #[default]
    System,
}

/// The picker's rows, in order: dark, light, system.
pub const CHOICES: [Choice; 3] = [Choice::Dark, Choice::Light, Choice::System];

impl Choice {
    /// The shared name the picker, the Settings row and the saved value use.
    pub fn name(self) -> &'static str {
        match self {
            Choice::Dark => THEME_DARK_NAME,
            Choice::Light => THEME_LIGHT_NAME,
            Choice::System => THEME_SYSTEM_NAME,
        }
    }

    /// The appearance NSH draws with for this choice.
    pub fn appearance(self) -> Appearance {
        match self {
            Choice::Dark => Appearance::Dark,
            Choice::Light => Appearance::Light,
            Choice::System => Appearance::System,
        }
    }

    /// Reads a name (case-insensitive). `dark` and `light` are accepted as
    /// short forms of the Oculus names.
    pub fn parse(text: &str) -> Option<Choice> {
        match text.trim().to_lowercase().as_str() {
            THEME_DARK_NAME | "dark" => Some(Choice::Dark),
            THEME_LIGHT_NAME | "light" => Some(Choice::Light),
            THEME_SYSTEM_NAME => Some(Choice::System),
            _ => None,
        }
    }
}

/// Where a saved choice lives. `read` returns the saved name, if any.
pub trait Store: Send + Sync {
    fn read(&self) -> Option<String>;
    fn write(&self, name: &str) -> Result<(), String>;
}

static STORE: Mutex<Option<Arc<dyn Store>>> = Mutex::new(None);

/// Installs the store the host saves to (the terminal's config file, the
/// web's localStorage). Until one is installed, choices are not saved.
pub fn install_store(store: Arc<dyn Store>) {
    if let Ok(mut slot) = STORE.lock() {
        *slot = Some(store);
    }
}

/// The choice saved in `store`; [`Choice::System`] when none is saved or the
/// value is not a known name.
pub fn saved(store: &dyn Store) -> Choice {
    store.read().as_deref().and_then(Choice::parse).unwrap_or_default()
}

/// The choice saved in the installed store (system when none is installed).
pub fn saved_installed() -> Choice {
    let store = STORE.lock().ok().and_then(|slot| slot.clone());
    store.map(|store| saved(store.as_ref())).unwrap_or_default()
}

/// Draws the app with `choice`, and saves it when `remember` is set (a
/// choice the user made; a restore on startup is not saved). A failed save
/// keeps the choice for this run only.
pub fn change(choice: Choice, remember: bool) {
    set_color_scheme(choice.appearance());
    if !remember {
        return;
    }
    let store = STORE.lock().ok().and_then(|slot| slot.clone());
    if let Some(store) = store {
        let _ = store.write(choice.name());
    }
}

/// The terminal's store: `theme = "<name>"` in `app.toml` under the user's
/// config directory (`$XDG_CONFIG_HOME/modron`, else `~/.config/modron`).
/// The file is created only when the user picks a theme; other lines in it
/// are kept.
#[cfg(not(target_arch = "wasm32"))]
pub struct FileStore {
    pub path: std::path::PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
impl FileStore {
    /// The default file, or None when neither `XDG_CONFIG_HOME` nor `HOME`
    /// is set.
    pub fn default_path() -> Option<std::path::PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
            })?;
        Some(base.join("modron").join("app.toml"))
    }
}

/// The value of a `theme = "name"` line, the one key the file holds.
#[cfg(not(target_arch = "wasm32"))]
fn theme_line_value(line: &str) -> Option<String> {
    let (key, value) = line.split_once('=')?;
    (key.trim() == "theme").then(|| value.trim().trim_matches('"').to_string())
}

#[cfg(not(target_arch = "wasm32"))]
impl Store for FileStore {
    fn read(&self) -> Option<String> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        text.lines().find_map(theme_line_value)
    }

    fn write(&self, name: &str) -> Result<(), String> {
        let mut lines: Vec<String> = std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter(|line| theme_line_value(line).is_none())
            .map(str::to_string)
            .collect();
        lines.push(format!("theme = \"{name}\""));
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&self.path, lines.join("\n") + "\n").map_err(|e| e.to_string())
    }
}

/// The web's store: the page's localStorage (`modron.theme`). Every access
/// may fail (private windows, blocked storage): a failure reads as no value.
#[cfg(target_arch = "wasm32")]
pub struct LocalStore;

#[cfg(target_arch = "wasm32")]
const LOCAL_KEY: &str = "modron.theme";

#[cfg(target_arch = "wasm32")]
impl Store for LocalStore {
    fn read(&self) -> Option<String> {
        web_sys::window()?.local_storage().ok()??.get_item(LOCAL_KEY).ok()?
    }

    fn write(&self, name: &str) -> Result<(), String> {
        let storage = web_sys::window()
            .and_then(|w| w.local_storage().ok().flatten())
            .ok_or_else(|| "no local storage".to_string())?;
        storage.set_item(LOCAL_KEY, name).map_err(|_| "local storage refused".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory store: no file or page storage is touched.
    struct MemStore(Mutex<Option<String>>);

    impl Store for MemStore {
        fn read(&self) -> Option<String> {
            self.0.lock().ok()?.clone()
        }
        fn write(&self, name: &str) -> Result<(), String> {
            *self.0.lock().map_err(|e| e.to_string())? = Some(name.to_string());
            Ok(())
        }
    }

    #[test]
    fn names_round_trip_and_the_short_forms_parse() {
        for choice in CHOICES {
            assert_eq!(Choice::parse(choice.name()), Some(choice));
        }
        assert_eq!(Choice::parse(" Dark "), Some(Choice::Dark));
        assert_eq!(Choice::parse("light"), Some(Choice::Light));
        assert_eq!(Choice::parse("dracula"), None);
    }

    #[test]
    fn each_choice_maps_to_its_appearance() {
        assert_eq!(Choice::Dark.appearance(), Appearance::Dark);
        assert_eq!(Choice::Light.appearance(), Appearance::Light);
        assert_eq!(Choice::System.appearance(), Appearance::System);
    }

    #[test]
    fn a_missing_or_unknown_saved_value_means_system() {
        assert_eq!(saved(&MemStore(Mutex::new(None))), Choice::System);
        assert_eq!(saved(&MemStore(Mutex::new(Some("dracula".into())))), Choice::System);
        assert_eq!(saved(&MemStore(Mutex::new(Some("oculus light".into())))), Choice::Light);
    }

    #[test]
    fn the_saved_name_is_what_the_store_holds_after_a_choice() {
        let store = MemStore(Mutex::new(None));
        store.write(Choice::Dark.name()).expect("write");
        assert_eq!(saved(&store), Choice::Dark);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_file_store_keeps_other_lines_and_reads_back_the_choice() {
        let dir = std::env::temp_dir().join(format!("modron-theme-test-{}", std::process::id()));
        let path = dir.join("app.toml");
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileStore { path: path.clone() };
        assert!(!path.exists(), "nothing is created before a choice");
        assert_eq!(saved(&store), Choice::System);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(&path, "# keep me\nwidth = 3\n").expect("seed");
        store.write(Choice::Light.name()).expect("write");
        store.write(Choice::Dark.name()).expect("rewrite");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "# keep me\nwidth = 3\ntheme = \"oculus dark\"\n");
        assert_eq!(saved(&store), Choice::Dark);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
