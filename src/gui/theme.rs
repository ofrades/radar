//! Reading omarchy's theme so panes look like the user's desktop.
//!
//! omarchy writes the active theme to
//! `~/.local/state/omarchy/current/theme/`, with a `colors.toml` that carries
//! the sixteen ANSI colours (plus background/foreground/accent) and a per-app
//! config for each terminal it supports. We read colours from there, the
//! panel roundness from Hyprland's live `decoration:rounding`, and the font
//! from the user's own alacritty/ghostty config, falling back to something
//! sane so radar is usable on any machine.

use std::path::{Path, PathBuf};

use gtk::gdk;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub background: gdk::RGBA,
    pub foreground: gdk::RGBA,
    /// The sixteen ANSI colours, normal then bright.
    pub palette: Vec<gdk::RGBA>,
    /// The theme's own highlight (`accent` in colors.toml). Radar's controls
    /// key off it, so switching omarchy themes recolours the whole app.
    pub accent: gdk::RGBA,
    /// Secondary text and quiet chrome (colors.toml `muted`).
    pub muted: gdk::RGBA,
    /// Destructive and attention colour (colors.toml `red`).
    pub warning: gdk::RGBA,
    /// Hyprland's live `decoration:rounding` — the same number omarchy's own
    /// shell uses for panel corners. Panes match it so radar blends in.
    pub panel_radius: i32,
    pub font_family: String,
    pub font_size: f64,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            dark: true,
            background: gdk::RGBA::parse("#111111").unwrap_or(gdk::RGBA::BLACK),
            foreground: gdk::RGBA::parse("#dddddd").unwrap_or(gdk::RGBA::WHITE),
            palette: DEFAULT_PALETTE
                .iter()
                .map(|hex| gdk::RGBA::parse(*hex).unwrap_or(gdk::RGBA::BLACK))
                .collect(),
            // Only used when omarchy (or a colors.toml) is absent entirely.
            accent: gdk::RGBA::parse("#ff7958").unwrap_or(gdk::RGBA::BLACK),
            muted: gdk::RGBA::parse("#808080").unwrap_or(gdk::RGBA::BLACK),
            warning: gdk::RGBA::parse("#cc0000").unwrap_or(gdk::RGBA::BLACK),
            panel_radius: 8,
            font_family: "monospace".to_string(),
            font_size: 11.0,
        }
    }
}

/// A decent fallback: the classic xterm palette.
const DEFAULT_PALETTE: [&str; 16] = [
    "#000000", "#cd0000", "#00cd00", "#cdcd00", "#0000ee", "#cd00cd", "#00cdcd", "#e5e5e5",
    "#7f7f7f", "#ff0000", "#00ff00", "#ffff00", "#5c5cff", "#ff00ff", "#00ffff", "#ffffff",
];

impl Theme {
    /// Load the active omarchy theme, falling back per-field.
    pub fn load() -> Theme {
        let mut theme = Theme::default();
        if let Some(colors) = omarchy_colors() {
            theme.dark = colors.get("mode").map(|m| m != "light").unwrap_or(true);
            theme.background = color(&colors, &["background"])
                .unwrap_or(theme.background)
                .with_alpha(1.0);
            theme.foreground = color(&colors, &["foreground"])
                .unwrap_or(theme.foreground)
                .with_alpha(1.0);
            theme.accent = color(&colors, &["accent"])
                .unwrap_or(theme.accent)
                .with_alpha(1.0);
            theme.muted = color(&colors, &["muted"])
                .unwrap_or(theme.muted)
                .with_alpha(1.0);
            theme.warning = color(&colors, &["red", "bright_red"])
                .unwrap_or(theme.warning)
                .with_alpha(1.0);
            theme.palette = ansi_palette(&colors);
        }
        if let Some(radius) = hyprland_panel_radius() {
            theme.panel_radius = radius;
        }
        if let Some((family, size)) = terminal_font() {
            theme.font_family = family;
            theme.font_size = size;
        }
        theme
    }
}

/// Directory omarchy keeps the active theme in.
pub fn omarchy_theme_dir() -> Option<PathBuf> {
    let dir = dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))?
        .join("omarchy/current/theme");
    dir.is_dir().then_some(dir)
}

/// Parse `colors.toml` into a key/value map.
fn omarchy_colors() -> Option<std::collections::HashMap<String, String>> {
    let path = omarchy_theme_dir()?.join("colors.toml");
    let text = std::fs::read_to_string(path).ok()?;
    Some(parse_toml_scalars(&text))
}

