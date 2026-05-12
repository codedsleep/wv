//! Pure tmux layout-to-BSP reconciliation diff.

use super::layout::LayoutAst;
use crate::backend::PaneId;
use crate::layout;
use crate::layout::geometry::{Rect, Split};

const RATIO_EPSILON: f32 = 0.001;

pub type StructuralPath = Vec<usize>;

#[derive(Clone, Debug, PartialEq)]
pub enum LayoutDelta {
    AddPane {
        path: StructuralPath,
        pane: PaneId,
        rect: Rect,
    },
    RemovePane {
        path: StructuralPath,
        pane: PaneId,
    },
    SplitInternal {
        path: StructuralPath,
        split: Split,
        ratio: f32,
    },
    MergeInternal {
        path: StructuralPath,
    },
    ResizeRatio {
        path: StructuralPath,
        from: f32,
        to: f32,
    },
    SwapLeaves {
        a_path: StructuralPath,
        b_path: StructuralPath,
        a: PaneId,
        b: PaneId,
    },
}

pub fn diff(old: &layout::tree::Node, new: &LayoutAst) -> Vec<LayoutDelta> {
    let new = normalize(new.clone());
    let mut deltas = Vec::new();
    let old_leaves = old_leaf_infos(old);
    let new_leaves = new_leaf_infos(&new);

    diff_shape(old, &new, &mut Vec::new(), &mut deltas);
    diff_swaps(&old_leaves, &new_leaves, &mut deltas);

    for old_leaf in &old_leaves {
        if !new_leaves
            .iter()
            .any(|new_leaf| new_leaf.pane == old_leaf.pane)
        {
            deltas.push(LayoutDelta::RemovePane {
                path: old_leaf.path.clone(),
                pane: old_leaf.pane,
            });
        }
    }

    for new_leaf in &new_leaves {
        if !old_leaves
            .iter()
            .any(|old_leaf| old_leaf.pane == new_leaf.pane)
        {
            deltas.push(LayoutDelta::AddPane {
                path: new_leaf.path.clone(),
                pane: new_leaf.pane,
                rect: new_leaf.rect,
            });
        }
    }

    deltas
}

pub fn normalize(ast: LayoutAst) -> LayoutAst {
    match ast {
        LayoutAst::Leaf { .. } => ast,
        LayoutAst::Horizontal { rect, children } => {
            normalize_group(rect, children, LayoutAstKind::Horizontal)
        }
        LayoutAst::Vertical { rect, children } => {
            normalize_group(rect, children, LayoutAstKind::Vertical)
        }
    }
}

#[derive(Copy, Clone)]
enum LayoutAstKind {
    Horizontal,
    Vertical,
}

fn normalize_group(rect: Rect, children: Vec<LayoutAst>, kind: LayoutAstKind) -> LayoutAst {
    let children: Vec<_> = children.into_iter().map(normalize).collect();
    if children.len() <= 2 {
        return group(kind, rect, children);
    }

    let mut iter = children.into_iter();
    let first = iter
        .next()
        .expect("len checked above; N-ary group has a first child");
    let remaining_children: Vec<_> = iter.collect();
    let rest_rect = cumulative_rect(kind, &remaining_children);
    group(
        kind,
        rect,
        vec![first, normalize_group(rest_rect, remaining_children, kind)],
    )
}

fn group(kind: LayoutAstKind, rect: Rect, children: Vec<LayoutAst>) -> LayoutAst {
    match kind {
        LayoutAstKind::Horizontal => LayoutAst::Horizontal { rect, children },
        LayoutAstKind::Vertical => LayoutAst::Vertical { rect, children },
    }
}

fn cumulative_rect(kind: LayoutAstKind, children: &[LayoutAst]) -> Rect {
    let mut rect = children
        .first()
        .expect("right-leaning normalization only builds non-empty rest groups")
        .rect();

    for child in &children[1..] {
        let child_rect = child.rect();
        match kind {
            LayoutAstKind::Horizontal => {
                let right = rect_right(rect).max(rect_right(child_rect));
                rect.x = rect.x.min(child_rect.x);
                rect.w = right.saturating_sub(rect.x);
                rect.y = rect.y.min(child_rect.y);
                rect.h = rect.h.max(child_rect.h);
            }
            LayoutAstKind::Vertical => {
                let bottom = rect_bottom(rect).max(rect_bottom(child_rect));
                rect.y = rect.y.min(child_rect.y);
                rect.h = bottom.saturating_sub(rect.y);
                rect.x = rect.x.min(child_rect.x);
                rect.w = rect.w.max(child_rect.w);
            }
        }
    }

    rect
}

fn rect_right(rect: Rect) -> u16 {
    rect.x.saturating_add(rect.w)
}

