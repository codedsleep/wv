//! BSP `Node` tree.

use crate::backend::PaneId;
use crate::layout::geometry::{Direction, FRect, Rect, Split};

/// Smallest share of a split either side may be squeezed to.
///
/// Expressed as a ratio rather than cells so it holds at any terminal size; a
/// pane squeezed past this has no room left for its borders.
const MIN_SPLIT_RATIO: f32 = 0.05;

fn clamp_ratio(ratio: f32) -> f32 {
    ratio.clamp(MIN_SPLIT_RATIO, 1.0 - MIN_SPLIT_RATIO)
}

/// The split axis a direction moves a boundary along.
///
/// Left and right move a vertical divider — the one that splits the width.
const fn split_for_direction(dir: Direction) -> Split {
    match dir {
        Direction::Left | Direction::Right => Split::Vertical,
        Direction::Up | Direction::Down => Split::Horizontal,
    }
}

#[derive(Clone, Debug)]
pub enum Node {
    Leaf {
        pane: PaneId,
        rect_current: FRect,
        rect_target: Rect,
    },
    Internal {
        split: Split,
        ratio: f32,
        ratio_target: f32,
        a: Box<Node>,
        b: Box<Node>,
        rect: Rect,
    },
}

impl Node {
    pub fn compute_layout(&mut self, root_rect: Rect) {
        match self {
            Self::Leaf { rect_target, .. } => *rect_target = root_rect,
            Self::Internal {
                split,
                ratio_target,
                a,
                b,
                rect,
                ..
            } => {
                *rect = root_rect;
                let (a_rect, b_rect) = root_rect.split(*split, *ratio_target);
                a.compute_layout(a_rect);
                b.compute_layout(share_divider(b_rect, *split));
            }
        }
    }

    pub fn find_leaf(&self, pane: PaneId) -> Option<&Self> {
        match self {
            Self::Leaf {
                pane: leaf_pane, ..
            } if *leaf_pane == pane => Some(self),
            Self::Leaf { .. } => None,
            Self::Internal { a, b, .. } => a.find_leaf(pane).or_else(|| b.find_leaf(pane)),
        }
    }

    pub fn find_leaf_mut(&mut self, pane: PaneId) -> Option<&mut Self> {
        match self {
            Self::Leaf {
                pane: leaf_pane, ..
            } if *leaf_pane == pane => Some(self),
            Self::Leaf { .. } => None,
            Self::Internal { a, b, .. } => a.find_leaf_mut(pane).or_else(|| b.find_leaf_mut(pane)),
        }
    }

    pub fn split_focused(&mut self, focused: PaneId, split: Split, new_pane: PaneId) {
        let _ = self.try_split_focused(focused, split, new_pane);
    }

    pub fn close(&mut self, pane: PaneId) -> bool {
        match self {
            Self::Leaf { .. } => false,
            Self::Internal { a, b, .. } if a.is_leaf_for(pane) => {
                *self = (**b).clone();
                true
            }
            Self::Internal { a, b, .. } if b.is_leaf_for(pane) => {
                *self = (**a).clone();
                true
            }
            Self::Internal { a, b, .. } => a.close(pane) || b.close(pane),
        }
    }

    pub fn focus_neighbor(&self, focused: PaneId, dir: Direction) -> Option<PaneId> {
        let focused_rect = self.leaf_rect(focused)?;
        let mut best = None;
        self.find_neighbor(focused, focused_rect, dir, &mut best);
        best.map(|candidate| candidate.pane)
    }

    fn try_split_focused(&mut self, focused: PaneId, split: Split, new_pane: PaneId) -> bool {
        match self {
            Self::Leaf {
                pane,
                rect_current,
                rect_target,
            } if *pane == focused => {
                let old_pane = *pane;
                let placeholder_current = *rect_current;
                let placeholder_target = *rect_target;
                *self = Self::Internal {
                    split,
                    ratio: 0.5,
                    ratio_target: 0.5,
                    a: Box::new(Self::Leaf {
                        pane: old_pane,
                        rect_current: placeholder_current,
                        rect_target: placeholder_target,
                    }),
                    b: Box::new(Self::Leaf {
                        pane: new_pane,
                        rect_current: placeholder_current,
                        rect_target: placeholder_target,
                    }),
                    rect: placeholder_target,
                };
                true
            }
            Self::Leaf { .. } => false,
            Self::Internal { a, b, .. } => {
                a.try_split_focused(focused, split, new_pane)
                    || b.try_split_focused(focused, split, new_pane)
            }
        }
    }

