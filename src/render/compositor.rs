//! Compose panes into back surface.

use crate::anim::timeline::Timeline;
use crate::config::ThemeConfig;
use crate::layout::tree::Node;
use crate::render::chrome;
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

/// Where the real terminal cursor belongs this frame, in screen cells.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CursorPlacement {
    pub x: u16,
    pub y: u16,
}

/// The focused pane's cursor, mapped from its grid into screen coordinates.
///
/// The surface carries no cursor of its own — it is a grid of cells, and the
/// diff renderer leaves the physical cursor wherever the last run of text
/// ended. Something has to say where it really goes, and this is it.
///
/// `None` means no cursor should be shown at all: nothing is focused, the
/// pane's program hid it, the focused pane is behind a zoomed one, or its
/// cursor sits outside the rectangle the pane is currently drawn in.
pub fn focused_cursor(
    root: Option<&Node>,
    panes: &[Pane],
    focused: Option<PaneId>,
    options: ComposeOptions,
) -> Option<CursorPlacement> {
    let focused = focused?;

    // Compose draws the zoomed leaf alone, so a focused pane that is not the
    // zoomed one is not on screen to put a cursor in.
    if options.zoomed.is_some_and(|zoomed| zoomed != focused) {
        return None;
    }

    let Some(Node::Leaf { rect_current, .. }) = root?.find_leaf(focused) else {
        return None;
    };
    let pane = panes.iter().find(|candidate| candidate.id() == focused)?;
    if pane.screen().hide_cursor() {
        return None;
    }

    // The same rectangle `compose_node` blits the pane into, so the cursor
    // tracks the pane through a resize tween instead of jumping at the end.
    let content = frect_to_screen_rect(*rect_current).content();
    let (row, col) = pane.screen().cursor_position();

    // A pane whose emulator grid is still the old size can have its cursor
    // outside the rect it is drawn in; the blit clips those cells away, and
    // the cursor has to be clipped with them.
    if col >= content.w || row >= content.h {
        return None;
    }

    Some(CursorPlacement {
        x: content.x.saturating_add(col),
        y: content.y.saturating_add(row),
    })
}

fn compose_node(node: &Node, panes: &[Pane], back: &mut Surface) {
    match node {
        Node::Leaf {
            pane, rect_current, ..
        } => {
            let Some(pane) = panes.iter().find(|candidate| candidate.id() == *pane) else {
                return;
            };

            // The border is drawn over the outermost cells of this same rect
            // by `chrome::draw_borders`, so the two move together through a
            // tween instead of the frame snapping ahead of the content.
            let content_rect = frect_to_screen_rect(*rect_current).content();
            pane.blit_into(back, content_rect);
        }
        Node::Internal { a, b, .. } => {
            compose_node(a, panes, back);
            compose_node(b, panes, back);
        }
    }
}

/// The cells a tweened rect occupies this frame.
///
/// Each edge is rounded to the nearest cell boundary on its own, rather than
/// rounding the origin and the size separately. Two panes that share an edge
/// in continuous space then share it on the grid too: `a.x + a.w == b.x` gives
/// `round(a.x + a.w) == round(b.x)`, so they never overlap by a cell or leave a
/// gap between them part-way through an animation.
///
/// Rounding rather than covering (`floor` of the near edge, `ceil` of the far)
/// matters for the same reason: a covering rect would claim a partially
/// entered cell for both neighbours at once, and whoever draws second wins
/// that column for a frame.
pub(crate) fn frect_to_screen_rect(rect: FRect) -> Rect {
    let left = round_to_u16(rect.x);
    let top = round_to_u16(rect.y);
    let right = round_to_u16(rect.x + rect.w);
    let bottom = round_to_u16(rect.y + rect.h);

    Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left),
        h: bottom.saturating_sub(top),
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn round_to_u16(value: f32) -> u16 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }

    if value >= f32::from(u16::MAX) {
        return u16::MAX;
    }

    value.round() as u16
}

