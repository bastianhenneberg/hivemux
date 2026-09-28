//! The key menus drawn from the binding table: the which-key popup shown
//! while the prefix is active, and the full help overlay.

use ratatui::Frame;
use ratatui::layout::{Margin, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Padding, Paragraph};

use crate::bindings::{Binding, Group, Menu, PREFIX_LABEL, in_group};
use crate::config::{Config, SETTINGS, Side};
use crate::theme;

/// Space between the key column and the description, and between groups.
const GAP: usize = 2;
const GROUP_GAP: usize = 4;

/// The which-key popup in the bottom corner of `area` on `side`.
/// `commands` are the user's own, as (key, name), listed in their menu.
pub fn draw_which_key(
    frame: &mut Frame,
    area: Rect,
    side: Side,
    menu: Menu,
    commands: &[(char, String)],
) {
    let area = inset(area);
    // Border and one column of padding on each side.
    let lines = match menu {
        Menu::Commands => command_lines(commands),
        menu => groups(menu.groups(), usize::from(area.width).saturating_sub(4)),
    };

    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 4;
    let height = lines.len() as u16 + 2;
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = match side {
        Side::Left => area.x,
        Side::Right => area.right() - width,
    };
    let popup = Rect::new(x, area.bottom() - height, width, height);

    let title = match menu {
        Menu::Root => format!(" ⬢ {PREFIX_LABEL} "),
        menu => format!(" ⬢ {PREFIX_LABEL} {} ", menu.path()),
    };
    draw_box(frame, popup, title, 1, lines);
}

/// The full key reference in the middle of `area`, scrolled down by
/// `scroll` lines when it is taller than the screen.
pub fn draw_help(frame: &mut Frame, area: Rect, scroll: u16) {
    let area = inset(area);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut lines = vec![
        Line::from(vec![
            Span::raw("Press "),
            Span::styled(PREFIX_LABEL, key_style()),
            Span::raw(", then one of these keys:"),
        ]),
        Line::default(),
    ];
    // Border and two columns of padding on each side.
    let max_width = usize::from(area.width).saturating_sub(6);
    lines.extend(groups(Menu::Root.groups(), max_width));
    let submenus: Vec<Group> = Menu::ALL
        .iter()
        .filter(|m| **m != Menu::Root)
        .flat_map(|m| m.groups().iter().copied())
        .collect();
    lines.push(Line::default());
    lines.extend(groups(&submenus, max_width));
    lines.extend([
        Line::default(),
        Line::styled("Focus and resize repeat: for a moment afterwards,", dim),
        Line::styled("arrow keys work without the prefix.", dim),
        Line::styled("`hivemux keys` prints this list in a shell.", dim),
        Line::default(),
        Line::from(vec![
            Span::styled("j k", key_style()),
            Span::raw(" scroll · "),
            Span::styled("any other key", key_style()),
            Span::raw(" close"),
        ]),
    ]);

    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 6;
    let height = lines.len() as u16 + 2;
    let width = width.min(area.width);
    let height = height.min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );

    // Scroll no further than the last line reaching the bottom.
    let overflow = (lines.len() as u16 + 2).saturating_sub(height);
    draw_box_scrolled(
        frame,
        popup,
        " ⬢ hivemux keys ".into(),
        2,
        lines,
        scroll.min(overflow),
    );
}

/// `area` minus one cell on each side, so popups leave the pane borders
/// around them visible.
fn inset(area: Rect) -> Rect {
    area.inner(Margin::new(1, 1))
}

fn draw_box(
    frame: &mut Frame,
    popup: Rect,
    title: String,
    padding: u16,
    lines: Vec<Line<'static>>,
) {
    draw_box_scrolled(frame, popup, title, padding, lines, 0);
}

fn draw_box_scrolled(
    frame: &mut Frame,
    popup: Rect,
    title: String,
    padding: u16,
    lines: Vec<Line<'static>>,
    scroll: u16,
) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::current().accent))
        .title(Span::styled(title, key_style()))
        .padding(Padding::horizontal(padding));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(block).scroll((scroll, 0)),
        popup,
    );
}

/// All groups, side by side when they fit into `max_width`, stacked
/// otherwise.
fn groups(groups: &[Group], max_width: usize) -> Vec<Line<'static>> {
    let columns: Vec<Vec<Line>> = groups.iter().map(|&g| group_lines(g)).collect();
    let widths: Vec<usize> = columns
        .iter()
        .map(|lines| lines.iter().map(Line::width).max().unwrap_or(0))
        .collect();
    let side_by_side = widths.iter().sum::<usize>() + GROUP_GAP * (widths.len() - 1);
    if side_by_side <= max_width {
        beside(columns, &widths)
    } else {
        stacked(columns)
    }
}

