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

/// The powerline separators, in the order a segment uses them: the solid
/// wedge that closes a left-aligned segment, the hairline inside one, and the
/// two facing the other way for the right-hand block.
///
/// These are private-use codepoints, so they are only glyphs at all if the
/// terminal is running a patched font. `status-powerline off` swaps in the
/// plain forms below.
const SEP_RIGHT: char = '\u{e0b0}';
const SEP_RIGHT_THIN: char = '\u{e0b1}';
const SEP_LEFT: char = '\u{e0b2}';
const SEP_LEFT_THIN: char = '\u{e0b3}';
const PLAIN_THIN_RIGHT: char = '>';
const PLAIN_THIN_LEFT: char = '|';

const DATE_FORMAT: &str = "%Y-%m-%d";
const TIME_FORMAT: &str = "%H:%M";

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
    /// Which one of its kind this is, counting from one.
    pub index: u8,
    pub name: String,
    pub state: AgentState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceIndicator {
    pub number: u8,
    pub name: String,
    pub is_current: bool,
    pub pane_count: usize,
    /// tmux's `#F`: `*` current, `-` last, `Z` zoomed.
    pub flags: String,
}

/// Everything the status bar draws, gathered by whoever knows the session.
pub struct StatusBar<'a> {
    /// What sits in the leftmost segment: normally the session name, and a
    /// message or an open prompt while either is up.
    pub left: &'a str,
    pub workspaces: &'a [WorkspaceIndicator],
    pub agents: &'a [AgentIndicator],
    /// The host, as tmux's `#H` shows it.
    pub host: &'a str,
    pub now: chrono::DateTime<chrono::Local>,
    /// Whether the separators can be the powerline glyphs.
    pub powerline: bool,
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
/// Laid out as tmux's nord theme lays it out: the session on the left, then a
/// run of window segments, and a block of clock and host held against the
/// right edge. Every boundary is a colour change with a wedge over it, so the
/// bar reads as blocks rather than as a line of text.
pub fn draw_status_bar(surface: &mut Surface, bar: &StatusBar<'_>, theme: ThemeConfig) {
    if surface.width == 0 || surface.height == 0 {
        return;
    }

    let y = surface.height - 1;
    for x in 0..surface.width {
        surface.set(x, y, status_cell(' ', theme));
    }

    let max_x = surface.width;
    let left_end = left_run(bar, theme).draw(surface, 0, y, max_x);

    // A narrow bar gives up the date and the host before it gives up the
    // clock: the time is the part that is glanced at rather than read.
    for run in [right_run(bar, theme, true), right_run(bar, theme, false)] {
        let start = max_x.saturating_sub(run.width);
        if start < left_end {
            continue;
        }
        run.draw(surface, start, y, max_x);
        // Agents sit between the windows and that block, right-aligned against
        // it: the bar's left end changes length as windows come and go, and an
        // indicator that moves is one you have to read rather than glance at.
        draw_agents(surface, y, left_end, start, bar.agents, theme);
        break;
    }
}

/// The session segment and the window segments, drawn from the left edge.
fn left_run(bar: &StatusBar<'_>, theme: ThemeConfig) -> Run {
    let mut run = Run::default();
    run.push_str(
        &format!(" {} ", bar.left),
        theme.status_bg,
        theme.status_session,
    );
    run.close_segment(bar, theme.status_session, theme);

    let thin = if bar.powerline {
        SEP_RIGHT_THIN
    } else {
        PLAIN_THIN_RIGHT
    };
    for ws in bar.workspaces {
        let (fg, bg) = if ws.is_current {
            (theme.status_bg, theme.accent)
        } else {
            (theme.status_fg, theme.status_segment)
        };
        // The number stays because `Alt+1` and `-t :1` still address it; the
        // name is what makes a window recognisable.
        run.push_str(&format!(" {} {thin} {} {} ", ws.number, ws.name, ws.flags), fg, bg);
        run.close_segment(bar, bg, theme);
    }
    run
}

/// The clock block, drawn against the right edge.
///
/// `full` carries the date and the host as well; without it only the clock is
/// left, for a bar too narrow to hold the rest.
fn right_run(bar: &StatusBar<'_>, theme: ThemeConfig, full: bool) -> Run {
    let mut run = Run::default();
    if bar.powerline {
        run.push(SEP_LEFT, theme.status_segment, theme.status_bg);
    }

    if full {
        run.push_str(
            &format!(" {} ", bar.now.format(DATE_FORMAT)),
            theme.status_fg,
            theme.status_segment,
        );
        run.push(
            if bar.powerline {
                SEP_LEFT_THIN
            } else {
                PLAIN_THIN_LEFT
            },
            theme.status_fg,
            theme.status_segment,
        );
    }

    run.push_str(
        &format!(" {} ", bar.now.format(TIME_FORMAT)),
        theme.status_fg,
        theme.status_segment,
    );

    if full {
        if bar.powerline {
            run.push(SEP_LEFT, theme.accent, theme.status_segment);
        }
        run.push_str(&format!(" {} ", bar.host), theme.status_bg, theme.accent);
    }
    run
}