#[cfg(test)]
mod tests {
    use super::{compose, focused_cursor, ComposeOptions, CursorPlacement};
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
        status_segment: crossterm::style::Color::DarkBlue,
        status_session: crossterm::style::Color::DarkBlue,
        accent: crossterm::style::Color::Red,
        agent_working: crossterm::style::Color::Green,
        agent_waiting: crossterm::style::Color::Yellow,
        agent_idle: crossterm::style::Color::DarkGrey,
    };

    /// A leaf filling the whole terminal, for the cursor tests.
    fn fullscreen_leaf(pane: PaneId, w: u16, h: u16) -> Node {
        let rect = Rect { x: 0, y: 0, w, h };
        Node::Leaf {
            pane,
            rect_current: FRect::from(rect),
            rect_target: rect,
        }
    }

    /// The pane's cursor is in its own grid; the terminal's is on the screen.
    /// The border between them is the whole of the offset.
    #[test]
    fn focused_cursor_maps_the_pane_grid_onto_the_screen() {
        let mut pane = Pane::new(PaneId(1), 80, 24);
        let root = fullscreen_leaf(PaneId(1), 80, 24);

        pane.process(b"hi");

        assert_eq!(
            focused_cursor(Some(&root), &[pane], Some(PaneId(1)), ComposeOptions::default()),
            Some(CursorPlacement { x: 3, y: 1 })
        );
    }

    #[test]
    fn focused_cursor_is_none_when_the_program_hid_it() {
        let mut pane = Pane::new(PaneId(1), 80, 24);
        let root = fullscreen_leaf(PaneId(1), 80, 24);

        pane.process(b"\x1b[?25l");

        assert_eq!(
            focused_cursor(Some(&root), &[pane], Some(PaneId(1)), ComposeOptions::default()),
            None
        );
    }

    #[test]
    fn focused_cursor_is_none_with_nothing_focused() {
        let pane = Pane::new(PaneId(1), 80, 24);
        let root = fullscreen_leaf(PaneId(1), 80, 24);

        assert_eq!(
            focused_cursor(Some(&root), &[pane], None, ComposeOptions::default()),
            None
        );
    }

    /// Compose draws the zoomed leaf alone, so a focused pane hidden behind it
    /// must not leave a cursor floating over somebody else's cells.
    #[test]
    fn focused_cursor_is_none_for_a_pane_behind_a_zoomed_one() {
        let pane = Pane::new(PaneId(1), 80, 24);
        let root = fullscreen_leaf(PaneId(1), 80, 24);

        assert_eq!(
            focused_cursor(
                Some(&root),
                &[pane],
                Some(PaneId(1)),
                ComposeOptions {
                    pane_titles: true,
                    zoomed: Some(PaneId(2)),
                },
            ),
            None
        );
    }

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

    /// A leaf whose `rect_current` is part-way from one rect to another,
    /// as it is on every frame of a resize tween.
    fn tweening_leaf(pane: PaneId, current: FRect, target: Rect) -> Node {
        Node::Leaf {
            pane,
            rect_current: current,
            rect_target: target,
        }
    }

    fn compose_one(root: &Node, pane: Pane, surface: &mut Surface) {
        compose(
            Some(root),
            &[pane],
            Some(PaneId(1)),
            TEST_THEME,
            &Timeline::new(),
            surface,
            ComposeOptions { pane_titles: false, zoomed: None },
        );
    }

    /// The border is part of the pane: it has to move with the content
    /// through a tween rather than standing at the destination while the
    /// content slides in behind it.
    #[test]
    fn borders_follow_the_tweened_rect_not_the_target() {
        let mut surface = Surface::new(40, 10);
        let pane = Pane::new(PaneId(1), 18, 8);
        // Half-way through sliding from x=0 to x=20.
        let root = tweening_leaf(
            PaneId(1),
            FRect { x: 10.0, y: 0.0, w: 20.0, h: 10.0 },
            Rect { x: 20, y: 0, w: 20, h: 10 },
        );

        compose_one(&root, pane, &mut surface);

        assert_eq!(surface.get(10, 0).expect("cell exists").ch, '┌');
        assert_eq!(surface.get(29, 0).expect("cell exists").ch, '┐');
        assert_eq!(surface.get(10, 9).expect("cell exists").ch, '└');
        assert_eq!(surface.get(29, 9).expect("cell exists").ch, '┘');
        // Nothing of a frame at the destination yet.
        assert_eq!(surface.get(39, 0).expect("cell exists").ch, ' ');
        assert_eq!(surface.get(0, 0).expect("cell exists").ch, ' ');
    }

    /// The black bars: a pane edge that fell between two cells used to be
    /// painted as a half-block glyph whose colour was `Reset` blended with
    /// `Reset`, which comes out as opaque black on any terminal that is not
    /// black itself. An edge between cells is now simply the border, on
    /// whichever cell is nearer.
    #[test]
    fn fractional_edges_are_borders_not_black_half_blocks() {
        let mut surface = Surface::new(40, 10);
        let pane = Pane::new(PaneId(1), 18, 8);
        let root = tweening_leaf(
            PaneId(1),
            FRect { x: 10.4, y: 0.6, w: 20.0, h: 9.0 },
            Rect { x: 20, y: 0, w: 20, h: 10 },
        );

        compose_one(&root, pane, &mut surface);

        let black = crossterm::style::Color::Rgb { r: 0, g: 0, b: 0 };
        for cell in &surface.cells {
            assert!(
                !matches!(cell.ch, '▐' | '▌' | '▄' | '▀' | '▗' | '▖' | '▝' | '▘'),
                "half-block glyph {:?} left in the frame",
                cell.ch
            );
            assert_ne!(cell.fg, black, "an opaque black cell left in the frame");
        }
        // 10.4 rounds down, 0.6 rounds up: the frame sits at (10, 1).
        assert_eq!(surface.get(10, 1).expect("cell exists").ch, '┌');
    }

    /// Two panes meeting on a fractional edge share that boundary on the grid:
    /// no cell is claimed by both, and none is left to neither.
    #[test]
    fn neighbours_on_a_fractional_edge_neither_overlap_nor_gap() {
        let mut surface = Surface::new(40, 10);
        let left = Pane::new(PaneId(1), 18, 8);
        let right = Pane::new(PaneId(2), 18, 8);
        let boundary = 20.6;
        let root = Node::Internal {
            split: Split::Vertical,
            ratio: 0.5,
            ratio_target: 0.5,
            a: Box::new(tweening_leaf(
                PaneId(1),
                FRect { x: 0.0, y: 0.0, w: boundary, h: 10.0 },
                Rect { x: 0, y: 0, w: 20, h: 10 },
            )),
            b: Box::new(tweening_leaf(
                PaneId(2),
                FRect { x: boundary, y: 0.0, w: 40.0 - boundary, h: 10.0 },
                Rect { x: 20, y: 0, w: 20, h: 10 },
            )),
            rect: Rect { x: 0, y: 0, w: 40, h: 10 },
        };

        compose(
            Some(&root),
            &[left, right],
            Some(PaneId(1)),
            TEST_THEME,
            &Timeline::new(),
            &mut surface,
            ComposeOptions { pane_titles: false, zoomed: None },
        );

        // 20.6 rounds to 21: the left pane's right border is column 20 and
        // the right pane's left border is column 21.
        assert_eq!(surface.get(20, 0).expect("cell exists").ch, '┐');
        assert_eq!(surface.get(21, 0).expect("cell exists").ch, '┌');
        assert_eq!(surface.get(20, 5).expect("cell exists").ch, '│');
        assert_eq!(surface.get(21, 5).expect("cell exists").ch, '│');
    }

    /// A pane collapsing shut is not framed once there is no room for both
    /// sides of the frame: one column of `│` on its own is debris.
    #[test]
    fn a_pane_too_narrow_for_a_frame_draws_none() {
        let mut surface = Surface::new(40, 10);
        let pane = Pane::new(PaneId(1), 18, 8);
        let root = tweening_leaf(
            PaneId(1),
            FRect { x: 20.0, y: 0.0, w: 1.2, h: 10.0 },
            Rect { x: 20, y: 0, w: 0, h: 10 },
        );

        compose_one(&root, pane, &mut surface);

        assert!(surface.cells.iter().all(|cell| cell.ch == ' '));
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
