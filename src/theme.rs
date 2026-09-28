//! Colours. The built-in `hivemux` theme is honey on the terminal's own
//! colours. `omarchy` follows the desktop's current Omarchy theme, and every
//! Omarchy theme can be picked by name, read from its `colors.toml`.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ratatui::style::Color;

pub const BUILTIN: &str = "hivemux";
/// Follows whatever theme Omarchy currently uses.
pub const FOLLOW: &str = "omarchy";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    /// Focused borders, badges, keys in menus.
    pub accent: Color,
    /// Text on an accent background.
    pub on_accent: Color,
    /// Unfocused borders, the selected row in menus.
    pub subtle: Color,
    /// Blocked agents, warnings, destructive actions.
    pub danger: Color,
    /// Idle agents.
    pub success: Color,
    /// Working agents.
    pub warning: Color,
    /// More hues, to tell workspaces, modes and menu groups apart.
    pub blue: Color,
    pub cyan: Color,
    pub magenta: Color,
    pub orange: Color,
    /// Quieter text: hints, directories, times.
    pub muted: Color,
    /// The background of the bars, a shade off the terminal's.
    pub surface: Color,
}

pub const HONEY: Theme = Theme {
    accent: Color::Rgb(250, 190, 0),
    on_accent: Color::Black,
    subtle: Color::DarkGray,
    danger: Color::LightRed,
    success: Color::LightGreen,
    warning: Color::Rgb(250, 190, 0),
    blue: Color::Rgb(97, 175, 239),
    cyan: Color::Rgb(86, 182, 194),
    magenta: Color::Rgb(198, 120, 221),
    orange: Color::Rgb(232, 145, 74),
    muted: Color::Gray,
    // The terminal's own background: honey keeps its look.
    surface: Color::Reset,
};

impl Theme {
    /// Colours for things that should look different from each other, like
    /// workspaces.
    pub fn palette(&self) -> [Color; 6] {
        [
            self.blue,
            self.magenta,
            self.cyan,
            self.success,
            self.orange,
            self.warning,
        ]
    }

    /// The colour of workspace `n`, the same wherever it shows.
    pub fn workspace(&self, n: u8) -> Color {
        let palette = self.palette();
        palette[usize::from(n.saturating_sub(1)) % palette.len()]
    }
}

thread_local! {
    static CURRENT: Cell<Theme> = const { Cell::new(HONEY) };
}

/// The theme everything is drawn with. Drawing happens on the server's main
/// thread, which sets it with `set` whenever the choice changes.
pub fn current() -> Theme {
    CURRENT.with(Cell::get)
}

pub fn set(theme: Theme) {
    CURRENT.with(|c| c.set(theme));
}

/// The theme called `name`, falling back to the built-in one when it cannot
/// be found or read.
pub fn load(name: &str) -> Theme {
    let path = match name {
        BUILTIN => return HONEY,
        FOLLOW => omarchy_state().join("theme/colors.toml"),
        name => match theme_dir(name) {
            Some(dir) => dir.join("colors.toml"),
            None => return HONEY,
        },
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|text| parse(&text))
        .unwrap_or(HONEY)
}

/// The name of Omarchy's current theme, e.g. `catppuccin`.
pub fn omarchy_current() -> Option<String> {
    let name = fs::read_to_string(omarchy_state().join("theme.name")).ok()?;
    Some(name.trim().to_owned()).filter(|n| !n.is_empty())
}

/// What changes when Omarchy switches themes: the name and when its
/// colours were written. The colours can land after the name, so both count.
pub type Stamp = (Option<String>, Option<std::time::SystemTime>);

/// Omarchy's current theme as a [`Stamp`], `None` without Omarchy.
pub fn omarchy_stamp() -> Option<Stamp> {
    let state = omarchy_state();
    if !state.is_dir() {
        return None;
    }
    let written = fs::metadata(state.join("theme/colors.toml"))
        .and_then(|m| m.modified())
        .ok();
    Some((omarchy_current(), written))
}

/// The theme to start with: following Omarchy where it is installed, the
/// built-in one elsewhere.
pub fn default_name() -> String {
    if omarchy_state().is_dir() {
        FOLLOW
    } else {
        BUILTIN
    }
    .to_owned()
}

/// Every theme to choose from: the built-in one, following Omarchy, then
/// the installed Omarchy themes by name.
pub fn available() -> Vec<String> {
    let mut names = vec![BUILTIN.to_owned(), FOLLOW.to_owned()];
    let mut found = BTreeSet::new();
    for dir in theme_dirs() {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.path().join("colors.toml").is_file() {
                found.insert(entry.file_name().to_string_lossy().into_owned());
            }
        }
    }
    names.extend(found);
    names
}

