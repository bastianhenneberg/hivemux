//! The key bindings after the prefix. This table is the single source for
//! what a key does and for every place that shows keys to the user: the
//! which-key menu, the help overlay and `hivemux keys`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::layout::Direction;

/// The prefix key, Ctrl+B like tmux and herdr.
pub const PREFIX: Key = Key::ctrl(KeyCode::Char('b'));
pub const PREFIX_LABEL: &str = "Ctrl+B";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SplitRow,
    SplitColumn,
    ClosePane,
    NextPane,
    Focus(Direction),
    Resize(Direction, u16),
    Detach,
    SessionMenu,
    Settings,
    Quit,
    Help,
    SendPrefix,
    Cancel,
}

impl Command {
    /// Repeatable commands keep the prefix active for a moment, so arrow keys
    /// can be pressed again without it.
    pub fn repeatable(self) -> bool {
        matches!(self, Command::Focus(_) | Command::Resize(..))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Panes,
    Navigate,
    Session,
}

impl Group {
    pub const ALL: [Group; 3] = [Group::Panes, Group::Navigate, Group::Session];

    pub fn title(self) -> &'static str {
        match self {
            Group::Panes => "Panes",
            Group::Navigate => "Navigate",
            Group::Session => "Session",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Key {
    code: KeyCode,
    mods: KeyModifiers,
}

impl Key {
    const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::NONE,
        }
    }

    const fn ctrl(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::CONTROL,
        }
    }

    const fn alt(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::ALT,
        }
    }

    /// Whether `event` is this key. Shift is ignored: it is part of the
    /// character already (`%`, `Q`), and terminals disagree on whether they
    /// report it on top.
    pub fn matches(self, event: KeyEvent) -> bool {
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT;
        event.code == self.code && event.modifiers & relevant == self.mods
    }
}

/// One line in the menu. Several keys can share a line, like the four arrows.
pub struct Binding {
    pub label: &'static str,
    pub description: &'static str,
    pub group: Group,
    pub keys: &'static [(Key, Command)],
}

use Command as C;
use Direction::{Down, Left, Right, Up};
use KeyCode::{Char, Esc};

pub const BINDINGS: &[Binding] = &[
    Binding {
        label: "%",
        description: "split side by side",
        group: Group::Panes,
        keys: &[(Key::plain(Char('%')), C::SplitRow)],
    },
    Binding {
        label: "\"",
        description: "split top and bottom",
        group: Group::Panes,
        keys: &[(Key::plain(Char('"')), C::SplitColumn)],
    },
    Binding {
        label: "x",
        description: "close pane",
        group: Group::Panes,
        keys: &[(Key::plain(Char('x')), C::ClosePane)],
    },
    Binding {
        label: "←↑↓→",
        description: "focus pane",
        group: Group::Navigate,
        keys: &[
            (Key::plain(KeyCode::Left), C::Focus(Left)),
            (Key::plain(KeyCode::Right), C::Focus(Right)),
            (Key::plain(KeyCode::Up), C::Focus(Up)),
            (Key::plain(KeyCode::Down), C::Focus(Down)),
        ],
    },
    Binding {
        label: "o",
        description: "next pane",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('o')), C::NextPane)],
    },
    Binding {
        label: "Ctrl ←↑↓→",
        description: "resize by 1",
        group: Group::Navigate,
        keys: &[
            (Key::ctrl(KeyCode::Left), C::Resize(Left, 1)),
            (Key::ctrl(KeyCode::Right), C::Resize(Right, 1)),
            (Key::ctrl(KeyCode::Up), C::Resize(Up, 1)),
            (Key::ctrl(KeyCode::Down), C::Resize(Down, 1)),
        ],
    },
    Binding {
        label: "Alt ←↑↓→",
        description: "resize by 5",
        group: Group::Navigate,
        keys: &[
            (Key::alt(KeyCode::Left), C::Resize(Left, 5)),
            (Key::alt(KeyCode::Right), C::Resize(Right, 5)),
            (Key::alt(KeyCode::Up), C::Resize(Up, 5)),
            (Key::alt(KeyCode::Down), C::Resize(Down, 5)),
        ],
    },
    Binding {
        label: "q",
        description: "quit menu",
        group: Group::Session,
        keys: &[(Key::plain(Char('q')), C::SessionMenu)],
    },
    Binding {
        label: "d",
        description: "detach",
        group: Group::Session,
        keys: &[(Key::plain(Char('d')), C::Detach)],
    },
    Binding {
        label: "Q",
        description: "quit hivemux",
        group: Group::Session,
        keys: &[(Key::plain(Char('Q')), C::Quit)],
    },
    Binding {
        label: ",",
        description: "settings",
        group: Group::Session,
        keys: &[(Key::plain(Char(',')), C::Settings)],
    },
    Binding {
        label: "?",
        description: "all keys",
        group: Group::Session,
        keys: &[(Key::plain(Char('?')), C::Help)],
    },
    Binding {
        label: PREFIX_LABEL,
        description: "send Ctrl+B",
        group: Group::Session,
        keys: &[(PREFIX, C::SendPrefix)],
    },
    Binding {
        label: "Esc",
        description: "cancel",
        group: Group::Session,
        keys: &[(Key::plain(Esc), C::Cancel)],
    },
];

