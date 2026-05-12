use criterion::{black_box, criterion_group, criterion_main, Criterion};
use weave::backend::tmux::layout::LayoutAst;
use weave::backend::tmux::reconcile::{diff, normalize};
use weave::backend::PaneId;
use weave::layout::geometry::{FRect, Rect, Split};
use weave::layout::tree::Node;

const WIDTH: u16 = 160;
const HEIGHT: u16 = 48;
const CHANGES_PER_ITER: usize = 50;

fn bench_script_storm(c: &mut Criterion) {
    let layouts = script_storm_layouts();

    c.bench_function("script_storm_50_layout_changes", |b| {
        b.iter(|| {
            let mut old = node_from_layout_ast(&normalize(layouts[0].clone()));

            for layout in layouts.iter().skip(1).take(CHANGES_PER_ITER) {
                let normalized = normalize(black_box(layout.clone()));
                let deltas = diff(black_box(&old), black_box(&normalized));
                black_box(&deltas);
                old = node_from_layout_ast(&normalized);
            }

            black_box(old);
        });
    });
}

fn script_storm_layouts() -> Vec<LayoutAst> {
    (0..=CHANGES_PER_ITER)
        .map(|step| layout_for_step(step, root_rect()))
        .collect()
}

fn layout_for_step(step: usize, rect: Rect) -> LayoutAst {
    let count = [1, 2, 4, 6, 8, 5, 3, 7, 8, 4, 2, 1][step % 12];
    let mut pane_ids = (1..=count)
        .map(u64::try_from)
        .collect::<Result<Vec<_>, _>>()
        .expect("benchmark pane count fits u64");
    let rotate_by = (step * 3) % pane_ids.len();
    pane_ids.rotate_left(rotate_by);
    if step % 5 == 0 {
        pane_ids.reverse();
    }

    match step % 4 {
        0 => horizontal_layout(&pane_ids, rect, step),
        1 => vertical_layout(&pane_ids, rect, step),
        2 => grid_layout(&pane_ids, rect, step),
        _ => mixed_layout(&pane_ids, rect, step),
    }
}

fn horizontal_layout(pane_ids: &[u64], rect: Rect, seed: usize) -> LayoutAst {
    grouped_layout(pane_ids, rect, seed, SplitAxis::Horizontal)
}

fn vertical_layout(pane_ids: &[u64], rect: Rect, seed: usize) -> LayoutAst {
    grouped_layout(pane_ids, rect, seed, SplitAxis::Vertical)
}

fn grouped_layout(pane_ids: &[u64], rect: Rect, seed: usize, axis: SplitAxis) -> LayoutAst {
    if let [pane_id] = pane_ids {
        return leaf(*pane_id, rect);
    }

    let rects = weighted_rects(rect, pane_ids.len(), seed, axis);
    let children = pane_ids
        .iter()
        .copied()
        .zip(rects)
        .map(|(pane_id, rect)| leaf(pane_id, rect))
        .collect();

    match axis {
        SplitAxis::Horizontal => LayoutAst::Horizontal { rect, children },
        SplitAxis::Vertical => LayoutAst::Vertical { rect, children },
    }
}

fn grid_layout(pane_ids: &[u64], rect: Rect, seed: usize) -> LayoutAst {
    if pane_ids.len() <= 2 {
        return horizontal_layout(pane_ids, rect, seed);
    }

    let split_at = pane_ids.len().div_ceil(2);
    let column_rects = weighted_rects(rect, 2, seed, SplitAxis::Horizontal);
    LayoutAst::Horizontal {
        rect,
        children: vec![
            vertical_layout(&pane_ids[..split_at], column_rects[0], seed + 1),
            vertical_layout(&pane_ids[split_at..], column_rects[1], seed + 2),
        ],
    }
}

