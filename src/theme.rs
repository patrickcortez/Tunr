use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

pub const THEMES_FILE_VERSION: u32 = 1;
pub const DEFAULT_THEME_NAME: &str = "tunr-dark";
pub const BUILTIN_THEME_NAMES: [&str; 5] =
    ["tunr-dark", "tunr-light", "rose-pine", "nord", "dracula"];

/// Color token supporting `#RRGGBB` hex strings and standard ANSI color names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColor(pub Color);

impl ThemeColor {
    /// Return the underlying `ratatui::style::Color`.
    pub const fn to_color(self) -> Color {
        self.0
    }

    /// Parse a `#RRGGBB` hex string or case-insensitive ANSI color name.
    pub fn parse(input: &str) -> std::result::Result<Self, String> {
        let trimmed = input.trim();
        if let Some(hex) = trimmed.strip_prefix('#') {
            if hex.len() != 6 || !hex.is_ascii() {
                return Err(format!(
                    "invalid hex color '{input}': expected #RRGGBB"
                ));
            }
            let r = u8::from_str_radix(&hex[0..2], 16)
                .map_err(|_| format!("invalid hex color '{input}': expected #RRGGBB"))?;
            let g = u8::from_str_radix(&hex[2..4], 16)
                .map_err(|_| format!("invalid hex color '{input}': expected #RRGGBB"))?;
            let b = u8::from_str_radix(&hex[4..6], 16)
                .map_err(|_| format!("invalid hex color '{input}': expected #RRGGBB"))?;
            return Ok(Self(Color::Rgb(r, g, b)));
        }

        let normalized: String = trimmed
            .chars()
            .filter(|ch| *ch != '_' && *ch != '-')
            .map(|ch| ch.to_ascii_lowercase())
            .collect();

        let color = match normalized.as_str() {
            "reset" | "default" => Color::Reset,
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "gray" | "grey" => Color::Gray,
            "darkgray" | "darkgrey" => Color::DarkGray,
            "lightred" => Color::LightRed,
            "lightgreen" => Color::LightGreen,
            "lightyellow" => Color::LightYellow,
            "lightblue" => Color::LightBlue,
            "lightmagenta" => Color::LightMagenta,
            "lightcyan" => Color::LightCyan,
            "white" => Color::White,
            _ => {
                return Err(format!(
                    "invalid color '{input}': expected #RRGGBB or ANSI color name"
                ));
            }
        };

        Ok(Self(color))
    }
}

impl From<ThemeColor> for Color {
    fn from(value: ThemeColor) -> Self {
        value.0
    }
}

impl From<Color> for ThemeColor {
    fn from(value: Color) -> Self {
        Self(value)
    }
}

impl FromStr for ThemeColor {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl fmt::Display for ThemeColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Color::Reset => f.write_str("reset"),
            Color::Black => f.write_str("black"),
            Color::Red => f.write_str("red"),
            Color::Green => f.write_str("green"),
            Color::Yellow => f.write_str("yellow"),
            Color::Blue => f.write_str("blue"),
            Color::Magenta => f.write_str("magenta"),
            Color::Cyan => f.write_str("cyan"),
            Color::Gray => f.write_str("gray"),
            Color::DarkGray => f.write_str("dark_gray"),
            Color::LightRed => f.write_str("light_red"),
            Color::LightGreen => f.write_str("light_green"),
            Color::LightYellow => f.write_str("light_yellow"),
            Color::LightBlue => f.write_str("light_blue"),
            Color::LightMagenta => f.write_str("light_magenta"),
            Color::LightCyan => f.write_str("light_cyan"),
            Color::White => f.write_str("white"),
            Color::Rgb(r, g, b) => write!(f, "#{r:02x}{g:02x}{b:02x}"),
            Color::Indexed(idx) => write!(f, "indexed({idx})"),
        }
    }
}