/// Reads an Omarchy `colors.toml`. Missing colours keep the built-in ones.
pub fn parse(text: &str) -> Option<Theme> {
    let table: toml::Table = toml::from_str(text).ok()?;
    let color = |key: &str| table.get(key)?.as_str().and_then(hex);
    let accent = color("accent")?;
    Some(Theme {
        accent,
        on_accent: color("background").unwrap_or(HONEY.on_accent),
        subtle: color("selection")
            .or_else(|| color("lighter_background"))
            .unwrap_or(HONEY.subtle),
        danger: color("red").unwrap_or(HONEY.danger),
        success: color("green").unwrap_or(HONEY.success),
        warning: color("yellow").unwrap_or(accent),
        blue: color("blue")
            .or_else(|| color("bright_blue"))
            .unwrap_or(HONEY.blue),
        cyan: color("cyan")
            .or_else(|| color("bright_cyan"))
            .unwrap_or(HONEY.cyan),
        magenta: color("magenta")
            .or_else(|| color("bright_magenta"))
            .unwrap_or(HONEY.magenta),
        orange: color("orange").unwrap_or(HONEY.orange),
        muted: color("dark_foreground")
            .or_else(|| color("muted"))
            .unwrap_or(HONEY.muted),
        surface: color("lighter_background")
            .or_else(|| color("dark_background"))
            .unwrap_or(HONEY.surface),
    })
}

fn hex(text: &str) -> Option<Color> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(digits.get(i..i + 2)?, 16).ok();
    Some(Color::Rgb(byte(0)?, byte(2)?, byte(4)?))
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn omarchy_state() -> PathBuf {
    home().join(".local/state/omarchy/current")
}

/// User themes first, so they win over shipped ones with the same name.
fn theme_dirs() -> Vec<PathBuf> {
    vec![
        home().join(".config/omarchy/themes"),
        home().join(".local/share/omarchy/themes"),
    ]
}

fn theme_dir(name: &str) -> Option<PathBuf> {
    // Names come from the config file, never let them leave the theme dirs.
    if name.contains('/') || name.starts_with('.') {
        return None;
    }
    theme_dirs()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|dir| Path::new(dir).join("colors.toml").is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATPPUCCIN: &str = r##"
mode = "dark"
accent = "#89b4fa"
selection = "#45475a"
background = "#1e1e2e"
red = "#f38ba8"
yellow = "#f9e2af"
green = "#a6e3a1"
"##;

    #[test]
    fn reads_omarchy_colors() {
        let theme = parse(CATPPUCCIN).unwrap();
        assert_eq!(theme.accent, Color::Rgb(0x89, 0xb4, 0xfa));
        assert_eq!(theme.on_accent, Color::Rgb(0x1e, 0x1e, 0x2e));
        assert_eq!(theme.subtle, Color::Rgb(0x45, 0x47, 0x5a));
        assert_eq!(theme.danger, Color::Rgb(0xf3, 0x8b, 0xa8));
        assert_eq!(theme.warning, Color::Rgb(0xf9, 0xe2, 0xaf));
    }

    #[test]
    fn reads_the_extra_hues_and_gives_workspaces_colors() {
        let theme = parse("accent = \"#010203\"\nblue = \"#0000ff\"\nmagenta = \"#ff00ff\"\nlighter_background = \"#222222\"").unwrap();
        assert_eq!(theme.blue, Color::Rgb(0, 0, 255));
        assert_eq!(theme.surface, Color::Rgb(0x22, 0x22, 0x22));
        assert_eq!(theme.workspace(1), theme.blue);
        assert_eq!(theme.workspace(2), theme.magenta);
        assert_eq!(theme.workspace(7), theme.blue);
        assert_eq!(theme.cyan, HONEY.cyan);
    }

    #[test]
    fn missing_colors_fall_back_and_no_accent_means_no_theme() {
        let theme = parse("accent = \"#112233\"").unwrap();
        assert_eq!(theme.danger, HONEY.danger);
        assert_eq!(theme.warning, Color::Rgb(0x11, 0x22, 0x33));
        assert_eq!(parse("red = \"#ff0000\""), None);
        assert_eq!(parse("not toml ["), None);
    }

    #[test]
    fn hex_colors() {
        assert_eq!(hex("#FFa000"), Some(Color::Rgb(255, 160, 0)));
        assert_eq!(hex("fff"), None);
        assert_eq!(hex("#12345"), None);
        assert_eq!(hex("#zz0000"), None);
    }

    #[test]
    fn names_cannot_escape_the_theme_folders() {
        assert_eq!(theme_dir("../../etc"), None);
        assert_eq!(theme_dir(".hidden"), None);
        assert_eq!(load("../../etc"), HONEY);
    }

    #[test]
    fn builtin_and_follow_come_first() {
        let names = available();
        assert_eq!(&names[..2], &[BUILTIN.to_owned(), FOLLOW.to_owned()]);
    }
}
