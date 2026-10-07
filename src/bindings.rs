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
    RenamePane,
    Sessions,
    Hives,
    /// Start a new session, asking for its name.
    NewSession,
    /// Rename this session.
    RenameSession,
    /// End this session, asking first.
    EndSession,
    /// Save every running session as a hive, asking for its name.
    SaveHive,
    PickPane,
    ReloadConfig,
    ScrollbackEditor,
    Zoom,
    Swap(bool),
    Equalize,
    LastPane,
    RenameWorkspace,
    /// Give the sidebar the keyboard.
    FocusSidebar,
    /// Show or hide the sidebar.
    ToggleSidebar,
    /// Hide bars, sidebar and borders and show only the focused pane.
    FocusMode,
    /// Focus the agent waiting longest for the user.
    JumpToWaiting,
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
    /// The user's own commands from `[[commands]]` in the config.
    Commands,
    /// Sessions and hives.
    Session,
}

impl Menu {
    pub const ALL: [Menu; 5] = [
        Menu::Root,
        Menu::Floating,
        Menu::Workspaces,
        Menu::Session,
        Menu::Commands,
    ];

    pub fn groups(self) -> &'static [Group] {
        match self {
            Menu::Root => &[Group::Panes, Group::Navigate, Group::View, Group::Session],
            Menu::Floating => &[Group::Floating],
            Menu::Workspaces => &[Group::Workspaces],
            Menu::Commands => &[Group::Commands],
            Menu::Session => &[Group::Sessions],
        }
    }

    /// The keys that open this menu, after the prefix, e.g. `f`.
    pub fn path(self) -> &'static str {
        match self {
            Menu::Root => "",
            Menu::Floating => "f",
            Menu::Workspaces => "w",
            Menu::Commands => "c",
            Menu::Session => "s",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    Panes,
    Navigate,
    View,
    Session,
    /// Keys of the main menu that the which-key popup lists only in its
    /// footer: rarely needed, or variations of listed ones.
    More,
    Floating,
    Workspaces,
    Commands,
    /// The session submenu.
    Sessions,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Panes => "Panes",
            Group::Navigate => "Go to",
            Group::View => "View",
            Group::Session => "Session",
            Group::More => "More",
            Group::Floating => "Floating panes",
            Group::Workspaces => "Windows",
            Group::Commands => "Your commands",
            Group::Sessions => "Sessions and hives",
        }
    }

    /// The colour of the group's heading in menus.
    pub fn color(self) -> ratatui::style::Color {
        let t = crate::theme::current();
        match self {
            Group::Panes | Group::Workspaces => t.blue,
            Group::Navigate | Group::Floating => t.cyan,
            Group::View => t.orange,
            Group::Session | Group::Commands | Group::Sessions => t.magenta,
            Group::More => t.muted,
        }
    }

    pub fn menu(self) -> Menu {
        match self {
            Group::Panes | Group::Navigate | Group::View | Group::Session | Group::More => {
                Menu::Root
            }
            Group::Floating => Menu::Floating,
            Group::Workspaces => Menu::Workspaces,
            Group::Commands => Menu::Commands,
            Group::Sessions => Menu::Session,
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
    // The main menu, after the prefix.
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
        label: "z",
        description: "zoom pane",
        group: Group::Panes,
        keys: &[(Key::plain(Char('z')), C::Zoom)],
    },
    Binding {
        label: "{ }",
        description: "swap with prev/next",
        group: Group::Panes,
        keys: &[
            (Key::plain(Char('{')), C::Swap(false)),
            (Key::plain(Char('}')), C::Swap(true)),
        ],
    },
    Binding {
        label: "=",
        description: "equal sizes",
        group: Group::Panes,
        keys: &[(Key::plain(Char('=')), C::Equalize)],
    },
    Binding {
        label: "r",
        description: "name pane",
        group: Group::Panes,
        keys: &[(Key::plain(Char('r')), C::RenamePane)],
    },
    Binding {
        label: "f",
        description: "floating…",
        group: Group::Panes,
        keys: &[(Key::plain(Char('f')), C::Open(Menu::Floating))],
    },
    Binding {
        label: "←↑↓→",
        description: "pane there",
        group: Group::Navigate,
        keys: &[
            (Key::plain(KeyCode::Left), C::Focus(Left)),
            (Key::plain(KeyCode::Right), C::Focus(Right)),
            (Key::plain(KeyCode::Up), C::Focus(Up)),
            (Key::plain(KeyCode::Down), C::Focus(Down)),
        ],
    },
    Binding {
        label: "o ;",
        description: "next / last pane",
        group: Group::Navigate,
        keys: &[
            (Key::plain(Char('o')), C::NextPane),
            (Key::plain(Char(';')), C::LastPane),
        ],
    },
    Binding {
        label: "g",
        description: "any pane…",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('g')), C::PickPane)],
    },
    Binding {
        label: "a",
        description: "waiting agent",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('a')), C::JumpToWaiting)],
    },
    Binding {
        label: "1-9",
        description: "window",
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
        description: "windows…",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('w')), C::Open(Menu::Workspaces))],
    },
    Binding {
        label: "n",
        description: "new window",
        group: Group::Navigate,
        keys: &[(Key::plain(Char('n')), C::NewWorkspace)],
    },
    Binding {
        label: "[",
        description: "copy mode",
        group: Group::View,
        keys: &[(Key::plain(Char('[')), C::CopyMode)],
    },
    Binding {
        label: "u",
        description: "page up",
        group: Group::View,
        keys: &[
            (Key::plain(Char('u')), C::ScrollBack),
            (Key::plain(KeyCode::PageUp), C::ScrollBack),
        ],
    },
    Binding {
        label: "E",
        description: "history in $EDITOR",
        group: Group::View,
        keys: &[(Key::plain(Char('E')), C::ScrollbackEditor)],
    },
    Binding {
        label: "b",
        description: "sidebar on/off",
        group: Group::View,
        keys: &[(Key::plain(Char('b')), C::ToggleSidebar)],
    },
    Binding {
        label: "e",
        description: "into the sidebar",
        group: Group::View,
        keys: &[(Key::plain(Char('e')), C::FocusSidebar)],
    },
    Binding {
        label: "F",
        description: "focus mode",
        group: Group::View,
        keys: &[(Key::plain(Char('F')), C::FocusMode)],
    },
    Binding {
        label: "q",
        description: "quit…",
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
        label: "s",
        description: "session…",
        group: Group::Session,
        keys: &[(Key::plain(Char('s')), C::Open(Menu::Session))],
    },
    Binding {
        label: "c",
        description: "commands…",
        group: Group::Session,
        keys: &[(Key::plain(Char('c')), C::Open(Menu::Commands))],
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
    // Listed in the footer only.
    Binding {
        label: "S",
        description: "session list",
        group: Group::More,
        keys: &[(Key::plain(Char('S')), C::Sessions)],
    },
    Binding {
        label: "H",
        description: "hives",
        group: Group::More,
        keys: &[(Key::plain(Char('H')), C::Hives)],
    },
    Binding {
        label: "Ctrl/Alt ←↑↓→",
        description: "resize",
        group: Group::More,
        keys: &[
            (Key::ctrl(KeyCode::Left), C::Resize(Left, 1)),
            (Key::ctrl(KeyCode::Right), C::Resize(Right, 1)),
            (Key::ctrl(KeyCode::Up), C::Resize(Up, 1)),
            (Key::ctrl(KeyCode::Down), C::Resize(Down, 1)),
            (Key::alt(KeyCode::Left), C::Resize(Left, 5)),
            (Key::alt(KeyCode::Right), C::Resize(Right, 5)),
            (Key::alt(KeyCode::Up), C::Resize(Up, 5)),
            (Key::alt(KeyCode::Down), C::Resize(Down, 5)),
        ],
    },
    Binding {
        label: "Shift ←↑↓→",
        description: "move float",
        group: Group::More,
        keys: &[
            (Key::shift(KeyCode::Left), C::Move(Left, 1)),
            (Key::shift(KeyCode::Right), C::Move(Right, 1)),
            (Key::shift(KeyCode::Up), C::Move(Up, 1)),
            (Key::shift(KeyCode::Down), C::Move(Down, 1)),
        ],
    },
    Binding {
        label: "Q",
        description: "quit hivemux",
        group: Group::More,
        keys: &[(Key::plain(Char('Q')), C::Quit)],
    },
    Binding {
        label: "R",
        description: "reload config",
        group: Group::More,
        keys: &[(Key::plain(Char('R')), C::ReloadConfig)],
    },
    Binding {
        label: PREFIX_LABEL,
        description: "sends Ctrl+B",
        group: Group::More,
        keys: &[(PREFIX, C::SendPrefix)],
    },
    Binding {
        label: "Esc",
        description: "cancel",
        group: Group::More,
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
    // The user's commands, after the prefix and `c`. The commands themselves
    // come from the config, these are only the keys every menu has.
    Binding {
        label: "⌫",
        description: "back",
        group: Group::Commands,
        keys: &[(Key::plain(KeyCode::Backspace), C::Back)],
    },
    Binding {
        label: "Esc",
        description: "cancel",
        group: Group::Commands,
        keys: &[(Key::plain(Esc), C::Cancel)],
    },
    // Workspaces, after the prefix and `w`.
    Binding {
        label: "1-9",
        description: "go to window",
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
        label: "r",
        description: "name window",
        group: Group::Workspaces,
        keys: &[(Key::plain(Char('r')), C::RenameWorkspace)],
    },
    Binding {
        label: "c",
        description: "new window",
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
    // The session submenu.
    Binding {
        label: "s S",
        description: "switch…",
        group: Group::Sessions,
        keys: &[
            (Key::plain(Char('s')), C::Sessions),
            (Key::plain(Char('S')), C::Sessions),
        ],
    },
    Binding {
        label: "n",
        description: "new session…",
        group: Group::Sessions,
        keys: &[(Key::plain(Char('n')), C::NewSession)],
    },
    Binding {
        label: "r",
        description: "rename this session",
        group: Group::Sessions,
        keys: &[(Key::plain(Char('r')), C::RenameSession)],
    },
    Binding {
        label: "x",
        description: "end this session",
        group: Group::Sessions,
        keys: &[(Key::plain(Char('x')), C::EndSession)],
    },
    Binding {
        label: "d",
        description: "detach",
        group: Group::Sessions,
        keys: &[(Key::plain(Char('d')), C::Detach)],
    },
    Binding {
        label: "h H",
        description: "hives…",
        group: Group::Sessions,
        keys: &[
            (Key::plain(Char('h')), C::Hives),
            (Key::plain(Char('H')), C::Hives),
        ],
    },
    Binding {
        label: "a",
        description: "save all as hive…",
        group: Group::Sessions,
        keys: &[(Key::plain(Char('a')), C::SaveHive)],
    },
    Binding {
        label: "⌫",
        description: "back",
        group: Group::Sessions,
        keys: &[(Key::plain(KeyCode::Backspace), C::Back)],
    },
    Binding {
        label: "Esc",
        description: "cancel",
        group: Group::Sessions,
        keys: &[(Key::plain(Esc), C::Cancel)],
    },
];

/// A key and what it does, as a menu's footer shows it.
pub type KeyHint = (&'static str, &'static str);

/// The keys of the session list (`Ctrl+B S`) besides its entries. Its
/// popup, the help and `hivemux keys` all show this list.
pub const SESSION_LIST_KEYS: &[KeyHint] = &[
    ("↑↓", "select"),
    ("Enter", "or key switch"),
    ("x", "end"),
    ("r", "rename"),
    ("s", "save all as hive"),
    ("h", "hives"),
    ("Esc", "cancel"),
];

/// The keys of the hives list (`Ctrl+B H`) besides its entries.
pub const HIVE_LIST_KEYS: &[KeyHint] = &[
    ("↑↓", "select"),
    ("Enter", "or digit bring back"),
    ("s", "save new"),
    ("u", "update"),
    ("r", "rename"),
    ("x", "delete"),
    ("Esc", "cancel"),
];

/// The lists with keys of their own, for the help: title, how to open it,
/// its keys.
pub const LISTS: &[(&str, &str, &[KeyHint])] = &[
    ("Session list", "S", SESSION_LIST_KEYS),
    ("Hives", "H", HIVE_LIST_KEYS),
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
    let groups = Menu::ALL
        .iter()
        .flat_map(|m| m.groups().iter().copied())
        .chain([Group::More]);
    for group in groups {
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
    for (title, key, keys) in LISTS {
        out.push_str(&format!("\n{title} ({PREFIX_LABEL}, {key}), in the list\n"));
        for (label, description) in *keys {
            let pad = width.saturating_sub(label.chars().count());
            out.push_str(&format!("  {label}{}  {description}\n", " ".repeat(pad)));
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
        assert_eq!(lookup(Menu::Root, event(Char('y'), none)), None);
        assert_eq!(
            lookup(Menu::Root, event(Char(';'), none)),
            Some(C::LastPane)
        );
        assert_eq!(lookup(Menu::Root, event(Char('z'), none)), Some(C::Zoom));
        assert_eq!(
            lookup(Menu::Root, event(Char('n'), none)),
            Some(C::NewWorkspace)
        );
        assert_eq!(
            lookup(Menu::Workspaces, event(Char('c'), none)),
            Some(C::NewWorkspace)
        );
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
    fn session_menu_has_its_commands_and_the_old_keys_still_work() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            lookup(Menu::Root, event(Char('s'), none)),
            Some(C::Open(Menu::Session))
        );
        assert_eq!(
            lookup(Menu::Root, event(Char('S'), none)),
            Some(C::Sessions)
        );
        assert_eq!(lookup(Menu::Root, event(Char('H'), none)), Some(C::Hives));
        assert_eq!(
            lookup(Menu::Session, event(Char('r'), none)),
            Some(C::RenameSession)
        );
        assert_eq!(
            lookup(Menu::Session, event(Char('x'), none)),
            Some(C::EndSession)
        );
        assert_eq!(
            lookup(Menu::Session, event(Char('a'), none)),
            Some(C::SaveHive)
        );
        assert!(reference().contains("save new") && reference().contains("or key switch"));
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