    /// Every leaf's pane id, in layout order.
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Self::Leaf { pane, .. } => out.push(*pane),
            Self::Internal { a, b, .. } => {
                a.collect_leaves(out);
                b.collect_leaves(out);
            }
        }
    }

    /// Set the ratio of the split directly above `pane`.
    ///
    /// This is what `split-window -p 30` seeds so the new pane arrives at the
    /// size that was asked for rather than at half.
    pub fn set_parent_ratio(&mut self, pane: PaneId, ratio: f32) -> bool {
        match self {
            Self::Leaf { .. } => false,
            Self::Internal {
                ratio_target, a, b, ..
            } if a.is_leaf_for(pane) || b.is_leaf_for(pane) => {
                *ratio_target = clamp_ratio(ratio);
                true
            }
            Self::Internal { a, b, .. } => {
                a.set_parent_ratio(pane, ratio) || b.set_parent_ratio(pane, ratio)
            }
        }
    }

    /// Move the boundary nearest `pane` along `dir` by `cells`.
    ///
    /// Which side of that boundary the pane sits on decides whether it grows
    /// or shrinks, which is how tmux's `resize-pane -L` behaves: it moves a
    /// border, it does not always enlarge.
    ///
    /// Returns false when there is no split on that axis — a lone pane, or one
    /// whose only splits run the other way.
    pub fn resize_leaf(&mut self, pane: PaneId, dir: Direction, cells: u16) -> bool {
        let axis = split_for_direction(dir);
        let Some(delta) = self.boundary_delta(pane, axis, dir, cells) else {
            return false;
        };

        self.adjust_nearest_split(pane, axis, delta)
    }

    /// Set the size of `pane` along one axis, for `resize-pane -x`/`-y`.
    pub fn resize_leaf_to(&mut self, pane: PaneId, axis: Split, cells: u16) -> bool {
        let Some((extent, pane_is_first)) = self.split_extent_for(pane, axis) else {
            return false;
        };
        if extent == 0 {
            return false;
        }

        let wanted = f32::from(cells) / f32::from(extent);
        let ratio = if pane_is_first { wanted } else { 1.0 - wanted };

        self.adjust_nearest_split_to(pane, axis, clamp_ratio(ratio))
    }

    /// Exchange the positions of two panes, leaving the shape alone.
    pub fn swap_leaves(&mut self, first: PaneId, second: PaneId) -> bool {
        if first == second {
            return false;
        }
        if self.find_leaf(first).is_none() || self.find_leaf(second).is_none() {
            return false;
        }

        // One pass, each leaf visited once. Replacing them one at a time
        // instead would find the leaf just written and swap it straight back.
        self.walk_leaves_mut(&mut |pane| {
            if *pane == first {
                *pane = second;
            } else if *pane == second {
                *pane = first;
            }
        });

        true
    }

    /// Overwrite every leaf's pane in layout order, for `rotate-window`.
    ///
    /// The shape is untouched; only which pane sits where changes.
    pub fn set_leaves(&mut self, panes: &mut impl Iterator<Item = PaneId>) {
        self.walk_leaves_mut(&mut |pane| {
            if let Some(next) = panes.next() {
                *pane = next;
            }
        });
    }

    fn walk_leaves_mut(&mut self, visit: &mut impl FnMut(&mut PaneId)) {
        match self {
            Self::Leaf { pane, .. } => visit(pane),
            Self::Internal { a, b, .. } => {
                a.walk_leaves_mut(visit);
                b.walk_leaves_mut(visit);
            }
        }
    }

    /// How far a boundary can move, as a ratio delta on the split that owns it.
    fn boundary_delta(
        &self,
        pane: PaneId,
        axis: Split,
        dir: Direction,
        cells: u16,
    ) -> Option<f32> {
        let (extent, _) = self.split_extent_for(pane, axis)?;
        if extent == 0 {
            return None;
        }

        let magnitude = f32::from(cells) / f32::from(extent);

        Some(match dir {
            Direction::Left | Direction::Up => -magnitude,
            Direction::Right | Direction::Down => magnitude,
        })
    }

    /// The extent of the nearest split on `axis` above `pane`, and whether the
    /// pane sits in its first half.
    fn split_extent_for(&self, pane: PaneId, axis: Split) -> Option<(u16, bool)> {
        match self {
            Self::Leaf { .. } => None,
            Self::Internal {
                split, a, b, rect, ..
            } if *split == axis && (a.find_leaf(pane).is_some() || b.find_leaf(pane).is_some()) => {
                // Prefer a deeper split on the same axis: the nearest boundary
                // is the one the user means.
                let deeper = a
                    .split_extent_for(pane, axis)
                    .or_else(|| b.split_extent_for(pane, axis));
                if let Some(deeper) = deeper {
                    return Some(deeper);
                }

                let extent = match axis {
                    Split::Vertical => rect.w,
                    Split::Horizontal => rect.h,
                };
                Some((extent, a.find_leaf(pane).is_some()))
            }
            Self::Internal { a, b, .. } => a
                .split_extent_for(pane, axis)
                .or_else(|| b.split_extent_for(pane, axis)),
        }
    }

    fn adjust_nearest_split(&mut self, pane: PaneId, axis: Split, delta: f32) -> bool {
        self.with_nearest_split(pane, axis, &mut |ratio_target| {
            *ratio_target = clamp_ratio(*ratio_target + delta);
        })
    }

    fn adjust_nearest_split_to(&mut self, pane: PaneId, axis: Split, ratio: f32) -> bool {
        self.with_nearest_split(pane, axis, &mut |ratio_target| *ratio_target = ratio)
    }

    fn with_nearest_split(
        &mut self,
        pane: PaneId,
        axis: Split,
        apply: &mut impl FnMut(&mut f32),
    ) -> bool {
        match self {
            Self::Leaf { .. } => false,
            Self::Internal {
                split,
                ratio_target,
                a,
                b,
                ..
            } => {
                let holds = a.find_leaf(pane).is_some() || b.find_leaf(pane).is_some();
                if holds && *split == axis {
                    // Try deeper first: the nearest boundary wins.
                    if a.with_nearest_split(pane, axis, apply)
                        || b.with_nearest_split(pane, axis, apply)
                    {
                        return true;
                    }
                    apply(ratio_target);
                    return true;
                }

                a.with_nearest_split(pane, axis, apply)
                    || b.with_nearest_split(pane, axis, apply)
            }
        }
    }

    fn is_leaf_for(&self, pane: PaneId) -> bool {
        matches!(self, Self::Leaf { pane: leaf_pane, .. } if *leaf_pane == pane)
    }

    fn leaf_rect(&self, pane: PaneId) -> Option<Rect> {
        match self.find_leaf(pane)? {
            Self::Leaf { rect_target, .. } => Some(*rect_target),
            Self::Internal { .. } => None,
        }
    }

    fn find_neighbor(
        &self,
        focused: PaneId,
        focused_rect: Rect,
        dir: Direction,
        best: &mut Option<NeighborCandidate>,
    ) {
        match self {
            Self::Leaf {
                pane, rect_target, ..
            } => {
                if *pane == focused {
                    return;
                }

                let Some(overlap) = shared_edge_overlap(focused_rect, *rect_target, dir) else {
                    return;
                };

                if best.map_or(true, |candidate| overlap > candidate.overlap) {
                    *best = Some(NeighborCandidate {
                        pane: *pane,
                        overlap,
                    });
                }
            }
            Self::Internal { a, b, .. } => {
                a.find_neighbor(focused, focused_rect, dir, best);
                b.find_neighbor(focused, focused_rect, dir, best);
            }
        }
    }
}

