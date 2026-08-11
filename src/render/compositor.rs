//! Compose panes into back surface.

use crossterm::style::Color;

use crate::anim::timeline::Timeline;
use crate::config::ThemeConfig;
use crate::layout::tree::Node;
use crate::render::{chrome, subcell};
use crate::term::pane::Pane;
use crate::term::surface::Surface;
use crate::{
    backend::PaneId,
    layout::geometry::{FRect, Rect},
};

/// How the frame should be presented, as opposed to what is in it.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct ComposeOptions {
    pub pane_titles: bool,
    /// The pane filling the window, if one is zoomed.
    pub zoomed: Option<PaneId>,
}

pub fn compose(
    root: Option<&Node>,
    panes: &[Pane],
    focused: Option<PaneId>,
    theme: ThemeConfig,
    timeline: &Timeline,
    back: &mut Surface,
    options: ComposeOptions,
) {
    let Some(root) = root else {
        back.clear();
        return;
    };

    // A zoomed pane covers everything, so drawing the rest would only show
    // through at the edges mid-tween. Compose the zoomed leaf alone.
    let zoomed_leaf = options.zoomed.and_then(|pane| root.find_leaf(pane));
    let visible = zoomed_leaf.unwrap_or(root);

    back.clear();
    compose_node(visible, panes, back);
    chrome::draw_borders(
        back,
        visible,
        panes,
        focused,
        theme,
        timeline,
        options.pane_titles,
    );
}

fn compose_node(node: &Node, panes: &[Pane], back: &mut Surface) {
    match node {
        Node::Leaf {
            pane, rect_current, ..
        } => {
            let Some(pane) = panes.iter().find(|candidate| candidate.id() == *pane) else {
                return;
            };

            let content_rect = frect_to_covering_rect(*rect_current).content();
            let mut clipped = Surface::new(content_rect.w, content_rect.h);
            pane.cells_into(&mut clipped, 0, 0);
            back.blit(&clipped, content_rect.x, content_rect.y);
            subcell::draw_edges(back, *rect_current, Color::Reset, Color::Reset);
        }
        Node::Internal { a, b, .. } => {
            compose_node(a, panes, back);
            compose_node(b, panes, back);
        }
    }
}

fn frect_to_covering_rect(rect: FRect) -> Rect {
    let left = floor_to_u16(rect.x);
    let top = floor_to_u16(rect.y);
    let right = ceil_to_u16(rect.x + rect.w);
    let bottom = ceil_to_u16(rect.y + rect.h);

    Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn floor_to_u16(value: f32) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }

    if value >= f32::from(u16::MAX) {
        return u16::MAX;
    }

    value.floor() as u16
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ceil_to_u16(value: f32) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }

    if value >= f32::from(u16::MAX) {
        return u16::MAX;
    }

    value.ceil() as u16
}

#[cfg(test)]
mod tests {
    use super::{compose, ComposeOptions};
    use crate::anim::timeline::Timeline;
    use crate::backend::PaneId;
    use crate::config::ThemeConfig;
    use crate::layout::geometry::{FRect, Rect, Split};
    use crate::layout::tree::Node;
    use crate::render::chrome;
    use crate::term::pane::Pane;
    use crate::term::surface::Surface;

    const TEST_THEME: ThemeConfig = ThemeConfig {
        border_focused: crossterm::style::Color::Cyan,
        border_unfocused: crossterm::style::Color::DarkGrey,
        status_fg: crossterm::style::Color::White,
        status_bg: crossterm::style::Color::DarkBlue,
        accent: crossterm::style::Color::Red,
    };

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
            TEST_THEME,
            &Timeline::new(),
            &mut surface,
            ComposeOptions { pane_titles: true, zoomed: None },
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
            TEST_THEME,
            &Timeline::new(),
            &mut surface,
            ComposeOptions { pane_titles: true, zoomed: None },
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
            TEST_THEME,
            &Timeline::new(),
            &mut surface,
            ComposeOptions { pane_titles: true, zoomed: None },
        );
        compose(
            None,
            &[],
            None,
            TEST_THEME,
            &Timeline::new(),
            &mut surface,
            ComposeOptions { pane_titles: true, zoomed: None },
        );

        assert_eq!(surface.get(0, 0).expect("cell exists").ch, ' ');
    }

    #[test]
    fn integer_frects_match_target_rect_composition() {
        let mut new_surface = Surface::new(80, 24);
        let mut old_surface = Surface::new(80, 24);
        let mut top = Pane::new(PaneId(1), 80, 12);
        let mut bottom = Pane::new(PaneId(2), 80, 12);
        let tree = Node::Internal {
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
        let panes = [top, bottom];

        compose(
            Some(&tree),
            &panes,
            Some(PaneId(1)),
            TEST_THEME,
            &Timeline::new(),
            &mut new_surface,
            ComposeOptions { pane_titles: true, zoomed: None },
        );
        compose_legacy_target_rects(&tree, &panes, Some(PaneId(1)), TEST_THEME, &mut old_surface);

        assert_eq!(new_surface.cells, old_surface.cells);
    }

    fn compose_legacy_target_rects(
        root: &Node,
        panes: &[Pane],
        focused: Option<PaneId>,
        theme: ThemeConfig,
        back: &mut Surface,
    ) {
        compose_legacy_node(root, panes, back);
        chrome::draw_borders(back, root, panes, focused, theme, &Timeline::new(), true);
    }

    fn compose_legacy_node(node: &Node, panes: &[Pane], back: &mut Surface) {
        match node {
            Node::Leaf {
                pane, rect_target, ..
            } => {
                let Some(pane) = panes.iter().find(|candidate| candidate.id() == *pane) else {
                    return;
                };

                let content_rect = rect_target.content();
                let mut clipped = Surface::new(content_rect.w, content_rect.h);
                pane.cells_into(&mut clipped, 0, 0);
                back.blit(&clipped, content_rect.x, content_rect.y);
            }
            Node::Internal { a, b, .. } => {
                compose_legacy_node(a, panes, back);
                compose_legacy_node(b, panes, back);
            }
        }
    }
}
