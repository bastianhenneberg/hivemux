//! The tiling layout: a binary space partitioning tree whose leaves are panes.
//!
//! The tree only knows geometry. It does not own the panes, it refers to them
//! by `PaneId`.

use ratatui::layout::{Position, Rect};
use serde::{Deserialize, Serialize};

pub type PaneId = usize;

/// How a split arranges its two children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Axis {
    /// Side by side, divided by a vertical line (tmux `%`).
    Row,
    /// Stacked, divided by a horizontal line (tmux `"`).
    Column,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Row,
            Direction::Up | Direction::Down => Axis::Column,
        }
    }
}

/// The smallest size a pane may have along a split axis: a border on each
/// side plus one cell of content.
pub const MIN_PANE_SIZE: u16 = 3;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum Node {
    Leaf(PaneId),
    Split {
        axis: Axis,
        /// Share of the first child, between 0 and 1.
        ratio: f32,
        first: Box<Node>,
        second: Box<Node>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    root: Option<Node>,
}

impl Layout {
    pub fn new(pane: PaneId) -> Self {
        Self {
            root: Some(Node::Leaf(pane)),
        }
    }

    /// A layout without panes.
    pub fn empty() -> Self {
        Self { root: None }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.root.as_ref().is_some_and(|root| root.contains(pane))
    }

    /// Splits `target` in half along `axis`, putting `new` second (right or
    /// below). Returns false if `target` is not in the layout.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        match &mut self.root {
            Some(root) => root.split(target, axis, new),
            None => false,
        }
    }

    /// Removes `target` and gives its space to its sibling. Returns the pane
    /// that should receive focus next, if any is left.
    pub fn remove(&mut self, target: PaneId) -> Option<PaneId> {
        let root = self.root.take()?;
        match root {
            Node::Leaf(id) if id == target => None,
            mut root => {
                let next = root.remove(target);
                self.root = Some(root);
                next.or_else(|| self.panes().first().copied())
            }
        }
    }

    /// All panes in reading order (left to right, top to bottom within each
    /// split), with the area each one covers inside `area`.
    pub fn rects(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            root.rects(area, &mut out);
        }
        out
    }

    pub fn panes(&self) -> Vec<PaneId> {
        self.rects(Rect::new(0, 0, u16::MAX / 2, u16::MAX / 2))
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// The pane next to `from` in direction `dir`: of all panes touching that
    /// edge, the one sharing the longest stretch of it.
    pub fn neighbor(&self, from: PaneId, dir: Direction, area: Rect) -> Option<PaneId> {
        let rects = self.rects(area);
        let (_, r) = *rects.iter().find(|(id, _)| *id == from)?;
        rects
            .iter()
            .filter(|(id, _)| *id != from)
            .filter_map(|&(id, c)| {
                let (touches, overlap) = match dir {
                    Direction::Left => (
                        c.right() == r.left(),
                        overlap(c.y, c.bottom(), r.y, r.bottom()),
                    ),
                    Direction::Right => (
                        c.left() == r.right(),
                        overlap(c.y, c.bottom(), r.y, r.bottom()),
                    ),
                    Direction::Up => (
                        c.bottom() == r.top(),
                        overlap(c.x, c.right(), r.x, r.right()),
                    ),
                    Direction::Down => (
                        c.top() == r.bottom(),
                        overlap(c.x, c.right(), r.x, r.right()),
                    ),
                };
                (touches && overlap > 0).then_some((id, overlap))
            })
            // On a tie the first pane in reading order wins, i.e. the top or
            // left one.
            .min_by_key(|&(_, overlap)| std::cmp::Reverse(overlap))
            .map(|(id, _)| id)
    }

    /// Exchanges the places of panes `a` and `b`.
    pub fn swap(&mut self, a: PaneId, b: PaneId) {
        if let Some(root) = &mut self.root {
            root.map_leaves(&mut |id| {
                if id == a {
                    b
                } else if id == b {
                    a
                } else {
                    id
                }
            });
        }
    }

    /// The divider under `pos`, as the path to its split (false = first
    /// child, true = second). With a border on each pane, the two border
    /// cells meeting at a divider both count. The innermost divider wins.
    pub fn divider_at(&self, pos: Position, area: Rect) -> Option<Vec<bool>> {
        let mut path = Vec::new();
        self.root
            .as_ref()?
            .divider_at(pos, area, &mut path)
            .then_some(path)
    }

    /// Moves the divider at `path` to `to`: its column for side by side
    /// panes, its row for stacked ones, within the limits.
    pub fn move_divider(&mut self, path: &[bool], to: Position, area: Rect) {
        if let Some(root) = &mut self.root {
            root.move_divider(path, to, area);
        }
    }

    /// Gives every pane in a row or column the same share of it.
    pub fn equalize(&mut self) {
        if let Some(root) = &mut self.root {
            root.equalize();
        }
    }

    /// Moves the divider closest to `target` along `dir`'s axis by `cells`
    /// in direction `dir`. Returns false if there is no such divider.
    pub fn resize(&mut self, target: PaneId, dir: Direction, cells: u16, area: Rect) -> bool {
        let delta = match dir {
            Direction::Left | Direction::Up => -i32::from(cells),
            Direction::Right | Direction::Down => i32::from(cells),
        };
        match &mut self.root {
            Some(root) => root.resize(target, dir.axis(), delta, area) == Resize::Done,
            None => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Resize {
    NotHere,
    /// The target is in this subtree, but no split on the way had the right axis.
    Pending,
    Done,
}

impl Node {
    fn divider_at(&self, pos: Position, area: Rect, path: &mut Vec<bool>) -> bool {
        let Node::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return false;
        };
        if !area.contains(pos) {
            return false;
        }
        let (a, b) = split_rect(area, *axis, *ratio);
        path.push(false);
        if first.divider_at(pos, a, path) {
            return true;
        }
        path.pop();
        path.push(true);
        if second.divider_at(pos, b, path) {
            return true;
        }
        path.pop();
        let (edge, at) = match axis {
            Axis::Row => (b.x, pos.x),
            Axis::Column => (b.y, pos.y),
        };
        at + 1 == edge || at == edge
    }

    fn move_divider(&mut self, path: &[bool], to: Position, area: Rect) {
        let Node::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        else {
            return;
        };
        let (a, b) = split_rect(area, *axis, *ratio);
        match path.split_first() {
            Some((false, rest)) => first.move_divider(rest, to, a),
            Some((true, rest)) => second.move_divider(rest, to, b),
            None => {
                let (start, total, to) = match axis {
                    Axis::Row => (area.x, area.width, to.x),
                    Axis::Column => (area.y, area.height, to.y),
                };
                if total < 2 * MIN_PANE_SIZE {
                    return;
                }
                let first_size = to
                    .saturating_sub(start)
                    .clamp(MIN_PANE_SIZE, total - MIN_PANE_SIZE);
                *ratio = f32::from(first_size) / f32::from(total);
            }
        }
    }

    fn map_leaves(&mut self, f: &mut impl FnMut(PaneId) -> PaneId) {
        match self {
            Node::Leaf(id) => *id = f(*id),
            Node::Split { first, second, .. } => {
                first.map_leaves(f);
                second.map_leaves(f);
            }
        }
    }

    /// How many panes share this node's space along `axis`: nested splits
    /// in the same direction add up, anything else counts as one.
    fn weight(&self, axis: Axis) -> u32 {
        match self {
            Node::Split {
                axis: own,
                first,
                second,
                ..
            } if *own == axis => first.weight(axis) + second.weight(axis),
            _ => 1,
        }
    }

    fn equalize(&mut self) {
        if let Node::Split {
            axis,
            ratio,
            first,
            second,
        } = self
        {
            let a = first.weight(*axis) as f32;
            let b = second.weight(*axis) as f32;
            *ratio = a / (a + b);
            first.equalize();
            second.equalize();
        }
    }

    fn contains(&self, target: PaneId) -> bool {
        match self {
            Node::Leaf(id) => *id == target,
            Node::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    fn first_leaf(&self) -> PaneId {
        match self {
            Node::Leaf(id) => *id,
            Node::Split { first, .. } => first.first_leaf(),
        }
    }

    fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        match self {
            Node::Leaf(id) if *id == target => {
                *self = Node::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(Node::Leaf(target)),
                    second: Box::new(Node::Leaf(new)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { first, second, .. } => {
                first.split(target, axis, new) || second.split(target, axis, new)
            }
        }
    }

    /// Removes `target` from below this node. Must not be called on a leaf
    /// that is `target` itself, the parent handles that case.
    fn remove(&mut self, target: PaneId) -> Option<PaneId> {
        let Node::Split { first, second, .. } = self else {
            return None;
        };
        let survivor = if matches!(**first, Node::Leaf(id) if id == target) {
            std::mem::replace(&mut **second, Node::Leaf(0))
        } else if matches!(**second, Node::Leaf(id) if id == target) {
            std::mem::replace(&mut **first, Node::Leaf(0))
        } else if first.contains(target) {
            return first.remove(target);
        } else {
            return second.remove(target);
        };
        let focus = survivor.first_leaf();
        *self = survivor;
        Some(focus)
    }

    fn rects(&self, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match self {
            Node::Leaf(id) => out.push((*id, area)),
            Node::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                let (a, b) = split_rect(area, *axis, *ratio);
                first.rects(a, out);
                second.rects(b, out);
            }
        }
    }

    fn resize(&mut self, target: PaneId, axis: Axis, delta: i32, area: Rect) -> Resize {
        let Node::Split {
            axis: own_axis,
            ratio,
            first,
            second,
        } = self
        else {
            return match self {
                Node::Leaf(id) if *id == target => Resize::Pending,
                _ => Resize::NotHere,
            };
        };

        let (a, b) = split_rect(area, *own_axis, *ratio);
        let result = match first.resize(target, axis, delta, a) {
            Resize::NotHere => second.resize(target, axis, delta, b),
            found => found,
        };
        if result != Resize::Pending || *own_axis != axis {
            return result;
        }

        let total = i32::from(match axis {
            Axis::Row => area.width,
            Axis::Column => area.height,
        });
        let min = i32::from(MIN_PANE_SIZE);
        if total < 2 * min {
            return Resize::Pending;
        }
        let current = (total as f32 * *ratio).round() as i32;
        let wanted = (current + delta).clamp(min, total - min);
        *ratio = wanted as f32 / total as f32;
        Resize::Done
    }
}

/// Splits `area` into two parts along `axis`, the first one getting `ratio`
/// of the space, but each at least `MIN_PANE_SIZE` when there is room.
fn split_rect(area: Rect, axis: Axis, ratio: f32) -> (Rect, Rect) {
    let total = match axis {
        Axis::Row => area.width,
        Axis::Column => area.height,
    };
    let first = if total >= 2 * MIN_PANE_SIZE {
        ((f32::from(total) * ratio).round() as u16).clamp(MIN_PANE_SIZE, total - MIN_PANE_SIZE)
    } else {
        total / 2
    };
    match axis {
        Axis::Row => (
            Rect {
                width: first,
                ..area
            },
            Rect {
                x: area.x + first,
                width: total - first,
                ..area
            },
        ),
        Axis::Column => (
            Rect {
                height: first,
                ..area
            },
            Rect {
                y: area.y + first,
                height: total - first,
                ..area
            },
        ),
    }
}

fn overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> u16 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 100, 40);

    /// 1 | 2
    ///   | -
    ///   | 3
    fn three_panes() -> Layout {
        let mut layout = Layout::new(1);
        assert!(layout.split(1, Axis::Row, 2));
        assert!(layout.split(2, Axis::Column, 3));
        layout
    }

    #[test]
    fn split_divides_the_area() {
        let rects = three_panes().rects(AREA);
        assert_eq!(
            rects,
            vec![
                (1, Rect::new(0, 0, 50, 40)),
                (2, Rect::new(50, 0, 50, 20)),
                (3, Rect::new(50, 20, 50, 20)),
            ]
        );
    }

    #[test]
    fn split_unknown_pane_does_nothing() {
        let mut layout = Layout::new(1);
        assert!(!layout.split(7, Axis::Row, 2));
        assert_eq!(layout.panes(), vec![1]);
    }

    #[test]
    fn remove_gives_space_to_the_sibling() {
        let mut layout = three_panes();
        assert_eq!(layout.remove(2), Some(3));
        assert_eq!(
            layout.rects(AREA),
            vec![(1, Rect::new(0, 0, 50, 40)), (3, Rect::new(50, 0, 50, 40))]
        );

        assert_eq!(layout.remove(1), Some(3));
        assert_eq!(layout.rects(AREA), vec![(3, AREA)]);

        assert_eq!(layout.remove(3), None);
        assert!(layout.is_empty());
    }

    #[test]
    fn remove_first_child_focuses_the_nearest_leaf_of_the_sibling() {
        let mut layout = three_panes();
        assert_eq!(layout.remove(1), Some(2));
        assert_eq!(layout.panes(), vec![2, 3]);
    }

    #[test]
    fn neighbors() {
        let layout = three_panes();
        assert_eq!(layout.neighbor(1, Direction::Right, AREA), Some(2));
        assert_eq!(layout.neighbor(3, Direction::Left, AREA), Some(1));
        assert_eq!(layout.neighbor(2, Direction::Down, AREA), Some(3));
        assert_eq!(layout.neighbor(3, Direction::Up, AREA), Some(2));
        assert_eq!(layout.neighbor(1, Direction::Left, AREA), None);
        assert_eq!(layout.neighbor(1, Direction::Up, AREA), None);
    }

    #[test]
    fn resize_moves_the_nearest_matching_divider() {
        let mut layout = three_panes();

        // Pane 3 has no row split of its own, so the root divider moves.
        assert!(layout.resize(3, Direction::Left, 10, AREA));
        assert_eq!(layout.rects(AREA)[0].1.width, 40);

        // Pane 3 moves the divider between 2 and 3.
        assert!(layout.resize(3, Direction::Up, 5, AREA));
        assert_eq!(layout.rects(AREA)[1].1.height, 15);
    }

    #[test]
    fn dividers_are_found_and_moved() {
        let mut layout = three_panes();
        // The root divider sits between column 49 (pane 1's border) and 50.
        assert_eq!(layout.divider_at(Position::new(49, 10), AREA), Some(vec![]));
        assert_eq!(layout.divider_at(Position::new(50, 30), AREA), Some(vec![]));
        // The divider between 2 and 3, inside the right half.
        assert_eq!(
            layout.divider_at(Position::new(70, 19), AREA),
            Some(vec![true])
        );
        assert_eq!(layout.divider_at(Position::new(20, 10), AREA), None);

        layout.move_divider(&[], Position::new(30, 5), AREA);
        assert_eq!(layout.rects(AREA)[0].1.width, 30);
        layout.move_divider(&[true], Position::new(60, 10), AREA);
        assert_eq!(layout.rects(AREA)[1].1.height, 10);
        layout.move_divider(&[], Position::new(99, 5), AREA);
        assert_eq!(layout.rects(AREA)[0].1.width, 100 - MIN_PANE_SIZE);
    }

    #[test]
    fn swap_exchanges_places() {
        let mut layout = three_panes();
        layout.swap(1, 3);
        assert_eq!(layout.panes(), vec![3, 2, 1]);
        assert_eq!(layout.rects(AREA)[0], (3, Rect::new(0, 0, 50, 40)));
    }

    #[test]
    fn equalize_shares_rows_and_columns_evenly() {
        // 1 | 2 | 3 as nested row splits: each gets a third.
        let mut layout = Layout::new(1);
        layout.split(1, Axis::Row, 2);
        layout.split(2, Axis::Row, 3);
        layout.resize(1, Direction::Right, 30, AREA);
        layout.equalize();
        let widths: Vec<u16> = layout
            .rects(Rect::new(0, 0, 99, 40))
            .iter()
            .map(|(_, r)| r.width)
            .collect();
        assert_eq!(widths, vec![33, 33, 33]);
    }

    #[test]
    fn resize_is_clamped_and_needs_a_divider() {
        let mut layout = three_panes();
        assert!(layout.resize(1, Direction::Right, 500, AREA));
        assert_eq!(layout.rects(AREA)[0].1.width, 100 - MIN_PANE_SIZE);

        // Pane 1 spans the full height, there is no row divider to move.
        assert!(!layout.resize(1, Direction::Down, 1, AREA));
        assert!(!Layout::new(1).resize(1, Direction::Left, 1, AREA));
    }
}