#[derive(Copy, Clone)]
struct NeighborCandidate {
    pane: PaneId,
    overlap: u16,
}

fn shared_edge_overlap(focused: Rect, candidate: Rect, dir: Direction) -> Option<u16> {
    // Panes that share a border overlap by one cell on that edge, so "touching"
    // means the edges meet *or* overlap by one — not that they are equal.
    let touches = match dir {
        Direction::Left => edges_meet(rect_right(candidate), focused.x),
        Direction::Right => edges_meet(rect_right(focused), candidate.x),
        Direction::Up => edges_meet(rect_bottom(candidate), focused.y),
        Direction::Down => edges_meet(rect_bottom(focused), candidate.y),
    };
    if !touches {
        return None;
    }

    // How much of the shared edge the two panes have in common, which is what
    // picks the best neighbour when several sit along it.
    let (a_start, a_end, b_start, b_end) = match dir {
        Direction::Left | Direction::Right => (
            focused.y,
            rect_bottom(focused),
            candidate.y,
            rect_bottom(candidate),
        ),
        Direction::Up | Direction::Down => (
            focused.x,
            rect_right(focused),
            candidate.x,
            rect_right(candidate),
        ),
    };
    if !ranges_overlap(a_start, a_end, b_start, b_end) {
        return None;
    }

    Some(range_overlap(a_start, a_end, b_start, b_end))
}

