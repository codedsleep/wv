//! Borders, status bar, debug overlay.

use crossterm::style::Color;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::agent::AgentState;
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
/// The dot beside an agent's name. Its colour carries the state.
const AGENT_MARK: char = '\u{25cf}';
const DEBUG_FG: Color = Color::Black;
const DEBUG_BG: Color = Color::White;

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct DebugOverlay {
    pub fps: f64,
    pub frame_ms: f64,
    pub tweens: usize,
    pub dirty_cells: usize,
}

/// One agent in the status bar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentIndicator {
    /// The window it is running in, so the bar says where to go.
    pub window: u8,
    pub name: String,
    pub state: AgentState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceIndicator {
    pub number: u8,
    pub name: String,
    pub is_current: bool,
    pub pane_count: usize,
}

pub fn draw_borders(
    surface: &mut Surface,
    tree: &Node,
    panes: &[Pane],
    focused: Option<PaneId>,
    theme: ThemeConfig,
    timeline: &Timeline,
    pane_titles: bool,
) {
    match tree {
        Node::Leaf {
            pane, rect_target, ..
        } => {
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
            draw_borders(surface, a, panes, focused, theme, timeline, pane_titles);
            draw_borders(surface, b, panes, focused, theme, timeline, pane_titles);
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
    agents: &[AgentIndicator],
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

        // Agents sit between the windows and the clock, right-aligned against
        // it: the bar's left end changes length as windows come and go, and an
        // indicator that moves is one you have to read rather than glance at.
        draw_agents(surface, y, x, clock_start, agents, theme);
    }
}

/// The agent indicators, right-aligned into `[left, right)`.
///
/// Drawn only if the whole run fits. Half a list is worse than none: a dot
/// with no name beside it says an agent needs you without saying which.
fn draw_agents(
    surface: &mut Surface,
    y: u16,
    left: u16,
    right: u16,
    agents: &[AgentIndicator],
    theme: ThemeConfig,
) {
    if agents.is_empty() {
        return;
    }

    let labels: Vec<String> = agents
        .iter()
        .map(|agent| format!("{AGENT_MARK} {}:{} ", agent.window, agent.name))
        .collect();
    let width = labels
        .iter()
        .map(|label| UnicodeWidthStr::width(label.as_str()))
        .sum::<usize>();
    let Ok(width) = u16::try_from(width) else {
        return;
    };
    if width > right.saturating_sub(left) {
        return;
    }

    let mut x = right - width;
    for (agent, label) in agents.iter().zip(&labels) {
        let fg = match agent.state {
            AgentState::Working => theme.agent_working,
            AgentState::Waiting => theme.agent_waiting,
            AgentState::Idle => theme.agent_idle,
        };
        x = write_status_text(surface, x, y, right, label, fg, theme.status_bg);
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

    use super::{
        draw_borders, draw_debug_overlay, draw_status_bar, truncate_title, DebugOverlay,
        AGENT_MARK,
    };
    use crate::agent::AgentState;
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
        agent_working: Color::Green,
        agent_waiting: Color::Yellow,
        agent_idle: Color::DarkGrey,
    };

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

    fn agent(window: u8, name: &str, state: AgentState) -> super::AgentIndicator {
        super::AgentIndicator {
            window,
            name: name.to_owned(),
            state,
        }
    }

    fn bottom_row(surface: &Surface) -> String {
        (0..surface.width)
            .map(|x| surface.get(x, surface.height - 1).expect("cell exists").ch)
            .collect()
    }

    fn test_time() -> chrono::DateTime<chrono::Local> {
        chrono::Local
            .with_ymd_and_hms(2026, 5, 11, 14, 23, 11)
            .single()
            .expect("test time exists")
    }

    #[test]
    fn agents_are_right_aligned_against_the_clock_and_coloured_by_state() {
        let mut surface = Surface::new(60, 4);
        let agents = [
            agent(1, "claude", AgentState::Working),
            agent(2, "codex", AgentState::Waiting),
            agent(3, "opencode", AgentState::Idle),
        ];

        draw_status_bar(&mut surface, "s", &[], &agents, test_time(), TEST_THEME);

        let bottom = bottom_row(&surface);
        assert!(bottom.contains("1:claude"), "{bottom}");
        assert!(bottom.contains("2:codex"), "{bottom}");
        assert!(bottom.contains("3:opencode"), "{bottom}");
        // Right up against the clock, which keeps the far right.
        assert!(bottom.ends_with("14:23:11"), "{bottom}");

        // Cell positions, not byte offsets: the mark is three bytes wide but
        // one cell.
        let cells: Vec<char> = bottom.chars().collect();
        let colour_of = |needle: &str| {
            let needle: Vec<char> = needle.chars().collect();
            let at = cells
                .windows(needle.len())
                .position(|window| window == needle.as_slice())
                .expect("label is on the bar");
            // The mark sits two cells before the window number.
            let x = u16::try_from(at - 2).expect("fits");
            surface.get(x, surface.height - 1).expect("cell exists")
        };
        assert_eq!(colour_of("1:claude").ch, AGENT_MARK);
        assert_eq!(colour_of("1:claude").fg, TEST_THEME.agent_working);
        assert_eq!(colour_of("2:codex").fg, TEST_THEME.agent_waiting);
        assert_eq!(colour_of("3:opencode").fg, TEST_THEME.agent_idle);
    }

    /// Half a list is worse than none: a dot with no name beside it says an
    /// agent needs you without saying which.
    #[test]
    fn agents_are_dropped_rather_than_clipped_when_the_bar_is_narrow() {
        let mut surface = Surface::new(24, 4);
        let agents = [agent(1, "claude", AgentState::Working)];

        draw_status_bar(
            &mut surface,
            "a-long-session-name",
            &[],
            &agents,
            test_time(),
            TEST_THEME,
        );

        let bottom = bottom_row(&surface);
        assert!(!bottom.contains(AGENT_MARK), "{bottom}");
    }

    #[test]
    fn no_agents_leaves_the_bar_as_it_was() {
        let mut surface = Surface::new(60, 4);

        draw_status_bar(&mut surface, "s", &[], &[], test_time(), TEST_THEME);

        assert!(!bottom_row(&surface).contains(AGENT_MARK));
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

        draw_status_bar(&mut surface, "NORMAL", &workspaces, &[], now, TEST_THEME);

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
