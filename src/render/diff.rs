//! Diff front vs back, flush.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute, Attributes, Color, Print, SetAttribute, SetAttributes, SetBackgroundColor,
    SetForegroundColor,
};

use crate::term::cell::{Cell, CellAttrs};
use crate::term::surface::{self, Surface};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Style {
    fg: Color,
    bg: Color,
    attrs: CellAttrs,
}

impl Style {
    fn from_cell(cell: Cell, color_mode: ColorMode) -> Self {
        Self {
            fg: color_mode.render_color(cell.fg),
            bg: color_mode.render_color(cell.bg),
            attrs: cell.attrs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Truecolor,
    Quantized,
}

impl ColorMode {
    pub fn from_env() -> Self {
        match std::env::var("COLORTERM") {
            Ok(value) if value == "truecolor" || value == "24bit" => Self::Truecolor,
            _ => Self::Quantized,
        }
    }

    fn render_color(self, color: Color) -> Color {
        match (self, color) {
            (Self::Quantized, Color::Rgb { r, g, b }) => {
                Color::AnsiValue(quantize_rgb_to_xterm256(r, g, b))
            }
            _ => color,
        }
    }
}

pub struct DiffRenderer {
    queue: Vec<u8>,
    color_mode: ColorMode,
}

impl DiffRenderer {
    pub fn new() -> Self {
        Self::with_color_mode(ColorMode::from_env())
    }

    /// Override the color depth, e.g. from an attached client's capabilities.
    pub fn set_color_mode(&mut self, color_mode: ColorMode) {
        self.color_mode = color_mode;
    }

    pub const fn with_color_mode(color_mode: ColorMode) -> Self {
        Self {
            queue: Vec::new(),
            color_mode,
        }
    }

    /// Write terminal updates needed to transform `front` into `back`.
    ///
    /// This method reuses an internal queue buffer for command batching. It
    /// does not mutate `front`; the caller is responsible for copying or
    /// swapping buffers after a successful flush.
    ///
    /// # Errors
    ///
    /// Returns any I/O error raised while encoding crossterm commands or
    /// writing the queued bytes to `out`.
    pub fn flush<W: Write>(
        &mut self,
        front: &Surface,
        back: &Surface,
        out: &mut W,
    ) -> io::Result<()> {
        self.paint(Some(front), back, out)
    }

    /// Write every cell of `back`, for a screen whose contents are unknown.
    ///
    /// A terminal that has just resized, or that has only now started
    /// watching, is showing something nobody has a record of. Clearing it and
    /// diffing against a blank `front` looks like the same thing and is not:
    /// `ESC[2J` fills the screen with the *current* background colour, so
    /// every cell the diff then skips for being blank on both sides keeps
    /// whatever colour the last frame happened to leave set — a band of status
    /// bar behind an empty pane. Painting outright asks the screen for
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns any I/O error raised while encoding crossterm commands or
    /// writing the queued bytes to `out`.
    pub fn repaint<W: Write>(&mut self, back: &Surface, out: &mut W) -> io::Result<()> {
        self.paint(None, back, out)
    }

    /// Write a blank into every column a wide character covers, before the
    /// glyph that covers it is written.
    ///
    /// Two terminals disagreeing about a glyph's width cuts both ways, and the
    /// direction the main loop cannot answer is the one where this side counts
    /// two columns and the host draws one. Nothing is ever printed into the
    /// second column — printing there would land a column right of where the
    /// surface says on a host that *does* draw two — so on a host that draws
    /// one, that column is never written at all. In a diff that is survivable:
    /// there is a record of what the screen holds. In a repaint there is no
    /// record, and the column keeps whatever was there before the attach or
    /// the resize.
    ///
    /// Order is what makes this safe, which is why it is a pass of its own and
    /// not a branch in the loop. The blank goes down *first*: a host that draws
    /// one column keeps it and the stale cell is gone, and a host that draws
    /// two covers it with the glyph's own right half a moment later. Neither
    /// needs to be told apart from the other.
    ///
    /// The cursor is left wherever the last blank put it; the main loop opens
    /// by saying where it is painting.
    fn blank_covered_columns(
        &mut self,
        back: &Surface,
        current_style: &mut Option<Style>,
    ) -> io::Result<()> {
        let width = usize::from(back.width);

        for index in 0..back.cells.len() {
            if !back.cells[index].is_continuation() || !covered_by_wide(back, index) {
                continue;
            }

            // The last cell of the grid is the one column a blank cannot be
            // written into safely: on a host that drew the glyph across two,
            // the cursor is already past the screen's corner and printing
            // there scrolls the row away.
            if index + 1 == back.cells.len() {
                continue;
            }

            let x = u16::try_from(index % width).unwrap_or(u16::MAX);
            let y = u16::try_from(index / width).unwrap_or(u16::MAX);
            queue!(self.queue, MoveTo(x, y))?;
            // The continuation carries the wide cell's colours, so the blank
            // is the background that column is meant to be showing.
            let style = Style::from_cell(back.cells[index], self.color_mode);
            emit_style(&mut self.queue, current_style, style)?;
            queue!(self.queue, Print(' '))?;
        }

        Ok(())
    }

    fn paint<W: Write>(
        &mut self,
        front: Option<&Surface>,
        back: &Surface,
        out: &mut W,
    ) -> io::Result<()> {
        self.queue.clear();

        if back.width == 0 {
            out.write_all(&self.queue)?;
            return Ok(());
        }

        let mut current_style = None;
        let mut index = 0;
        // No `front` at all means the screen is unknown and every cell is
        // painted. So does a `front` of another shape, which is not a record of
        // this screen either: its cell at a given index sits at a different
        // column and row, so a pair that compares equal is not a column already
        // showing the right thing.
        let comparable =
            front.filter(|front| front.width == back.width && front.height == back.height);
        let len = comparable.map_or(back.cells.len(), |front| {
            front.cells.len().min(back.cells.len())
        });
        let unchanged =
            |index: usize| comparable.is_some_and(|front| front.cells[index] == back.cells[index]);

        // A screen nobody has a record of has to be written whole, and the
        // main loop deliberately writes nothing into the second column of a
        // wide character — so on a host that draws that character in one
        // column, that column keeps whatever it was showing before. Clearing
        // those columns first, rather than skipping them, is what makes the
        // whole grid actually written.
        if comparable.is_none() {
            self.blank_covered_columns(back, &mut current_style)?;
        }

        while index < len {
            if unchanged(index) {
                index += 1;
                continue;
            }

            // Where the next thing printed must land. Set again whenever the
            // terminal's cursor may not be where this side thinks it is.
            let mut anchor = true;
            // One column that must be painted even though the record says it is
            // already right — see `spills_right`.
            let mut force = false;

            while index < len && (!unchanged(index) || force) {
                // Nothing is ever printed into the second column of a wide
                // character, so step over it before anchoring rather than
                // moving the cursor to a column nothing lands in.
                if back.cells[index].is_continuation() && covered_by_wide(back, index) {
                    index += 1;
                    anchor = true;
                    force = false;
                    continue;
                }

                if anchor {
                    let x = u16::try_from(index % usize::from(back.width)).unwrap_or(u16::MAX);
                    let y = u16::try_from(index / usize::from(back.width)).unwrap_or(u16::MAX);
                    queue!(self.queue, MoveTo(x, y))?;
                    anchor = false;
                }

                let style = Style::from_cell(back.cells[index], self.color_mode);
                emit_style(&mut self.queue, &mut current_style, style)?;

                let mut run = String::new();
                while index < len
                    && (!unchanged(index) || force)
                    && Style::from_cell(back.cells[index], self.color_mode) == style
                {
                    let cell = back.cells[index];
                    force = false;
                    if cell.is_continuation() {
                        // The right half of a wide character: printing the
                        // glyph in front of it already moved the terminal's
                        // cursor across this column, so anything printed here
                        // would land a column further right than the surface
                        // says, and take the rest of the row with it. A stray
                        // continuation with no glyph in front of it — which
                        // the surface tries not to keep — still needs the
                        // column painted, so it gets a blank.
                        if covered_by_wide(back, index) {
                            index += 1;
                            anchor = true;
                            break;
                        }
                        run.push(' ');
                        index += 1;
                        continue;
                    }

                    run.push(emitted_char(cell.ch));
                    index += 1;

                    // Anything but ASCII is a guess about how far the cursor
                    // just moved, and the terminal on the other end is the one
                    // who decides: its width table is its own, built from its
                    // own version of Unicode. Rather than carry the guess into
                    // the rest of the row, say where the next column is.
                    //
                    // More of the same character first, though. A disagreement
                    // over a glyph repeated fifty times is a border drawn to
                    // the wrong length and no worse — where it ends is stated
                    // outright either way — and a border is most of what is
                    // ever painted in one character. So the anchor goes after
                    // the run, not after every glyph in it.
                    if !cell.ch.is_ascii() {
                        while index < len
                            && !unchanged(index)
                            && back.cells[index].ch == cell.ch
                            && Style::from_cell(back.cells[index], self.color_mode) == style
                        {
                            run.push(emitted_char(cell.ch));
                            index += 1;
                        }
                        anchor = true;
                        force = spills_right(back, cell.ch, index);
                        break;
                    }
                }

                if !run.is_empty() {
                    queue!(self.queue, Print(run))?;
                }
            }
        }

        out.write_all(&self.queue)
    }
}

/// What actually goes on the wire for a cell's character.
///
/// A cell is one column, and a zero-width character is not one: a terminal
/// hangs it on the glyph *before* it and moves the cursor nowhere. Printed as
/// though it were a column of its own it would change a neighbour this frame
/// never meant to touch and leave every column after it on the row one to the
/// left. It reaches a cell of its own only when it arrived with no glyph to
/// join — a combining mark at the start of a line, a stray variation selector
/// — so a blank is both what it means and what the surface says is there.
fn emitted_char(ch: char) -> char {
    if surface::char_is_zero_width(ch) {
        ' '
    } else {
        ch
    }
}

/// Whether the column at `index` has to be repainted because the glyph just
/// printed in front of it may have landed on it.
///
/// Saying where the next column is keeps a width disagreement from walking the
/// rest of the row sideways, but it cannot un-draw anything: a glyph this side
/// counts as one column and the host draws across two has already covered the
/// column beside it. When that column changed too it is painted anyway and the
/// damage repairs itself, which is why a screen being filled in looks fine.
/// When it did not change — a glyph appearing beside text that was already
/// there, which is a status bar, a powerline separator, a Nerd Font icon in a
/// nested weave's own bar — nothing repaints it, and the record of what the
/// terminal is showing says that column is right. The corruption stays until
/// something else happens to touch it.
///
/// So the neighbour of a glyph whose width is the host's to decide is painted
/// on the strength of that, not on the diff. `ch` is the glyph that was
/// printed and `index` the column after it; a glyph this side already knows to
/// be two columns wide owns that column legitimately and is not in question.
fn spills_right(back: &Surface, ch: char, index: usize) -> bool {
    back.width != 0
        && index < back.cells.len()
        && index % usize::from(back.width) != 0
        && surface::char_width(ch) == 1
}

/// Whether the cell at `index` is the second column of a wide character, as
/// opposed to a continuation left behind on its own.
fn covered_by_wide(back: &Surface, index: usize) -> bool {
    if back.width == 0 || index % usize::from(back.width) == 0 {
        return false;
    }

    surface::char_width(back.cells[index - 1].ch) == 2
}

impl Default for DiffRenderer {
    fn default() -> Self {
        Self::new()
    }
}

pub fn quantize_rgb_to_xterm256(r: u8, g: u8, b: u8) -> u8 {
    let mut best_index = 16;
    let mut best_distance = u32::MAX;

    for index in 16..=231 {
        let palette = xterm256_color(index);
        let distance = color_distance_squared((r, g, b), palette);
        if distance < best_distance {
            best_index = index;
            best_distance = distance;
        }
    }

    for index in 232..=255 {
        let palette = xterm256_color(index);
        let distance = color_distance_squared((r, g, b), palette);
        if distance < best_distance {
            best_index = index;
            best_distance = distance;
        }
    }

    best_index
}

fn xterm256_color(index: u8) -> (u8, u8, u8) {
    if index >= 232 {
        let gray = 8 + ((index - 232) * 10);
        return (gray, gray, gray);
    }

    let cube_index = index - 16;
    (
        xterm_cube_channel(cube_index / 36),
        xterm_cube_channel((cube_index / 6) % 6),
        xterm_cube_channel(cube_index % 6),
    )
}

const fn xterm_cube_channel(index: u8) -> u8 {
    match index {
        0 => 0,
        1 => 95,
        2 => 135,
        3 => 175,
        4 => 215,
        _ => 255,
    }
}

fn color_distance_squared(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    channel_distance_squared(a.0, b.0)
        + channel_distance_squared(a.1, b.1)
        + channel_distance_squared(a.2, b.2)
}

fn channel_distance_squared(a: u8, b: u8) -> u32 {
    let distance = a.abs_diff(b);
    u32::from(distance) * u32::from(distance)
}

fn emit_style(
    queue_buf: &mut Vec<u8>,
    current_style: &mut Option<Style>,
    next_style: Style,
) -> io::Result<()> {
    if *current_style == Some(next_style) {
        return Ok(());
    }

    let reset_attrs = current_style.is_some_and(|style| style.attrs != next_style.attrs);
    if reset_attrs {
        queue!(queue_buf, SetAttribute(Attribute::Reset))?;
    }

    if current_style.map_or(true, |style| style.fg != next_style.fg) || reset_attrs {
        queue!(queue_buf, SetForegroundColor(next_style.fg))?;
    }

    if current_style.map_or(true, |style| style.bg != next_style.bg) || reset_attrs {
        queue!(queue_buf, SetBackgroundColor(next_style.bg))?;
    }

    if current_style.map_or(true, |style| style.attrs != next_style.attrs) {
        queue!(
            queue_buf,
            SetAttributes(to_crossterm_attrs(next_style.attrs))
        )?;
    }

    *current_style = Some(next_style);
    Ok(())
}

fn to_crossterm_attrs(attrs: CellAttrs) -> Attributes {
    let mut converted = Attributes::none();

    if attrs.contains(CellAttrs::BOLD) {
        converted.set(Attribute::Bold);
    }
    if attrs.contains(CellAttrs::ITALIC) {
        converted.set(Attribute::Italic);
    }
    if attrs.contains(CellAttrs::UNDERLINE) {
        converted.set(Attribute::Underlined);
    }
    if attrs.contains(CellAttrs::REVERSE) {
        converted.set(Attribute::Reverse);
    }

    converted
}

#[cfg(test)]
mod tests {
    use crossterm::style::Color;

    use super::{quantize_rgb_to_xterm256, ColorMode, DiffRenderer};
    use crate::term::cell::{Cell, CellAttrs};
    use crate::term::surface::Surface;

    #[test]
    fn flush_moves_to_changed_run_and_prints_text() {
        let front = Surface::new(5, 1);
        let mut back = Surface::new(5, 1);
        let mut out = Vec::new();
        let mut renderer = DiffRenderer::new();

        for (x, ch) in "hello".chars().enumerate() {
            back.set(
                u16::try_from(x).expect("test index fits u16"),
                0,
                Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty()),
            );
        }

        renderer
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");

        assert!(out.starts_with(b"\x1b[1;1H"));
        assert!(out.windows(b"hello".len()).any(|window| window == b"hello"));
    }

    /// The terminal draws a wide glyph across two columns and leaves its cursor
    /// past both. Printing anything for the second column — a blank, or the NUL
    /// the continuation cell holds — puts the rest of the row one column right
    /// of where the surface says it is. Nothing is printed there, and what
    /// comes after says its own column rather than trusting the glyph to have
    /// moved the cursor exactly two.
    #[test]
    fn flush_prints_nothing_for_the_second_column_of_a_wide_character() {
        let front = Surface::new(5, 1);
        let mut back = Surface::new(5, 1);
        let mut out = Vec::new();

        let wide = Cell::new('\u{754c}', Color::Reset, Color::Reset, CellAttrs::empty());
        back.set_char(0, 0, wide, 5);
        back.set(2, 0, Cell::new('o', Color::Reset, Color::Reset, CellAttrs::empty()));
        back.set(3, 0, Cell::new('k', Color::Reset, Color::Reset, CellAttrs::empty()));

        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");

        let text = String::from_utf8(out).expect("output is utf-8");
        assert!(!text.contains('\0'), "a NUL reached the terminal: {text:?}");
        assert!(
            text.contains("\u{754c}\x1b[1;3Hok"),
            "the columns after a wide glyph must be placed, not assumed: {text:?}"
        );
    }

    /// A continuation with no glyph in front of it should not exist, but if one
    /// ever does the column still has to be painted rather than left stale.
    #[test]
    fn flush_paints_a_blank_for_a_stray_continuation() {
        let front = Surface::new(3, 1);
        let mut back = Surface::new(3, 1);
        let mut out = Vec::new();

        back.cells[1] = Cell::new(
            Cell::CONTINUATION,
            Color::Reset,
            Color::Reset,
            CellAttrs::empty(),
        );
        back.cells[0] = Cell::new('a', Color::Reset, Color::Reset, CellAttrs::empty());

        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");

        let text = String::from_utf8(out).expect("output is utf-8");
        assert!(!text.contains('\0'), "a NUL reached the terminal: {text:?}");
        assert!(text.contains("a "), "stray column not painted: {text:?}");
    }

    /// A terminal that measures a glyph differently than weave does.
    ///
    /// Every terminal carries its own width table, built from its own version
    /// of Unicode: kitty draws `☰` across two columns where the emulator behind
    /// a pane calls it one, and hangs a combining mark on the glyph before it
    /// where weave gives it a column. Both are real, both are current, and
    /// neither side can be talked out of it — so what is written has to survive
    /// the disagreement.
    fn columns_after(text: &str, misjudged: char, host_width: usize) -> Vec<Option<char>> {
        let mut columns: Vec<Option<char>> = vec![None; 16];
        let mut cursor = 0_usize;
        let mut rest = text;

        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix("\x1b[") {
                let end = tail
                    .find(|ch: char| ch.is_ascii_alphabetic())
                    .expect("a finished escape sequence");
                let (params, kind) = tail.split_at(end);
                if kind.starts_with('H') {
                    // `CSI row;col H`, one-based, and only ever one row here.
                    let col = params.split(';').nth(1).unwrap_or("1");
                    cursor = col.parse::<usize>().expect("a column") - 1;
                }
                rest = &tail[end + 1..];
                continue;
            }

            let ch = rest.chars().next().expect("a character");
            rest = &rest[ch.len_utf8()..];
            let width = if ch == misjudged {
                host_width
            } else {
                usize::from(crate::term::surface::char_width(ch))
            };
            if width > 0 && cursor < columns.len() {
                columns[cursor] = Some(ch);
            }
            cursor += width;
        }

        columns
    }