/// Whether one pane's far edge meets another's near edge.
///
/// Exactly equal when the panes merely abut, and one apart when they share a
/// border — [`share_divider`] pulls the second pane back onto the first's last
/// line, so its start sits one before the other's end.
fn edges_meet(end: u16, start: u16) -> bool {
    end == start || end == start.saturating_add(1)
}

/// Pull the second half of a split back onto the first half's last line, so
/// the two panes share one border rather than drawing two side by side.
///
/// Without this every pane draws its own box and adjacent panes leave a
/// two-cell gap between their contents — which reads as far worse vertically,
/// where a terminal cell is about twice as tall as it is wide. Overlapping by
/// one means both panes draw a border on the same line, over each other, with
/// the same glyph.
///
/// Only ever shrinks the gap, never a pane below the two cells it needs for a
/// border of its own.
fn share_divider(rect: Rect, split: Split) -> Rect {
    match split {
        Split::Horizontal if rect.y > 0 && rect.h >= MIN_SHARED_EXTENT => Rect {
            y: rect.y - 1,
            h: rect.h + 1,
            ..rect
        },
        Split::Vertical if rect.x > 0 && rect.w >= MIN_SHARED_EXTENT => Rect {
            x: rect.x - 1,
            w: rect.w + 1,
            ..rect
        },
        _ => rect,
    }
}

/// A pane needs two cells across to have a border at all; below that, sharing
/// one would leave nothing to share.
const MIN_SHARED_EXTENT: u16 = 2;

fn rect_right(rect: Rect) -> u16 {
    rect.x.saturating_add(rect.w)
}

fn rect_bottom(rect: Rect) -> u16 {
    rect.y.saturating_add(rect.h)
}

fn ranges_overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> bool {
    range_overlap(a_start, a_end, b_start, b_end) > 0
}

fn range_overlap(a_start: u16, a_end: u16, b_start: u16, b_end: u16) -> u16 {
    a_end.min(b_end).saturating_sub(a_start.max(b_start))
}

/// Build a tree that divides `rect` evenly between `panes` along one axis.
///
/// A left-leaning chain with `1/n` at each step gives every pane the same
/// size, which is what `select-layout even-horizontal` and friends want.
pub fn even_chain(panes: &[PaneId], split: Split, rect: Rect) -> Option<Node> {
    let (&head, tail) = panes.split_first()?;

    if tail.is_empty() {
        return Some(Node::Leaf {
            pane: head,
            rect_current: FRect::from(rect),
            rect_target: rect,
        });
    }

    // This pane takes 1/n, the remainder is divided among the others.
    let ratio = 1.0 / f32::from(u16::try_from(panes.len()).unwrap_or(u16::MAX));
    let (a_rect, b_rect) = rect.split(split, ratio);

    Some(Node::Internal {
        split,
        ratio,
        ratio_target: ratio,
        a: Box::new(Node::Leaf {
            pane: head,
            rect_current: FRect::from(a_rect),
            rect_target: a_rect,
        }),
        b: Box::new(even_chain(tail, split, b_rect)?),
        rect,
    })
}

/// Build a roughly square grid, for `select-layout tiled`.
///
/// Panes are laid out in rows: the tree splits horizontally into rows, and
/// each row splits vertically into its panes.
pub fn tiled(panes: &[PaneId], rect: Rect) -> Option<Node> {
    if panes.is_empty() {
        return None;
    }

    // A roughly square grid: ceil(sqrt(n)) rows.
    let rows = (1..=panes.len())
        .find(|rows| rows * rows >= panes.len())
        .unwrap_or(1);
    let per_row = panes.len().div_ceil(rows);
    let chunks: Vec<&[PaneId]> = panes.chunks(per_row).collect();

    build_rows(&chunks, rect)
}

fn build_rows(rows: &[&[PaneId]], rect: Rect) -> Option<Node> {
    let (&head, tail) = rows.split_first()?;

    if tail.is_empty() {
        return even_chain(head, Split::Vertical, rect);
    }

    let ratio = 1.0 / f32::from(u16::try_from(rows.len()).unwrap_or(u16::MAX));
    let (a_rect, b_rect) = rect.split(Split::Horizontal, ratio);

    Some(Node::Internal {
        split: Split::Horizontal,
        ratio,
        ratio_target: ratio,
        a: Box::new(even_chain(head, Split::Vertical, a_rect)?),
        b: Box::new(build_rows(tail, b_rect)?),
        rect,
    })
}

