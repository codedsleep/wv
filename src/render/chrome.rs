//! Borders, status bar, debug overlay.

use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::agent::AgentState;
use crate::anim::timeline::Timeline;
use crate::backend::PaneId;
use crate::config::ThemeConfig;
use crate::layout::geometry::Rect;
use crate::layout::tree::Node;
use crate::picker::{Filtered, Picker};
use crate::term::cell::{Cell, CellAttrs};
use crate::term::pane::Pane;
use crate::term::surface::{self, Surface};

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
    /// Whether `left` is a message or an open prompt rather than the session
    /// name. tmux gives those their own `message-style`, and the block
    /// changing colour is what says "this is not what you usually read here".
    pub left_is_notice: bool,
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
    // `message-style bg=brightblack,fg=cyan`, and unbolded: a message is read
    // once and gone, so it should not also carry the session's weight.
    let (text_color, block) = if bar.left_is_notice {
        (theme.accent, theme.status_segment)
    } else {
        (theme.status_bg, theme.status_session)
    };
    let left = format!(" {} ", bar.left);
    if bar.left_is_notice {
        run.push_str(&left, text_color, block);
    } else {
        run.push_str_bold(&left, text_color, block);
    }
    run.close_segment(bar, block, theme);

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
        run.open_segment(bar, bg, theme);
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
        run.push_str_bold(&format!(" {} ", bar.host), theme.status_bg, theme.accent);
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
    cells: Vec<Cell>,
    width: u16,
}

impl Run {
    fn push(&mut self, ch: char, fg: Color, bg: Color) {
        self.push_cell(Cell::new(ch, fg, bg, CellAttrs::empty()));
    }

    fn push_cell(&mut self, cell: Cell) {
        self.width = self.width.saturating_add(char_step(cell.ch));
        self.cells.push(cell);
    }

    fn push_str(&mut self, text: &str, fg: Color, bg: Color) {
        for ch in text.chars() {
            self.push(ch, fg, bg);
        }
    }

    /// The two blocks tmux sets `bold` on: the session and the host.
    ///
    /// They are the ones that answer "where am I", and the weight is what
    /// makes them read as headings rather than as more of the same row.
    fn push_str_bold(&mut self, text: &str, fg: Color, bg: Color) {
        for ch in text.chars() {
            self.push_cell(Cell::new(ch, fg, bg, CellAttrs::BOLD));
        }
    }

    /// Open a left-aligned segment coloured `bg`.
    ///
    /// A wedge in the bar's own colour, cut into the segment rather than laid
    /// over the bar. Paired with the closing one it puts two arrows at every
    /// boundary — which is what keeps two segments of the same colour apart,
    /// and what the powerline look actually is.
    fn open_segment(&mut self, bar: &StatusBar<'_>, bg: Color, theme: ThemeConfig) {
        if bar.powerline {
            self.push(SEP_RIGHT, theme.status_bg, bg);
        }
    }

    /// End a left-aligned segment coloured `bg`: the same wedge the other way
    /// round, the segment's colour laid over the bar.
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
        for &cell in &self.cells {
            if x >= max_x {
                break;
            }
            x = x.saturating_add(surface.set_char(x, y, cell, max_x));
        }
        x
    }
}

fn char_step(ch: char) -> u16 {
    surface::char_width(ch)
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
        let cell = Cell::new(ch, fg, bg, CellAttrs::empty());
        x = x.saturating_add(surface.set_char(x, y, cell, max_x));
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
    let limit = surface.width;
    let surface_width = usize::from(limit);
    let text_width = text.chars().count();
    let rendered_width = text_width.min(surface_width);
    let start = surface_width.saturating_sub(rendered_width);
    let skip = text_width.saturating_sub(rendered_width);

    for (offset, ch) in text.chars().skip(skip).enumerate() {
        let x = u16::try_from(start + offset).unwrap_or(u16::MAX);
        surface.set_char(x, 0, debug_cell(ch), limit);
    }
}

