//! Borders, status bar, debug overlay.

use crossterm::style::Color;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::anim::timeline::Timeline;
use crate::backend::PaneId;
use crate::config::ThemeConfig;
use crate::layout::geometry::Rect;
use crate::layout::tree::Node;
use crate::term::cell::{Cell, CellAttrs};
use crate::term::pane::Pane;
use crate::term::surface::Surface;

pub const UNFOCUSED_BORDER: Color = Color::Rgb {
    r: 0x41,
    g: 0x48,
    b: 0x68,
};
const DEBUG_FG: Color = Color::Black;
const DEBUG_BG: Color = Color::White;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DebugOverlay {
    pub fps: f64,
    pub frame_ms: f64,
    pub tweens: usize,
    pub dirty_cells: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceIndicator {
    pub number: u8,
    pub name: String,
    pub is_current: bool,
    pub pane_count: usize,
}

/// Draw every pane's border.
///
/// Two passes, unfocused first. Adjacent panes share border cells, so whoever
/// draws last owns the colour of the shared cell — and that has to be the
/// focused pane, or its highlight disappears wherever it touches a neighbour.
pub fn draw_borders(
    surface: &mut Surface,
    tree: &Node,
    panes: &[Pane],
    focused: Option<PaneId>,
    theme: ThemeConfig,
    timeline: &Timeline,
    pane_titles: bool,
) {
    draw_border_pass(surface, tree, panes, focused, theme, timeline, pane_titles, false);
    draw_border_pass(surface, tree, panes, focused, theme, timeline, pane_titles, true);
}

#[allow(clippy::too_many_arguments)]
fn draw_border_pass(
    surface: &mut Surface,
    tree: &Node,
    panes: &[Pane],
    focused: Option<PaneId>,
    theme: ThemeConfig,
    timeline: &Timeline,
    pane_titles: bool,
    focused_pass: bool,
) {
    match tree {
        Node::Leaf {
            pane, rect_target, ..
        } => {
            if (focused == Some(*pane)) != focused_pass {
                return;
            }

            let color = timeline.pane_border_color(
                *pane,
                focused,
                theme.border_focused,
                theme.border_unfocused,
            );
            draw_rect_border(surface, *rect_target, color);
            if pane_titles {
                let title = panes
                    .iter()
                    .find(|candidate| candidate.id() == *pane)
                    .and_then(Pane::title);
                draw_pane_title(surface, *rect_target, title, color);
            }
        }
        Node::Internal { a, b, .. } => {
            draw_border_pass(
                surface, a, panes, focused, theme, timeline, pane_titles, focused_pass,
            );
            draw_border_pass(
                surface, b, panes, focused, theme, timeline, pane_titles, focused_pass,
            );
        }
    }
}

/// Draw the status bar.
///
/// `status_left` is what sits at the far left in brackets: normally the
/// session name, and a message or an open prompt while either is up.
pub fn draw_status_bar(
    surface: &mut Surface,
    status_left: &str,
    workspaces: &[WorkspaceIndicator],
    now: chrono::DateTime<chrono::Local>,
    theme: ThemeConfig,
) {
    if surface.width == 0 || surface.height == 0 {
        return;
    }

    let y = surface.height - 1;
    for x in 0..surface.width {
        surface.set(x, y, status_cell(' ', theme));
    }

    let mut x: u16 = 0;
    let max_x = surface.width;

    let prefix = format!("[{status_left}] ");
    x = write_status_text(
        surface,
        x,
        y,
        max_x,
        &prefix,
        theme.status_fg,
        theme.status_bg,
    );

    for ws in workspaces {
        if x >= max_x {
            break;
        }
        // `1:build` — the number stays because `Alt+1` and `-t :1` still
        // address it; the name is what makes a window recognisable.
        let label = if ws.name.is_empty() {
            format!(" {} ", ws.number)
        } else {
            format!(" {}:{} ", ws.number, ws.name)
        };
        let (fg, bg) = if ws.is_current {
            (theme.status_bg, theme.accent)
        } else {
            (theme.status_fg, theme.status_bg)
        };
        x = write_status_text(surface, x, y, max_x, &label, fg, bg);
        // Separator between workspaces.
        x = write_status_text(surface, x, y, max_x, " ", theme.status_fg, theme.status_bg);
    }

    let clock = now.format("%H:%M:%S").to_string();
    let clock_width = u16::try_from(UnicodeWidthStr::width(clock.as_str())).unwrap_or(u16::MAX);
    if clock_width < max_x {
        let clock_start = max_x - clock_width;
        if clock_start >= x {
            write_status_text(
                surface,
                clock_start,
                y,
                max_x,
                &clock,
                theme.status_fg,
                theme.status_bg,
            );
        }
    }
}

fn write_status_text(
    surface: &mut Surface,
    start_x: u16,
    y: u16,
    max_x: u16,
    text: &str,
    fg: Color,
    bg: Color,
) -> u16 {
    let mut x = start_x;
    for ch in text.chars() {
        if x >= max_x {
            break;
        }
        surface.set(x, y, Cell::new(ch, fg, bg, CellAttrs::empty()));
        let step = u16::try_from(UnicodeWidthChar::width(ch).unwrap_or(1).max(1)).unwrap_or(1);
        x = x.saturating_add(step);
    }
    x
}

pub fn draw_debug_overlay(surface: &mut Surface, stats: DebugOverlay) {
    if surface.width == 0 || surface.height == 0 {
        return;
    }

    let text = format!(
        "fps:{:.0} frame:{:.1}ms tweens:{} dirty:{}",
        stats.fps, stats.frame_ms, stats.tweens, stats.dirty_cells
    );
    let surface_width = usize::from(surface.width);
    let text_width = text.chars().count();
    let rendered_width = text_width.min(surface_width);
    let start = surface_width.saturating_sub(rendered_width);
    let skip = text_width.saturating_sub(rendered_width);

    for (offset, ch) in text.chars().skip(skip).enumerate() {
        let x = u16::try_from(start + offset).unwrap_or(u16::MAX);
        surface.set(x, 0, debug_cell(ch));
    }
}

pub fn leaf_count(tree: Option<&Node>) -> usize {
    tree.map_or(0, count_leaves)
}

/// Which way a box-drawing glyph connects, as a bitmask.
///
/// Adjacent panes share border cells, so a glyph landing on one already drawn
/// has to *merge* with it — a corner arriving on a straight run is a
/// T-junction, not a corner. Turning both into edge sets and back is the
/// simplest way to get every combination right.
mod edges {
    pub const NORTH: u8 = 1;
    pub const SOUTH: u8 = 2;
    pub const EAST: u8 = 4;
    pub const WEST: u8 = 8;

    /// The edges a glyph connects along, or `None` if it is not a border.
    pub const fn of(ch: char) -> Option<u8> {
        Some(match ch {
            '─' => EAST | WEST,
            '│' => NORTH | SOUTH,
            '┌' => SOUTH | EAST,
            '┐' => SOUTH | WEST,
            '└' => NORTH | EAST,
            '┘' => NORTH | WEST,
            '├' => NORTH | SOUTH | EAST,
            '┤' => NORTH | SOUTH | WEST,
            '┬' => EAST | WEST | SOUTH,
            '┴' => EAST | WEST | NORTH,
            '┼' => NORTH | SOUTH | EAST | WEST,
            _ => return None,
        })
    }

    /// The glyph that connects exactly these edges.
    pub const fn glyph(mask: u8) -> char {
        match mask {
            m if m == EAST | WEST => '─',
            m if m == NORTH | SOUTH => '│',
            m if m == SOUTH | EAST => '┌',
            m if m == SOUTH | WEST => '┐',
            m if m == NORTH | EAST => '└',
            m if m == NORTH | WEST => '┘',
            m if m == NORTH | SOUTH | EAST => '├',
            m if m == NORTH | SOUTH | WEST => '┤',
            m if m == EAST | WEST | SOUTH => '┬',
            m if m == EAST | WEST | NORTH => '┴',
            m if m == NORTH | SOUTH | EAST | WEST => '┼',
            // A lone stub — one edge, or none — reads best as a straight run.
            m if m & (NORTH | SOUTH) != 0 => '│',
            _ => '─',
        }
    }
}

fn draw_rect_border(surface: &mut Surface, rect: Rect, color: Color) {
    if rect.w == 0 || rect.h == 0 {
        return;
    }

    let right = rect.x.saturating_add(rect.w.saturating_sub(1));
    let bottom = rect.y.saturating_add(rect.h.saturating_sub(1));

    // The runs stop short of the corners, which are set explicitly below.
    // Merging a run's edges into a corner would invent connections that are
    // not there — a top-left corner would pick up north and west and come out
    // as a cross.
    for x in (rect.x.saturating_add(1))..right {
        merge_border(surface, x, rect.y, '─', color);
        merge_border(surface, x, bottom, '─', color);
    }

    for y in (rect.y.saturating_add(1))..bottom {
        merge_border(surface, rect.x, y, '│', color);
        merge_border(surface, right, y, '│', color);
    }

    merge_border(surface, rect.x, rect.y, '┌', color);
    merge_border(surface, right, rect.y, '┐', color);
    merge_border(surface, rect.x, bottom, '└', color);
    merge_border(surface, right, bottom, '┘', color);
}

/// Draw a border glyph, joining it to whatever border is already there.
///
/// Panes that share a divider draw over each other, so a corner landing on an
/// existing run becomes the junction that connects both — otherwise the last
/// pane drawn wins and its neighbours look like they are missing edges.
fn merge_border(surface: &mut Surface, x: u16, y: u16, ch: char, color: Color) {
    let Some(incoming) = edges::of(ch) else {
        return;
    };

    let existing = surface
        .get(x, y)
        .and_then(|cell| edges::of(cell.ch))
        .unwrap_or(0);

    surface.set(x, y, border_cell(edges::glyph(existing | incoming), color));
}

fn border_cell(ch: char, color: Color) -> Cell {
    Cell::new(ch, color, Color::Reset, CellAttrs::empty())
}

fn draw_pane_title(surface: &mut Surface, rect: Rect, title: Option<&str>, color: Color) {
    let Some(title) = title else {
        return;
    };
    if rect.w < 5 {
        return;
    }

    let max_title_width = usize::from(rect.w.saturating_sub(4));
    if max_title_width == 0 {
        return;
    }

    let title = truncate_title(title, max_title_width);
    if title.is_empty() {
        return;
    }

    let overlay = format!("┤ {title} ├");
    let overlay_width = UnicodeWidthStr::width(overlay.as_str());
    if overlay_width > usize::from(rect.w) {
        return;
    }

    let start_offset = (usize::from(rect.w) - overlay_width) / 2;
    let mut x = rect
        .x
        .saturating_add(u16::try_from(start_offset).unwrap_or(0));
    for ch in overlay.chars() {
        surface.set(x, rect.y, border_cell(ch, color));
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let step = u16::try_from(width.max(1)).unwrap_or(1);
        x = x.saturating_add(step);
    }
}

fn truncate_title(title: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(title) <= max_width {
        return title.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_owned();
    }

    let mut truncated = String::new();
    let mut width = 0;
    let content_width = max_width - 1;
    for ch in title.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > content_width {
            break;
        }
        truncated.push(ch);
        width += char_width;
    }
    truncated.push('…');
    truncated
}

