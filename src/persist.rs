//! Saving the session's layout, so a server that dies with the machine can
//! bring it back: workspaces, splits, floating panes, each pane's directory,
//! and agents that can be resumed.
//!
//! Processes and screen contents cannot survive a restart. The shells start
//! fresh in their old directories, agents with their resume command.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::PaneId;
use crate::workspace::Workspace;

const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    pub version: u32,
    /// The workspace that was shown.
    pub active: u8,
    pub workspaces: BTreeMap<u8, Workspace>,
    pub panes: BTreeMap<PaneId, SavedPane>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedPane {
    pub cwd: Option<PathBuf>,
    /// The agent running in the pane, if any.
    pub agent: Option<String>,
    /// The agent's session id, from its hooks.
    pub session: Option<String>,
    /// The name the user gave the pane.
    #[serde(default)]
    pub name: Option<String>,
}

impl Saved {
    pub fn new(
        active: u8,
        workspaces: BTreeMap<u8, Workspace>,
        panes: BTreeMap<PaneId, SavedPane>,
    ) -> Self {
        Self {
            version: VERSION,
            active,
            workspaces,
            panes,
        }
    }
}

impl SavedPane {
    /// The command that picks the agent's conversation up again, typed into
    /// the fresh shell.
    pub fn resume_command(&self) -> Option<String> {
        let session = self.session.as_deref()?;
        // Session ids are uuids. Anything else is not typed into a shell.
        if session.is_empty()
            || !session
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        match self.agent.as_deref()? {
            "claude" => Some(format!("claude --resume {session}")),
            "codex" => Some(format!("codex resume {session}")),
            _ => None,
        }
    }
}

/// Where the session is saved: `$HIVEMUX_STATE`, else next to a socket set
/// with `$HIVEMUX_SOCKET` (so test servers keep their own), else
/// `$XDG_STATE_HOME/hivemux/session.json` for the default session and
/// `<name>.json` for named ones, falling back to `~/.local/state`.
/// Not the runtime directory of the socket, that one is gone after a reboot.
pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HIVEMUX_STATE") {
        return Ok(PathBuf::from(path));
    }
    if let Some(socket) = std::env::var_os("HIVEMUX_SOCKET") {
        return Ok(PathBuf::from(socket).with_extension("state.json"));
    }
    path_for(&crate::protocol::session_name())
}

/// Where session `name` is saved, without the overrides of `path`.
pub fn path_for(name: &str) -> Result<PathBuf> {
    let file = if name == crate::protocol::DEFAULT_SESSION {
        "session.json".to_owned()
    } else {
        format!("{name}.json")
    };
    Ok(state_dir()?.join(file))
}

/// `$XDG_STATE_HOME/hivemux`, falling back to `~/.local/state/hivemux`.
fn state_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".local/state")
        }
    };
    Ok(base.join("hivemux"))
}

/// A save file: the layouts of several sessions at once, like a
/// tmux-resurrect save. `last` is kept up to date by every session that
/// ends on purpose, others are made with `hivemux save NAME`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Hive {
    pub sessions: BTreeMap<String, HiveSession>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HiveSession {
    /// When it was saved, in seconds since 1970.
    pub saved_at: u64,
    pub layout: Saved,
}

/// The save every session ending on purpose puts itself into.
pub const LAST: &str = "last";

/// Where save files go: `saves` next to the session files, or next to the
/// socket or state file a test set.
pub fn saves_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("HIVEMUX_STATE") {
        return Ok(PathBuf::from(path).with_extension("saves"));
    }
    if let Some(socket) = std::env::var_os("HIVEMUX_SOCKET") {
        return Ok(PathBuf::from(socket).with_extension("saves"));
    }
    Ok(state_dir()?.join("saves"))
}

/// The save file called `name`.
pub fn save_path(name: &str) -> Result<PathBuf> {
    if !crate::protocol::valid_session_name(name) {
        anyhow::bail!("{name:?} is no save name: letters, digits, - and _ only");
    }
    Ok(saves_dir()?.join(format!("{name}.json")))
}

pub fn load_hive(name: &str) -> Result<Option<Hive>> {
    let path = save_path(name)?;
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let hive: Hive =
        serde_json::from_str(&text).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Some(hive))
}

pub fn write_hive(name: &str, hive: &Hive) -> Result<PathBuf> {
    let path = save_path(name)?;
    write(&path, &serde_json::to_string_pretty(hive)?)?;
    Ok(path)
}