impl Serialize for ThemeColor {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ThemeColor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(de::Error::custom)
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> ThemeColor {
    ThemeColor(Color::Rgb(r, g, b))
}

const fn ansi(color: Color) -> ThemeColor {
    ThemeColor(color)
}

/// Complete resolved semantic color palette used across all Tunr TUI views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Theme {
    pub bg: ThemeColor,
    pub surface: ThemeColor,
    pub fg: ThemeColor,
    pub fg_emphasis: ThemeColor,
    pub fg_secondary: ThemeColor,
    pub fg_muted: ThemeColor,
    pub border: ThemeColor,
    pub border_focused: ThemeColor,
    pub accent: ThemeColor,
    pub accent_fg: ThemeColor,
    pub accent_secondary: ThemeColor,
    pub selection_bg: ThemeColor,
    pub selection_fg: ThemeColor,
    pub playing: ThemeColor,
    pub warning: ThemeColor,
    pub error: ThemeColor,
    pub live: ThemeColor,
}

impl Default for Theme {
    fn default() -> Self {
        Self::tunr_dark()
    }
}

impl Theme {
    /// Original Tunr dark theme preserving terminal background (`reset`) and ANSI palette.
    pub const fn tunr_dark() -> Self {
        Self {
            bg: ansi(Color::Reset),
            surface: ansi(Color::DarkGray),
            fg: ansi(Color::Reset),
            fg_emphasis: ansi(Color::White),
            fg_secondary: ansi(Color::Gray),
            fg_muted: ansi(Color::DarkGray),
            border: ansi(Color::DarkGray),
            border_focused: ansi(Color::Cyan),
            accent: ansi(Color::Cyan),
            accent_fg: ansi(Color::Black),
            accent_secondary: ansi(Color::Magenta),
            selection_bg: ansi(Color::DarkGray),
            selection_fg: ansi(Color::White),
            playing: ansi(Color::Green),
            warning: ansi(Color::Yellow),
            error: ansi(Color::Red),
            live: ansi(Color::LightRed),
        }
    }

    /// Clean high-contrast light theme with explicit surface background.
    pub const fn tunr_light() -> Self {
        Self {
            bg: rgb(0xf6, 0xf8, 0xfa),
            surface: rgb(0xe6, 0xeb, 0xf1),
            fg: rgb(0x1f, 0x23, 0x28),
            fg_emphasis: rgb(0x0d, 0x11, 0x17),
            fg_secondary: rgb(0x57, 0x60, 0x6a),
            fg_muted: rgb(0x59, 0x63, 0x6e),
            border: rgb(0xd0, 0xd7, 0xde),
            border_focused: rgb(0x09, 0x69, 0xda),
            accent: rgb(0x09, 0x69, 0xda),
            accent_fg: rgb(0xff, 0xff, 0xff),
            accent_secondary: rgb(0x82, 0x50, 0xdf),
            selection_bg: rgb(0xdd, 0xe5, 0xef),
            selection_fg: rgb(0x0d, 0x11, 0x17),
            playing: rgb(0x1a, 0x7f, 0x37),
            warning: rgb(0x9a, 0x67, 0x00),
            error: rgb(0xcf, 0x22, 0x2e),
            live: rgb(0xe1, 0x2d, 0x39),
        }
    }

    /// Rosé Pine main palette with explicit background.
    pub const fn rose_pine() -> Self {
        Self {
            bg: rgb(0x19, 0x17, 0x24),
            surface: rgb(0x26, 0x23, 0x3a),
            fg: rgb(0xe0, 0xde, 0xf4),
            fg_emphasis: rgb(0xe0, 0xde, 0xf4),
            fg_secondary: rgb(0x90, 0x8c, 0xaa),
            fg_muted: rgb(0x6e, 0x6a, 0x86),
            border: rgb(0x40, 0x3d, 0x52),
            border_focused: rgb(0x9c, 0xcf, 0xd8),
            accent: rgb(0xc4, 0xa7, 0xe7),
            accent_fg: rgb(0x19, 0x17, 0x24),
            accent_secondary: rgb(0xeb, 0xbc, 0xba),
            selection_bg: rgb(0x26, 0x23, 0x3a),
            selection_fg: rgb(0xe0, 0xde, 0xf4),
            playing: rgb(0x9c, 0xcf, 0xd8),
            warning: rgb(0xf6, 0xc1, 0x77),
            error: rgb(0xeb, 0x6f, 0x92),
            live: rgb(0xeb, 0x6f, 0x92),
        }
    }