    /// The same host, but starting from a screen that already shows something —
    /// which is what a diff against a real `front` is. `seed` is what those
    /// columns held before this frame was written.
    fn columns_over(
        seed: &str,
        text: &str,
        misjudged: char,
        host_width: usize,
    ) -> Vec<Option<char>> {
        let mut columns: Vec<Option<char>> = seed.chars().map(Some).collect();
        columns.resize(16, None);
        let mut cursor = 0_usize;
        let mut rest = text;

        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix("\x1b[") {
                let end = tail
                    .find(|ch: char| ch.is_ascii_alphabetic())
                    .expect("a finished escape sequence");
                let (params, kind) = tail.split_at(end);
                if kind.starts_with('H') {
                    let col = params.split(';').nth(1).unwrap_or("1");
                    cursor = col.parse::<usize>().expect("a column") - 1;
                }
                rest = &tail[end + 1..];
                continue;
            }

            let ch = rest.chars().next().expect("a character");
            rest = &rest[ch.len_utf8()..];
            let width = if ch == misjudged {
                host_width
            } else {
                usize::from(crate::term::surface::char_width(ch))
            };
            for step in 0..width {
                if cursor + step < columns.len() {
                    columns[cursor + step] = Some(if step == 0 { ch } else { ' ' });
                }
            }
            cursor += width;
        }