/// Puts `session`'s layout into the save `last`, next to the sessions that
/// are there already.
pub fn keep_in_last(session: &str, layout: Saved) -> Result<()> {
    let mut hive = load_hive(LAST).ok().flatten().unwrap_or_default();
    hive.sessions.insert(
        session.to_owned(),
        HiveSession {
            saved_at: now(),
            layout,
        },
    );
    write_hive(LAST, &hive)?;
    Ok(())
}

/// The save files by name.
pub fn saves() -> Vec<String> {
    let Ok(entries) = saves_dir().and_then(|dir| Ok(fs::read_dir(dir)?)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.strip_suffix(".json")?.to_owned();
            Some(name)
        })
        .collect();
    names.sort();
    names
}

/// Seconds since 1970.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub fn to_json(saved: &Saved) -> Result<String> {
    Ok(serde_json::to_string_pretty(saved)?)
}

/// Writes `json` to `path` through a temporary file, so a crash while
/// writing never leaves half a file.
pub fn write(path: &Path, json: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// The saved session, `None` if there is none. A file that cannot be read
/// is an error, the caller starts fresh.
pub fn load(path: &Path) -> Result<Option<Saved>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let saved: Saved =
        serde_json::from_str(&text).with_context(|| format!("cannot read {}", path.display()))?;
    if saved.version != VERSION {
        anyhow::bail!("{} is from another hivemux version", path.display());
    }
    Ok(Some(saved))
}

pub fn remove(path: &Path) {
    let _ = fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Axis;
    use ratatui::layout::Rect;

    fn sample() -> Saved {
        let mut one = Workspace::new(0);
        one.split(Axis::Row, 1);
        one.add_float(2, Rect::new(0, 0, 100, 40));
        let workspaces = BTreeMap::from([(1, one), (3, Workspace::new(5))]);
        let panes = BTreeMap::from([
            (
                0,
                SavedPane {
                    cwd: Some("/tmp".into()),
                    ..Default::default()
                },
            ),
            (
                1,
                SavedPane {
                    cwd: Some("/home".into()),
                    agent: Some("claude".into()),
                    session: Some("0b8a-42".into()),
                    name: Some("backend".into()),
                },
            ),
            (2, SavedPane::default()),
            (5, SavedPane::default()),
        ]);
        Saved::new(3, workspaces, panes)
    }

    #[test]
    fn survives_a_round_trip_through_a_file() {
        let dir = std::env::temp_dir().join(format!("hivemux-test-{}", std::process::id()));
        let path = dir.join("session.json");
        let saved = sample();
        write(&path, &to_json(&saved).unwrap()).unwrap();
        assert_eq!(load(&path).unwrap(), Some(saved));
        remove(&path);
        assert_eq!(load(&path).unwrap(), None);
        let _ = fs::remove_dir(dir);
    }

    #[test]
    fn restored_layout_keeps_its_panes() {
        let saved = sample();
        let text = to_json(&saved).unwrap();
        let back: Saved = serde_json::from_str(&text).unwrap();
        assert_eq!(back.workspaces[&1].panes(), vec![0, 1, 2]);
        assert!(back.workspaces[&1].is_floating(2));
        assert_eq!(back.active, 3);
    }

    #[test]
    fn only_known_agents_with_clean_session_ids_resume() {
        let claude = |session: &str| SavedPane {
            agent: Some("claude".into()),
            session: Some(session.into()),
            ..Default::default()
        };
        assert_eq!(
            claude("9f1c-77ab").resume_command().as_deref(),
            Some("claude --resume 9f1c-77ab")
        );
        assert_eq!(claude("x; rm -rf ~").resume_command(), None);
        assert_eq!(claude("").resume_command(), None);
        let aider = SavedPane {
            agent: Some("aider".into()),
            session: Some("abc".into()),
            ..Default::default()
        };
        assert_eq!(aider.resume_command(), None);
    }

    #[test]
    fn last_collects_sessions_that_ended() {
        let dir = std::env::temp_dir().join(format!("hivemux-saves-{}", std::process::id()));
        // SAFETY: the only test that sets HIVEMUX_STATE.
        unsafe { std::env::set_var("HIVEMUX_STATE", dir.join("state.json")) };
        keep_in_last("work", sample()).unwrap();
        keep_in_last("play", Saved::new(1, BTreeMap::new(), BTreeMap::new())).unwrap();
        keep_in_last("work", sample()).unwrap();
        let hive = load_hive(LAST).unwrap().unwrap();
        assert_eq!(hive.sessions.keys().collect::<Vec<_>>(), ["play", "work"]);
        assert_eq!(hive.sessions["work"].layout, sample());
        assert_eq!(saves(), ["last"]);
        assert!(save_path("../evil").is_err());
        unsafe { std::env::remove_var("HIVEMUX_STATE") };
        let _ = fs::remove_dir_all(dir);
    }
}