fn mixed_layout(pane_ids: &[u64], rect: Rect, seed: usize) -> LayoutAst {
    if pane_ids.len() <= 2 {
        return vertical_layout(pane_ids, rect, seed);
    }

    let split_at = pane_ids.len() / 2;
    let row_rects = weighted_rects(rect, 2, seed, SplitAxis::Vertical);
    LayoutAst::Vertical {
        rect,
        children: vec![
            horizontal_layout(&pane_ids[..split_at], row_rects[0], seed + 1),
            horizontal_layout(&pane_ids[split_at..], row_rects[1], seed + 2),
        ],
    }
}

fn weighted_rects(rect: Rect, count: usize, seed: usize, axis: SplitAxis) -> Vec<Rect> {
    let weights = (0..count)
        .map(|index| 1 + u16::try_from((seed + index * 3) % 5).expect("weight fits u16"))
        .collect::<Vec<_>>();
    let mut remaining_extent = match axis {
        SplitAxis::Horizontal => rect.w,
        SplitAxis::Vertical => rect.h,
    };
    let mut remaining_weight = weights.iter().copied().sum::<u16>();
    let mut cursor = match axis {
        SplitAxis::Horizontal => rect.x,
        SplitAxis::Vertical => rect.y,
    };

    weights
        .iter()
        .enumerate()
        .map(|(index, weight)| {
            let extent = if index + 1 == count {
                remaining_extent
            } else {
                let weighted =
                    u32::from(remaining_extent) * u32::from(*weight) / u32::from(remaining_weight);
                u16::try_from(weighted.max(1)).expect("weighted extent fits u16")
            };
            let child = match axis {
                SplitAxis::Horizontal => Rect {
                    x: cursor,
                    y: rect.y,
                    w: extent,
                    h: rect.h,
                },
                SplitAxis::Vertical => Rect {
                    x: rect.x,
                    y: cursor,
                    w: rect.w,
                    h: extent,
                },
            };
            cursor = cursor.saturating_add(extent);
            remaining_extent = remaining_extent.saturating_sub(extent);
            remaining_weight = remaining_weight.saturating_sub(*weight);
            child
        })
        .collect()
}

fn node_from_layout_ast(ast: &LayoutAst) -> Node {
    match ast {
        LayoutAst::Leaf { pane_id, rect } => Node::Leaf {
            pane: PaneId(*pane_id),
            rect_current: FRect::from(*rect),
            rect_target: *rect,
        },
        LayoutAst::Horizontal { rect, children } => {
            let [first, second] = children.as_slice() else {
                panic!("normalized horizontal layout should be binary");
            };
            Node::Internal {
                split: Split::Vertical,
                ratio: ratio(first.rect().w, rect.w),
                ratio_target: ratio(first.rect().w, rect.w),
                a: Box::new(node_from_layout_ast(first)),
                b: Box::new(node_from_layout_ast(second)),
                rect: *rect,
            }
        }
        LayoutAst::Vertical { rect, children } => {
            let [first, second] = children.as_slice() else {
                panic!("normalized vertical layout should be binary");
            };
            Node::Internal {
                split: Split::Horizontal,
                ratio: ratio(first.rect().h, rect.h),
                ratio_target: ratio(first.rect().h, rect.h),
                a: Box::new(node_from_layout_ast(first)),
                b: Box::new(node_from_layout_ast(second)),
                rect: *rect,
            }
        }
    }
}

fn ratio(first: u16, total: u16) -> f32 {
    if total == 0 {
        return 0.5;
    }

    f32::from(first) / f32::from(total)
}

fn leaf(pane_id: u64, rect: Rect) -> LayoutAst {
    LayoutAst::Leaf { pane_id, rect }
}

fn root_rect() -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: WIDTH,
        h: HEIGHT,
    }
}

trait LayoutAstRect {
    fn rect(&self) -> Rect;
}

impl LayoutAstRect for LayoutAst {
    fn rect(&self) -> Rect {
        match self {
            Self::Leaf { rect, .. }
            | Self::Horizontal { rect, .. }
            | Self::Vertical { rect, .. } => *rect,
        }
    }
}

#[derive(Copy, Clone)]
enum SplitAxis {
    Horizontal,
    Vertical,
}

criterion_group!(benches, bench_script_storm);
criterion_main!(benches);