/// Draw the goto picker over the composed frame.
///
/// The box is centred and grows out of its own middle as the open tween runs,
/// so it arrives from where it will end up rather than sliding in from an edge
/// — the same thing the pane animations do. Everything is clipped to the box,
/// and the box is clipped to the screen, so a terminal too small for the
/// picker gets a smaller picker rather than a corrupted frame.
pub fn draw_picker(surface: &mut Surface, picker: &Picker, theme: ThemeConfig) {
    if surface.width < PICKER_MIN_WIDTH || surface.height < PICKER_MIN_HEIGHT {
        return;
    }

    let progress = picker.progress();
    if progress <= 0.0 {
        return;
    }

    // Four lines of chrome around the rows: the top border, the query line,
    // the rule under it, and the bottom border.
    let rows_wanted = picker.match_count().max(1);
    let full_height = (rows_wanted + 4).min(usize::from(surface.height));
    let width = usize::from(surface.width)
        .saturating_sub(4)
        .clamp(PICKER_MIN_WIDTH.into(), PICKER_MAX_WIDTH);

    // Height is what animates; the width is fixed so the text does not reflow
    // on every frame of the open.
    let height = scale(full_height, progress).max(PICKER_MIN_HEIGHT.into());
    let Some(rect) = centered(surface, width, height) else {
        return;
    };

    let border = theme.border_focused;
    fill(surface, rect, theme.status_bg);
    draw_rect_border(surface, rect, border);
    draw_box_title(surface, rect, "goto", border);

    // Mid-animation the box is too short for its contents; drawing the query
    // line only once there is room for it and a row keeps the growth reading
    // as a box opening rather than as text appearing and jumping.
    if rect.h < 4 {
        return;
    }

    let inner_x = rect.x + 1;
    let inner_w = rect.w - 2;
    let query_y = rect.y + 1;
    draw_query_line(surface, inner_x, query_y, inner_w, picker, theme);

    // The separator under the query, then the rows.
    for x in inner_x..inner_x + inner_w {
        surface.set(x, query_y + 1, border_cell('─', border));
    }
    surface.set(rect.x, query_y + 1, border_cell('├', border));
    surface.set(rect.x + rect.w - 1, query_y + 1, border_cell('┤', border));

    let first_row_y = query_y + 2;
    let visible = usize::from(rect.y + rect.h - 1 - first_row_y);
    if visible == 0 {
        return;
    }

    if picker.match_count() == 0 {
        write_status_text(
            surface,
            inner_x + 1,
            first_row_y,
            inner_x + inner_w,
            "no match",
            theme.status_segment,
            theme.status_bg,
        );
        return;
    }

    // Scroll so the selection is always on screen, and keep it away from the
    // edges while there is list on both sides of it.
    let first = scroll_offset(picker.selected(), picker.match_count(), visible);
    for (offset, matched) in picker.matches().skip(first).take(visible).enumerate() {
        let index = first + offset;
        let y = first_row_y + u16::try_from(offset).unwrap_or(0);
        draw_picker_row(
            surface,
            inner_x,
            y,
            inner_w,
            &matched,
            index == picker.selected(),
            theme,
        );
    }
}

/// The narrowest and shortest the box is allowed to be, and the widest it is
/// allowed to grow — a picker spanning a wide monitor is harder to read, not
/// easier, because the eye has to travel from label to count.
const PICKER_MIN_WIDTH: u16 = 24;
const PICKER_MIN_HEIGHT: u16 = 3;
const PICKER_MAX_WIDTH: usize = 60;
/// The marker in front of the selected row, and in front of a session row.
const PICKER_SELECTED: &str = "\u{25b8} ";
const PICKER_CURRENT: char = '*';

fn draw_query_line(
    surface: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    picker: &Picker,
    theme: ThemeConfig,
) {
    let limit = x + width;
    let mut at = write_status_text(surface, x + 1, y, limit, "> ", theme.accent, theme.status_bg);

    // A block *at* the cursor rather than after it, the way the command prompt
    // draws its own, so it sits over the character it would replace.
    let mut query: Vec<char> = picker.query().chars().collect();
    query.insert(picker.cursor().min(query.len()), '\u{2588}');
    for ch in query {
        if at >= limit {
            break;
        }
        let cell = Cell::new(ch, theme.status_fg, theme.status_bg, CellAttrs::empty());
        at = at.saturating_add(surface.set_char(at, y, cell, limit));
    }
}

