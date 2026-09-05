//! The window layout tree: how panes tile a window, as pure geometry.
//!
//! This is the structured layout from `docs/04` - a binary tree of splits with
//! pane leaves - kept entirely pure so every case (geometry, directional
//! navigation, splitting, removal-with-collapse) is table-testable in isolation.
//! The server turns the computed rects into real PTY sizes and render offsets.

use crate::PaneId;

/// A rectangle in cell coordinates: top-left `(x, y)`, size `w` x `h`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> u16 {
        self.x + self.w
    }
    pub fn bottom(&self) -> u16 {
        self.y + self.h
    }
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// A split's orientation.
#[derive(Clone, Copy, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum SplitDir {
    /// Panes side by side, separated by a vertical divider (tmux `%`).
    Horizontal,
    /// Panes stacked, separated by a horizontal divider (tmux `"`).
    Vertical,
}

/// A focus-movement direction.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// The layout tree: either a single pane, or a split of two subtrees.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LayoutNode {
    Leaf(PaneId),
    Split {
        dir: SplitDir,
        /// Fraction of the usable space given to `first` (0..1).
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

/// A 1-cell-thick divider drawn between two panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Divider {
    pub dir: SplitDir,
    pub rect: Rect,
}

/// The result of laying a tree into an area: each pane's rect, plus the dividers.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    pub panes: Vec<(PaneId, Rect)>,
    pub dividers: Vec<Divider>,
}

impl Layout {
    pub fn rect_of(&self, id: PaneId) -> Option<Rect> {
        self.panes.iter().find(|(p, _)| *p == id).map(|(_, r)| *r)
    }
}

impl LayoutNode {
    /// Lay the tree into `area`, producing pane rects and dividers.
    pub fn compute(&self, area: Rect) -> Layout {
        let mut out = Layout::default();
        self.walk(area, &mut out);
        out
    }

    fn walk(&self, area: Rect, out: &mut Layout) {
        match self {
            LayoutNode::Leaf(id) => out.panes.push((*id, area)),
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => match dir {
                SplitDir::Horizontal => {
                    if area.w < 3 {
                        // Too narrow to divide; give it all to `first`.
                        first.walk(area, out);
                        return;
                    }
                    let avail = area.w - 1; // reserve 1 col for the divider
                    let w1 = ((avail as f32 * ratio).round() as u16).clamp(1, avail - 1);
                    let w2 = avail - w1;
                    first.walk(Rect::new(area.x, area.y, w1, area.h), out);
                    out.dividers.push(Divider {
                        dir: *dir,
                        rect: Rect::new(area.x + w1, area.y, 1, area.h),
                    });
                    second.walk(Rect::new(area.x + w1 + 1, area.y, w2, area.h), out);
                }
                SplitDir::Vertical => {
                    if area.h < 3 {
                        first.walk(area, out);
                        return;
                    }
                    let avail = area.h - 1;
                    let h1 = ((avail as f32 * ratio).round() as u16).clamp(1, avail - 1);
                    let h2 = avail - h1;
                    first.walk(Rect::new(area.x, area.y, area.w, h1), out);
                    out.dividers.push(Divider {
                        dir: *dir,
                        rect: Rect::new(area.x, area.y + h1, area.w, 1),
                    });
                    second.walk(Rect::new(area.x, area.y + h1 + 1, area.w, h2), out);
                }
            },
        }
    }

    /// The first pane in a left-to-right, depth-first walk.
    pub fn first_pane(&self) -> PaneId {
        match self {
            LayoutNode::Leaf(id) => *id,
            LayoutNode::Split { first, .. } => first.first_pane(),
        }
    }

    /// All pane ids in the tree.
    pub fn pane_ids(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.collect(&mut v);
        v
    }

    fn collect(&self, v: &mut Vec<PaneId>) {
        match self {
            LayoutNode::Leaf(id) => v.push(*id),
            LayoutNode::Split { first, second, .. } => {
                first.collect(v);
                second.collect(v);
            }
        }
    }

    pub fn contains_pane(&self, id: PaneId) -> bool {
        match self {
            LayoutNode::Leaf(p) => *p == id,
            LayoutNode::Split { first, second, .. } => {
                first.contains_pane(id) || second.contains_pane(id)
            }
        }
    }