    /// Nord Polar Night / Snow Storm / Frost / Aurora palette with explicit background.
    pub const fn nord() -> Self {
        Self {
            bg: rgb(0x2e, 0x34, 0x40),
            surface: rgb(0x3b, 0x42, 0x52),
            fg: rgb(0xe5, 0xe9, 0xf0),
            fg_emphasis: rgb(0xec, 0xef, 0xf4),
            fg_secondary: rgb(0xd8, 0xde, 0xe9),
            fg_muted: rgb(0x61, 0x6e, 0x88),
            border: rgb(0x4c, 0x56, 0x6a),
            border_focused: rgb(0x88, 0xc0, 0xd0),
            accent: rgb(0x88, 0xc0, 0xd0),
            accent_fg: rgb(0x2e, 0x34, 0x40),
            accent_secondary: rgb(0xb4, 0x8e, 0xad),
            selection_bg: rgb(0x43, 0x4c, 0x5e),
            selection_fg: rgb(0xec, 0xef, 0xf4),
            playing: rgb(0xa3, 0xbe, 0x8c),
            warning: rgb(0xeb, 0xcb, 0x8b),
            error: rgb(0xbf, 0x61, 0x6a),
            live: rgb(0xd0, 0x87, 0x70),
        }
    }

    /// Dracula palette with explicit background.
    pub const fn dracula() -> Self {
        Self {
            bg: rgb(0x28, 0x2a, 0x36),
            surface: rgb(0x44, 0x47, 0x5a),
            fg: rgb(0xf8, 0xf8, 0xf2),
            fg_emphasis: rgb(0xff, 0xff, 0xff),
            fg_secondary: rgb(0xd8, 0xd8, 0xd2),
            fg_muted: rgb(0x62, 0x72, 0xa4),
            border: rgb(0x62, 0x72, 0xa4),
            border_focused: rgb(0x8b, 0xe9, 0xfd),
            accent: rgb(0x8b, 0xe9, 0xfd),
            accent_fg: rgb(0x28, 0x2a, 0x36),
            accent_secondary: rgb(0xff, 0x79, 0xc6),
            selection_bg: rgb(0x44, 0x47, 0x5a),
            selection_fg: rgb(0xf8, 0xf8, 0xf2),
            playing: rgb(0x50, 0xfa, 0x7b),
            warning: rgb(0xf1, 0xfa, 0x8c),
            error: rgb(0xff, 0x55, 0x55),
            live: rgb(0xff, 0xb8, 0x6c),
        }
    }

    /// Base frame/container style setting both primary foreground and background.
    pub fn base_style(&self) -> Style {
        Style::new().fg(self.fg.into()).bg(self.bg.into())
    }

    /// Emphasized foreground style (leaves `bg` unset so selection backgrounds remain intact).
    pub fn emphasis_style(&self) -> Style {
        Style::new().fg(self.fg_emphasis.into())
    }

    /// Bold emphasized foreground style (leaves `bg` unset).
    pub fn emphasis_bold_style(&self) -> Style {
        self.emphasis_style().add_modifier(Modifier::BOLD)
    }

    /// Secondary foreground style (leaves `bg` unset).
    pub fn secondary_style(&self) -> Style {
        Style::new().fg(self.fg_secondary.into())
    }

    /// Muted foreground style (leaves `bg` unset).
    pub fn muted_style(&self) -> Style {
        Style::new().fg(self.fg_muted.into())
    }

    /// Bold muted foreground style (leaves `bg` unset).
    pub fn muted_bold_style(&self) -> Style {
        self.muted_style().add_modifier(Modifier::BOLD)
    }

    /// Block border style including explicit theme background.
    pub fn border_style(&self, focused: bool) -> Style {
        let fg = if focused {
            self.border_focused
        } else {
            self.border
        };
        Style::new().fg(fg.into()).bg(self.bg.into())
    }

    /// Pane title style for two-pane views (`Playlists` / `Tracks`).
    pub fn pane_title_style(&self, focused: bool) -> Style {
        if focused {
            self.accent_bold_style()
        } else {
            self.secondary_style()
        }
    }

    /// Single-pane panel title style (`Results` / `Downloads`).
    pub fn panel_title_style(&self) -> Style {
        self.emphasis_bold_style()
    }