/// Hyprland's live panel roundness: the same `hyprctl` query omarchy's shell
/// runs, so whatever set `decoration:rounding` (theme, user config) wins.
fn hyprland_panel_radius() -> Option<i32> {
    let output = std::process::Command::new("hyprctl")
        .args(["-j", "getoption", "decoration:rounding"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let json = String::from_utf8(output.stdout).ok()?;
    parse_hyprctl_int(&json).filter(|radius| (0..=64).contains(radius))
}

/// The `"int": 8` out of hyprctl's JSON for a scalar option.
fn parse_hyprctl_int(json: &str) -> Option<i32> {
    let after_key = json.split("\"int\"").nth(1)?;
    let after_colon = after_key.split_once(':')?.1.trim_start();
    let digits: usize = after_colon
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .count();
    if digits == 0 {
        return None;
    }
    after_colon[..digits].parse().ok()
}

fn ansi_palette(colors: &std::collections::HashMap<String, String>) -> Vec<gdk::RGBA> {
    let normal = [
        "dark_background",
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "light_foreground",
    ];
    let bright = [
        "muted",
        "bright_red",
        "bright_green",
        "bright_yellow",
        "bright_blue",
        "bright_magenta",
        "bright_cyan",
        "bright_foreground",
    ];
    let fallback = Theme::default().palette;
    let mut palette = Vec::with_capacity(16);
    for (index, key) in normal.iter().enumerate() {
        palette.push(
            color(colors, &[key, "background"])
                .unwrap_or_else(|| fallback.get(index).cloned().unwrap_or(gdk::RGBA::BLACK)),
        );
    }
    for (index, key) in bright.iter().enumerate() {
        palette.push(
            color(colors, &[key, "foreground"])
                .unwrap_or_else(|| fallback.get(index + 8).cloned().unwrap_or(gdk::RGBA::WHITE)),
        );
    }
    palette
}

fn color(colors: &std::collections::HashMap<String, String>, keys: &[&str]) -> Option<gdk::RGBA> {
    for key in keys {
        if let Some(value) = colors.get(*key) {
            if let Ok(rgba) = gdk::RGBA::parse(value) {
                return Some(rgba);
            }
        }
    }
    None
}

/// Find the terminal font: the user's terminal config first, then the theme's.
fn terminal_font() -> Option<(String, f64)> {
    let home = dirs::home_dir()?;
    let candidates = [
        home.join(".config/alacritty/alacritty.toml"),
        omarchy_theme_dir()
            .unwrap_or_default()
            .join("alacritty.toml"),
        home.join(".config/ghostty/config"),
        home.join(".config/kitty/kitty.conf"),
    ];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some(font) = font_from_config(&path, &text) {
                return Some(font);
            }
        }
    }
    None
}

fn font_from_config(path: &Path, text: &str) -> Option<(String, f64)> {
    // Identify the terminal by its whole path: ghostty's config file is called
    // `config`, so the file name alone says nothing.
    let where_ = path.to_string_lossy();
    if where_.contains("ghostty") {
        // ghostty: font-family = JetBrainsMono Nerd Font / font-size = 11
        let family = scalar_after(text, "font-family")?;
        let size = scalar_after(text, "font-size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(11.0);
        return Some((family, size));
    }
    if where_.contains("kitty") {
        // kitty: font_family JetBrainsMono Nerd Font / font_size 11
        let family = line_value(text, "font_family")?;
        let size = line_value(text, "font_size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(11.0);
        return Some((family, size));
    }
    // alacritty: `family = "X"`, either its own key or inside an inline table
    // (`normal = { family = "X" }`), plus a plain `size = 8`.
    let family = text
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            if key.trim() == "family" {
                return Some(value.trim().trim_matches('"').to_string());
            }
            // Inside an inline table: take what follows `family =`.
            let after = value.split_once("family")?.1;
            let inner = after.split_once('=')?.1;
            Some(
                inner
                    .trim()
                    .trim_end_matches('}')
                    .trim()
                    .trim_matches('"')
                    .to_string(),
            )
        })
        .find(|family| !family.is_empty())?;
    let size = text
        .lines()
        .filter_map(|line| line.split_once('='))
        .find(|(key, _)| key.trim() == "size")
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(11.0);
    Some((family, size))
}

/// `key = value` from a config that uses equals signs.
fn scalar_after(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .find(|(candidate, _)| candidate.trim() == key)
        .map(|(_, value)| value.trim().trim_matches('"').to_string())
}

