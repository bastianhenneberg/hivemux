//! A workspace: a tiling layout with floating panes on top, and which pane
//! has focus. Pure geometry, the panes themselves live in the app.

use std::sync::atomic::{AtomicU32, Ordering};

use ratatui::layout::{Position, Rect};
use serde::{Deserialize, Serialize};

use crate::layout::{Axis, Direction, Layout, PaneId};

/// The smallest floating pane, border included.
pub const MIN_FLOAT_WIDTH: u16 = 12;
pub const MIN_FLOAT_HEIGHT: u16 = 5;

/// Columns and rows between tiled panes, from the settings. Global like the
/// theme, so every caller of `rects` sees the same gaps.
static GAPS: AtomicU32 = AtomicU32::new(0);

pub fn set_gaps(columns: u16, rows: u16) {
    GAPS.store(
        u32::from(columns) << 16 | u32::from(rows),
        Ordering::Relaxed,
    );
}

fn gaps() -> (u16, u16) {
    let gaps = GAPS.load(Ordering::Relaxed);
    ((gaps >> 16) as u16, gaps as u16)
}

/// `tiles` with `columns` and `rows` of space between neighbours: every tile
/// that does not reach the right or bottom edge of `body` gives up that much
/// on that side, as long as it keeps room for its border and a cell.
fn with_gaps(
    tiles: Vec<(PaneId, Rect)>,
    body: Rect,
    (columns, rows): (u16, u16),
) -> Vec<(PaneId, Rect)> {
    tiles
        .into_iter()
        .map(|(id, mut r)| {
            if r.right() < body.right() && r.width > columns + 2 {
                r.width -= columns;
            }
            if r.bottom() < body.bottom() && r.height > rows + 2 {
                r.height -= rows;
            }
            (id, r)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Float {
    pub id: PaneId,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Workspace {
    pub layout: Layout,
    /// Floating panes, bottom to top.
    pub floats: Vec<Float>,
    pub focus: PaneId,
    /// The tiled pane that had focus last, where focus returns to from a
    /// floating pane.
    last_tiled: Option<PaneId>,
    /// A name the user gave it, shown instead of the project directory.
    #[serde(default)]
    pub name: Option<String>,
    /// The pane shown alone over the whole workspace, if any.
    #[serde(default)]
    pub zoomed: Option<PaneId>,
}

impl Workspace {
    /// A workspace with one tiled pane.
    pub fn new(pane: PaneId) -> Self {
        Self {
            layout: Layout::new(pane),
            floats: Vec::new(),
            focus: pane,
            last_tiled: Some(pane),
            name: None,
            zoomed: None,
        }
    }

    /// A workspace with nothing in it, as a placeholder.
    pub fn empty() -> Self {
        Self {
            layout: Layout::empty(),
            floats: Vec::new(),
            focus: 0,
            last_tiled: None,
            name: None,
            zoomed: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.layout.is_empty() && self.floats.is_empty()
    }

    /// Every pane in this workspace, tiled ones first.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut all = self.layout.panes();
        all.extend(self.floats.iter().map(|f| f.id));
        all
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.layout.contains(pane) || self.is_floating(pane)
    }

    pub fn is_floating(&self, pane: PaneId) -> bool {
        self.floats.iter().any(|f| f.id == pane)
    }

    /// Every pane with its area, tiled ones first, then the floating ones
    /// bottom to top, i.e. in drawing order. Floating panes are kept inside
    /// `body`.
    pub fn rects(&self, body: Rect) -> Vec<(PaneId, Rect)> {
        if let Some(id) = self.zoomed.filter(|id| self.contains(*id)) {
            return vec![(id, body)];
        }
        let mut out = with_gaps(self.layout.rects(body), body, gaps());
        out.extend(self.floats.iter().map(|f| (f.id, clamp(f.rect, body))));
        out
    }

    /// The pane at `pos`, the topmost one where floating panes overlap, with
    /// its area.
    pub fn pane_at(&self, pos: Position, body: Rect) -> Option<(PaneId, Rect)> {
        self.rects(body)
            .into_iter()
            .rev()
            .find(|(_, rect)| rect.contains(pos))
    }

    /// Puts floating pane `pane` at `rect`, kept inside `body` and no smaller
    /// than the minimum.
    pub fn set_float_rect(&mut self, pane: PaneId, rect: Rect, body: Rect) {
        if let Some(float) = self.floats.iter_mut().find(|f| f.id == pane) {
            let rect = Rect {
                width: rect.width.max(MIN_FLOAT_WIDTH),
                height: rect.height.max(MIN_FLOAT_HEIGHT),
                ..rect
            };
            float.rect = clamp(rect, body);
        }
    }

    /// Focuses `pane`, raising it to the top if it floats.
    pub fn focus(&mut self, pane: PaneId) {
        // Looking at another pane ends a zoom, as in tmux.
        if self.zoomed.is_some_and(|z| z != pane) {
            self.zoomed = None;
        }
        if let Some(i) = self.floats.iter().position(|f| f.id == pane) {
            let float = self.floats.remove(i);
            self.floats.push(float);
        } else if self.layout.contains(pane) {
            self.last_tiled = Some(pane);
        } else {
            return;
        }
        self.focus = pane;
    }

    /// Moves focus to the pane in direction `dir`. From a floating pane, any
    /// direction goes back to the tiled pane that had focus last.
    pub fn focus_direction(&mut self, dir: Direction, body: Rect) {
        let next = if self.is_floating(self.focus) {
            self.last_tiled.filter(|p| self.layout.contains(*p))
        } else {
            self.layout.neighbor(self.focus, dir, body)
        };
        if let Some(next) = next {
            self.focus(next);
        }
    }

    /// Focuses the next pane: through the tiled ones, then the floating ones.
    pub fn cycle_focus(&mut self) {
        let all = self.panes();
        if let Some(i) = all.iter().position(|&p| p == self.focus) {
            self.focus(all[(i + 1) % all.len()]);
        }
    }

    /// Focuses the next floating pane. From a tiled pane that is the top
    /// one, from a floating pane the bottom one, which then comes to the top,
    /// so pressing it again goes through all of them.
    pub fn next_float(&mut self) {
        let next = if self.is_floating(self.focus) {
            self.floats.first()
        } else {
            self.floats.last()
        };
        if let Some(next) = next.map(|f| f.id) {
            self.focus(next);
        }
    }

    /// Splits the focused tiled pane and focuses `new`.
    /// Shows the focused pane alone over the whole workspace, or ends that.
    pub fn toggle_zoom(&mut self) {
        self.zoomed = match self.zoomed {
            Some(_) => None,
            None => Some(self.focus),
        };
    }

    /// Exchanges the focused tiled pane with the next or previous one;
    /// focus moves along with the pane.
    pub fn swap(&mut self, forward: bool) {
        let panes = self.layout.panes();
        let Some(i) = panes.iter().position(|&p| p == self.focus) else {
            return;
        };
        let len = panes.len();
        let other = panes[if forward {
            (i + 1) % len
        } else {
            (i + len - 1) % len
        }];
        self.layout.swap(self.focus, other);
    }

    pub fn split(&mut self, axis: Axis, new: PaneId) -> bool {
        self.zoomed = None;
        if !self.layout.split(self.focus, axis, new) {
            return false;
        }
        self.focus(new);
        true
    }

    /// Adds `pane` as a new floating pane in the middle of `body`, a little
    /// offset from the ones already there, and focuses it.
    pub fn add_float(&mut self, pane: PaneId, body: Rect) {
        let width = (body.width * 3 / 5).max(MIN_FLOAT_WIDTH);
        let height = (body.height * 3 / 5).max(MIN_FLOAT_HEIGHT);
        let offset = (self.floats.len() % 5) as u16;
        let rect = Rect::new(
            body.x + body.width.saturating_sub(width) / 2 + offset * 2,
            body.y + body.height.saturating_sub(height) / 2 + offset,
            width,
            height,
        );
        self.floats.push(Float {
            id: pane,
            rect: clamp(rect, body),
        });
        self.focus(pane);
    }

    /// Takes the focused pane out of the tiling and floats it, or puts a
    /// floating one back into the tiling next to the last focused tiled pane.
    pub fn toggle_float(&mut self, body: Rect) {
        let pane = self.focus;
        if let Some(i) = self.floats.iter().position(|f| f.id == pane) {
            self.floats.remove(i);
            match self.last_tiled.filter(|p| self.layout.contains(*p)) {
                Some(target) => {
                    self.layout.split(target, Axis::Row, pane);
                }
                None => match self.layout.panes().first() {
                    Some(&target) => {
                        self.layout.split(target, Axis::Row, pane);
                    }
                    None => self.layout = Layout::new(pane),
                },
            }
            self.focus(pane);
        } else if self.layout.contains(pane) {
            let next = self.layout.remove(pane);
            self.last_tiled = next;
            self.add_float(pane, body);
        }
    }

    /// Removes `pane`. Focus moves to a neighbour if it had it.
    pub fn remove(&mut self, pane: PaneId) {
        if self.zoomed == Some(pane) {
            self.zoomed = None;
        }
        let next = if let Some(i) = self.floats.iter().position(|f| f.id == pane) {
            self.floats.remove(i);
            self.floats.last().map(|f| f.id).or(self.last_tiled)
        } else {
            let next = self.layout.remove(pane);
            if self.last_tiled == Some(pane) {
                self.last_tiled = next;
            }
            next
        };
        if self.focus == pane {
            let fallback = next
                .filter(|p| self.contains(*p))
                .or_else(|| self.layout.panes().first().copied())
                .or_else(|| self.floats.last().map(|f| f.id));
            if let Some(next) = fallback {
                self.focus(next);
            }
        }
    }

    /// Moves the focused floating pane by `cells` (twice that sideways, since
    /// cells are about twice as tall as wide). Returns false for tiled panes.
    pub fn move_float(&mut self, dir: Direction, cells: u16, body: Rect) -> bool {
        let Some(float) = self.floats.iter_mut().find(|f| f.id == self.focus) else {
            return false;
        };
        let mut r = clamp(float.rect, body);
        match dir {
            Direction::Left => r.x = r.x.saturating_sub(cells * 2).max(body.x),
            Direction::Right => r.x = (r.x + cells * 2).min(body.right() - r.width),
            Direction::Up => r.y = r.y.saturating_sub(cells).max(body.y),
            Direction::Down => r.y = (r.y + cells).min(body.bottom() - r.height),
        }
        float.rect = r;
        true
    }

    /// Grows (right, down) or shrinks (left, up) the focused floating pane.
    /// Returns false for tiled panes.
    pub fn resize_float(&mut self, dir: Direction, cells: u16, body: Rect) -> bool {
        let Some(float) = self.floats.iter_mut().find(|f| f.id == self.focus) else {
            return false;
        };
        let mut r = clamp(float.rect, body);
        match dir {
            Direction::Right => r.width = (r.width + cells * 2).min(body.right() - r.x),
            Direction::Left => r.width = r.width.saturating_sub(cells * 2).max(MIN_FLOAT_WIDTH),
            Direction::Down => r.height = (r.height + cells).min(body.bottom() - r.y),
            Direction::Up => r.height = r.height.saturating_sub(cells).max(MIN_FLOAT_HEIGHT),
        }
        float.rect = clamp(r, body);
        true
    }
}

/// `rect` shrunk and shifted to lie inside `body`.
fn clamp(rect: Rect, body: Rect) -> Rect {
    let width = rect.width.min(body.width);
    let height = rect.height.min(body.height);
    let x = rect.x.clamp(body.x, body.right() - width);
    let y = rect.y.clamp(body.y, body.bottom() - height);
    Rect::new(x, y, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY: Rect = Rect::new(0, 0, 100, 40);

    #[test]
    fn gaps_separate_tiles_but_not_the_edges() {
        let body = Rect::new(0, 0, 80, 24);
        let tiles = vec![
            (1, Rect::new(0, 0, 40, 24)),
            (2, Rect::new(40, 0, 40, 12)),
            (3, Rect::new(40, 12, 40, 12)),
        ];
        let spaced = with_gaps(tiles, body, (2, 1));
        assert_eq!(spaced[0].1, Rect::new(0, 0, 38, 24));
        assert_eq!(spaced[1].1, Rect::new(40, 0, 40, 11));
        assert_eq!(spaced[2].1, Rect::new(40, 12, 40, 12));
    }

    #[test]
    fn floats_are_drawn_on_top_and_focus_raises_them() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        ws.add_float(3, BODY);
        let order: Vec<PaneId> = ws.rects(BODY).iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![1, 2, 3]);
        assert_eq!(ws.focus, 3);

        ws.focus(2);
        let order: Vec<PaneId> = ws.rects(BODY).iter().map(|(id, _)| *id).collect();
        assert_eq!(order, vec![1, 3, 2]);
    }

    #[test]
    fn next_float_goes_through_all_floats() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        ws.add_float(3, BODY);
        ws.add_float(4, BODY);
        ws.focus(1);
        let mut seen = Vec::new();
        for _ in 0..3 {
            ws.next_float();
            seen.push(ws.focus);
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![2, 3, 4]);
    }

    #[test]
    fn zoom_shows_one_pane_until_focus_moves() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::Row, 2);
        ws.toggle_zoom();
        assert_eq!(ws.rects(BODY), vec![(2, BODY)]);
        ws.focus_direction(Direction::Left, BODY);
        assert_eq!(ws.zoomed, None);
        assert_eq!(ws.rects(BODY).len(), 2);
    }

    #[test]
    fn swap_moves_the_focused_pane() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::Row, 2);
        ws.split(Axis::Column, 3);
        ws.focus(1);
        ws.swap(true);
        assert_eq!(ws.layout.panes(), vec![2, 1, 3]);
        assert_eq!(ws.focus, 1);
        ws.swap(false);
        assert_eq!(ws.layout.panes(), vec![1, 2, 3]);
    }

    #[test]
    fn toggle_float_and_back() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::Row, 2);
        assert_eq!(ws.focus, 2);

        ws.toggle_float(BODY);
        assert!(ws.is_floating(2));
        assert_eq!(ws.layout.panes(), vec![1]);
        assert_eq!(ws.focus, 2);

        ws.toggle_float(BODY);
        assert!(!ws.is_floating(2));
        assert_eq!(ws.layout.panes(), vec![1, 2]);
    }

    #[test]
    fn floating_the_only_tiled_pane_leaves_an_empty_layout() {
        let mut ws = Workspace::new(1);
        ws.toggle_float(BODY);
        assert!(ws.layout.is_empty());
        assert!(!ws.is_empty());

        ws.toggle_float(BODY);
        assert_eq!(ws.layout.panes(), vec![1]);
        assert!(ws.floats.is_empty());
    }

    #[test]
    fn focus_returns_from_a_float_to_the_last_tiled_pane() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::Row, 2);
        ws.add_float(3, BODY);
        ws.focus_direction(Direction::Left, BODY);
        assert_eq!(ws.focus, 2);
    }

    #[test]
    fn cycle_goes_through_tiled_then_floating() {
        let mut ws = Workspace::new(1);
        ws.split(Axis::Row, 2);
        ws.add_float(3, BODY);
        ws.focus(1);
        ws.cycle_focus();
        assert_eq!(ws.focus, 2);
        ws.cycle_focus();
        assert_eq!(ws.focus, 3);
        ws.cycle_focus();
        assert_eq!(ws.focus, 1);
    }

    #[test]
    fn removing_the_focused_float_focuses_the_next_one_down() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        ws.add_float(3, BODY);
        ws.remove(3);
        assert_eq!(ws.focus, 2);
        ws.remove(2);
        assert_eq!(ws.focus, 1);
        ws.remove(1);
        assert!(ws.is_empty());
    }

    #[test]
    fn move_and_resize_stay_inside_the_body() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        for _ in 0..100 {
            ws.move_float(Direction::Right, 1, BODY);
            ws.move_float(Direction::Down, 1, BODY);
        }
        let (_, r) = ws.rects(BODY)[1];
        assert_eq!((r.right(), r.bottom()), (BODY.right(), BODY.bottom()));

        for _ in 0..100 {
            ws.resize_float(Direction::Left, 1, BODY);
            ws.resize_float(Direction::Up, 1, BODY);
        }
        let (_, r) = ws.rects(BODY)[1];
        assert_eq!((r.width, r.height), (MIN_FLOAT_WIDTH, MIN_FLOAT_HEIGHT));

        ws.focus(1);
        assert!(!ws.move_float(Direction::Left, 1, BODY));
    }

    #[test]
    fn pane_at_finds_the_topmost() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        let (_, float) = ws.rects(BODY)[1];
        let inside = Position::new(float.x + 1, float.y + 1);
        assert_eq!(ws.pane_at(inside, BODY).map(|(id, _)| id), Some(2));
        assert_eq!(
            ws.pane_at(Position::new(0, 0), BODY).map(|(id, _)| id),
            Some(1)
        );
        assert_eq!(ws.pane_at(Position::new(0, 40), BODY), None);
    }

    #[test]
    fn set_float_rect_clamps() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        ws.set_float_rect(2, Rect::new(95, 38, 2, 2), BODY);
        let (_, r) = ws.rects(BODY)[1];
        assert_eq!((r.width, r.height), (MIN_FLOAT_WIDTH, MIN_FLOAT_HEIGHT));
        assert!(r.right() <= BODY.right() && r.bottom() <= BODY.bottom());
    }

    #[test]
    fn floats_shrink_with_the_screen() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        let small = Rect::new(0, 0, 30, 10);
        let (_, r) = ws.rects(small)[1];
        assert!(r.right() <= 30 && r.bottom() <= 10);
    }
}