    /// Primary accent foreground style (leaves `bg` unset).
    pub fn accent_style(&self) -> Style {
        Style::new().fg(self.accent.into())
    }

    /// Bold primary accent foreground style (leaves `bg` unset).
    pub fn accent_bold_style(&self) -> Style {
        self.accent_style().add_modifier(Modifier::BOLD)
    }

    /// Secondary accent foreground style (leaves `bg` unset).
    pub fn accent_secondary_style(&self) -> Style {
        Style::new().fg(self.accent_secondary.into())
    }

    /// Bold secondary accent foreground style (leaves `bg` unset).
    pub fn accent_secondary_bold_style(&self) -> Style {
        self.accent_secondary_style().add_modifier(Modifier::BOLD)
    }

    /// Active text-input cursor cell style (`accent_fg` on `accent`).
    pub fn cursor_style(&self) -> Style {
        Style::new()
            .fg(self.accent_fg.into())
            .bg(self.accent.into())
            .add_modifier(Modifier::BOLD)
    }

    /// List highlight style for the focused pane in `tui.rs`.
    pub fn focused_selection_style(&self) -> Style {
        Style::new()
            .fg(self.accent_fg.into())
            .bg(self.accent.into())
            .add_modifier(Modifier::BOLD)
    }

    /// List highlight style for the unfocused pane in `tui.rs`.
    pub fn unfocused_selection_style(&self) -> Style {
        Style::new()
            .fg(self.selection_fg.into())
            .bg(self.selection_bg.into())
            .add_modifier(Modifier::BOLD)
    }

    /// List highlight style preserving multi-span item foreground colors (`search_tui`, `download_tui`).
    pub fn passive_selection_style(&self) -> Style {
        Style::new()
            .bg(self.selection_bg.into())
            .add_modifier(Modifier::BOLD)
    }

    /// Active/playing/success foreground style (leaves `bg` unset).
    pub fn playing_style(&self) -> Style {
        Style::new().fg(self.playing.into())
    }

    /// Bold active/playing/success foreground style (leaves `bg` unset).
    pub fn playing_bold_style(&self) -> Style {
        self.playing_style().add_modifier(Modifier::BOLD)
    }

    /// Warning/paused/notice foreground style (leaves `bg` unset).
    pub fn warning_style(&self) -> Style {
        Style::new().fg(self.warning.into())
    }

    /// Bold warning/paused/notice foreground style (leaves `bg` unset).
    pub fn warning_bold_style(&self) -> Style {
        self.warning_style().add_modifier(Modifier::BOLD)
    }

    /// Error/destructive foreground style (leaves `bg` unset).
    pub fn error_style(&self) -> Style {
        Style::new().fg(self.error.into())
    }

    /// Bold error/destructive foreground style (leaves `bg` unset).
    pub fn error_bold_style(&self) -> Style {
        self.error_style().add_modifier(Modifier::BOLD)
    }

    /// Live-stream indicator foreground style (leaves `bg` unset).
    pub fn live_style(&self) -> Style {
        Style::new().fg(self.live.into())
    }

    /// Unfilled `Gauge` track and label style.
    pub fn gauge_style(&self) -> Style {
        Style::new().fg(self.fg_emphasis.into()).bg(self.surface.into())
    }

    /// Filled `Gauge` bar style.
    pub fn gauge_fill_style(&self) -> Style {
        Style::new().fg(self.accent.into()).bg(self.surface.into())
    }

    /// Border style for the destructive confirmation modal (includes explicit `bg`).
    pub fn modal_border_style(&self) -> Style {
        Style::new().fg(self.error.into()).bg(self.bg.into())
    }
}

/// Look up one of the 5 built-in themes by canonical name.
pub fn builtin_theme(name: &str) -> Option<Theme> {
    match name {
        "tunr-dark" => Some(Theme::tunr_dark()),
        "tunr-light" => Some(Theme::tunr_light()),
        "rose-pine" => Some(Theme::rose_pine()),
        "nord" => Some(Theme::nord()),
        "dracula" => Some(Theme::dracula()),
        _ => None,
    }
}

