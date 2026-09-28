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
    Move(Direction, u16),
    ToggleFloat,
    NewFloat,
    Detach,
    SessionMenu,
    Settings,
    Workspace(u8),
    NewWorkspace,
    NextWorkspace,
    PrevWorkspace,
    NextFloat,
    /// Scroll back and select text with the keyboard.
    CopyMode,
    /// Copy mode, scrolled up half a page right away.
    ScrollBack,
    Quit,
    Help,
    SendPrefix,
    /// Opens a submenu, like a group in which-key.nvim.
    Open(Menu),
    /// Goes back from a submenu to the main menu.
    Back,
    Cancel,
}

impl Command {
    /// Repeatable commands keep the prefix active for a moment, so arrow keys
    /// can be pressed again without it.
    pub fn repeatable(self) -> bool {
        matches!(
            self,
            Command::Focus(_)
                | Command::Resize(..)
                | Command::Move(..)
                | Command::NextFloat
                | Command::NextWorkspace
                | Command::PrevWorkspace
        )
    }
}

/// The menus after the prefix: the main one and its submenus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Root,
    Floating,
    Workspaces,
}

impl Menu {
    pub const ALL: [Menu; 3] = [Menu::Root, Menu::Floating, Menu::Workspaces];

    pub fn groups(self) -> &'static [Group] {
        match self {
            Menu::Root => &[Group::Panes, Group::Navigate, Group::Session],
            Menu::Floating => &[Group::Floating],
            Menu::Workspaces => &[Group::Workspaces],
        }
    }

    /// The keys that open this menu, after the prefix, e.g. `f`.
    pub fn path(self) -> &'static str {
        match self {
            Menu::Root => "",
            Menu::Floating => "f",
            Menu::Workspaces => "w",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Panes,
    Navigate,
    Session,
    Floating,
    Workspaces,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Panes => "Panes",
            Group::Navigate => "Navigate",
            Group::Session => "Session",
            Group::Floating => "Floating panes",
            Group::Workspaces => "Workspaces",
        }
    }

    pub fn menu(self) -> Menu {
        match self {
            Group::Panes | Group::Navigate | Group::Session => Menu::Root,
            Group::Floating => Menu::Floating,
            Group::Workspaces => Menu::Workspaces,
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

    const fn shift(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::SHIFT,
        }
    }

    const fn alt(code: KeyCode) -> Self {
        Self {
            code,
            mods: KeyModifiers::ALT,
        }
    }

    /// Whether `event` is this key. For characters Shift is ignored: it is
    /// part of the character already (`%`, `Q`), and terminals disagree on
    /// whether they report it on top. For other keys, like arrows, Shift
    /// counts.
    pub fn matches(self, event: KeyEvent) -> bool {
        let relevant = match self.code {
            KeyCode::Char(_) => KeyModifiers::CONTROL | KeyModifiers::ALT,
            _ => KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
        };
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
        label: "f",
        description: "floating…",
        group: Group::Panes,
        keys: &[(Key::plain(Char('f')), C::Open(Menu::Floating))],
    },
    Binding {
        label: "Shift ←↑↓→",
        description: "move floating pane",
        group: Group::Panes,
        keys: &[
            (Key::shift(KeyCode::Left), C::Move(Left, 1)),
            (Key::shift(KeyCode::Right), C::Move(Right, 1)),
            (Key::shift(KeyCode::Up), C::Move(Up, 1)),
            (Key::shift(KeyCode::Down), C::Move(Down, 1)),
        ],
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
        label: "[",
        description: "copy mode",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('[')), C::CopyMode)],
    },
    Binding {
        label: "u",
        description: "scroll back a page",
        group: Group::Navigate,
        keys: &[
            (Key::plain(Char('u')), C::ScrollBack),
            (Key::plain(KeyCode::PageUp), C::ScrollBack),
        ],
    },
    Binding {
        label: "o",
        description: "next pane",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('o')), C::NextPane)],
    },
    Binding {
        label: "1-9",
        description: "go to workspace",
        group: Group::Navigate,
        keys: &[
            (Key::plain(Char('1')), C::Workspace(1)),
            (Key::plain(Char('2')), C::Workspace(2)),
            (Key::plain(Char('3')), C::Workspace(3)),
            (Key::plain(Char('4')), C::Workspace(4)),
            (Key::plain(Char('5')), C::Workspace(5)),
            (Key::plain(Char('6')), C::Workspace(6)),
            (Key::plain(Char('7')), C::Workspace(7)),
            (Key::plain(Char('8')), C::Workspace(8)),
            (Key::plain(Char('9')), C::Workspace(9)),
        ],
    },
    Binding {
        label: "w",
        description: "workspaces…",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('w')), C::Open(Menu::Workspaces))],
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
    // Floating panes, after the prefix and `f`.
    Binding {
        label: "f",
        description: "float / tile this pane",
        group: Group::Floating,
        keys: &[(Key::plain(Char('f')), C::ToggleFloat)],
    },
    Binding {
        label: "n",
        description: "new floating pane",
        group: Group::Floating,
        keys: &[(Key::plain(Char('n')), C::NewFloat)],
    },
    Binding {
        label: "o",
        description: "next floating pane",
        group: Group::Floating,
        keys: &[
            (Key::plain(Char('o')), C::NextFloat),
            (Key::plain(KeyCode::Tab), C::NextFloat),
        ],
    },
    Binding {
        label: "←↑↓→",
        description: "move",
        group: Group::Floating,
        keys: &[
            (Key::plain(KeyCode::Left), C::Move(Left, 1)),
            (Key::plain(KeyCode::Right), C::Move(Right, 1)),
            (Key::plain(KeyCode::Up), C::Move(Up, 1)),
            (Key::plain(KeyCode::Down), C::Move(Down, 1)),
        ],
    },
    Binding {
        label: "Ctrl ←↑↓→",
        description: "resize by 1",
        group: Group::Floating,
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
        group: Group::Floating,
        keys: &[
            (Key::alt(KeyCode::Left), C::Resize(Left, 5)),
            (Key::alt(KeyCode::Right), C::Resize(Right, 5)),
            (Key::alt(KeyCode::Up), C::Resize(Up, 5)),
            (Key::alt(KeyCode::Down), C::Resize(Down, 5)),
        ],
    },
    Binding {
        label: "⌫",
        description: "back",
        group: Group::Floating,
        keys: &[(Key::plain(KeyCode::Backspace), C::Back)],
    },
    Binding {
        label: "Esc",
        description: "cancel",
        group: Group::Floating,
        keys: &[(Key::plain(Esc), C::Cancel)],
    },
    // Workspaces, after the prefix and `w`.
    Binding {
        label: "1-9",
        description: "go to workspace",
        group: Group::Workspaces,
        keys: &[
            (Key::plain(Char('1')), C::Workspace(1)),
            (Key::plain(Char('2')), C::Workspace(2)),
            (Key::plain(Char('3')), C::Workspace(3)),
            (Key::plain(Char('4')), C::Workspace(4)),
            (Key::plain(Char('5')), C::Workspace(5)),
            (Key::plain(Char('6')), C::Workspace(6)),
            (Key::plain(Char('7')), C::Workspace(7)),
            (Key::plain(Char('8')), C::Workspace(8)),
            (Key::plain(Char('9')), C::Workspace(9)),
        ],
    },
    Binding {
        label: "c",
        description: "new workspace",
        group: Group::Workspaces,
        keys: &[(Key::plain(Char('c')), C::NewWorkspace)],
    },
    Binding {
        label: "n p ←→",
        description: "next / previous",
        group: Group::Workspaces,
        keys: &[
            (Key::plain(Char('n')), C::NextWorkspace),
            (Key::plain(Char('p')), C::PrevWorkspace),
            (Key::plain(KeyCode::Right), C::NextWorkspace),
            (Key::plain(KeyCode::Left), C::PrevWorkspace),
        ],
    },
    Binding {
        label: "⌫",
        description: "back",
        group: Group::Workspaces,
        keys: &[(Key::plain(KeyCode::Backspace), C::Back)],
    },
    Binding {
        label: "Esc",
        description: "cancel",
        group: Group::Workspaces,
        keys: &[(Key::plain(Esc), C::Cancel)],
    },
];

