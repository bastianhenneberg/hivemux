//! The sidebar, like the rail in TUIOS and herdr's sidebar: every workspace
//! with what its agents are doing, every agent across all workspaces, and
//! the git branch of the focused pane and the files of its project. Rows are
//! clickable.

use std::fs;
use std::path::Path;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::agent::AgentState;
use crate::filetree::Entry;
use crate::layout::PaneId;
use crate::theme;

/// The sidebar's width, border included.
pub const WIDTH: u16 = 30;

pub struct WorkspaceRow {
    pub number: u8,
    /// The project: the directory name of the workspace's focused pane.
    pub name: String,
    pub active: bool,
    /// The state that needs attention most among its agents.
    pub state: Option<AgentState>,
    pub agents: usize,
}

pub struct AgentRow {
    pub pane: PaneId,
    pub program: String,
    pub workspace: u8,
    pub dir: String,
    pub state: AgentState,
    /// How long it has been in this state.
    pub since: Duration,
    pub focused: bool,
}

pub struct Data {
    pub workspaces: Vec<WorkspaceRow>,
    pub agents: Vec<AgentRow>,
    pub branch: Option<String>,
    /// Changed files in the focused pane's repository, as `git status`
    /// shows them: two status letters and the path from the repository root.
    pub changes: Vec<Change>,
    /// The file tree: the root's name and what is shown of it.
    pub files: Option<(String, Vec<Entry>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub status: String,
    pub path: String,
}

/// At most this many changed files are listed, the rest as a count.
const MAX_CHANGES: usize = 12;

/// What a click on a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Workspace(u8),
    Pane(PaneId),
    /// The changed file at this index in `Data::changes`.
    Change(usize),
    /// The file tree's entry at this index.
    File(usize),
}

/// The order agents are listed in: those that need the user first.
pub fn urgency(state: AgentState) -> u8 {
    match state {
        AgentState::Blocked => 0,
        AgentState::Done => 1,
        AgentState::Working => 2,
        AgentState::Idle => 3,
    }
}

pub fn state_style(state: AgentState) -> Style {
    let t = theme::current();
    match state {
        AgentState::Working => Style::new().fg(t.warning),
        AgentState::Blocked => Style::new().fg(t.danger).add_modifier(Modifier::BOLD),
        AgentState::Done => Style::new().fg(t.success).add_modifier(Modifier::BOLD),
        AgentState::Idle => Style::new().fg(t.success),
    }
}