/// The command bound to `event` after the prefix.
pub fn lookup(event: KeyEvent) -> Option<Command> {
    BINDINGS
        .iter()
        .flat_map(|binding| binding.keys)
        .find(|(key, _)| key.matches(event))
        .map(|&(_, command)| command)
}

pub fn in_group(group: Group) -> impl Iterator<Item = &'static Binding> {
    BINDINGS
        .iter()
        .filter(move |binding| binding.group == group)
}

/// The key reference as plain text, for `hivemux keys`.
pub fn reference() -> String {
    let width = BINDINGS
        .iter()
        .map(|b| b.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = format!("Press {PREFIX_LABEL}, then:\n");
    for group in Group::ALL {
        out.push_str(&format!("\n{}\n", group.title()));
        for binding in in_group(group) {
            let pad = width - binding.label.chars().count();
            out.push_str(&format!(
                "  {}{}  {}\n",
                binding.label,
                " ".repeat(pad),
                binding.description
            ));
        }
    }
    out.push_str(
        "\nFocus and resize repeat: for a moment afterwards, arrow keys work without the prefix.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn looks_up_commands() {
        let none = KeyModifiers::NONE;
        assert_eq!(lookup(event(Char('%'), none)), Some(C::SplitRow));
        assert_eq!(lookup(event(KeyCode::Left, none)), Some(C::Focus(Left)));
        assert_eq!(
            lookup(event(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(C::Resize(Up, 1))
        );
        assert_eq!(
            lookup(event(KeyCode::Up, KeyModifiers::ALT)),
            Some(C::Resize(Up, 5))
        );
        assert_eq!(
            lookup(event(Char('b'), KeyModifiers::CONTROL)),
            Some(C::SendPrefix)
        );
        assert_eq!(lookup(event(Char('z'), none)), None);
    }

    #[test]
    fn shift_is_ignored_but_ctrl_and_alt_are_not() {
        assert_eq!(
            lookup(event(Char('%'), KeyModifiers::SHIFT)),
            Some(C::SplitRow)
        );
        assert_eq!(lookup(event(Char('Q'), KeyModifiers::SHIFT)), Some(C::Quit));
        assert_eq!(
            lookup(event(Char('q'), KeyModifiers::NONE)),
            Some(C::SessionMenu)
        );
        assert_eq!(lookup(event(Char('x'), KeyModifiers::CONTROL)), None);
    }

    #[test]
    fn every_key_is_bound_once() {
        let keys: Vec<Key> = BINDINGS
            .iter()
            .flat_map(|b| b.keys)
            .map(|&(k, _)| k)
            .collect();
        for (i, key) in keys.iter().enumerate() {
            assert!(!keys[i + 1..].contains(key), "{key:?} is bound twice");
        }
    }

    #[test]
    fn reference_lists_every_binding() {
        let text = reference();
        for binding in BINDINGS {
            assert!(
                text.contains(binding.description),
                "{} missing",
                binding.description
            );
        }
    }
}
