//! BSP `Node` tree.

use crate::backend::PaneId;
use crate::layout::geometry::{Direction, FRect, Rect, Split};

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
                b.compute_layout(b_rect);
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
    let overlap = match dir {
        Direction::Left
            if rect_right(candidate) == focused.x
                && ranges_overlap(
                    focused.y,
                    rect_bottom(focused),
                    candidate.y,
                    rect_bottom(candidate),
                ) =>
        {
            range_overlap(
                focused.y,
                rect_bottom(focused),
                candidate.y,
                rect_bottom(candidate),
            )
        }
        Direction::Right
            if candidate.x == rect_right(focused)
                && ranges_overlap(
                    focused.y,
                    rect_bottom(focused),
                    candidate.y,
                    rect_bottom(candidate),
                ) =>
        {
            range_overlap(
                focused.y,
                rect_bottom(focused),
                candidate.y,
                rect_bottom(candidate),
            )
        }
        Direction::Up
            if rect_bottom(candidate) == focused.y
                && ranges_overlap(
                    focused.x,
                    rect_right(focused),
                    candidate.x,
                    rect_right(candidate),
                ) =>
        {
            range_overlap(
                focused.x,
                rect_right(focused),
                candidate.x,
                rect_right(candidate),
            )
        }
        Direction::Down
            if candidate.y == rect_bottom(focused)
                && ranges_overlap(
                    focused.x,
                    rect_right(focused),
                    candidate.x,
                    rect_right(candidate),
                ) =>
        {
            range_overlap(
                focused.x,
                rect_right(focused),
                candidate.x,
                rect_right(candidate),
            )
        }
        _ => 0,
    };

    (overlap > 0).then_some(overlap)
}

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

#[cfg(test)]
mod tests {
    use super::{rect_right, Node};
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
    fn split_then_compute_layout_covers_root_without_overlap() {
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
        assert_eq!(rect_right(left), right.x);
        assert_eq!(left.w + right.w, ROOT.w);
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
