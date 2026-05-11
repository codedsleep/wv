//! Compose panes into back surface.

use crate::layout::tree::Node;
use crate::render::chrome;
use crate::term::pane::Pane;
use crate::term::surface::Surface;
use crate::{backend::PaneId, layout::geometry::Rect};

pub fn compose(
    root: Option<&Node>,
    panes: &[Pane],
    focused: Option<PaneId>,
    focused_border_color: crossterm::style::Color,
    back: &mut Surface,
) {
    let Some(root) = root else {
        back.clear();
        return;
    };

    compose_node(root, panes, back);
    chrome::draw_borders(back, root, focused, focused_border_color);
}

fn compose_node(node: &Node, panes: &[Pane], back: &mut Surface) {
    match node {
        Node::Leaf {
            pane, rect_target, ..
        } => {
            let Some(pane) = panes.iter().find(|candidate| candidate.id() == *pane) else {
                return;
            };

            let content_rect = inset_rect(*rect_target);
            let mut clipped = Surface::new(content_rect.w, content_rect.h);
            pane.cells_into(&mut clipped, 0, 0);
            back.blit(&clipped, content_rect.x, content_rect.y);
        }
        Node::Internal { a, b, .. } => {
            compose_node(a, panes, back);
            compose_node(b, panes, back);
        }
    }
}

fn inset_rect(rect: Rect) -> Rect {
    Rect {
        x: rect.x.saturating_add(1),
        y: rect.y.saturating_add(1),
        w: rect.w.saturating_sub(2),
        h: rect.h.saturating_sub(2),
    }
}

#[cfg(test)]
mod tests {
    use super::compose;
    use crate::backend::PaneId;
    use crate::layout::geometry::{FRect, Rect, Split};
    use crate::layout::tree::Node;
    use crate::term::pane::Pane;
    use crate::term::surface::Surface;

    #[test]
    fn compose_blits_first_pane_fullscreen() {
        let mut surface = Surface::new(80, 24);
        let mut pane = Pane::new(PaneId(1), 80, 24);
        let root = Node::Leaf {
            pane: PaneId(1),
            rect_current: FRect::from(Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            }),
            rect_target: Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        };

        pane.process(b"hi");
        compose(
            Some(&root),
            &[pane],
            Some(PaneId(1)),
            crossterm::style::Color::Cyan,
            &mut surface,
        );

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, '┌');
        assert_eq!(surface.get(1, 1).expect("cell exists").ch, 'h');
        assert_eq!(surface.get(2, 1).expect("cell exists").ch, 'i');
    }

    #[test]
    fn compose_blits_two_leaf_horizontal_split() {
        let mut surface = Surface::new(80, 24);
        let mut top = Pane::new(PaneId(1), 80, 12);
        let mut bottom = Pane::new(PaneId(2), 80, 12);
        let root = Node::Internal {
            split: Split::Horizontal,
            ratio: 0.5,
            ratio_target: 0.5,
            a: Box::new(Node::Leaf {
                pane: PaneId(1),
                rect_current: FRect::from(Rect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 12,
                }),
                rect_target: Rect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 12,
                },
            }),
            b: Box::new(Node::Leaf {
                pane: PaneId(2),
                rect_current: FRect::from(Rect {
                    x: 0,
                    y: 12,
                    w: 80,
                    h: 12,
                }),
                rect_target: Rect {
                    x: 0,
                    y: 12,
                    w: 80,
                    h: 12,
                },
            }),
            rect: Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        };

        top.process(b"A");
        bottom.process(b"B");
        compose(
            Some(&root),
            &[top, bottom],
            Some(PaneId(1)),
            crossterm::style::Color::Cyan,
            &mut surface,
        );

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, '┌');
        assert_eq!(surface.get(1, 1).expect("cell exists").ch, 'A');
        assert_eq!(surface.get(1, 13).expect("cell exists").ch, 'B');
    }

    #[test]
    fn compose_without_tree_clears_back_surface() {
        let mut surface = Surface::new(2, 1);
        let mut pane = Pane::new(PaneId(1), 2, 1);

        pane.process(b"x");
        compose(
            Some(&Node::Leaf {
                pane: PaneId(1),
                rect_current: FRect::from(Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                }),
                rect_target: Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                },
            }),
            &[pane],
            Some(PaneId(1)),
            crossterm::style::Color::Cyan,
            &mut surface,
        );
        compose(None, &[], None, crossterm::style::Color::Cyan, &mut surface);

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, ' ');
    }
}
