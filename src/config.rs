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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub which_key: WhichKey,
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
    fn cycling_twice_returns_to_the_start() {
        for setting in SETTINGS {
            let mut config = Config::default();
            let before = (setting.value)(&config);
            (setting.cycle)(&mut config);
            assert_ne!((setting.value)(&config), before, "{}", setting.name);
            (setting.cycle)(&mut config);
            assert_eq!((setting.value)(&config), before, "{}", setting.name);
        }
    }
}
