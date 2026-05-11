//! Compose panes into back surface.

use crate::layout::tree::Node;
use crate::term::pane::Pane;
use crate::term::surface::Surface;

pub fn compose(root: Option<&Node>, panes: &[Pane], back: &mut Surface) {
    let Some(root) = root else {
        back.clear();
        return;
    };

    compose_node(root, panes, back);
}

fn compose_node(node: &Node, panes: &[Pane], back: &mut Surface) {
    match node {
        Node::Leaf { pane, rect } => {
            let Some(pane) = panes.iter().find(|candidate| candidate.id() == *pane) else {
                return;
            };

            let mut clipped = Surface::new(rect.w, rect.h);
            pane.cells_into(&mut clipped, 0, 0);
            back.blit(&clipped, rect.x, rect.y);
        }
        Node::Internal { a, b, .. } => {
            compose_node(a, panes, back);
            compose_node(b, panes, back);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::compose;
    use crate::backend::PaneId;
    use crate::layout::geometry::{Rect, Split};
    use crate::layout::tree::Node;
    use crate::term::pane::Pane;
    use crate::term::surface::Surface;

    #[test]
    fn compose_blits_first_pane_fullscreen() {
        let mut surface = Surface::new(80, 24);
        let mut pane = Pane::new(PaneId(1), 80, 24);
        let root = Node::Leaf {
            pane: PaneId(1),
            rect: Rect {
                x: 0,
                y: 0,
                w: 80,
                h: 24,
            },
        };

        pane.process(b"hi");
        compose(Some(&root), &[pane], &mut surface);

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, 'h');
        assert_eq!(surface.get(1, 0).expect("cell exists").ch, 'i');
    }

    #[test]
    fn compose_blits_two_leaf_horizontal_split() {
        let mut surface = Surface::new(80, 24);
        let mut top = Pane::new(PaneId(1), 80, 12);
        let mut bottom = Pane::new(PaneId(2), 80, 12);
        let root = Node::Internal {
            split: Split::Horizontal,
            ratio: 0.5,
            a: Box::new(Node::Leaf {
                pane: PaneId(1),
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 12,
                },
            }),
            b: Box::new(Node::Leaf {
                pane: PaneId(2),
                rect: Rect {
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
        compose(Some(&root), &[top, bottom], &mut surface);

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, 'A');
        assert_eq!(surface.get(0, 12).expect("cell exists").ch, 'B');
    }

    #[test]
    fn compose_without_tree_clears_back_surface() {
        let mut surface = Surface::new(2, 1);
        let mut pane = Pane::new(PaneId(1), 2, 1);

        pane.process(b"x");
        compose(
            Some(&Node::Leaf {
                pane: PaneId(1),
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 2,
                    h: 1,
                },
            }),
            &[pane],
            &mut surface,
        );
        compose(None, &[], &mut surface);

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, ' ');
    }
}