fn status_cell(ch: char, theme: ThemeConfig) -> Cell {
    Cell::new(ch, theme.status_fg, theme.status_bg, CellAttrs::empty())
}

fn debug_cell(ch: char) -> Cell {
    Cell::new(ch, DEBUG_FG, DEBUG_BG, CellAttrs::empty())
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

    use super::{draw_borders, draw_debug_overlay, draw_status_bar, truncate_title, DebugOverlay};
    use crate::anim::timeline::Timeline;
    use crate::backend::PaneId;
    use crate::config::ThemeConfig;
    use crate::layout::geometry::{FRect, Rect, Split};
    use crate::layout::tree::Node;
    use crate::term::pane::Pane;
    use crate::term::surface::Surface;

    const TEST_THEME: ThemeConfig = ThemeConfig {
        border_focused: Color::Cyan,
        border_unfocused: Color::DarkGrey,
        status_fg: Color::White,
        status_bg: Color::DarkBlue,
        accent: Color::Red,
    };

    /// Every combination of edges resolves to the right glyph, and merging is
    /// order-independent — a corner arriving on a run must give the same
    /// junction as a run arriving on a corner.
    #[test]
    fn border_glyphs_merge_into_junctions() {
        use super::edges;

        for (a, b, expected) in [
            ('─', '│', '┼'),
            ('┌', '─', '┬'),
            ('┌', '│', '├'),
            ('┘', '─', '┴'),
            ('┐', '│', '┤'),
            ('└', '┐', '┼'),
            ('─', '─', '─'),
            ('│', '│', '│'),
        ] {
            let merged = edges::glyph(
                edges::of(a).expect("a border") | edges::of(b).expect("a border"),
            );
            assert_eq!(merged, expected, "{a} + {b}");

            let reversed = edges::glyph(
                edges::of(b).expect("a border") | edges::of(a).expect("a border"),
            );
            assert_eq!(reversed, expected, "{b} + {a} should match {a} + {b}");
        }
    }

    #[test]
    fn draw_borders_marks_focused_and_unfocused_panes() {
        let mut surface = Surface::new(4, 4);
        let tree = Node::Internal {
            split: Split::Horizontal,
            ratio: 0.5,
            ratio_target: 0.5,
            a: Box::new(Node::Leaf {
                pane: PaneId(1),
                rect_current: FRect::from(Rect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 2,
                }),
                rect_target: Rect {
                    x: 0,
                    y: 0,
                    w: 4,
                    h: 2,
                },
            }),
            b: Box::new(Node::Leaf {
                pane: PaneId(2),
                rect_current: FRect::from(Rect {
                    x: 0,
                    y: 2,
                    w: 4,
                    h: 2,
                }),
                rect_target: Rect {
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
        let panes = [Pane::new(PaneId(1), 4, 2), Pane::new(PaneId(2), 4, 2)];

        draw_borders(
            &mut surface,
            &tree,
            &panes,
            Some(PaneId(1)),
            TEST_THEME,
            &Timeline::new(),
            true,
        );

        let focused_corner = surface.get(0, 0).expect("cell exists");
        let unfocused_corner = surface.get(0, 2).expect("cell exists");
        assert_eq!(focused_corner.ch, '┌');
        assert_eq!(focused_corner.fg, Color::Cyan);
        assert_eq!(unfocused_corner.ch, '┌');
        assert_eq!(unfocused_corner.fg, Color::DarkGrey);
    }

    #[test]
    fn draw_borders_centers_pane_title_on_top_border() {
        let mut surface = Surface::new(16, 3);
        let tree = Node::Leaf {
            pane: PaneId(1),
            rect_current: FRect::from(Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 3,
            }),
            rect_target: Rect {
                x: 0,
                y: 0,
                w: 16,
                h: 3,
            },
        };
        let mut pane = Pane::new(PaneId(1), 14, 1);
        pane.process(b"\x1b]2;hello\x07");

        draw_borders(
            &mut surface,
            &tree,
            &[pane],
            Some(PaneId(1)),
            TEST_THEME,
            &Timeline::new(),
            true,
        );

        let top: String = (0..surface.width)
            .map(|x| surface.get(x, 0).expect("cell exists").ch)
            .collect();
        assert_eq!(top, "┌──┤ hello ├───┐");
    }

    #[test]
    fn truncate_title_is_width_aware() {
        assert_eq!(truncate_title("abcdef", 4), "abc…");
        assert_eq!(truncate_title("界界界界", 5), "界界…");
    }

    #[test]
    fn draw_status_bar_highlights_current_workspace() {
        let mut surface = Surface::new(48, 4);
        let now = chrono::Local
            .with_ymd_and_hms(2026, 5, 11, 14, 23, 11)
            .single()
            .expect("test time exists");
        let workspaces = [
            super::WorkspaceIndicator {
                number: 1,
                name: String::new(),
                is_current: true,
                pane_count: 2,
            },
            super::WorkspaceIndicator {
                number: 3,
                name: "build".to_owned(),
                is_current: false,
                pane_count: 1,
            },
        ];

        draw_status_bar(&mut surface, "NORMAL", &workspaces, now, TEST_THEME);

        let bottom: String = (0..surface.width)
            .map(|x| surface.get(x, surface.height - 1).expect("cell exists").ch)
            .collect();
        assert!(bottom.starts_with("[NORMAL] "));
        // A named window shows its name; an unnamed one is just its number.
        assert!(bottom.contains("3:build"), "{bottom}");
        assert!(bottom.contains(" 1 "));
        assert!(bottom.contains("14:23:11"));

        // Find the cell rendering the current workspace digit '1' and verify
        // its background uses the accent color.
        let one_idx = bottom.find(" 1 ").expect("workspace 1 present") + 1;
        let cell = surface
            .get(u16::try_from(one_idx).expect("fits"), surface.height - 1)
            .expect("cell exists");
        assert_eq!(cell.bg, Color::Red);
    }

    #[test]
    fn draw_debug_overlay_right_aligns_on_top_row() {
        let mut surface = Surface::new(48, 3);

        draw_debug_overlay(
            &mut surface,
            DebugOverlay {
                fps: 160.0,
                frame_ms: 6.25,
                tweens: 3,
                dirty_cells: 80,
            },
        );

        let top: String = (0..surface.width)
            .map(|x| surface.get(x, 0).expect("cell exists").ch)
            .collect();
        assert!(top.ends_with("fps:160 frame:6.2ms tweens:3 dirty:80"));

        let first_overlay_x = top.find("fps:").expect("overlay is present");
        let cell = surface
            .get(
                u16::try_from(first_overlay_x).expect("test width fits u16"),
                0,
            )
            .expect("cell exists");
        assert_eq!(cell.fg, Color::Black);
        assert_eq!(cell.bg, Color::White);
    }
}