/// Build a layout with one large pane and the rest stacked beside or below it.
///
/// `main_ratio` is the share the first pane keeps, which is how tmux's
/// `main-vertical` and `main-horizontal` are shaped.
pub fn main_and_stack(
    panes: &[PaneId],
    main_split: Split,
    main_ratio: f32,
    rect: Rect,
) -> Option<Node> {
    let (&head, tail) = panes.split_first()?;

    if tail.is_empty() {
        return Some(Node::Leaf {
            pane: head,
            rect_current: FRect::from(rect),
            rect_target: rect,
        });
    }

    let ratio = clamp_ratio(main_ratio);
    let (a_rect, b_rect) = rect.split(main_split, ratio);
    // The stack runs across the other axis so the panes sit side by side in it.
    let stack_split = match main_split {
        Split::Vertical => Split::Horizontal,
        Split::Horizontal => Split::Vertical,
    };

    Some(Node::Internal {
        split: main_split,
        ratio,
        ratio_target: ratio,
        a: Box::new(Node::Leaf {
            pane: head,
            rect_current: FRect::from(a_rect),
            rect_target: a_rect,
        }),
        b: Box::new(even_chain(tail, stack_split, b_rect)?),
        rect,
    })
}


#[cfg(test)]
mod tests {
    use super::{rect_bottom, rect_right, Node};
    use crate::backend::PaneId;
    use crate::layout::geometry::{Direction, FRect, Rect, Split};

    const ROOT: Rect = Rect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    fn leaf(id: u64) -> Node {
        Node::Leaf {
            pane: PaneId(id),
            rect_current: FRect::from(ROOT),
            rect_target: ROOT,
        }
    }

    fn rect_for(tree: &Node, id: u64) -> Rect {
        match tree.find_leaf(PaneId(id)).expect("leaf exists") {
            Node::Leaf { rect_target, .. } => *rect_target,
            Node::Internal { .. } => unreachable!("find_leaf only returns leaves"),
        }
    }

    #[test]
    fn split_leaf_becomes_internal_with_two_leaves() {
        let mut tree = leaf(1);

        tree.split_focused(PaneId(1), Split::Vertical, PaneId(2));

        match tree {
            Node::Internal {
                split,
                ratio,
                ratio_target,
                ref a,
                ref b,
                ..
            } => {
                assert_eq!(split, Split::Vertical);
                assert!((ratio - 0.5).abs() < f32::EPSILON);
                assert!((ratio_target - 0.5).abs() < f32::EPSILON);
                assert!(matches!(
                    **a,
                    Node::Leaf {
                        pane: PaneId(1),
                        ..
                    }
                ));
                assert!(matches!(
                    **b,
                    Node::Leaf {
                        pane: PaneId(2),
                        ..
                    }
                ));
            }
            Node::Leaf { .. } => panic!("expected internal node"),
        }
    }

    #[test]
    /// Halves cover the root and overlap by exactly one cell — the column
    /// they both draw a border on, so there is one divider between them
    /// rather than two.
    fn split_then_compute_layout_covers_root_sharing_one_divider() {
        let mut tree = leaf(1);
        tree.split_focused(PaneId(1), Split::Vertical, PaneId(2));
        tree.compute_layout(ROOT);

        let left = rect_for(&tree, 1);
        let right = rect_for(&tree, 2);

        assert_eq!(left.x, ROOT.x);
        assert_eq!(left.y, ROOT.y);
        assert_eq!(left.h, ROOT.h);
        assert_eq!(right.y, ROOT.y);
        assert_eq!(right.h, ROOT.h);
        assert_eq!(rect_right(left), right.x + 1, "one shared column");
        assert_eq!(rect_right(right), rect_right(ROOT), "still covers the root");
        assert_eq!(left.w + right.w, ROOT.w + 1, "the shared column counts twice");
    }

    /// The same for a horizontal split, which is where the gap was worst.
    #[test]
    fn a_horizontal_split_shares_one_row() {
        let mut tree = leaf(1);
        tree.split_focused(PaneId(1), Split::Horizontal, PaneId(2));
        tree.compute_layout(ROOT);

        let top = rect_for(&tree, 1);
        let bottom = rect_for(&tree, 2);

        assert_eq!(rect_bottom(top), bottom.y + 1, "one shared row");
        assert_eq!(rect_bottom(bottom), rect_bottom(ROOT));
    }