fn rect_bottom(rect: Rect) -> u16 {
    rect.y.saturating_add(rect.h)
}

fn diff_shape(
    old: &layout::tree::Node,
    new: &LayoutAst,
    path: &mut StructuralPath,
    deltas: &mut Vec<LayoutDelta>,
) {
    match (old, new) {
        (layout::tree::Node::Leaf { .. }, LayoutAst::Leaf { .. }) => {}
        (
            layout::tree::Node::Leaf { .. },
            LayoutAst::Horizontal { .. } | LayoutAst::Vertical { .. },
        ) => {
            if let Some((split, ratio)) = split_ratio(new) {
                deltas.push(LayoutDelta::SplitInternal {
                    path: path.clone(),
                    split,
                    ratio,
                });
            }
        }
        (layout::tree::Node::Internal { .. }, LayoutAst::Leaf { .. }) => {
            deltas.push(LayoutDelta::MergeInternal { path: path.clone() });
        }
        (
            layout::tree::Node::Internal {
                split: old_split,
                ratio_target,
                a,
                b,
                ..
            },
            LayoutAst::Horizontal { children, .. } | LayoutAst::Vertical { children, .. },
        ) => {
            if let Some((new_split, new_ratio)) = split_ratio(new) {
                if *old_split != new_split {
                    deltas.push(LayoutDelta::SplitInternal {
                        path: path.clone(),
                        split: new_split,
                        ratio: new_ratio,
                    });
                } else if (ratio_target - new_ratio).abs() > RATIO_EPSILON {
                    deltas.push(LayoutDelta::ResizeRatio {
                        path: path.clone(),
                        from: *ratio_target,
                        to: new_ratio,
                    });
                }
            }

            if let [new_a, new_b] = children.as_slice() {
                path.push(0);
                diff_shape(a, new_a, path, deltas);
                path.pop();

                path.push(1);
                diff_shape(b, new_b, path, deltas);
                path.pop();
            }
        }
    }
}

fn diff_swaps(old_leaves: &[LeafInfo], new_leaves: &[LeafInfo], deltas: &mut Vec<LayoutDelta>) {
    let mut emitted = Vec::new();

    for old_leaf in old_leaves {
        if emitted.contains(&old_leaf.pane) {
            continue;
        }

        let Some(new_leaf) = leaf_by_pane(new_leaves, old_leaf.pane) else {
            continue;
        };
        if old_leaf.path == new_leaf.path {
            continue;
        }

        let Some(displaced_old_leaf) = leaf_by_path(old_leaves, &new_leaf.path) else {
            continue;
        };
        let Some(displaced_new_leaf) = leaf_by_pane(new_leaves, displaced_old_leaf.pane) else {
            continue;
        };
        if displaced_new_leaf.path != old_leaf.path {
            continue;
        }

        deltas.push(LayoutDelta::SwapLeaves {
            a_path: old_leaf.path.clone(),
            b_path: displaced_old_leaf.path.clone(),
            a: old_leaf.pane,
            b: displaced_old_leaf.pane,
        });
        emitted.push(old_leaf.pane);
        emitted.push(displaced_old_leaf.pane);
    }
}

fn split_ratio(ast: &LayoutAst) -> Option<(Split, f32)> {
    match ast {
        LayoutAst::Leaf { .. } => None,
        LayoutAst::Horizontal { rect, children } => {
            let [first, _second] = children.as_slice() else {
                return None;
            };
            Some((Split::Vertical, ratio(first.rect().w, rect.w)))
        }
        LayoutAst::Vertical { rect, children } => {
            let [first, _second] = children.as_slice() else {
                return None;
            };
            Some((Split::Horizontal, ratio(first.rect().h, rect.h)))
        }
    }
}

fn ratio(first: u16, total: u16) -> f32 {
    if total == 0 {
        return 0.5;
    }

    f32::from(first) / f32::from(total)
}

#[derive(Clone, Debug)]
struct LeafInfo {
    path: StructuralPath,
    pane: PaneId,
    rect: Rect,
}

fn old_leaf_infos(root: &layout::tree::Node) -> Vec<LeafInfo> {
    let mut leaves = Vec::new();
    collect_old_leaves(root, &mut Vec::new(), &mut leaves);
    leaves
}

fn collect_old_leaves(
    node: &layout::tree::Node,
    path: &mut StructuralPath,
    leaves: &mut Vec<LeafInfo>,
) {
    match node {
        layout::tree::Node::Leaf {
            pane, rect_target, ..
        } => leaves.push(LeafInfo {
            path: path.clone(),
            pane: *pane,
            rect: *rect_target,
        }),
        layout::tree::Node::Internal { a, b, .. } => {
            path.push(0);
            collect_old_leaves(a, path, leaves);
            path.pop();

            path.push(1);
            collect_old_leaves(b, path, leaves);
            path.pop();
        }
    }
}