/// An entry of the quit menu.
pub struct MenuItem {
    pub key: char,
    pub title: String,
    pub hint: String,
    /// Destroys something, shown in red.
    pub danger: bool,
}

/// The quit menu in the middle of `area`, `selected` highlighted.
pub fn draw_quit_menu(frame: &mut Frame, area: Rect, items: &[MenuItem], selected: usize) {
    let area = inset(area);
    let title_width = items
        .iter()
        .map(|i| i.title.chars().count())
        .max()
        .unwrap_or(0);
    let hint_width = items
        .iter()
        .map(|i| i.hint.chars().count())
        .max()
        .unwrap_or(0);
    let row_width = 3 + GAP + title_width + GAP + hint_width;

    let mut lines = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let color = if item.danger {
            theme::current().danger
        } else {
            theme::current().accent
        };
        let title_pad = title_width - item.title.chars().count() + GAP;
        let mut spans = vec![
            Span::styled(
                format!(" {} ", item.key),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(GAP)),
            Span::styled(
                item.title.clone(),
                Style::new().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(title_pad)),
            Span::styled(item.hint.clone(), Style::new().add_modifier(Modifier::DIM)),
        ];
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        spans.push(Span::raw(" ".repeat(row_width.saturating_sub(used))));
        let mut line = Line::from(spans);
        if i == selected {
            line = line.patch_style(Style::new().bg(theme::current().subtle));
        }
        lines.push(line);
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("↑↓", key_style()),
        Span::raw(" select   "),
        Span::styled("Enter", key_style()),
        Span::raw(" or letter choose   "),
        Span::styled("Esc", key_style()),
        Span::raw(" cancel"),
    ]));

    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 6;
    let height = lines.len() as u16 + 2;
    let width = width.min(area.width);
    let height = height.min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    draw_box(frame, popup, " ⬢ Quit hivemux? ".into(), 2, lines);
}

/// The settings menu in the middle of `area`. `note` is shown below the
/// settings, e.g. where they were saved.
pub fn draw_settings(
    frame: &mut Frame,
    area: Rect,
    config: &Config,
    selected: usize,
    note: Option<&str>,
) {
    let area = inset(area);
    let name_width = SETTINGS
        .iter()
        .map(|s| s.name.chars().count())
        .max()
        .unwrap_or(0);
    let value_width = SETTINGS
        .iter()
        .map(|s| (s.value)(config).chars().count())
        .max()
        .unwrap_or(0)
        .max(6);

    let mut lines = Vec::new();
    for (i, setting) in SETTINGS.iter().enumerate() {
        let pad = name_width - setting.name.chars().count() + GROUP_GAP;
        let value = format!("‹ {:^value_width$} ›", (setting.value)(config));
        let mut line = Line::from(vec![
            Span::raw(" "),
            Span::raw(setting.name),
            Span::raw(" ".repeat(pad)),
            Span::styled(value, key_style()),
            Span::raw(" "),
        ]);
        if i == selected {
            line = line.patch_style(Style::new().bg(theme::current().subtle));
        }
        lines.push(line);
    }
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("↑↓", key_style()),
        Span::raw(" select   "),
        Span::styled("←→ Enter", key_style()),
        Span::raw(" change   "),
        Span::styled("Esc", key_style()),
        Span::raw(" close"),
    ]));
    // The note does not widen the box, a long path is cut off instead.
    let width = lines.iter().map(Line::width).max().unwrap_or(0) as u16 + 6;
    if let Some(note) = note {
        let hint = lines.len() - 1;
        lines.insert(
            hint,
            Line::styled(note.to_owned(), Style::new().add_modifier(Modifier::DIM)),
        );
    }
    let height = lines.len() as u16 + 2;
    let width = width.min(area.width);
    let height = height.min(area.height);
    let popup = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    draw_box(frame, popup, " ⬢ Settings ".into(), 2, lines);
}

fn key_style() -> Style {
    Style::new()
        .fg(theme::current().accent)
        .add_modifier(Modifier::BOLD)
}

