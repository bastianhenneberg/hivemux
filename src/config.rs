//! User settings, stored as TOML in `$XDG_CONFIG_HOME/hivemux/config.toml`.
//!
//! The server loads them at start. The settings menu changes them live and
//! writes them back right away.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    #[default]
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhichKey {
    /// Show the key menu after the prefix.
    pub enabled: bool,
    /// Which bottom corner the menu opens in.
    pub position: Side,
}

impl Default for WhichKey {
    fn default() -> Self {
        Self {
            enabled: true,
            position: Side::Left,
        }
    }
}

/// Where a bar element is shown: in a line above or below the panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Placement {
    Top,
    Bottom,
    Off,
}

impl Placement {
    fn name(self) -> &'static str {
        match self {
            Placement::Top => "top",
            Placement::Bottom => "bottom",
            Placement::Off => "off",
        }
    }
}

/// The elements of the bars around the panes. Elements on the same side
/// share one line: the control bar and the workspace tabs on the left, the
/// path on the right.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bars {
    /// The hivemux badge, mode and key hints. Never off, it is the only
    /// sign of the prefix when the which-key menu is off.
    pub control: Placement,
    /// The workspace tabs.
    pub tabs: Placement,
    /// The working directory of the focused pane.
    pub path: Placement,
}

impl Default for Bars {
    fn default() -> Self {
        Self {
            control: Placement::Bottom,
            tabs: Placement::Bottom,
            path: Placement::Bottom,
        }
    }
}

impl Bars {
    /// Whether anything is shown on `side`.
    pub fn uses(&self, side: Placement) -> bool {
        self.control() == side || self.tabs == side || self.path == side
    }

    /// The control bar's side, bottom if the config says off.
    pub fn control(&self) -> Placement {
        match self.control {
            Placement::Off => Placement::Bottom,
            side => side,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub which_key: WhichKey,
    pub bars: Bars,
}

impl Config {
    /// Parses a config file. Missing keys take their default.
    pub fn parse(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }

    /// Loads the config file. A missing file means all defaults. A broken
    /// one also falls back to the defaults, with the reason returned
    /// alongside, so a typo never keeps hivemux from starting.
    pub fn load() -> (Self, Option<String>) {
        let path = match path() {
            Ok(path) => path,
            Err(e) => return (Self::default(), Some(format!("{e:#}"))),
        };
        match fs::read_to_string(&path) {
            Ok(text) => match Self::parse(&text) {
                Ok(config) => (config, None),
                Err(e) => (Self::default(), Some(format!("{}: {e:#}", path.display()))),
            },
            Err(_) => (Self::default(), None),
        }
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = path()?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }
}

/// `$HIVEMUX_CONFIG` if set, otherwise `hivemux/config.toml` in
/// `$XDG_CONFIG_HOME`, or in `~/.config` without it.
pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HIVEMUX_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(base.join("hivemux").join("config.toml"))
}

/// One line in the settings menu. Every setting has a small set of values,
/// `cycle` steps to the next one.
pub struct Setting {
    pub name: &'static str,
    pub value: fn(&Config) -> &'static str,
    pub cycle: fn(&mut Config),
}

/// bottom → top → off → bottom, or without off for elements that must stay.
fn next_placement(p: Placement, allow_off: bool) -> Placement {
    match p {
        Placement::Bottom => Placement::Top,
        Placement::Top if allow_off => Placement::Off,
        Placement::Top | Placement::Off => Placement::Bottom,
    }
}

pub const SETTINGS: &[Setting] = &[
    Setting {
        name: "Which-key menu",
        value: |c| if c.which_key.enabled { "on" } else { "off" },
        cycle: |c| c.which_key.enabled = !c.which_key.enabled,
    },
    Setting {
        name: "Which-key position",
        value: |c| match c.which_key.position {
            Side::Left => "left",
            Side::Right => "right",
        },
        cycle: |c| {
            c.which_key.position = match c.which_key.position {
                Side::Left => Side::Right,
                Side::Right => Side::Left,
            }
        },
    },
    Setting {
        name: "Control bar",
        value: |c| c.bars.control().name(),
        cycle: |c| c.bars.control = next_placement(c.bars.control(), false),
    },
    Setting {
        name: "Workspace tabs",
        value: |c| c.bars.tabs.name(),
        cycle: |c| c.bars.tabs = next_placement(c.bars.tabs, true),
    },
    Setting {
        name: "Path",
        value: |c| c.bars.path.name(),
        cycle: |c| c.bars.path = next_placement(c.bars.path, true),
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_means_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config, Config::default());
        assert!(config.which_key.enabled);
        assert_eq!(config.which_key.position, Side::Left);
    }

    #[test]
    fn partial_file_keeps_other_defaults() {
        let config = Config::parse("[which_key]\nposition = \"right\"\n").unwrap();
        assert_eq!(config.which_key.position, Side::Right);
        assert!(config.which_key.enabled);
    }

    #[test]
    fn invalid_value_is_an_error() {
        assert!(Config::parse("[which_key]\nposition = \"middle\"\n").is_err());
    }

    #[test]
    fn round_trips_through_toml() {
        let mut config = Config::default();
        for setting in SETTINGS {
            (setting.cycle)(&mut config);
        }
        let text = toml::to_string_pretty(&config).unwrap();
        assert_eq!(Config::parse(&text).unwrap(), config);
        assert!(text.contains("position = \"right\""));
    }

    #[test]
    fn cycling_comes_back_to_the_start() {
        for setting in SETTINGS {
            let mut config = Config::default();
            let before = (setting.value)(&config);
            (setting.cycle)(&mut config);
            assert_ne!((setting.value)(&config), before, "{}", setting.name);
            let mut steps = 1;
            while (setting.value)(&config) != before {
                (setting.cycle)(&mut config);
                steps += 1;
                assert!(steps <= 3, "{} never comes back", setting.name);
            }
        }
    }

    #[test]
    fn control_bar_is_never_off() {
        let mut config = Config::parse("[bars]\ncontrol = \"off\"\n").unwrap();
        assert_eq!(config.bars.control(), Placement::Bottom);
        let setting = SETTINGS.iter().find(|s| s.name == "Control bar").unwrap();
        for _ in 0..4 {
            (setting.cycle)(&mut config);
            assert_ne!(config.bars.control, Placement::Off);
        }
    }

    #[test]
    fn bars_tell_which_sides_are_used() {
        let config = Config::parse("[bars]\ntabs = \"top\"\npath = \"off\"\n").unwrap();
        assert!(config.bars.uses(Placement::Top));
        assert!(config.bars.uses(Placement::Bottom));
        let config =
            Config::parse("[bars]\ncontrol = \"top\"\ntabs = \"top\"\npath = \"top\"\n").unwrap();
        assert!(!config.bars.uses(Placement::Bottom));
    }
}