fn new_leaf_infos(root: &LayoutAst) -> Vec<LeafInfo> {
    let mut leaves = Vec::new();
    collect_new_leaves(root, &mut Vec::new(), &mut leaves);
    leaves
}

fn collect_new_leaves(ast: &LayoutAst, path: &mut StructuralPath, leaves: &mut Vec<LeafInfo>) {
    match ast {
        LayoutAst::Leaf { pane_id, rect } => leaves.push(LeafInfo {
            path: path.clone(),
            pane: PaneId(*pane_id),
            rect: *rect,
        }),
        LayoutAst::Horizontal { children, .. } | LayoutAst::Vertical { children, .. } => {
            for (index, child) in children.iter().enumerate() {
                path.push(index);
                collect_new_leaves(child, path, leaves);
                path.pop();
            }
        }
    }
}

fn leaf_by_pane(leaves: &[LeafInfo], pane: PaneId) -> Option<&LeafInfo> {
    leaves.iter().find(|leaf| leaf.pane == pane)
}

fn leaf_by_path<'a>(leaves: &'a [LeafInfo], path: &[usize]) -> Option<&'a LeafInfo> {
    leaves.iter().find(|leaf| leaf.path == path)
}

trait LayoutAstExt {
    fn rect(&self) -> Rect;
}

