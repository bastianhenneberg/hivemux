//! The sidebar, like the rail in TUIOS and herdr's sidebar: every workspace
//! with what its agents are doing, every agent across all workspaces, and
//! the git branch of the focused pane. Rows are clickable.

use std::fs;
use std::path::Path;
use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::agent::AgentState;
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
}

/// What a click on a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Workspace(u8),
    Pane(PaneId),
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
    let heading = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let mut out = Vec::new();

    out.push((Line::styled("Workspaces", heading), None));
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
            Style::new().fg(t.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        let mut spans = vec![Span::styled(left, style), Span::raw(" ".repeat(pad))];
        if let Some(state) = ws.state {
            spans.push(Span::styled(right, state_style(state)));
        }
        out.push((Line::from(spans), Some(Target::Workspace(ws.number))));
    }

    out.push((Line::default(), None));
    out.push((Line::styled("Agents", heading), None));
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
            Span::styled(place, dim),
            Span::raw(" ".repeat(pad)),
            Span::styled(age, dim),
        ]);
        out.push((line, Some(Target::Pane(agent.pane))));
    }

    if let Some(branch) = &data.branch {
        out.push((Line::default(), None));
        out.push((
            Line::from(vec![
                Span::styled("Git ", heading),
                Span::styled(
                    fit(&format!(" {branch}"), width.saturating_sub(4)),
                    Style::new().fg(t.accent),
                ),
            ]),
            None,
        ));
    }
    out
}

/// Draws the sidebar. `selected` is the index of the highlighted row among
/// the clickable ones, when the sidebar has the keyboard.
pub fn draw(frame: &mut Frame, area: Rect, data: &Data, selected: Option<usize>) {
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
    frame.render_widget(Paragraph::new(rows).block(block), area);
}

/// The clickable rows' targets, top to bottom.
pub fn targets(data: &Data) -> Vec<Target> {
    lines(data, WIDTH)
        .into_iter()
        .filter_map(|(_, target)| target)
        .collect()
}

/// What the row at `pos` does when clicked, for a sidebar drawn at `area`.
pub fn target_at(data: &Data, area: Rect, pos: Position) -> Option<Target> {
    let inner = Block::bordered().inner(area);
    if !inner.contains(pos) {
        return None;
    }
    let row = usize::from(pos.y - inner.y);
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
        assert!(texts.last().unwrap().contains("main"));
    }

    #[test]
    fn click_positions_map_to_rows() {
        let area = Rect::new(70, 0, 30, 20);
        let data = data();
        // Row 0 is the heading, row 1 workspace 1, inside the border.
        assert_eq!(
            target_at(&data, area, Position::new(72, 2)),
            Some(Target::Workspace(1))
        );
        assert_eq!(target_at(&data, area, Position::new(72, 1)), None);
        assert_eq!(target_at(&data, area, Position::new(70, 2)), None);
    }

    #[test]
    fn targets_list_workspaces_then_agents() {
        assert_eq!(
            targets(&data()),
            vec![Target::Workspace(1), Target::Workspace(2), Target::Pane(7)]
        );
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
}