    /// Split the leaf holding `target` into `[target | new_pane]` (or stacked),
    /// giving each half of the space. Returns whether `target` was found.
    pub fn split(&mut self, target: PaneId, dir: SplitDir, new_pane: PaneId) -> bool {
        match self {
            LayoutNode::Leaf(id) if *id == target => {
                let old = *id;
                *self = LayoutNode::Split {
                    dir,
                    ratio: 0.5,
                    first: Box::new(LayoutNode::Leaf(old)),
                    second: Box::new(LayoutNode::Leaf(new_pane)),
                };
                true
            }
            LayoutNode::Leaf(_) => false,
            LayoutNode::Split { first, second, .. } => {
                first.split(target, dir, new_pane) || second.split(target, dir, new_pane)
            }
        }
    }

    /// Return a copy of the tree with `target` removed and its split collapsed
    /// into the sibling. `None` means the tree became empty (last pane closed).
    pub fn without(&self, target: PaneId) -> Option<LayoutNode> {
        match self {
            LayoutNode::Leaf(id) => {
                if *id == target {
                    None
                } else {
                    Some(LayoutNode::Leaf(*id))
                }
            }
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => match (first.without(target), second.without(target)) {
                (None, None) => None,
                (Some(n), None) | (None, Some(n)) => Some(n),
                (Some(a), Some(b)) => Some(LayoutNode::Split {
                    dir: *dir,
                    ratio: *ratio,
                    first: Box::new(a),
                    second: Box::new(b),
                }),
            },
        }
    }

    /// Nudge the split that resizes `target` in the given orientation by `delta`
    /// (as a fraction of the split's space). Adjusts the *nearest* ancestor split
    /// of that orientation on the path to `target`, clamped to a sane range.
    /// Returns whether a matching split was found and adjusted.
    pub fn resize_pane(&mut self, target: PaneId, orient: SplitDir, delta: f32) -> bool {
        matches!(self.resize_inner(target, orient, delta), Resize::Handled)
    }

    /// Drag the divider drawn at `divider` (as computed by `compute(area)`) so
    /// it lands on the pointer cell `(x, y)`. Walks the same geometry as
    /// `compute`, so the divider is identified by where it was drawn rather
    /// than by a tree path that a later split could invalidate. Returns false
    /// when no split draws a divider there.
    pub fn set_divider_at(&mut self, area: Rect, divider: Rect, x: u16, y: u16) -> bool {
        match self {
            LayoutNode::Leaf(_) => false,
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => match dir {
                SplitDir::Horizontal => {
                    if area.w < 3 {
                        return first.set_divider_at(area, divider, x, y);
                    }
                    let avail = area.w - 1;
                    let w1 = ((avail as f32 * *ratio).round() as u16).clamp(1, avail - 1);
                    let here = Rect::new(area.x + w1, area.y, 1, area.h);
                    if here == divider {
                        let wanted = x.saturating_sub(area.x).clamp(1, avail - 1);
                        *ratio = (f32::from(wanted) / f32::from(avail)).clamp(0.1, 0.9);
                        return true;
                    }
                    first.set_divider_at(Rect::new(area.x, area.y, w1, area.h), divider, x, y)
                        || second.set_divider_at(
                            Rect::new(area.x + w1 + 1, area.y, avail - w1, area.h),
                            divider,
                            x,
                            y,
                        )
                }
                SplitDir::Vertical => {
                    if area.h < 3 {
                        return first.set_divider_at(area, divider, x, y);
                    }
                    let avail = area.h - 1;
                    let h1 = ((avail as f32 * *ratio).round() as u16).clamp(1, avail - 1);
                    let here = Rect::new(area.x, area.y + h1, area.w, 1);
                    if here == divider {
                        let wanted = y.saturating_sub(area.y).clamp(1, avail - 1);
                        *ratio = (f32::from(wanted) / f32::from(avail)).clamp(0.1, 0.9);
                        return true;
                    }
                    first.set_divider_at(Rect::new(area.x, area.y, area.w, h1), divider, x, y)
                        || second.set_divider_at(
                            Rect::new(area.x, area.y + h1 + 1, area.w, avail - h1),
                            divider,
                            x,
                            y,
                        )
                }
            },
        }
    }