/// `key value` from a config that uses spaces (kitty).
fn line_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix(key))
        .map(|rest| rest.trim().trim_matches('"').to_string())
}

/// Minimal TOML reader for `key = "value"` / `key = 12` at the top level.
///
/// colours.toml is a flat list of scalars, so a full parser would be dead
/// weight.
pub fn parse_toml_scalars(text: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value.trim().trim_matches('"').to_string();
            if !key.is_empty() && !value.is_empty() {
                map.insert(key, value);
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_toml_scalars() {
        let map = parse_toml_scalars(
            r##"
            mode = "dark"
            accent = "#509475"
            # a comment
            [section]
            nested = "ignored"
            number = 12
            "##,
        );
        assert_eq!(map.get("mode").unwrap(), "dark");
        assert_eq!(map.get("accent").unwrap(), "#509475");
        assert_eq!(map.get("number").unwrap(), "12");
        // Section contents are not top level keys, but our reader keeps them
        // under their own name, which is harmless here.
        assert!(map.contains_key("nested"));
    }

    #[test]
    fn alacritty_font_is_read() {
        let config = r#"
[font]
normal = { family = "JetBrainsMono Nerd Font" }
size = 8.0
"#;
        let (family, size) = font_from_config(
            Path::new("/home/u/.config/alacritty/alacritty.toml"),
            config,
        )
        .unwrap();
        assert_eq!(family, "JetBrainsMono Nerd Font");
        assert_eq!(size, 8.0);
    }

    #[test]
    fn alacritty_inline_tables_are_read() {
        // The shape omarchy and most alacritty users have.
        let config = "[font]\nnormal = { family = \"JetBrainsMono Nerd Font\" }\nsize = 8\n";
        let (family, size) = font_from_config(
            Path::new("/home/u/.config/alacritty/alacritty.toml"),
            config,
        )
        .unwrap();
        assert_eq!(family, "JetBrainsMono Nerd Font");
        assert_eq!(size, 8.0);
    }

    #[test]
    fn a_config_without_a_font_is_not_a_match() {
        let config = "[colors]\nbackground = \"#000000\"\n";
        assert!(font_from_config(
            Path::new("/home/u/.config/alacritty/alacritty.toml"),
            config
        )
        .is_none());
    }

    #[test]
    fn ghostty_font_is_read() {
        let config = "font-family = JetBrainsMono Nerd Font\nfont-size = 11\n";
        let (family, size) =
            font_from_config(Path::new("/home/u/.config/ghostty/config"), config).unwrap();
        assert_eq!(family, "JetBrainsMono Nerd Font");
        assert_eq!(size, 11.0);
    }

    #[test]
    fn kitty_font_is_read() {
        let config = "font_family JetBrainsMono Nerd Font\nfont_size 13\n";
        let (family, size) =
            font_from_config(Path::new("/home/u/.config/kitty/kitty.conf"), config).unwrap();
        assert_eq!(family, "JetBrainsMono Nerd Font");
        assert_eq!(size, 13.0);
    }

    #[test]
    fn default_theme_is_usable_without_omarchy() {
        let theme = Theme::default();
        assert_eq!(theme.palette.len(), 16);
        assert!(theme.accent.alpha() > 0.0);
        assert!((0..=64).contains(&theme.panel_radius));
        assert!(theme.font_size > 0.0);
        assert!(!theme.font_family.is_empty());
    }

    #[test]
    fn hyprctl_json_is_parsed() {
        // The exact shape hyprctl emits for a scalar option.
        let json = r#"{"option": "decoration:rounding", "int": 12, "set": true }"#;
        assert_eq!(parse_hyprctl_int(json), Some(12));
        assert_eq!(parse_hyprctl_int(r#"{"int": 0, "set": true}"#), Some(0));
        assert_eq!(parse_hyprctl_int("no int here"), None);
        assert_eq!(parse_hyprctl_int(r#"{"int": }"#), None);
    }

    #[test]
    fn theme_loads_on_this_machine() {
        // Either omarchy colours are found, or the fallback is used: both must
        // produce a complete theme.
        let theme = Theme::load();
        assert_eq!(theme.palette.len(), 16);
        assert!(theme.foreground.alpha() > 0.0);
        assert!(theme.accent.alpha() > 0.0);
        assert!(theme.muted.alpha() > 0.0);
        assert!(theme.warning.alpha() > 0.0);
        assert!((0..=64).contains(&theme.panel_radius));
    }
}
