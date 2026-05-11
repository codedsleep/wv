//! Borders, status bar, debug overlay.

use crossterm::style::Color;

use crate::backend::PaneId;
use crate::layout::geometry::Rect;
use crate::layout::tree::Node;
use crate::term::cell::{Cell, CellAttrs};
use crate::term::surface::Surface;

const FOCUSED_BORDER: Color = Color::Cyan;
const UNFOCUSED_BORDER: Color = Color::DarkGrey;
const STATUS_FG: Color = Color::White;
const STATUS_BG: Color = Color::DarkBlue;

pub fn draw_borders(surface: &mut Surface, tree: &Node, focused: Option<PaneId>) {
    match tree {
        Node::Leaf { pane, rect } => {
            let color = if Some(*pane) == focused {
                FOCUSED_BORDER
            } else {
                UNFOCUSED_BORDER
            };
            draw_rect_border(surface, *rect, color);
        }
        Node::Internal { a, b, .. } => {
            draw_borders(surface, a, focused);
            draw_borders(surface, b, focused);
        }
    }
}

pub fn draw_status_bar(
    surface: &mut Surface,
    mode_label: &str,
    pane_count: usize,
    now: chrono::DateTime<chrono::Local>,
) {
    if surface.width == 0 || surface.height == 0 {
        return;
    }

    let y = surface.height - 1;
    for x in 0..surface.width {
        surface.set(x, y, status_cell(' '));
    }

    let text = format!(
        "[{mode_label}] panes:{pane_count} {}",
        now.format("%H:%M:%S")
    );

    for (x, ch) in (0..surface.width).zip(text.chars()) {
        surface.set(x, y, status_cell(ch));
    }
}

pub fn leaf_count(tree: Option<&Node>) -> usize {
    tree.map_or(0, count_leaves)
}

fn draw_rect_border(surface: &mut Surface, rect: Rect, color: Color) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }

    let right = rect.x.saturating_add(rect.w.saturating_sub(1));
    let bottom = rect.y.saturating_add(rect.h.saturating_sub(1));

    // Shared edges between adjacent panes overdraw, T-junctions deferred.
    for x in rect.x..=right {
        surface.set(x, rect.y, border_cell('─', color));
        surface.set(x, bottom, border_cell('─', color));
    }

    for y in rect.y..=bottom {
        surface.set(rect.x, y, border_cell('│', color));
        surface.set(right, y, border_cell('│', color));
    }

    surface.set(rect.x, rect.y, border_cell('┌', color));
    surface.set(right, rect.y, border_cell('┐', color));
    surface.set(rect.x, bottom, border_cell('└', color));
    surface.set(right, bottom, border_cell('┘', color));
}

fn border_cell(ch: char, color: Color) -> Cell {
    Cell::new(ch, color, Color::Reset, CellAttrs::empty())
}

fn status_cell(ch: char) -> Cell {
    Cell::new(ch, STATUS_FG, STATUS_BG, CellAttrs::empty())
}

fn count_leaves(node: &Node) -> usize {
    match node {
        Node::Leaf { .. } => 1,
        Node::Internal { a, b, .. } => count_leaves(a) + count_leaves(b),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use crossterm::style::Color;

    use super::{draw_borders, draw_status_bar};
    use crate::backend::PaneId;
    use crate::layout::geometry::{Rect, Split};
    use crate::layout::tree::Node;
    use crate::term::surface::Surface;

    #[test]
    fn draw_borders_marks_focused_and_unfocused_panes() {
        let mut surface = Surface::new(4, 4);
        let tree = Node::Internal {
            split: Split::Horizontal,
            ratio: 0.5,
            a: Box::new(Node::Leaf {
                pane: PaneId(1),
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 2,
                },
            }),
            b: Box::new(Node::Leaf {
                pane: PaneId(2),
                rect: Rect {
                    x: 0,
                    y: 2,
                    w: 4,
                    h: 2,
                },
            }),
            rect: Rect {
                x: 0,
                y: 0,
                w: 4,
                h: 4,
            },
        };

        draw_borders(&mut surface, &tree, Some(PaneId(1)));

        let focused_corner = surface.get(0, 0).expect("cell exists");
        let unfocused_corner = surface.get(0, 2).expect("cell exists");
        assert_eq!(focused_corner.ch, '┌');
        assert_eq!(focused_corner.fg, Color::Cyan);
        assert_eq!(unfocused_corner.ch, '┌');
        assert_eq!(unfocused_corner.fg, Color::DarkGrey);
    }

    #[test]
    fn draw_status_bar_writes_pane_count_on_bottom_row() {
        let mut surface = Surface::new(32, 4);
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 11, 14, 23, 11)
            .single()
            .expect("test time exists");

        draw_status_bar(&mut surface, "NORMAL", 2, now);

        let bottom: String = (0..surface.width)
            .map(|x| surface.get(x, surface.height - 1).expect("cell exists").ch)
            .collect();
        assert!(bottom.contains("panes:2"));
    }
}