/// A run of status-bar cells, built before anything is drawn.
///
/// Built rather than written straight out because the right-hand block has to
/// know its own width to sit against the right edge, and the agents in the
/// middle have to know where that block starts.
#[derive(Default)]
struct Run {
    cells: Vec<(char, Color, Color)>,
    width: u16,
}

impl Run {
    fn push(&mut self, ch: char, fg: Color, bg: Color) {
        self.cells.push((ch, fg, bg));
        self.width = self.width.saturating_add(char_step(ch));
    }

    fn push_str(&mut self, text: &str, fg: Color, bg: Color) {
        for ch in text.chars() {
            self.push(ch, fg, bg);
        }
    }

    /// End a left-aligned segment coloured `bg`.
    ///
    /// The wedge is drawn over the bar's own background, not the next
    /// segment's, which is what leaves the thin dark notch between two
    /// segments of the same colour.
    fn close_segment(&mut self, bar: &StatusBar<'_>, bg: Color, theme: ThemeConfig) {
        if bar.powerline {
            self.push(SEP_RIGHT, bg, theme.status_bg);
        } else {
            self.push(' ', theme.status_fg, theme.status_bg);
        }
    }

    /// Returns where the run ended, clipped at `max_x`.
    fn draw(&self, surface: &mut Surface, start_x: u16, y: u16, max_x: u16) -> u16 {
        let mut x = start_x;
        for &(ch, fg, bg) in &self.cells {
            if x >= max_x {
                break;
            }
            surface.set(x, y, Cell::new(ch, fg, bg, CellAttrs::empty()));
            x = x.saturating_add(char_step(ch));
        }
        x
    }
}