/// Partial or complete theme definition stored in `themes.json`.
///
/// Colors are stored as raw strings so a single invalid value in an inactive theme
/// never prevents parsing or loading the rest of `themes.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PartialTheme {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg_emphasis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg_secondary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg_muted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub border_focused: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent_fg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub accent_secondary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_bg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_fg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playing: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live: Option<String>,
}

impl PartialTheme {
    /// Convert a complete [`Theme`] into a fully populated [`PartialTheme`].
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            base: None,
            bg: Some(theme.bg.to_string()),
            surface: Some(theme.surface.to_string()),
            fg: Some(theme.fg.to_string()),
            fg_emphasis: Some(theme.fg_emphasis.to_string()),
            fg_secondary: Some(theme.fg_secondary.to_string()),
            fg_muted: Some(theme.fg_muted.to_string()),
            border: Some(theme.border.to_string()),
            border_focused: Some(theme.border_focused.to_string()),
            accent: Some(theme.accent.to_string()),
            accent_fg: Some(theme.accent_fg.to_string()),
            accent_secondary: Some(theme.accent_secondary.to_string()),
            selection_bg: Some(theme.selection_bg.to_string()),
            selection_fg: Some(theme.selection_fg.to_string()),
            playing: Some(theme.playing.to_string()),
            warning: Some(theme.warning.to_string()),
            error: Some(theme.error.to_string()),
            live: Some(theme.live.to_string()),
        }
    }

    fn apply_onto(
        &self,
        theme_name: &str,
        mut base: Theme,
        strict: bool,
        warnings: &mut Vec<String>,
    ) -> std::result::Result<Theme, String> {
        macro_rules! apply_field {
            ($field:ident) => {
                if let Some(raw) = &self.$field {
                    match ThemeColor::parse(raw) {
                        Ok(color) => base.$field = color,
                        Err(err) => {
                            let msg = format!(
                                "theme '{theme_name}' field '{}': {err}",
                                stringify!($field)
                            );
                            if strict {
                                return Err(msg);
                            }
                            warnings.push(msg);
                        }
                    }
                }
            };
        }

        apply_field!(bg);
        apply_field!(surface);
        apply_field!(fg);
        apply_field!(fg_emphasis);
        apply_field!(fg_secondary);
        apply_field!(fg_muted);
        apply_field!(border);
        apply_field!(border_focused);
        apply_field!(accent);
        apply_field!(accent_fg);
        apply_field!(accent_secondary);
        apply_field!(selection_bg);
        apply_field!(selection_fg);
        apply_field!(playing);
        apply_field!(warning);
        apply_field!(error);
        apply_field!(live);

        Ok(base)
    }
}

fn default_themes_version() -> u32 {
    THEMES_FILE_VERSION
}

fn default_active_theme() -> String {
    DEFAULT_THEME_NAME.to_string()
}

/// On-disk `themes.json` document containing the active theme name and theme definitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThemesFile {
    #[serde(default = "default_themes_version")]
    pub version: u32,
    #[serde(default = "default_active_theme")]
    pub active: String,
    #[serde(default)]
    pub themes: BTreeMap<String, PartialTheme>,
}

impl Default for ThemesFile {
    fn default() -> Self {
        Self::default_seeded()
    }
}

impl ThemesFile {
    /// Construct the default seeded `themes.json` containing all 5 built-in themes.
    pub fn default_seeded() -> Self {
        let mut themes = BTreeMap::new();
        for name in BUILTIN_THEME_NAMES {
            if let Some(theme) = builtin_theme(name) {
                themes.insert(name.to_string(), PartialTheme::from_theme(&theme));
            }
        }
        Self {
            version: THEMES_FILE_VERSION,
            active: DEFAULT_THEME_NAME.to_string(),
            themes,
        }
    }

    /// Return all selectable theme names (entries in `themes` plus built-in fallbacks), sorted.
    pub fn available_names(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for name in BUILTIN_THEME_NAMES {
            names.insert(name.to_string());
        }
        for name in self.themes.keys() {
            names.insert(name.clone());
        }
        names.into_iter().collect()
    }

    /// Return whether `name` exists either in `self.themes` or as a built-in theme.
    pub fn contains_theme(&self, name: &str) -> bool {
        self.themes.contains_key(name) || builtin_theme(name).is_some()
    }