/// The sidebar's rows for an inner width of `width`, each with what a click
/// on it does.
pub fn lines(data: &Data, width: u16) -> Vec<(Line<'static>, Option<Target>)> {
    let width = usize::from(width);
    let t = theme::current();
    // Each section has a colour of its own, workspaces the colour they have
    // everywhere.
    let heading = |color| Style::new().fg(color).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(t.muted);
    let mut out = Vec::new();

    out.push((Line::styled("Windows", heading(t.blue)), None));
    for ws in &data.workspaces {
        let marker = if ws.active { "▸ " } else { "  " };
        let mut right = String::new();
        if let Some(state) = ws.state {
            right = format!("{} {}", state.symbol(), ws.agents);
        }
        let left = format!("{marker}{} {}", ws.number, ws.name);
        let room = width.saturating_sub(right.chars().count() + 1);
        let left = fit(&left, room);
        let pad = width.saturating_sub(left.chars().count() + right.chars().count());
        let style = if ws.active {
            Style::new()
                .fg(t.workspace(ws.number))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(t.workspace(ws.number))
        };
        let mut spans = vec![Span::styled(left, style), Span::raw(" ".repeat(pad))];
        if let Some(state) = ws.state {
            spans.push(Span::styled(right, state_style(state)));
        }
        out.push((Line::from(spans), Some(Target::Workspace(ws.number))));
    }

    out.push((Line::default(), None));
    out.push((Line::styled("Agents", heading(t.magenta)), None));
    if data.agents.is_empty() {
        out.push((Line::styled("  none running", dim), None));
    }
    for agent in &data.agents {
        let age = age(agent.since);
        let head = format!("{} {}", agent.state.symbol(), agent.program);
        let place = format!(" {}·{}", agent.workspace, agent.dir);
        let room = width.saturating_sub(head.chars().count() + age.chars().count() + 1);
        let place = fit(&place, room);
        let pad = width
            .saturating_sub(head.chars().count() + place.chars().count() + age.chars().count());
        let name_style = if agent.focused {
            state_style(agent.state).add_modifier(Modifier::UNDERLINED)
        } else {
            state_style(agent.state)
        };
        let line = Line::from(vec![
            Span::styled(head, name_style),
            Span::styled(place, Style::new().fg(t.workspace(agent.workspace))),
            Span::raw(" ".repeat(pad)),
            Span::styled(age, dim),
        ]);
        out.push((line, Some(Target::Pane(agent.pane))));
    }

    if let Some(branch) = &data.branch {
        out.push((Line::default(), None));
        out.push((
            Line::from(vec![
                Span::styled("Git ", heading(t.orange)),
                Span::styled(
                    fit(&format!(" {branch}"), width.saturating_sub(4)),
                    Style::new().fg(t.accent),
                ),
            ]),
            None,
        ));
        for (i, change) in data.changes.iter().take(MAX_CHANGES).enumerate() {
            let color = match change.status.trim() {
                "??" => t.success,
                s if s.contains('D') => t.danger,
                _ => t.warning,
            };
            let status = format!("{:>2} ", change.status.trim());
            let path = fit_end(&change.path, width.saturating_sub(3));
            out.push((
                Line::from(vec![
                    Span::styled(status, Style::new().fg(color).add_modifier(Modifier::BOLD)),
                    Span::raw(path),
                ]),
                Some(Target::Change(i)),
            ));
        }
        if data.changes.len() > MAX_CHANGES {
            out.push((
                Line::styled(
                    format!("   +{} more", data.changes.len() - MAX_CHANGES),
                    dim,
                ),
                None,
            ));
        }
    }

    if let Some((root, entries)) = &data.files {
        out.push((Line::default(), None));
        out.push((
            Line::from(vec![
                Span::styled("Files ", heading(t.cyan)),
                Span::styled(fit(root, width.saturating_sub(6)), dim),
            ]),
            None,
        ));
        if entries.is_empty() {
            out.push((Line::styled("  empty", dim), None));
        }
        for (i, entry) in entries.iter().enumerate() {
            let indent = "  ".repeat(usize::from(entry.depth));
            let (marker, name, style) = if entry.dir {
                let marker = if entry.open { "▾ " } else { "▸ " };
                (marker, format!("{}/", entry.name), Style::new().fg(t.blue))
            } else if entry.changed {
                ("  ", entry.name.clone(), Style::new().fg(t.warning))
            } else {
                ("  ", entry.name.clone(), Style::new())
            };
            let room = width.saturating_sub(indent.chars().count() + 2);
            out.push((
                Line::from(vec![
                    Span::raw(indent),
                    Span::styled(marker, dim),
                    Span::styled(fit(&name, room), style),
                ]),
                Some(Target::File(i)),
            ));
        }
    }
    out
}

/// How far to scroll a sidebar of `height` rows so the `selected` clickable
/// row shows, starting from `scroll`.
pub fn scroll_to_show(data: &Data, height: u16, selected: usize, scroll: usize) -> usize {
    let height = usize::from(height.max(1));
    let Some(row) = lines(data, WIDTH)
        .iter()
        .enumerate()
        .filter(|(_, (_, target))| target.is_some())
        .nth(selected)
        .map(|(row, _)| row)
    else {
        return scroll;
    };
    if row < scroll {
        // One more, so the heading of the first section shows.
        row.saturating_sub(1)
    } else if row >= scroll + height {
        row + 1 - height
    } else {
        scroll
    }
}

/// The furthest the sidebar can scroll with `height` rows.
pub fn max_scroll(data: &Data, height: u16) -> usize {
    lines(data, WIDTH).len().saturating_sub(usize::from(height))
}

/// `text` cut to `room` characters keeping its end, for paths.
fn fit_end(text: &str, room: usize) -> String {
    let len = text.chars().count();
    if len <= room {
        return text.to_owned();
    }
    if room == 0 {
        return String::new();
    }
    let tail: String = text.chars().skip(len - (room - 1)).collect();
    format!("…{tail}")
}