fn draw_picker_row(
    surface: &mut Surface,
    x: u16,
    y: u16,
    width: u16,
    matched: &Filtered<'_>,
    is_selected: bool,
    theme: ThemeConfig,
) {
    let row = matched.row;
    let bg = if is_selected {
        theme.status_segment
    } else {
        theme.status_bg
    };
    let fg = if row.is_current {
        theme.accent
    } else {
        theme.status_fg
    };
    let limit = x + width;

    for at in x..limit {
        surface.set(at, y, Cell::new(' ', fg, bg, CellAttrs::empty()));
    }

    let marker = if is_selected {
        PICKER_SELECTED.to_owned()
    } else if row.is_current {
        format!("{PICKER_CURRENT} ")
    } else {
        "  ".to_owned()
    };
    let mut at = write_status_text(surface, x, y, limit, &marker, theme.accent, bg);

    // The count is held against the right edge, so the label gets whatever is
    // left — and loses its tail rather than the count if there is not enough.
    let detail_width = u16::try_from(UnicodeWidthStr::width(row.detail.as_str())).unwrap_or(0);
    let label_limit = limit.saturating_sub(detail_width + 1).max(at);

    for (index, ch) in row.label.chars().enumerate() {
        if at >= label_limit {
            break;
        }
        let attrs = if matched.positions.contains(&index) {
            CellAttrs::BOLD
        } else {
            CellAttrs::empty()
        };
        let cell = Cell::new(ch, fg, bg, attrs);
        at = at.saturating_add(surface.set_char(at, y, cell, label_limit));
    }

    let detail_x = limit.saturating_sub(detail_width);
    if detail_x > at {
        write_status_text(
            surface,
            detail_x,
            y,
            limit,
            &row.detail,
            theme.status_segment,
            bg,
        );
    }
}

/// Which match sits at the top of the visible window.
///
/// The selection is kept centred once the list is long enough to scroll, so
/// there is always context above and below it, and pinned at the ends so the
/// first and last rows can actually be reached.
fn scroll_offset(selected: usize, total: usize, visible: usize) -> usize {
    if total <= visible {
        return 0;
    }

    selected
        .saturating_sub(visible / 2)
        .min(total - visible)
}

/// A rectangle of `width` × `height` in the middle of the surface.
fn centered(surface: &Surface, width: usize, height: usize) -> Option<Rect> {
    let w = u16::try_from(width).ok()?.min(surface.width);
    let h = u16::try_from(height).ok()?.min(surface.height);
    if w < PICKER_MIN_WIDTH || h < PICKER_MIN_HEIGHT {
        return None;
    }

    Some(Rect {
        x: (surface.width - w) / 2,
        y: (surface.height - h) / 2,
        w,
        h,
    })
}

fn fill(surface: &mut Surface, rect: Rect, bg: Color) {
    for y in rect.y..rect.y.saturating_add(rect.h) {
        for x in rect.x..rect.x.saturating_add(rect.w) {
            surface.set(x, y, Cell::new(' ', Color::Reset, bg, CellAttrs::empty()));
        }
    }
}

/// A title sitting in the top border, as pane titles do.
fn draw_box_title(surface: &mut Surface, rect: Rect, title: &str, color: Color) {
    let overlay = format!("\u{2524} {title} \u{251c}");
    let width = u16::try_from(UnicodeWidthStr::width(overlay.as_str())).unwrap_or(0);
    if width + 4 > rect.w {
        return;
    }

    let limit = rect.x + rect.w;
    write_status_text(surface, rect.x + 2, rect.y, limit, &overlay, color, Color::Reset);
}

