use criterion::{black_box, criterion_group, criterion_main, Criterion};
use crossterm::style::Color;
use weave::render::diff::DiffRenderer;
use weave::term::cell::{Cell, CellAttrs};
use weave::term::surface::Surface;

const WIDTH: u16 = 200;
const HEIGHT: u16 = 60;

fn changed_surfaces(percent: usize) -> (Surface, Surface) {
    let front = Surface::new(WIDTH, HEIGHT);
    let mut back = Surface::new(WIDTH, HEIGHT);
    let changed = back.cells.len() * percent / 100;

    for index in 0..changed {
        back.cells[index] = marker_cell(index);
    }

    (front, back)
}

fn marker_cell(index: usize) -> Cell {
    let ch = if index & 1 == 0 { 'x' } else { 'y' };
    Cell::new(
        ch,
        Color::Rgb {
            r: 180,
            g: 220,
            b: 255,
        },
        Color::Reset,
        CellAttrs::empty(),
    )
}

fn bench_diff(c: &mut Criterion) {
    let mut group = c.benchmark_group("diff_flush");

    for percent in [0, 1, 50, 100] {
        let (front, back) = changed_surfaces(percent);
        let mut renderer = DiffRenderer::new();
        let mut out = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 8);

        group.bench_function(format!("{percent}_percent_changed"), |b| {
            b.iter(|| {
                out.clear();
                renderer
                    .flush(black_box(&front), black_box(&back), &mut out)
                    .expect("diff flush should succeed");
                black_box(out.len());
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_diff);
criterion_main!(benches);