/// Parses `git status --porcelain` output.
pub fn parse_changes(porcelain: &str) -> Vec<Change> {
    porcelain
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| {
            let path = &line[3..];
            // A rename shows as `old -> new`, the new name is the file now.
            let path = path.rsplit(" -> ").next().unwrap_or(path);
            Change {
                status: line[..2].to_owned(),
                path: path.trim_matches('"').to_owned(),
            }
        })
        .collect()
}

/// Draws the sidebar. `selected` is the index of the highlighted row among
/// the clickable ones, when the sidebar has the keyboard.
pub fn draw(frame: &mut Frame, area: Rect, data: &Data, selected: Option<usize>, scroll: usize) {
    let border = match selected {
        Some(_) => theme::current().accent,
        None => theme::current().subtle,
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Span::styled(
            " ⬢ hivemux ",
            Style::new()
                .fg(theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let mut clickable = 0;
    let rows: Vec<Line> = lines(data, inner.width)
        .into_iter()
        .map(|(line, target)| {
            let Some(_) = target else { return line };
            let highlight = selected == Some(clickable);
            clickable += 1;
            if highlight {
                let width = usize::from(inner.width).saturating_sub(line.width());
                let mut line = line;
                line.spans.push(Span::raw(" ".repeat(width)));
                line.patch_style(Style::new().bg(theme::current().subtle))
            } else {
                line
            }
        })
        .collect();
    frame.render_widget(Clear, area);
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(Paragraph::new(rows).block(block).scroll((scroll, 0)), area);
}

/// The clickable rows' targets, top to bottom.
pub fn targets(data: &Data) -> Vec<Target> {
    lines(data, WIDTH)
        .into_iter()
        .filter_map(|(_, target)| target)
        .collect()
}

/// What the row at `pos` does when clicked, for a sidebar drawn at `area`.
pub fn target_at(data: &Data, area: Rect, pos: Position, scroll: usize) -> Option<Target> {
    let inner = Block::bordered().inner(area);
    if !inner.contains(pos) {
        return None;
    }
    let row = usize::from(pos.y - inner.y) + scroll;
    lines(data, inner.width)
        .get(row)
        .and_then(|(_, target)| *target)
}

/// `text` cut to `room` characters with an ellipsis.
fn fit(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    if room == 0 {
        return String::new();
    }
    let mut cut: String = text.chars().take(room - 1).collect();
    cut.push('…');
    cut
}

/// A short age like `12s`, `3m` or `2h`.
pub fn age(since: Duration) -> String {
    let secs = since.as_secs();
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// The branch checked out in the repository containing `dir`, or the short
/// commit when detached. Reads `.git` directly instead of running git, since
/// this runs on every redraw.
pub fn git_branch(dir: &Path) -> Option<String> {
    let mut current = Some(dir);
    while let Some(d) = current {
        let git = d.join(".git");
        let head = if git.is_dir() {
            fs::read_to_string(git.join("HEAD")).ok()
        } else if git.is_file() {
            // A worktree: `.git` names the real git directory.
            let pointer = fs::read_to_string(&git).ok()?;
            let gitdir = pointer.trim().strip_prefix("gitdir: ")?;
            fs::read_to_string(d.join(gitdir).join("HEAD")).ok()
        } else {
            None
        };
        if let Some(head) = head {
            let head = head.trim();
            return Some(match head.strip_prefix("ref: refs/heads/") {
                Some(branch) => branch.to_owned(),
                None => head.chars().take(7).collect(),
            });
        }
        current = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> Data {
        Data {
            workspaces: vec![
                WorkspaceRow {
                    number: 1,
                    name: "hivemux".into(),
                    active: true,
                    state: Some(AgentState::Working),
                    agents: 1,
                },
                WorkspaceRow {
                    number: 2,
                    name: "a-very-long-project-name-indeed".into(),
                    active: false,
                    state: Some(AgentState::Blocked),
                    agents: 2,
                },
            ],
            agents: vec![AgentRow {
                pane: 7,
                program: "claude".into(),
                workspace: 2,
                dir: "api".into(),
                state: AgentState::Blocked,
                since: Duration::from_secs(185),
                focused: false,
            }],
            branch: Some("main".into()),
            changes: vec![Change {
                status: " M".into(),
                path: "src/app.rs".into(),
            }],
            files: None,
        }
    }

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn rows_fit_and_click_to_their_target() {
        let rows = lines(&data(), 28);
        for (line, _) in &rows {
            assert!(line.width() <= 28, "{:?} too wide", text(line));
        }
        let texts: Vec<String> = rows.iter().map(|(l, _)| text(l)).collect();
        assert!(texts[1].starts_with("▸ 1 hivemux") && texts[1].ends_with("● 1"));
        assert!(texts[2].contains('…') && texts[2].ends_with("◆ 2"));
        assert_eq!(rows[2].1, Some(Target::Workspace(2)));
        let agent = texts
            .iter()
            .position(|t| t.starts_with("◆ claude"))
            .unwrap();
        assert!(texts[agent].contains("2·api") && texts[agent].ends_with("3m"));
        assert_eq!(rows[agent].1, Some(Target::Pane(7)));
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("Git") && t.contains("main"))
        );
        assert_eq!(texts.last().unwrap(), " M src/app.rs");
        assert_eq!(rows.last().unwrap().1, Some(Target::Change(0)));
    }

    #[test]
    fn click_positions_map_to_rows() {
        let area = Rect::new(70, 0, 30, 20);
        let data = data();
        // Row 0 is the heading, row 1 workspace 1, inside the border.
        assert_eq!(
            target_at(&data, area, Position::new(72, 2), 0),
            Some(Target::Workspace(1))
        );
        assert_eq!(target_at(&data, area, Position::new(72, 1), 0), None);
        assert_eq!(target_at(&data, area, Position::new(70, 2), 0), None);
        // Scrolled down one row, the same place is workspace 2.
        assert_eq!(
            target_at(&data, area, Position::new(72, 2), 1),
            Some(Target::Workspace(2))
        );
    }

    #[test]
    fn targets_list_workspaces_then_agents() {
        assert_eq!(
            targets(&data()),
            vec![
                Target::Workspace(1),
                Target::Workspace(2),
                Target::Pane(7),
                Target::Change(0)
            ]
        );
    }

    #[test]
    fn parses_git_status() {
        let changes =
            parse_changes(" M src/app.rs\n?? notes.md\nR  old.rs -> new.rs\nD  gone.rs\n");
        let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["src/app.rs", "notes.md", "new.rs", "gone.rs"]);
        assert_eq!(changes[1].status, "??");
    }

    #[test]
    fn ages() {
        assert_eq!(age(Duration::from_secs(12)), "12s");
        assert_eq!(age(Duration::from_secs(185)), "3m");
        assert_eq!(age(Duration::from_secs(7300)), "2h");
    }

    #[test]
    fn finds_the_branch_of_this_repository() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        assert!(git_branch(&here).is_some());
        assert_eq!(git_branch(Path::new("/")), None);
    }

    #[test]
    fn file_tree_rows_indent_and_scroll_into_view() {
        let entry = |name: &str, depth, dir, open| Entry {
            path: name.into(),
            name: name.into(),
            depth,
            dir,
            open,
            changed: false,
        };
        let mut data = data();
        data.files = Some((
            "hivemux".into(),
            vec![
                entry("src", 0, true, true),
                entry("app.rs", 1, false, false),
            ],
        ));
        let rows = lines(&data, 28);
        let texts: Vec<String> = rows.iter().map(|(l, _)| text(l)).collect();
        assert!(texts.iter().any(|t| t == "Files hivemux"));
        assert!(texts.iter().any(|t| t == "▾ src/"));
        assert_eq!(texts.last().unwrap(), "    app.rs");
        assert_eq!(rows.last().unwrap().1, Some(Target::File(1)));
        // The last clickable row, in a sidebar of 5 rows, needs scrolling.
        let last = targets(&data).len() - 1;
        let scroll = scroll_to_show(&data, 5, last, 0);
        assert_eq!(scroll, rows.len() - 5);
        assert_eq!(scroll_to_show(&data, 5, 0, scroll), 0);
    }
}