/// The commands menu: the user's commands from the config, then back and
/// cancel.
fn command_lines(commands: &[(char, String)]) -> Vec<Line<'static>> {
    let mut lines = group_lines(Group::Commands);
    let fixed = lines.split_off(1);
    if commands.is_empty() {
        lines.push(Line::styled(
            "none yet, add [[commands]] to config.toml",
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    for (key, name) in commands {
        lines.push(Line::from(vec![
            Span::styled(key.to_string(), key_style()),
            Span::raw(" ".repeat(2 + GAP)),
            Span::raw(name.clone()),
        ]));
    }
    lines.extend(fixed);
    lines
}

/// A group's title followed by one line per binding, keys aligned.
fn group_lines(group: Group) -> Vec<Line<'static>> {
    let bindings: Vec<&Binding> = in_group(group).collect();
    let key_width = bindings
        .iter()
        .map(|b| b.label.chars().count())
        .max()
        .unwrap_or(0);

    let title = match group.menu() {
        Menu::Root => group.title().to_owned(),
        menu => format!("{} ({})", group.title(), menu.path()),
    };
    let mut lines = vec![Line::styled(
        title,
        Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
    )];
    for binding in bindings {
        let pad = key_width - binding.label.chars().count() + GAP;
        lines.push(Line::from(vec![
            Span::styled(binding.label, key_style()),
            Span::raw(" ".repeat(pad)),
            Span::raw(binding.description),
        ]));
    }
    lines
}

/// Joins columns of lines side by side, padding each column to its width.
fn beside(columns: Vec<Vec<Line<'static>>>, widths: &[usize]) -> Vec<Line<'static>> {
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0);
    (0..rows)
        .map(|row| {
            let mut spans = Vec::new();
            for (i, column) in columns.iter().enumerate() {
                let line = column.get(row).cloned().unwrap_or_default();
                let pad = widths[i] - line.width();
                spans.extend(line.spans);
                if i + 1 < columns.len() {
                    spans.push(Span::raw(" ".repeat(pad + GROUP_GAP)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

fn stacked(columns: Vec<Vec<Line<'static>>>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for (i, column) in columns.into_iter().enumerate() {
        if i > 0 {
            lines.push(Line::default());
        }
        lines.extend(column);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render(width: u16, height: u16, draw: impl Fn(&mut Frame, Rect)) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, frame.area())).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn which_key_fits_side_by_side_on_a_wide_screen() {
        let screen = render(120, 30, |f, a| {
            draw_which_key(f, a, Side::Right, Menu::Root, &[])
        });
        let row = screen
            .lines()
            .find(|l| l.contains("Panes"))
            .expect("Panes header");
        assert!(row.contains("Navigate") && row.contains("Session"));
        assert!(screen.contains("split side by side"));
        assert!(screen.contains("quit hivemux"));
    }

    #[test]
    fn which_key_stacks_on_a_narrow_screen() {
        let screen = render(40, 40, |f, a| {
            draw_which_key(f, a, Side::Left, Menu::Root, &[])
        });
        let row = screen
            .lines()
            .find(|l| l.contains("Panes"))
            .expect("Panes header");
        assert!(!row.contains("Navigate"));
        assert!(screen.contains("Navigate") && screen.contains("Session"));
    }

    #[test]
    fn which_key_opens_on_the_chosen_side() {
        let left = render(120, 30, |f, a| {
            draw_which_key(f, a, Side::Left, Menu::Root, &[])
        });
        let right = render(120, 30, |f, a| {
            draw_which_key(f, a, Side::Right, Menu::Root, &[])
        });
        let corner = |screen: &str| {
            let row = screen.lines().find(|l| l.contains("Ctrl+B")).unwrap();
            row.chars().position(|c| c == '╭').unwrap()
        };
        assert_eq!(corner(&left), 1);
        assert!(corner(&right) > corner(&left));
    }

    #[test]
    fn which_key_shows_the_open_submenu() {
        let screen = render(120, 30, |f, a| {
            draw_which_key(f, a, Side::Left, Menu::Floating, &[])
        });
        assert!(screen.contains("next floating pane"));
        assert!(!screen.contains("split side by side"));
    }

    #[test]
    fn commands_menu_lists_the_users_commands() {
        let commands = [('g', "lazygit".to_owned())];
        let screen = render(80, 20, |f, a| {
            draw_which_key(f, a, Side::Left, Menu::Commands, &commands)
        });
        assert!(screen.contains("lazygit") && screen.contains("cancel"));
    }

    #[test]
    fn settings_show_every_setting_with_its_value() {
        let config = Config::default();
        let screen = render(80, 20, |f, a| {
            draw_settings(f, a, &config, 0, Some("saved"))
        });
        for setting in SETTINGS {
            assert!(screen.contains(setting.name), "{} missing", setting.name);
            assert!(screen.contains(&(setting.value)(&config)));
        }
        assert!(screen.contains("saved"));
    }

    #[test]
    fn help_scrolls_when_the_screen_is_short() {
        let top = render(80, 20, |f, a| draw_help(f, a, 0));
        let scrolled = render(80, 20, |f, a| draw_help(f, a, 500));
        assert!(top.contains("Press"));
        assert!(!scrolled.contains("Press"));
        assert!(scrolled.contains("any other key"));
    }

    #[test]
    fn help_lists_every_binding() {
        let screen = render(160, 60, |f, a| draw_help(f, a, 0));
        for binding in crate::bindings::BINDINGS {
            assert!(
                screen.contains(binding.description),
                "{} missing",
                binding.description
            );
        }
    }
}