    /// Resolve a named theme with cycle-guarded `base` inheritance across `self.themes` and built-ins.
    ///
    /// When `strict` is `true`, unknown themes, inheritance cycles, and invalid color tokens fail
    /// with `Err(message)`. When `strict` is `false`, invalid pieces fall back to defaults and
    /// return a combined warning string.
    pub fn resolve_theme_named(
        &self,
        name: &str,
        strict: bool,
    ) -> std::result::Result<(Theme, Option<String>), String> {
        let mut visited = HashSet::new();
        let mut warnings = Vec::new();
        let theme = self.resolve_recursive(name, strict, &mut visited, &mut warnings)?;
        let warning = if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("; "))
        };
        Ok((theme, warning))
    }

    /// Resolve `self.active` non-strictly, always returning a usable [`Theme`] and optional warning.
    pub fn resolve_active(&self) -> (Theme, Option<String>) {
        match self.resolve_theme_named(&self.active, false) {
            Ok(outcome) => outcome,
            Err(err) => (Theme::tunr_dark(), Some(err)),
        }
    }

    fn resolve_recursive<'a>(
        &'a self,
        name: &'a str,
        strict: bool,
        visited: &mut HashSet<&'a str>,
        warnings: &mut Vec<String>,
    ) -> std::result::Result<Theme, String> {
        if !visited.insert(name) {
            let msg = format!("theme inheritance cycle detected at '{name}'");
            if strict {
                return Err(msg);
            }
            warnings.push(msg);
            return Ok(Theme::tunr_dark());
        }

        let Some(entry) = self.themes.get(name) else {
            visited.remove(name);
            if let Some(builtin) = builtin_theme(name) {
                return Ok(builtin);
            }
            let msg = format!("unknown theme '{name}'");
            if strict {
                return Err(msg);
            }
            warnings.push(format!("{msg}, falling back to '{DEFAULT_THEME_NAME}'"));
            return Ok(Theme::tunr_dark());
        };

        let base = if let Some(base_name) = entry.base.as_deref() {
            self.resolve_recursive(base_name, strict, visited, warnings)?
        } else {
            builtin_theme(name).unwrap_or_else(Theme::tunr_dark)
        };

        visited.remove(name);
        entry.apply_onto(name, base, strict, warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_color_parses_hex_and_ansi_names_case_insensitively() {
        assert_eq!(
            ThemeColor::parse("#1a2B3c").unwrap(),
            ThemeColor(Color::Rgb(0x1a, 0x2b, 0x3c))
        );
        assert_eq!(
            ThemeColor::parse("  #FF00aa ").unwrap().to_string(),
            "#ff00aa"
        );
        assert_eq!(ThemeColor::parse("Reset").unwrap(), ThemeColor(Color::Reset));
        assert_eq!(
            ThemeColor::parse("default").unwrap(),
            ThemeColor(Color::Reset)
        );
        assert_eq!(
            ThemeColor::parse("DARK_GRAY").unwrap(),
            ThemeColor(Color::DarkGray)
        );
        assert_eq!(
            ThemeColor::parse("dark-grey").unwrap(),
            ThemeColor(Color::DarkGray)
        );
        assert_eq!(
            ThemeColor::parse("lightred").unwrap(),
            ThemeColor(Color::LightRed)
        );
        assert_eq!(ThemeColor::parse("grey").unwrap(), ThemeColor(Color::Gray));

        assert!(ThemeColor::parse("#12345").is_err());
        assert!(ThemeColor::parse("#gggggg").is_err());
        assert!(ThemeColor::parse("not-a-color").is_err());
    }

    #[test]
    fn theme_color_serde_round_trips() {
        let colors = [
            ThemeColor(Color::Reset),
            ThemeColor(Color::DarkGray),
            ThemeColor(Color::LightRed),
            ThemeColor(Color::Rgb(0x19, 0x17, 0x24)),
        ];
        let json = serde_json::to_string(&colors).unwrap();
        assert_eq!(json, r##"["reset","dark_gray","light_red","#191724"]"##);
        let decoded: Vec<ThemeColor> = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, colors);
    }

    #[test]
    fn default_seeded_contains_all_builtins_and_round_trips() {
        let seeded = ThemesFile::default_seeded();
        assert_eq!(seeded.version, THEMES_FILE_VERSION);
        assert_eq!(seeded.active, "tunr-dark");
        for name in BUILTIN_THEME_NAMES {
            assert!(seeded.themes.contains_key(name), "missing {name}");
            let (resolved, warn) = seeded.resolve_theme_named(name, true).unwrap();
            assert!(warn.is_none());
            assert_eq!(resolved, builtin_theme(name).unwrap());
        }

        let json = serde_json::to_string_pretty(&seeded).unwrap();
        let parsed: ThemesFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, seeded);
    }

    #[test]
    fn partial_theme_inherits_from_base_and_other_custom_themes() {
        let raw = r##"{
            "version": 1,
            "active": "custom-nord",
            "themes": {
                "nord-base": {
                    "base": "nord",
                    "accent": "#ff00ff"
                },
                "custom-nord": {
                    "base": "nord-base",
                    "playing": "light_green"
                }
            }
        }"##;
        let file: ThemesFile = serde_json::from_str(raw).unwrap();
        let (theme, warn) = file.resolve_active();
        assert!(warn.is_none(), "unexpected warning: {warn:?}");
        assert_eq!(theme.bg, Theme::nord().bg);
        assert_eq!(theme.accent, ThemeColor(Color::Rgb(0xff, 0x00, 0xff)));
        assert_eq!(theme.playing, ThemeColor(Color::LightGreen));
    }

    #[test]
    fn inheritance_cycle_fails_strict_and_falls_back_non_strict() {
        let raw = r#"{
            "active": "a",
            "themes": {
                "a": { "base": "b" },
                "b": { "base": "a" }
            }
        }"#;
        let file: ThemesFile = serde_json::from_str(raw).unwrap();
        assert!(file.resolve_theme_named("a", true).is_err());

        let (theme, warn) = file.resolve_active();
        assert_eq!(theme, Theme::tunr_dark());
        assert!(warn.as_deref().is_some_and(|w| w.contains("cycle")));
    }

    #[test]
    fn bad_color_in_inactive_theme_does_not_break_active_or_deserialization() {
        let raw = r#"{
            "active": "nord",
            "themes": {
                "broken": {
                    "accent": "bogus-color"
                }
            }
        }"#;
        let file: ThemesFile = serde_json::from_str(raw).unwrap();
        let (active, warn) = file.resolve_active();
        assert_eq!(active, Theme::nord());
        assert!(warn.is_none());

        assert!(file.resolve_theme_named("broken", true).is_err());
        let (fallback, broken_warn) = file.resolve_theme_named("broken", false).unwrap();
        assert_eq!(fallback, Theme::tunr_dark());
        assert!(broken_warn.as_deref().is_some_and(|w| w.contains("accent")));
    }

    #[test]
    fn unknown_active_theme_falls_back_with_warning() {
        let file = ThemesFile {
            version: 1,
            active: "nonexistent".to_string(),
            themes: BTreeMap::new(),
        };
        assert!(file.resolve_theme_named("nonexistent", true).is_err());
        let (theme, warn) = file.resolve_active();
        assert_eq!(theme, Theme::tunr_dark());
        assert!(warn.as_deref().is_some_and(|w| w.contains("nonexistent")));
    }

    #[test]
    fn foreground_helpers_leave_bg_unset_for_passive_highlight_composition() {
        let theme = Theme::tunr_light();
        let fg_styles = [
            theme.emphasis_style(),
            theme.emphasis_bold_style(),
            theme.secondary_style(),
            theme.muted_style(),
            theme.muted_bold_style(),
            theme.accent_style(),
            theme.accent_bold_style(),
            theme.accent_secondary_style(),
            theme.accent_secondary_bold_style(),
            theme.playing_style(),
            theme.playing_bold_style(),
            theme.warning_style(),
            theme.warning_bold_style(),
            theme.error_style(),
            theme.error_bold_style(),
            theme.live_style(),
        ];
        for style in fg_styles {
            assert_eq!(style.bg, None, "fg helper should not clobber bg: {style:?}");
        }
        assert_eq!(
            theme.passive_selection_style().bg,
            Some(theme.selection_bg.to_color())
        );
    }
}
