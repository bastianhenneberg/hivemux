//! A workspace: a tiling layout with floating panes on top, and which pane
//! has focus. Pure geometry, the panes themselves live in the app.

use ratatui::layout::Rect;

use crate::layout::{Axis, Direction, Layout, PaneId};

/// The smallest floating pane, border included.
pub const MIN_FLOAT_WIDTH: u16 = 12;
pub const MIN_FLOAT_HEIGHT: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Float {
    pub id: PaneId,
    pub rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Workspace {
    pub layout: Layout,
    /// Floating panes, bottom to top.
    pub floats: Vec<Float>,
    pub focus: PaneId,
    /// The tiled pane that had focus last, where focus returns to from a
    /// floating pane.
    last_tiled: Option<PaneId>,
}

impl Workspace {
    /// A workspace with one tiled pane.
    pub fn new(pane: PaneId) -> Self {
        Self {
            layout: Layout::new(pane),
            floats: Vec::new(),
            focus: pane,
            last_tiled: Some(pane),
        }
    }

    /// A workspace with nothing in it, as a placeholder.
    pub fn empty() -> Self {
        Self {
            layout: Layout::empty(),
            floats: Vec::new(),
            focus: 0,
            last_tiled: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.layout.is_empty() && self.floats.is_empty()
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
        let mut out = self.layout.rects(body);
        out.extend(self.floats.iter().map(|f| (f.id, clamp(f.rect, body))));
        out
    }

    /// Focuses `pane`, raising it to the top if it floats.
    pub fn focus(&mut self, pane: PaneId) {
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
        let mut all = self.layout.panes();
        all.extend(self.floats.iter().map(|f| f.id));
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
    pub fn split(&mut self, axis: Axis, new: PaneId) -> bool {
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
    fn floats_shrink_with_the_screen() {
        let mut ws = Workspace::new(1);
        ws.add_float(2, BODY);
        let small = Rect::new(0, 0, 30, 10);
        let (_, r) = ws.rects(small)[1];
        assert!(r.right() <= 30 && r.bottom() <= 10);
    }
}