impl LayoutAstExt for LayoutAst {
    fn rect(&self) -> Rect {
        match self {
            Self::Leaf { rect, .. }
            | Self::Horizontal { rect, .. }
            | Self::Vertical { rect, .. } => *rect,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{diff, normalize, split_ratio, LayoutAstExt, LayoutDelta};
    use crate::backend::tmux::layout::LayoutAst;
    use crate::backend::PaneId;
    use crate::layout::geometry::{FRect, Rect, Split};
    use crate::layout::tree::Node;

    const ROOT: Rect = Rect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    fn old_leaf(id: u64, rect: Rect) -> Node {
        Node::Leaf {
            pane: PaneId(id),
            rect_current: FRect::from(rect),
            rect_target: rect,
        }
    }

    fn old_split(split: Split, ratio: f32, a: Node, b: Node, rect: Rect) -> Node {
        Node::Internal {
            split,
            ratio,
            ratio_target: ratio,
            a: Box::new(a),
            b: Box::new(b),
            rect,
        }
    }

    fn new_leaf(id: u64, rect: Rect) -> LayoutAst {
        LayoutAst::Leaf { pane_id: id, rect }
    }

    fn pane_ids(ast: &LayoutAst, out: &mut Vec<u64>) {
        match ast {
            LayoutAst::Leaf { pane_id, .. } => out.push(*pane_id),
            LayoutAst::Horizontal { children, .. } | LayoutAst::Vertical { children, .. } => {
                for child in children {
                    pane_ids(child, out);
                }
            }
        }
    }

    fn assert_rect_within_one_cell(left: Rect, right: Rect) {
        assert!(left.x.abs_diff(right.x) <= 1, "{left:?} != {right:?}");
        assert!(left.y.abs_diff(right.y) <= 1, "{left:?} != {right:?}");
        assert!(left.w.abs_diff(right.w) <= 1, "{left:?} != {right:?}");
        assert!(left.h.abs_diff(right.h) <= 1, "{left:?} != {right:?}");
    }

    fn assert_ratio_rects_match_children(ast: &LayoutAst) {
        match ast {
            LayoutAst::Leaf { .. } => {}
            LayoutAst::Horizontal { children, .. } | LayoutAst::Vertical { children, .. } => {
                let [first, second] = children.as_slice() else {
                    panic!("normalized groups should be binary");
                };
                let (split, ratio) = split_ratio(ast).expect("group has a ratio");
                let (first_rect, second_rect) = ast.rect().split(split, ratio);

                assert_rect_within_one_cell(first_rect, first.rect());
                assert_rect_within_one_cell(second_rect, second.rect());
                assert_ratio_rects_match_children(first);
                assert_ratio_rects_match_children(second);
            }
        }
    }

    #[test]
    fn pure_split_from_one_leaf_to_two() {
        let old = old_leaf(1, ROOT);
        let new = LayoutAst::Horizontal {
            rect: ROOT,
            children: vec![
                new_leaf(1, rect(0, 0, 40, 24)),
                new_leaf(2, rect(40, 0, 40, 24)),
            ],
        };

        assert_eq!(
            diff(&old, &new),
            vec![
                LayoutDelta::SplitInternal {
                    path: vec![],
                    split: Split::Vertical,
                    ratio: 0.5,
                },
                LayoutDelta::AddPane {
                    path: vec![1],
                    pane: PaneId(2),
                    rect: rect(40, 0, 40, 24),
                },
            ]
        );
    }

    #[test]
    fn four_pane_horizontal_layout_normalizes_to_right_leaning_bsp() {
        let raw = LayoutAst::Horizontal {
            rect: ROOT,
            children: vec![
                new_leaf(1, rect(0, 0, 20, 24)),
                new_leaf(2, rect(20, 0, 20, 24)),
                new_leaf(3, rect(40, 0, 20, 24)),
                new_leaf(4, rect(60, 0, 20, 24)),
            ],
        };
        let expected = LayoutAst::Horizontal {
            rect: ROOT,
            children: vec![
                new_leaf(1, rect(0, 0, 20, 24)),
                LayoutAst::Horizontal {
                    rect: rect(20, 0, 60, 24),
                    children: vec![
                        new_leaf(2, rect(20, 0, 20, 24)),
                        LayoutAst::Horizontal {
                            rect: rect(40, 0, 40, 24),
                            children: vec![
                                new_leaf(3, rect(40, 0, 20, 24)),
                                new_leaf(4, rect(60, 0, 20, 24)),
                            ],
                        },
                    ],
                },
            ],
        };

        let normalized = normalize(raw);
        let mut ids = Vec::new();
        pane_ids(&normalized, &mut ids);

        assert_eq!(normalized, expected);
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_ratio_rects_match_children(&normalized);
    }

    #[test]
    fn pure_resize_changes_ratio_only() {
        let old = old_split(
            Split::Vertical,
            0.5,
            old_leaf(1, rect(0, 0, 40, 24)),
            old_leaf(2, rect(40, 0, 40, 24)),
            ROOT,
        );
        let new = LayoutAst::Horizontal {
            rect: ROOT,
            children: vec![
                new_leaf(1, rect(0, 0, 30, 24)),
                new_leaf(2, rect(30, 0, 50, 24)),
            ],
        };

        assert_eq!(
            diff(&old, &new),
            vec![LayoutDelta::ResizeRatio {
                path: vec![],
                from: 0.5,
                to: 0.375,
            }]
        );
    }

    #[test]
    fn pane_death_merges_parent_and_removes_leaf() {
        let old = old_split(
            Split::Vertical,
            0.5,
            old_leaf(1, rect(0, 0, 40, 24)),
            old_leaf(2, rect(40, 0, 40, 24)),
            ROOT,
        );
        let new = new_leaf(1, ROOT);

        assert_eq!(
            diff(&old, &new),
            vec![
                LayoutDelta::MergeInternal { path: vec![] },
                LayoutDelta::RemovePane {
                    path: vec![1],
                    pane: PaneId(2),
                },
            ]
        );
    }

    #[test]
    fn swap_detects_two_leaves_trading_positions() {
        let old = old_split(
            Split::Vertical,
            0.5,
            old_leaf(1, rect(0, 0, 40, 24)),
            old_leaf(2, rect(40, 0, 40, 24)),
            ROOT,
        );
        let new = LayoutAst::Horizontal {
            rect: ROOT,
            children: vec![
                new_leaf(2, rect(0, 0, 40, 24)),
                new_leaf(1, rect(40, 0, 40, 24)),
            ],
        };

        assert_eq!(
            diff(&old, &new),
            vec![LayoutDelta::SwapLeaves {
                a_path: vec![0],
                b_path: vec![1],
                a: PaneId(1),
                b: PaneId(2),
            }]
        );
    }

    #[test]
    fn full_rebuild_with_no_leaf_matches_removes_and_adds_by_position() {
        let old = old_split(
            Split::Vertical,
            0.5,
            old_leaf(1, rect(0, 0, 40, 24)),
            old_leaf(2, rect(40, 0, 40, 24)),
            ROOT,
        );
        let new = LayoutAst::Horizontal {
            rect: ROOT,
            children: vec![
                new_leaf(3, rect(0, 0, 40, 24)),
                new_leaf(4, rect(40, 0, 40, 24)),
            ],
        };

        assert_eq!(
            diff(&old, &new),
            vec![
                LayoutDelta::RemovePane {
                    path: vec![0],
                    pane: PaneId(1),
                },
                LayoutDelta::RemovePane {
                    path: vec![1],
                    pane: PaneId(2),
                },
                LayoutDelta::AddPane {
                    path: vec![0],
                    pane: PaneId(3),
                    rect: rect(0, 0, 40, 24),
                },
                LayoutDelta::AddPane {
                    path: vec![1],
                    pane: PaneId(4),
                    rect: rect(40, 0, 40, 24),
                },
            ]
        );
    }
}