        columns
    }

    /// Saying where the next column is cannot un-draw anything.
    ///
    /// A glyph this side counts as one column and the host draws across two has
    /// already covered the column beside it. When that column changed too it
    /// gets painted anyway and the damage repairs itself — which is why a
    /// screen being filled in looks fine. When it did not change, nothing
    /// repaints it and the record says it is already right, so it stays wrong:
    /// a Nerd Font icon appearing in a nested weave's status bar beside text
    /// that was already there.
    #[test]
    fn a_wider_glyph_does_not_clobber_a_column_the_frame_left_alone() {
        let plain = |ch| Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());
        let mut front = Surface::new(8, 1);
        for (x, ch) in " cde".chars().enumerate() {
            front.set(u16::try_from(x).expect("a small column"), 0, plain(ch));
        }
        let mut back = front.clone();
        // `✔`: one column to weave's table, two to a host that gives it emoji
        // presentation. Only column 0 changes.
        back.set(0, 0, plain('\u{2714}'));
        assert_eq!(
            crate::term::surface::char_width('\u{2714}'),
            1,
            "this test needs a glyph weave counts as one column"
        );

        let mut out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");
        let text = String::from_utf8(out).expect("output is utf-8");

        let columns = columns_over(" cde", &text, '\u{2714}', 2);
        assert_eq!(columns[0], Some('\u{2714}'));
        assert_eq!(
            columns[1],
            Some('c'),
            "the glyph ate a column the diff called clean: {text:?}"
        );
    }

    /// The disagreement in the other direction, on a screen with no record.
    ///
    /// A repaint is sent to a terminal nobody knows anything about — freshly
    /// attached, just resized — and it has to leave every column showing what
    /// the surface says. A host that draws a wide character in one column
    /// never receives anything for the second, so whatever it was showing
    /// before stays on screen.
    #[test]
    fn a_repaint_leaves_nothing_in_the_column_a_wide_glyph_may_not_cover() {
        let plain = |ch| Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());
        let mut back = Surface::new(8, 1);
        // `界` is two columns here; the host in this test draws it in one.
        back.set_char(0, 0, plain('\u{754c}'), 8);
        back.set(2, 0, plain('o'));
        back.set(3, 0, plain('k'));

        let mut out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .repaint(&back, &mut out)
            .expect("repaint should succeed");
        let text = String::from_utf8(out).expect("output is utf-8");

        // Stale content from before the attach, in the column the glyph covers
        // on this side but not on the host.
        let columns = columns_over("XXXXXXXX", &text, '\u{754c}', 1);
        assert_eq!(columns[0], Some('\u{754c}'));
        assert_eq!(
            columns[1],
            Some(' '),
            "a column the repaint never wrote kept what was there: {text:?}"
        );
        assert_eq!(columns[2], Some('o'));
        assert_eq!(columns[3], Some('k'));
    }

    /// The same repaint on a host that agrees the glyph is two columns wide.
    /// The blank written into the covered column goes down before the glyph,
    /// so the glyph's own right half lands on top of it.
    #[test]
    fn a_repaint_still_draws_a_wide_glyph_whole_where_the_host_agrees() {
        let plain = |ch| Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());
        let mut back = Surface::new(8, 1);
        back.set_char(0, 0, plain('\u{754c}'), 8);
        back.set(2, 0, plain('o'));
        back.set(3, 0, plain('k'));

        let mut out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .repaint(&back, &mut out)
            .expect("repaint should succeed");
        let text = String::from_utf8(out).expect("output is utf-8");

        let columns = columns_over("XXXXXXXX", &text, '\u{754c}', 2);
        assert_eq!(columns[0], Some('\u{754c}'));
        assert_eq!(columns[1], Some(' '), "the glyph's own second column");
        assert_eq!(columns[2], Some('o'), "the row walked sideways");
        assert_eq!(columns[3], Some('k'), "the row walked sideways");
    }

    /// The bug this is here for: one glyph the host measures differently used
    /// to take the whole rest of the row with it, and the record of what that
    /// terminal is showing said the row was fine — so it stayed wrong until
    /// something else happened to repaint those columns.
    #[test]
    fn a_glyph_the_terminal_measures_differently_only_costs_its_own_column() {
        let front = Surface::new(8, 1);
        let mut back = Surface::new(8, 1);
        let plain = |ch| Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());

        // `☰`: one column to the emulator behind the pane, two to kitty.
        for (x, ch) in "ab\u{2630}cde".chars().enumerate() {
            back.set(u16::try_from(x).expect("a small column"), 0, plain(ch));
        }

        let mut out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");
        let text = String::from_utf8(out).expect("output is utf-8");

        let columns = columns_after(&text, '\u{2630}', 2);
        assert_eq!(columns[0], Some('a'));
        assert_eq!(columns[1], Some('b'));
        assert_eq!(columns[2], Some('\u{2630}'));
        // Column 3 is the one the glyph spilled into on this terminal. Every
        // column after it still lands where the surface says it does.
        assert_eq!(columns[3], Some('c'), "the row walked sideways");
        assert_eq!(columns[4], Some('d'), "the row walked sideways");
        assert_eq!(columns[5], Some('e'), "the row walked sideways");
    }

    /// The same disagreement in the other direction: a mark the terminal hangs
    /// on the glyph before it, moving the cursor nowhere at all.
    #[test]
    fn a_zero_width_character_is_not_sent_as_a_column() {
        let front = Surface::new(8, 1);
        let mut back = Surface::new(8, 1);
        let plain = |ch| Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());

        // A combining acute with nothing in front of it to join.
        for (x, ch) in "ab\u{0301}cd".chars().enumerate() {
            back.set(u16::try_from(x).expect("a small column"), 0, plain(ch));
        }

        let mut out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");
        let text = String::from_utf8(out).expect("output is utf-8");

        assert!(
            !text.contains('\u{0301}'),
            "a mark with no glyph to join went out as a column: {text:?}"
        );

        let columns = columns_after(&text, '\u{0301}', 0);
        assert_eq!(columns[0], Some('a'));
        assert_eq!(columns[1], Some('b'));
        assert_eq!(columns[2], Some(' '), "the column it had is left blank");
        assert_eq!(columns[3], Some('c'), "the row walked sideways");
        assert_eq!(columns[4], Some('d'), "the row walked sideways");
    }

    /// After a resize the surfaces are new grids of a new shape. A `front` of
    /// the old shape describes no column of this screen, so cells that compare
    /// equal by index must not be taken for cells already showing the right
    /// thing — every one of them gets painted.
    #[test]
    fn flush_repaints_everything_when_the_front_is_a_different_shape() {
        let front = Surface::new(4, 2);
        let mut back = Surface::new(2, 2);
        let mut out = Vec::new();

        for (index, ch) in "abcd".chars().enumerate() {
            back.cells[index] = Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());
        }
        // Equal to the blank `front` holds at this index, and painted anyway.
        back.cells[3] = Cell::default();

        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");

        let text = String::from_utf8(out).expect("output is utf-8");
        assert!(text.contains("abc "), "screen not painted in full: {text:?}");
    }

    /// The grown half of a resized screen is past the end of the old front, and
    /// a diff that stops there leaves it holding whatever was there before.
    #[test]
    fn flush_paints_past_the_end_of_a_smaller_front() {
        let front = Surface::new(2, 1);
        let mut back = Surface::new(4, 1);
        let mut out = Vec::new();

        back.cells[3] = Cell::new('z', Color::Reset, Color::Reset, CellAttrs::empty());

        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut out)
            .expect("flush should succeed");

        let text = String::from_utf8(out).expect("output is utf-8");
        assert!(text.contains('z'), "the new columns went unpainted: {text:?}");
    }

    /// A repaint is for a screen nobody has a record of, and a blank cell is
    /// part of that screen. Leaving it out is what let `ESC[2J` decide its
    /// colour — and the clear paints in whatever background the last frame set.
    #[test]
    fn repaint_paints_the_blank_cells_too() {
        let mut back = Surface::new(4, 1);
        let mut out = Vec::new();

        back.set(0, 0, Cell::new('a', Color::Reset, Color::Reset, CellAttrs::empty()));

        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .repaint(&back, &mut out)
            .expect("repaint should succeed");

        let text = String::from_utf8(out).expect("output is utf-8");
        assert!(text.contains("a   "), "the blank columns went unpainted: {text:?}");
    }

    /// And it starts from nothing known, so it does not skip cells that happen
    /// to match the last frame.
    #[test]
    fn repaint_ignores_what_the_screen_was_showing() {
        let mut back = Surface::new(3, 1);
        let mut out = Vec::new();

        for (index, ch) in "xyz".chars().enumerate() {
            back.cells[index] = Cell::new(ch, Color::Reset, Color::Reset, CellAttrs::empty());
        }

        let mut renderer = DiffRenderer::with_color_mode(ColorMode::Truecolor);
        renderer
            .flush(&back.clone(), &back, &mut out)
            .expect("flush should succeed");
        assert!(out.is_empty(), "an unchanged frame paints nothing: {out:?}");

        renderer
            .repaint(&back, &mut out)
            .expect("repaint should succeed");
        let text = String::from_utf8(out).expect("output is utf-8");
        assert!(text.contains("xyz"), "the row went unpainted: {text:?}");
    }

    #[test]
    fn quantizes_rgb_to_nearest_xterm256_color() {
        assert_eq!(quantize_rgb_to_xterm256(0, 0, 0), 16);
        assert_eq!(quantize_rgb_to_xterm256(255, 255, 255), 231);
        assert_eq!(quantize_rgb_to_xterm256(255, 0, 0), 196);
        assert_eq!(quantize_rgb_to_xterm256(0, 0, 255), 21);
        assert_eq!(quantize_rgb_to_xterm256(128, 128, 128), 244);
    }

    #[test]
    fn quantized_flush_emits_no_truecolor_escape_sequences() {
        let front = Surface::new(2, 1);
        let mut back = Surface::new(2, 1);
        back.set(
            0,
            0,
            Cell::new(
                'a',
                Color::Rgb { r: 1, g: 2, b: 3 },
                Color::Rgb {
                    r: 200,
                    g: 210,
                    b: 220,
                },
                CellAttrs::empty(),
            ),
        );
        back.set(
            1,
            0,
            Cell::new(
                'b',
                Color::Rgb { r: 255, g: 0, b: 0 },
                Color::Reset,
                CellAttrs::empty(),
            ),
        );

        let mut truecolor_out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Truecolor)
            .flush(&front, &back, &mut truecolor_out)
            .expect("truecolor flush should succeed");

        let mut quantized_out = Vec::new();
        DiffRenderer::with_color_mode(ColorMode::Quantized)
            .flush(&front, &back, &mut quantized_out)
            .expect("quantized flush should succeed");

        assert!(!contains_bytes(&quantized_out, b"38;2;"));
        assert!(!contains_bytes(&quantized_out, b"48;2;"));
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}

