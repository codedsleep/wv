use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossterm::style::Color;
use weave::anim::timeline::Timeline;
use weave::backend::PaneId;
use weave::layout::geometry::{FRect, Rect, Split};
use weave::layout::tree::Node;
use weave::render::compositor::compose;
use weave::term::pane::Pane;
use weave::term::surface::Surface;

const WIDTH: u16 = 200;
const HEIGHT: u16 = 60;

fn make_case(pane_count: u64) -> (Node, Vec<Pane>) {
    let mut next_id = 1;
    let mut leaves = Vec::new();
    let root = build_tree(
        pane_count,
        Rect {
            x: 0,
            y: 0,
            w: WIDTH,
            h: HEIGHT,
        },
        &mut next_id,
        &mut leaves,
    );
    let panes = leaves
        .into_iter()
        .map(|(id, rect)| {
            let mut pane = Pane::new(
                id,
                rect.w.saturating_sub(2).max(1),
                rect.h.saturating_sub(2).max(1),
            );
            pane.process(format!("pane {}", id.0).as_bytes());
            pane
        })
        .collect();

    (root, panes)
}

fn build_tree(
    pane_count: u64,
    rect: Rect,
    next_id: &mut u64,
    leaves: &mut Vec<(PaneId, Rect)>,
) -> Node {
    if pane_count == 1 {
        let id = PaneId(*next_id);
        *next_id += 1;
        leaves.push((id, rect));
        return Node::Leaf {
            pane: id,
            rect_current: FRect::from(rect),
            rect_target: rect,
        };
    }

    let split = if rect.w >= rect.h {
        Split::Vertical
    } else {
        Split::Horizontal
    };
    let first_count = pane_count / 2;
    let second_count = pane_count - first_count;
    let (first_rect, second_rect) = rect.split(split, 0.5);

    Node::Internal {
        split,
        ratio: 0.5,
        ratio_target: 0.5,
        a: Box::new(build_tree(first_count, first_rect, next_id, leaves)),
        b: Box::new(build_tree(second_count, second_rect, next_id, leaves)),
        rect,
    }
}

fn bench_compose(c: &mut Criterion) {
    let mut group = c.benchmark_group("compose");

    for pane_count in [1, 4, 16] {
        let (root, panes) = make_case(pane_count);
        let timeline = Timeline::new();
        let mut surface = Surface::new(WIDTH, HEIGHT);

        group.bench_function(format!("{pane_count}_panes"), |b| {
            b.iter(|| {
                surface.clear();
                compose(
                    Some(black_box(&root)),
                    black_box(&panes),
                    Some(PaneId(1)),
                    Color::Cyan,
                    black_box(&timeline),
                    black_box(&mut surface),
                );
                black_box(&surface);
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_compose);
criterion_main!(benches);