/// The command bound to `event` in `menu`.
pub fn lookup(menu: Menu, event: KeyEvent) -> Option<Command> {
    BINDINGS
        .iter()
        .filter(|binding| binding.group.menu() == menu)
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
    for &group in Menu::ALL.iter().flat_map(|m| m.groups()) {
        match group.menu() {
            Menu::Root => out.push_str(&format!("\n{}\n", group.title())),
            menu => out.push_str(&format!(
                "\n{} ({PREFIX_LABEL}, {}, then)\n",
                group.title(),
                menu.path()
            )),
        }
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
        "\nFocus, move and resize repeat: for a moment afterwards, the same keys work without the prefix.\n",
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
        assert_eq!(
            lookup(Menu::Root, event(Char('%'), none)),
            Some(C::SplitRow)
        );
        assert_eq!(
            lookup(Menu::Root, event(KeyCode::Left, none)),
            Some(C::Focus(Left))
        );
        assert_eq!(
            lookup(Menu::Root, event(KeyCode::Up, KeyModifiers::CONTROL)),
            Some(C::Resize(Up, 1))
        );
        assert_eq!(
            lookup(Menu::Root, event(KeyCode::Up, KeyModifiers::ALT)),
            Some(C::Resize(Up, 5))
        );
        assert_eq!(
            lookup(Menu::Root, event(Char('b'), KeyModifiers::CONTROL)),
            Some(C::SendPrefix)
        );
        assert_eq!(lookup(Menu::Root, event(Char('z'), none)), None);
    }

    #[test]
    fn shift_is_ignored_but_ctrl_and_alt_are_not() {
        assert_eq!(
            lookup(Menu::Root, event(Char('%'), KeyModifiers::SHIFT)),
            Some(C::SplitRow)
        );
        assert_eq!(
            lookup(Menu::Root, event(Char('Q'), KeyModifiers::SHIFT)),
            Some(C::Quit)
        );
        assert_eq!(
            lookup(Menu::Root, event(Char('q'), KeyModifiers::NONE)),
            Some(C::SessionMenu)
        );
        assert_eq!(
            lookup(Menu::Root, event(Char('x'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn shift_counts_for_arrows() {
        assert_eq!(
            lookup(Menu::Root, event(KeyCode::Left, KeyModifiers::NONE)),
            Some(C::Focus(Left))
        );
        assert_eq!(
            lookup(Menu::Root, event(KeyCode::Left, KeyModifiers::SHIFT)),
            Some(C::Move(Left, 1))
        );
    }

    #[test]
    fn submenus_have_their_own_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            lookup(Menu::Root, event(Char('f'), none)),
            Some(C::Open(Menu::Floating))
        );
        assert_eq!(
            lookup(Menu::Floating, event(Char('f'), none)),
            Some(C::ToggleFloat)
        );
        assert_eq!(
            lookup(Menu::Floating, event(KeyCode::Left, none)),
            Some(C::Move(Left, 1))
        );
        assert_eq!(
            lookup(Menu::Workspaces, event(Char('3'), none)),
            Some(C::Workspace(3))
        );
        assert_eq!(
            lookup(Menu::Workspaces, event(KeyCode::Backspace, none)),
            Some(C::Back)
        );
        assert_eq!(lookup(Menu::Workspaces, event(Char('%'), none)), None);
    }

    #[test]
    fn every_key_is_bound_once_per_menu() {
        for menu in Menu::ALL {
            let keys: Vec<Key> = BINDINGS
                .iter()
                .filter(|b| b.group.menu() == menu)
                .flat_map(|b| b.keys)
                .map(|&(k, _)| k)
                .collect();
            for (i, key) in keys.iter().enumerate() {
                assert!(
                    !keys[i + 1..].contains(key),
                    "{key:?} is bound twice in {menu:?}"
                );
            }
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