    fn resize_inner(&mut self, target: PaneId, orient: SplitDir, delta: f32) -> Resize {
        match self {
            LayoutNode::Leaf(id) => {
                if *id == target {
                    Resize::Found
                } else {
                    Resize::Absent
                }
            }
            LayoutNode::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let mut r = first.resize_inner(target, orient, delta);
                if matches!(r, Resize::Absent) {
                    r = second.resize_inner(target, orient, delta);
                }
                // The first matching-orientation ancestor on the way up handles
                // it (the split closest to `target`).
                if matches!(r, Resize::Found) && *dir == orient {
                    *ratio = (*ratio + delta).clamp(0.1, 0.9);
                    Resize::Handled
                } else {
                    r
                }
            }
        }
    }
}

/// Outcome of walking the tree during a resize.
enum Resize {
    /// `target` is not in this subtree.
    Absent,
    /// `target` found, but no matching split has adjusted yet.
    Found,
    /// A matching split adjusted its ratio.
    Handled,
}

fn overlaps_vertically(a: &Rect, b: &Rect) -> bool {
    a.y < b.bottom() && b.y < a.bottom()
}
fn overlaps_horizontally(a: &Rect, b: &Rect) -> bool {
    a.x < b.right() && b.x < a.right()
}

/// Find the nearest pane adjacent to `active` in `dir`, by geometry. Returns
/// `None` if there is no pane in that direction. Used for directional focus.
pub fn neighbor(panes: &[(PaneId, Rect)], active: PaneId, dir: Direction) -> Option<PaneId> {
    let a = panes.iter().find(|(p, _)| *p == active).map(|(_, r)| *r)?;
    let mut best: Option<(PaneId, u16)> = None;
    for (id, r) in panes {
        if *id == active {
            continue;
        }
        // `dist` uses saturating_sub because it is also evaluated for panes that
        // are not candidates in this direction (where the subtraction would
        // otherwise underflow); it is only consulted when `is_candidate`.
        let (is_candidate, dist) = match dir {
            Direction::Right => (
                r.x >= a.right() && overlaps_vertically(&a, r),
                r.x.saturating_sub(a.right()),
            ),
            Direction::Left => (
                r.right() <= a.x && overlaps_vertically(&a, r),
                a.x.saturating_sub(r.right()),
            ),
            Direction::Down => (
                r.y >= a.bottom() && overlaps_horizontally(&a, r),
                r.y.saturating_sub(a.bottom()),
            ),
            Direction::Up => (
                r.bottom() <= a.y && overlaps_horizontally(&a, r),
                a.y.saturating_sub(r.bottom()),
            ),
        };
        if is_candidate && best.is_none_or(|(_, d)| dist < d) {
            best = Some((*id, dist));
        }
    }
    best.map(|(id, _)| id)
}

/// Tile rectangles for an `n`-window overview (zoom out) inside `area`: a
/// near-square grid, row-major, remainders spread over the leading columns and
/// rows so the tiles fill the area exactly. Pure so the server's renderer and
/// its mouse hit-testing share one geometry.
pub fn overview_tiles(area: Rect, n: usize) -> Vec<Rect> {
    if n == 0 || area.w == 0 || area.h == 0 {
        return Vec::new();
    }
    let mut cols = 1usize;
    while cols * cols < n {
        cols += 1;
    }
    let rows = n.div_ceil(cols);
    let (bw, xw) = (area.w / cols as u16, (area.w % cols as u16) as usize);
    let (bh, xh) = (area.h / rows as u16, (area.h % rows as u16) as usize);
    let col_w = |c: usize| bw + u16::from(c < xw);
    let row_h = |r: usize| bh + u16::from(r < xh);
    let mut tiles = Vec::with_capacity(n);
    let mut y = area.y;
    let mut i = 0;
    for r in 0..rows {
        let mut x = area.x;
        for c in 0..cols {
            if i >= n {
                break;
            }
            tiles.push(Rect::new(x, y, col_w(c), row_h(r)));
            x += col_w(c);
            i += 1;
        }
        y += row_h(r);
    }
    tiles
}