/// Scale a length by an eased 0…1, never rounding a visible box down to nothing.
fn scale(length: usize, progress: f32) -> usize {
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let scaled = (length as f32 * progress).round() as usize;

    scaled
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
    let limit = rect.x.saturating_add(rect.w);
    for ch in overlay.chars() {
        x = x.saturating_add(surface.set_char(x, rect.y, border_cell(ch, color), limit));
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
        let char_width = usize::from(surface::char_width(ch));
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
        draw_borders, draw_debug_overlay, draw_picker, draw_status_bar, scroll_offset,
        truncate_title, DebugOverlay, AGENT_MARK, PLAIN_THIN_LEFT, SEP_LEFT, SEP_LEFT_THIN,
        SEP_RIGHT, SEP_RIGHT_THIN,
    };
    use crate::picker::{Picker, PickerRow, PICKER_TWEEN_DURATION};
    use crate::term::cell::{Cell, CellAttrs};
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
            left_is_notice: false,
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

        // The two blocks that answer "where am I" are the two tmux sets bold.
        assert!(session.attrs.contains(CellAttrs::BOLD), "{bottom}");
        assert!(
            cell_at(&surface, char_index(&bottom, "testhost"))
                .attrs
                .contains(CellAttrs::BOLD),
            "{bottom}"
        );
        assert!(
            !cell_at(&surface, char_index(&bottom, "edit"))
                .attrs
                .contains(CellAttrs::BOLD),
            "a window name is not a heading: {bottom}"
        );

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

    /// A message borrows the session's slot, so it has to be told apart by
    /// colour — otherwise a three-second notice reads as a renamed session.
    #[test]
    fn a_message_takes_the_session_block_but_not_its_colours() {
        let mut surface = Surface::new(100, 4);
        let mut bar = status_bar("saved", &[], &[]);
        bar.left_is_notice = true;

        draw_status_bar(&mut surface, &bar, TEST_THEME);

        let bottom = bottom_row(&surface);
        let notice = cell_at(&surface, char_index(&bottom, "saved"));
        assert_eq!(notice.bg, TEST_THEME.status_segment);
        assert_eq!(notice.fg, TEST_THEME.accent);
        assert!(
            !notice.attrs.contains(CellAttrs::BOLD),
            "read once and gone, so not a heading: {bottom}"
        );

        // The wedge closing it follows the block, or the seam shows.
        let wedge = cell_at(&surface, char_index(&bottom, &SEP_RIGHT.to_string()));
        assert_eq!(wedge.fg, TEST_THEME.status_segment);
    }

    /// Every window boundary carries two wedges, not one: the segment's colour
    /// laid over the bar, then the bar's colour cut into the next segment.
    /// With one wedge, two adjacent windows of the same colour run together.
    #[test]
    fn each_window_segment_is_wedged_at_both_ends() {
        let mut surface = Surface::new(120, 4);
        let workspaces = [workspace(1, "edit", false), workspace(2, "build", false)];

        draw_status_bar(
            &mut surface,
            &status_bar("dev", &workspaces, &[]),
            TEST_THEME,
        );

        let bottom = bottom_row(&surface);
        // The exact run tmux's `window-status-format` expands to.
        assert!(
            bottom.contains(&format!(
                "{SEP_RIGHT}{SEP_RIGHT} 1 {SEP_RIGHT_THIN} edit  {SEP_RIGHT}"
            )),
            "{bottom}"
        );

        let closing = char_index(&bottom, &format!("{SEP_RIGHT}{SEP_RIGHT} 1"));
        assert_eq!(cell_at(&surface, closing).fg, TEST_THEME.status_session);
        assert_eq!(cell_at(&surface, closing).bg, TEST_THEME.status_bg);
        // The second points the other way round: bar over segment.
        assert_eq!(cell_at(&surface, closing + 1).fg, TEST_THEME.status_bg);
        assert_eq!(cell_at(&surface, closing + 1).bg, TEST_THEME.status_segment);
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

    fn picker_rows() -> Vec<PickerRow> {
        vec![
            PickerRow::session("main".to_owned(), 2, true, true),
            PickerRow::window("main".to_owned(), 1, "editor", 2, true, true),
            PickerRow::window("main".to_owned(), 2, "dev-server", 1, false, true),
        ]
    }

    /// Read the surface back as lines, for asserting on what was drawn.
    fn lines(surface: &Surface) -> Vec<String> {
        (0..surface.height)
            .map(|y| {
                (0..surface.width)
                    .map(|x| surface.get(x, y).expect("cell exists").ch)
                    .collect()
            })
            .collect()
    }

    /// A picker that has finished opening.
    fn opened(rows: Vec<PickerRow>) -> Picker {
        let mut picker = Picker::new(rows);
        picker.advance(PICKER_TWEEN_DURATION);
        picker
    }

    #[test]
    fn the_picker_draws_a_centred_box_with_every_row_in_it() {
        let mut surface = Surface::new(40, 12);
        draw_picker(&mut surface, &opened(picker_rows()), TEST_THEME);

        let drawn = lines(&surface);
        let top = drawn
            .iter()
            .position(|line| line.contains('┌'))
            .expect("the box was drawn");
        let bottom = drawn
            .iter()
            .rposition(|line| line.contains('└'))
            .expect("the box is closed");

        // Seven rows of box (two borders, query, rule, three rows) in twelve,
        // so the clearance above and below can differ by the odd row and no
        // more.
        let below = drawn.len() - 1 - bottom;
        assert!(
            top.abs_diff(below) <= 1,
            "the box is vertically centred: {top} above, {below} below"
        );
        // Char positions, not byte offsets: the box is drawn in box-drawing
        // characters, every one of them three bytes wide.
        let columns: Vec<char> = drawn[top].chars().collect();
        let left = columns.iter().position(|ch| *ch == '┌').expect("a left edge");
        let right = columns.iter().rposition(|ch| *ch == '┐').expect("a right edge");
        assert_eq!(
            left,
            usize::from(surface.width) - 1 - right,
            "and horizontally centred"
        );

        assert!(drawn[top].contains("goto"), "the box is titled: {}", drawn[top]);
        let body = drawn.join("\n");
        for label in ["main", "main:1 editor", "main:2 dev-server"] {
            assert!(body.contains(label), "missing {label} in\n{body}");
        }
        for detail in ["2 windows", "2 panes", "1 pane"] {
            assert!(body.contains(detail), "missing {detail} in\n{body}");
        }
    }

    #[test]
    fn the_picker_marks_the_selected_row() {
        let mut surface = Surface::new(40, 12);
        let picker = opened(picker_rows());
        draw_picker(&mut surface, &picker, TEST_THEME);

        let marked: Vec<String> = lines(&surface)
            .into_iter()
            .filter(|line| line.contains('\u{25b8}'))
            .collect();
        assert_eq!(marked.len(), 1, "exactly one row carries the marker");
        assert!(
            marked[0].contains("main:1 editor"),
            "the window we are in: {}",
            marked[0]
        );
    }

    /// Mid-tween the box is short, and must still be a box rather than a row of
    /// stray border characters.
    #[test]
    fn a_half_open_picker_is_still_a_closed_box() {
        let mut surface = Surface::new(40, 12);
        let mut picker = Picker::new(picker_rows());
        picker.advance(PICKER_TWEEN_DURATION / 2);
        draw_picker(&mut surface, &picker, TEST_THEME);

        let drawn = lines(&surface);
        let corners = drawn.iter().filter(|line| line.contains('┌')).count();
        assert_eq!(corners, 1, "one top edge, wherever the tween has got to");
        assert_eq!(drawn.iter().filter(|line| line.contains('└')).count(), 1);
    }

    /// A terminal too small for the box gets no box, not a corrupted frame.
    #[test]
    fn the_picker_gives_up_on_a_tiny_terminal() {
        let mut surface = Surface::new(10, 3);
        draw_picker(&mut surface, &opened(picker_rows()), TEST_THEME);

        assert!(
            lines(&surface).iter().all(|line| line.trim().is_empty()),
            "nothing was drawn"
        );
    }

    #[test]
    fn a_filter_that_matches_nothing_says_so() {
        let mut surface = Surface::new(40, 12);
        let mut picker = opened(picker_rows());
        for ch in "zzz".chars() {
            picker.insert(ch);
        }
        draw_picker(&mut surface, &picker, TEST_THEME);

        assert!(lines(&surface).join("\n").contains("no match"));
    }

    #[test]
    fn the_scroll_window_keeps_the_selection_visible() {
        // Short enough list: no scrolling at all.
        assert_eq!(scroll_offset(3, 4, 10), 0);
        // Long list, selection at the top: still pinned to the start.
        assert_eq!(scroll_offset(0, 20, 5), 0);
        // In the middle: centred, so there is context either side.
        assert_eq!(scroll_offset(10, 20, 5), 8);
        // At the end: pinned, so the last row can be reached.
        assert_eq!(scroll_offset(19, 20, 5), 15);
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