    /// A pane too small to hold a border of its own keeps its rect rather than
    /// being shrunk further.
    #[test]
    fn sharing_never_shrinks_a_pane_past_its_border() {
        let tiny = Rect { x: 0, y: 0, w: 4, h: 1 };
        assert_eq!(super::share_divider(tiny, Split::Horizontal), tiny);

        // Nor does it pull a pane off the top-left edge of the screen.
        let at_origin = Rect { x: 0, y: 0, w: 4, h: 4 };
        assert_eq!(super::share_divider(at_origin, Split::Horizontal), at_origin);
        assert_eq!(super::share_divider(at_origin, Split::Vertical), at_origin);
    }

    #[test]
    fn close_removes_leaf_and_replaces_parent_with_sibling() {
        let mut tree = leaf(1);
        tree.split_focused(PaneId(1), Split::Vertical, PaneId(2));
        tree.compute_layout(ROOT);

        assert!(tree.close(PaneId(2)));

        match tree {
            Node::Leaf { pane, .. } => assert_eq!(pane, PaneId(1)),
            Node::Internal { .. } => panic!("expected sibling leaf to replace parent"),
        }
    }

    #[test]
    fn close_root_leaf_returns_false() {
        let mut tree = leaf(1);

        assert!(!tree.close(PaneId(1)));
        assert!(matches!(
            tree,
            Node::Leaf {
                pane: PaneId(1),
                ..
            }
        ));
    }

    #[test]
    fn compute_layout_does_not_touch_leaf_current_rects() {
        let mut tree = leaf(1);
        let current_before = match &tree {
            Node::Leaf { rect_current, .. } => *rect_current,
            Node::Internal { .. } => unreachable!("test starts with a leaf"),
        };

        tree.compute_layout(Rect {
            x: 10,
            y: 11,
            w: 12,
            h: 13,
        });

        match tree {
            Node::Leaf {
                rect_current,
                rect_target,
                ..
            } => {
                assert_eq!(rect_current, current_before);
                assert_eq!(
                    rect_target,
                    Rect {
                        x: 10,
                        y: 11,
                        w: 12,
                        h: 13,
                    }
                );
            }
            Node::Internal { .. } => unreachable!("test starts with a leaf"),
        }
    }

    #[test]
    fn focus_neighbor_horizontal_split_top_and_bottom() {
        let mut tree = leaf(1);
        tree.split_focused(PaneId(1), Split::Horizontal, PaneId(2));
        tree.compute_layout(ROOT);

        assert_eq!(
            tree.focus_neighbor(PaneId(1), Direction::Down),
            Some(PaneId(2))
        );
        assert_eq!(
            tree.focus_neighbor(PaneId(2), Direction::Up),
            Some(PaneId(1))
        );
        assert_eq!(tree.focus_neighbor(PaneId(1), Direction::Left), None);
        assert_eq!(tree.focus_neighbor(PaneId(1), Direction::Right), None);
        assert_eq!(tree.focus_neighbor(PaneId(2), Direction::Left), None);
        assert_eq!(tree.focus_neighbor(PaneId(2), Direction::Right), None);
    }

    #[test]
    fn focus_neighbor_vertical_split_left_and_right() {
        let mut tree = leaf(1);
        tree.split_focused(PaneId(1), Split::Vertical, PaneId(2));
        tree.compute_layout(ROOT);

        assert_eq!(
            tree.focus_neighbor(PaneId(1), Direction::Right),
            Some(PaneId(2))
        );
        assert_eq!(
            tree.focus_neighbor(PaneId(2), Direction::Left),
            Some(PaneId(1))
        );
    }

    #[test]
    fn focus_neighbor_nested_four_leaf_case() {
        let mut tree = leaf(1);
        tree.split_focused(PaneId(1), Split::Vertical, PaneId(2));
        tree.split_focused(PaneId(1), Split::Horizontal, PaneId(3));
        tree.split_focused(PaneId(2), Split::Horizontal, PaneId(4));
        tree.compute_layout(ROOT);

        assert_eq!(
            tree.focus_neighbor(PaneId(3), Direction::Right),
            Some(PaneId(4))
        );
        assert_eq!(
            tree.focus_neighbor(PaneId(4), Direction::Left),
            Some(PaneId(3))
        );
        assert_eq!(
            tree.focus_neighbor(PaneId(1), Direction::Down),
            Some(PaneId(3))
        );
    }
}