fn char_step(ch: char) -> u16 {
    u16::try_from(UnicodeWidthChar::width(ch).unwrap_or(1).max(1)).unwrap_or(1)
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
        .map(|agent| format!("{AGENT_MARK} {}:{} ", agent.index, agent.name))
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
        AGENT_MARK, PLAIN_THIN_LEFT, SEP_LEFT, SEP_LEFT_THIN, SEP_RIGHT, SEP_RIGHT_THIN,
    };
    use crate::term::cell::Cell;
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
        status_segment: Color::DarkBlue,
        status_session: Color::DarkBlue,
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

    fn agent(index: u8, name: &str, state: AgentState) -> super::AgentIndicator {
        super::AgentIndicator {
            index,
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

    fn workspace(number: u8, name: &str, is_current: bool) -> super::WorkspaceIndicator {
        super::WorkspaceIndicator {
            number,
            name: name.to_owned(),
            is_current,
            pane_count: 1,
            flags: if is_current { "*" } else { "" }.to_owned(),
        }
    }

    fn status_bar<'a>(
        left: &'a str,
        workspaces: &'a [super::WorkspaceIndicator],
        agents: &'a [super::AgentIndicator],
    ) -> super::StatusBar<'a> {
        super::StatusBar {
            left,
            workspaces,
            agents,
            host: "testhost",
            now: test_time(),
            powerline: true,
        }
    }

    /// The cell at a character offset into the bottom row.
    ///
    /// Character offsets, not byte ones: every separator is three bytes wide
    /// and one cell.
    fn cell_at(surface: &Surface, offset: usize) -> &Cell {
        surface
            .get(u16::try_from(offset).expect("fits"), surface.height - 1)
            .expect("cell exists")
    }

    fn char_index(row: &str, needle: &str) -> usize {
        let cells: Vec<char> = row.chars().collect();
        let needle: Vec<char> = needle.chars().collect();
        let text: String = needle.iter().collect();
        cells
            .windows(needle.len())
            .position(|window| window == needle.as_slice())
            .unwrap_or_else(|| panic!("`{text}` is on the bar: {row}"))
    }

    #[test]
    fn agents_are_right_aligned_against_the_clock_and_coloured_by_state() {
        let mut surface = Surface::new(100, 4);
        let agents = [
            agent(1, "claude", AgentState::Working),
            agent(2, "claude", AgentState::Waiting),
            agent(1, "codex", AgentState::Idle),
        ];

        draw_status_bar(&mut surface, &status_bar("s", &[], &agents), TEST_THEME);

        let bottom = bottom_row(&surface);
        // Two claudes side by side, each numbered within its own kind.
        assert!(bottom.contains("1:claude"), "{bottom}");
        assert!(bottom.contains("2:claude"), "{bottom}");
        assert!(bottom.contains("1:codex"), "{bottom}");
        // Right up against the clock block, which keeps the far right.
        assert!(bottom.ends_with(" testhost "), "{bottom}");

        let colour_of = |needle: &str| {
            // The mark sits two cells before the agent's number.
            cell_at(&surface, char_index(&bottom, needle) - 2)
        };
        assert_eq!(colour_of("1:claude").ch, AGENT_MARK);
        assert_eq!(colour_of("1:claude").fg, TEST_THEME.agent_working);
        assert_eq!(colour_of("2:claude").fg, TEST_THEME.agent_waiting);
        assert_eq!(colour_of("1:codex").fg, TEST_THEME.agent_idle);
    }

    /// Half a list is worse than none: a dot with no name beside it says an
    /// agent needs you without saying which.
    #[test]
    fn agents_are_dropped_rather_than_clipped_when_the_bar_is_narrow() {
        let mut surface = Surface::new(24, 4);
        let agents = [agent(1, "claude", AgentState::Working)];

        draw_status_bar(
            &mut surface,
            &status_bar("a-long-session-name", &[], &agents),
            TEST_THEME,
        );

        let bottom = bottom_row(&surface);
        assert!(!bottom.contains(AGENT_MARK), "{bottom}");
    }

    #[test]
    fn no_agents_leaves_the_bar_as_it_was() {
        let mut surface = Surface::new(100, 4);

        draw_status_bar(&mut surface, &status_bar("s", &[], &[]), TEST_THEME);

        assert!(!bottom_row(&surface).contains(AGENT_MARK));
    }

    /// The session, the windows and the host each get their own block, and the
    /// wedge between two blocks carries the colour of the one it closes.
    #[test]
    fn draw_status_bar_builds_powerline_segments() {
        let mut surface = Surface::new(100, 4);
        let workspaces = [workspace(1, "edit", true), workspace(2, "build", false)];

        draw_status_bar(
            &mut surface,
            &status_bar("dev", &workspaces, &[]),
            TEST_THEME,
        );

        let bottom = bottom_row(&surface);
        assert!(bottom.starts_with(" dev "), "{bottom}");
        assert!(
            bottom.contains(&format!("1 {SEP_RIGHT_THIN} edit *")),
            "{bottom}"
        );
        assert!(
            bottom.contains(&format!("2 {SEP_RIGHT_THIN} build")),
            "{bottom}"
        );
        assert!(bottom.contains("2026-05-11"), "{bottom}");
        assert!(bottom.ends_with(" testhost "), "{bottom}");

        // The session sits on its own colour, and the wedge after it is that
        // colour over the bar's background.
        let session = cell_at(&surface, char_index(&bottom, "dev"));
        assert_eq!(session.bg, TEST_THEME.status_session);
        assert_eq!(session.fg, TEST_THEME.status_bg);
        let wedge = cell_at(&surface, char_index(&bottom, &SEP_RIGHT.to_string()));
        assert_eq!(wedge.fg, TEST_THEME.status_session);
        assert_eq!(wedge.bg, TEST_THEME.status_bg);

        // The current window takes the accent; the others stay quiet.
        assert_eq!(
            cell_at(&surface, char_index(&bottom, "edit")).bg,
            TEST_THEME.accent
        );
        assert_eq!(
            cell_at(&surface, char_index(&bottom, "build")).bg,
            TEST_THEME.status_segment
        );
        assert_eq!(
            cell_at(&surface, char_index(&bottom, "testhost")).bg,
            TEST_THEME.accent
        );
    }

    /// The wedges are private-use codepoints, so a terminal without a patched
    /// font would draw a row of tofu. Turning them off has to leave the bar
    /// readable, not just glyph-free.
    #[test]
    fn powerline_off_falls_back_to_plain_separators() {
        let mut surface = Surface::new(100, 4);
        let workspaces = [workspace(1, "edit", true)];
        let mut bar = status_bar("dev", &workspaces, &[]);
        bar.powerline = false;

        draw_status_bar(&mut surface, &bar, TEST_THEME);

        let bottom = bottom_row(&surface);
        for glyph in [SEP_RIGHT, SEP_RIGHT_THIN, SEP_LEFT, SEP_LEFT_THIN] {
            assert!(!bottom.contains(glyph), "{bottom}");
        }
        assert!(bottom.contains("1 > edit *"), "{bottom}");
        assert!(
            bottom.contains(&format!("2026-05-11 {PLAIN_THIN_LEFT} 14:23")),
            "{bottom}"
        );
        // The colours still separate the blocks.
        assert_eq!(
            cell_at(&surface, char_index(&bottom, "edit")).bg,
            TEST_THEME.accent
        );
    }

    /// A bar with no room for the whole right-hand block keeps the clock: the
    /// time is the part you glance at, the date and host the parts you know.
    #[test]
    fn a_narrow_bar_keeps_the_clock_and_drops_the_date_and_host() {
        let mut surface = Surface::new(48, 4);
        let workspaces = [workspace(1, "build", true)];

        draw_status_bar(
            &mut surface,
            &status_bar("dev", &workspaces, &[]),
            TEST_THEME,
        );

        let bottom = bottom_row(&surface);
        assert!(bottom.ends_with(" 14:23 "), "{bottom}");
        assert!(!bottom.contains("2026-05-11"), "{bottom}");
        assert!(!bottom.contains("testhost"), "{bottom}");
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