/// The column count [`overview_tiles`] uses for `n` tiles (for keyboard
/// up/down movement across the same grid).
pub fn overview_cols(n: usize) -> usize {
    let mut cols = 1usize;
    while cols * cols < n {
        cols += 1;
    }
    cols
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u64) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn single_pane_fills_area() {
        let tree = LayoutNode::Leaf(p(1));
        let l = tree.compute(Rect::new(0, 0, 80, 24));
        assert_eq!(l.panes, vec![(p(1), Rect::new(0, 0, 80, 24))]);
        assert!(l.dividers.is_empty());
    }

    #[test]
    fn horizontal_split_reserves_divider() {
        let mut tree = LayoutNode::Leaf(p(1));
        assert!(tree.split(p(1), SplitDir::Horizontal, p(2)));
        let l = tree.compute(Rect::new(0, 0, 81, 24));
        // 80 usable cols -> 40 | divider | 40
        assert_eq!(l.rect_of(p(1)), Some(Rect::new(0, 0, 40, 24)));
        assert_eq!(l.rect_of(p(2)), Some(Rect::new(41, 0, 40, 24)));
        assert_eq!(l.dividers.len(), 1);
        assert_eq!(l.dividers[0].rect, Rect::new(40, 0, 1, 24));
    }

    #[test]
    fn vertical_split_stacks() {
        let mut tree = LayoutNode::Leaf(p(1));
        assert!(tree.split(p(1), SplitDir::Vertical, p(2)));
        let l = tree.compute(Rect::new(0, 0, 80, 25));
        assert_eq!(l.rect_of(p(1)), Some(Rect::new(0, 0, 80, 12)));
        assert_eq!(l.rect_of(p(2)), Some(Rect::new(0, 13, 80, 12)));
    }

    #[test]
    fn nested_splits() {
        let mut tree = LayoutNode::Leaf(p(1));
        tree.split(p(1), SplitDir::Horizontal, p(2)); // 1 | 2
        tree.split(p(2), SplitDir::Vertical, p(3)); // 1 | (2 over 3)
        let l = tree.compute(Rect::new(0, 0, 81, 25));
        assert_eq!(l.pane_ids_len(), 3);
        assert_eq!(l.rect_of(p(1)), Some(Rect::new(0, 0, 40, 25)));
        // right column split into top/bottom
        assert_eq!(l.rect_of(p(2)).unwrap().x, 41);
        assert_eq!(l.rect_of(p(3)).unwrap().x, 41);
        assert!(l.rect_of(p(2)).unwrap().y < l.rect_of(p(3)).unwrap().y);
    }

    #[test]
    fn remove_collapses_to_sibling() {
        let mut tree = LayoutNode::Leaf(p(1));
        tree.split(p(1), SplitDir::Horizontal, p(2));
        let after = tree.without(p(2)).unwrap();
        assert!(matches!(after, LayoutNode::Leaf(x) if x == p(1)));
    }

    #[test]
    fn remove_last_pane_empties_tree() {
        let tree = LayoutNode::Leaf(p(1));
        assert!(tree.without(p(1)).is_none());
    }

    #[test]
    fn resize_adjusts_nearest_matching_split() {
        let mut tree = LayoutNode::Leaf(p(1));
        tree.split(p(1), SplitDir::Horizontal, p(2)); // 1 | 2, ratio 0.5
                                                      // Grow pane 1 rightward: horizontal split ratio increases.
        assert!(tree.resize_pane(p(1), SplitDir::Horizontal, 0.1));
        let l = tree.compute(Rect::new(0, 0, 101, 24));
        // 100 usable cols; ratio 0.6 -> pane 1 gets 60.
        assert_eq!(l.rect_of(p(1)).unwrap().w, 60);
        // A vertical resize finds no vertical ancestor -> no-op.
        assert!(!tree.resize_pane(p(1), SplitDir::Vertical, 0.1));
    }

    #[test]
    fn resize_clamps_ratio() {
        let mut tree = LayoutNode::Leaf(p(1));
        tree.split(p(1), SplitDir::Horizontal, p(2));
        for _ in 0..50 {
            tree.resize_pane(p(1), SplitDir::Horizontal, 0.1);
        }
        let l = tree.compute(Rect::new(0, 0, 101, 24));
        // Clamped to 0.9 -> 90 cols, never the whole width.
        assert_eq!(l.rect_of(p(1)).unwrap().w, 90);
    }

    #[test]
    fn directional_neighbor() {
        // 1 | 2  side by side
        let panes = vec![
            (p(1), Rect::new(0, 0, 40, 24)),
            (p(2), Rect::new(41, 0, 40, 24)),
        ];
        assert_eq!(neighbor(&panes, p(1), Direction::Right), Some(p(2)));
        assert_eq!(neighbor(&panes, p(2), Direction::Left), Some(p(1)));
        assert_eq!(neighbor(&panes, p(1), Direction::Left), None);
        assert_eq!(neighbor(&panes, p(1), Direction::Up), None);
    }

    #[test]
    fn neighbor_picks_nearest() {
        // active at left; two panes to the right at different distances.
        let panes = vec![
            (p(1), Rect::new(0, 0, 20, 24)),
            (p(2), Rect::new(21, 0, 20, 24)),
            (p(3), Rect::new(42, 0, 20, 24)),
        ];
        assert_eq!(neighbor(&panes, p(1), Direction::Right), Some(p(2)));
    }

    // small helper for the nested test
    impl Layout {
        fn pane_ids_len(&self) -> usize {
            self.panes.len()
        }
    }

    #[test]
    fn overview_tiles_fill_the_area_exactly() {
        let area = Rect::new(0, 1, 101, 29); // odd sizes exercise remainders
        for n in 1..=9 {
            let tiles = overview_tiles(area, n);
            assert_eq!(tiles.len(), n);
            // Tiles cover the area: total cell count matches, no overlaps, and
            // every tile stays inside.
            let cols = overview_cols(n);
            let rows = n.div_ceil(cols);
            for t in &tiles {
                assert!(t.x >= area.x && t.right() <= area.right());
                assert!(t.y >= area.y && t.bottom() <= area.bottom());
                assert!(t.w > 0 && t.h > 0);
            }
            // A full row of tiles spans the full width.
            let first_row: u16 = tiles.iter().take(cols.min(n)).map(|t| t.w).sum();
            if n >= cols {
                assert_eq!(first_row, area.w, "n={n}");
            }
            // A full column of tiles spans the full height (row heights).
            let col_h: u16 = (0..rows)
                .filter_map(|r| tiles.get(r * cols).map(|t| t.h))
                .sum();
            assert_eq!(col_h, area.h, "n={n}");
        }
    }

    #[test]
    fn overview_grid_shape_is_near_square() {
        assert_eq!(overview_cols(1), 1);
        assert_eq!(overview_cols(2), 2);
        assert_eq!(overview_cols(4), 2);
        assert_eq!(overview_cols(5), 3);
        assert_eq!(overview_cols(9), 3);
        assert_eq!(overview_cols(10), 4);
        assert!(overview_tiles(Rect::new(0, 0, 80, 24), 0).is_empty());
    }

    #[test]
    fn dragging_a_divider_moves_it_to_the_pointer_and_stays_bounded() {
        let mut root = LayoutNode::Split {
            dir: SplitDir::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(PaneId(1))),
            second: Box::new(LayoutNode::Split {
                dir: SplitDir::Vertical,
                ratio: 0.5,
                first: Box::new(LayoutNode::Leaf(PaneId(2))),
                second: Box::new(LayoutNode::Leaf(PaneId(3))),
            }),
        };
        let area = Rect::new(0, 0, 81, 21);
        let layout = root.compute(area);
        assert_eq!(layout.dividers.len(), 2);
        let outer = layout.dividers[0].rect;
        assert_eq!(outer, Rect::new(40, 0, 1, 21));

        // Dragging the outer divider to column 20 gives the left pane 20 cells.
        assert!(root.set_divider_at(area, outer, 20, 5));
        let moved = root.compute(area);
        assert_eq!(moved.rect_of(PaneId(1)), Some(Rect::new(0, 0, 20, 21)));

        // The nested divider is found through the recomputed geometry, and a
        // drag past the edge clamps to the 10 to 90 percent band.
        assert_eq!(moved.dividers[1].dir, SplitDir::Vertical);
        let inner = moved.dividers[1].rect;
        assert!(root.set_divider_at(area, inner, 60, 0));
        let clamped = root.compute(area);
        let top = clamped.rect_of(PaneId(2)).unwrap();
        assert_eq!(top.h, 2);

        // A rect where no divider is drawn changes nothing.
        assert!(!root.set_divider_at(area, Rect::new(5, 5, 1, 1), 5, 5));
    }
}
