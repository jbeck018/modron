//! [`brand`] -- the app's design system, ported from iron-rain
//! (howlerops/iron-rain, the Oculus design system) on 2026-11-11:
//! the gold-on-black palette (`assets/oculus-theme.json`, both schemes),
//! the brand typefaces (DM Sans for UI, Instrument Serif for display
//! text; OFL, bundled with their licenses in `assets/fonts/`), and the
//! one type scale every `text_size` in the app must come from.
//!
//! Fonts: iron-rain itself ships no font files (its Swift app uses the
//! system faces; its site stacks DM Sans + Instrument Serif), so the
//! TTFs are fetched from Google Fonts and committed here. Registration
//! happens once at startup; after that the theme's `font.family` picks
//! DM Sans up by name and [`DISPLAY_FONT`] is used directly.

use std::borrow::Cow;
use std::rc::Rc;

use gpui_kit::component::theme::{Theme, ThemeConfig, ThemeMode, ThemeSet};

/// The UI typeface (iron-rain site `--font-sans`).
pub const SANS_FONT: &str = "DM Sans";
/// The display/wordmark typeface (iron-rain site `--font-display`).
pub const DISPLAY_FONT: &str = "Instrument Serif";

/// Bundled font bytes, registered into GPUI's text system at startup.
const FONT_FILES: &[&[u8]] = &[
    include_bytes!("../assets/fonts/DMSans-Regular.ttf"),
    include_bytes!("../assets/fonts/DMSans-Medium.ttf"),
    include_bytes!("../assets/fonts/DMSans-Bold.ttf"),
    include_bytes!("../assets/fonts/InstrumentSerif-Regular.ttf"),
    include_bytes!("../assets/fonts/InstrumentSerif-Italic.ttf"),
];

/// The one type scale for the whole app (iron-rain's discipline: no
/// hand-picked magic sizes). Every `text_size` call must use one of
/// these constants so hierarchy reads consistently.
pub mod text {
    /// Timestamps, badges, tiny labels.
    pub const XS: f32 = 11.0;
    /// Secondary / meta text.
    pub const SM: f32 = 12.0;
    /// Body: transcript, session rows, inputs.
    pub const BASE: f32 = 13.0;
    /// Titles, section headers.
    pub const LG: f32 = 14.0;
    /// Panel titles.
    pub const XL: f32 = 16.0;
    /// The greeting display line.
    pub const DISPLAY: f32 = 28.0;
}

/// Register the bundled fonts and swap the app's light/dark themes for
/// the Oculus palette. Call once at startup, after `gpui_kit::init`;
/// every later `Theme::change` (mode toggle, OS appearance switch)
/// reloads from these configs.
pub fn install(cx: &mut gpui_kit::App) {
    if let Err(e) = cx.text_system().add_fonts(
        FONT_FILES
            .iter()
            .map(|bytes| Cow::Borrowed(*bytes))
            .collect::<Vec<_>>(),
    ) {
        eprintln!("brand fonts failed to load: {e}");
    }

    let set: ThemeSet = serde_json::from_str(include_str!("../assets/oculus-theme.json"))
        .expect("oculus-theme.json parses");
    let configs: Vec<Rc<ThemeConfig>> = set.themes.into_iter().map(Rc::new).collect();
    Theme::update(cx, |theme| {
        for config in configs {
            match config.mode {
                ThemeMode::Light => theme.light_theme = config,
                ThemeMode::Dark => theme.dark_theme = config,
            }
        }
    });
    // Reload the current mode so the Oculus palette takes effect
    // immediately (Theme::change re-reads light_theme/dark_theme).
    let mode = ThemeMode::from(cx.window_appearance());
    Theme::change(mode, None, cx);
}
