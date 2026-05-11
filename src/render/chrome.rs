//! Borders, status bar, debug overlay.

use crossterm::style::Color;

use crate::backend::PaneId;
use crate::layout::geometry::Rect;
use crate::layout::tree::Node;
use crate::term::cell::{Cell, CellAttrs};
use crate::term::surface::Surface;

const FOCUSED_BORDER: Color = Color::Cyan;
const UNFOCUSED_BORDER: Color = Color::DarkGrey;

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

#[cfg(test)]
mod tests {
    use crossterm::style::Color;

    use super::draw_borders;
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
}
