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

        while index < len {
            if unchanged(index) {
                index += 1;
                continue;
            }

            let x = u16::try_from(index % usize::from(back.width)).unwrap_or(u16::MAX);
            let y = u16::try_from(index / usize::from(back.width)).unwrap_or(u16::MAX);
            queue!(self.queue, MoveTo(x, y))?;

            while index < len && !unchanged(index) {
                let style = Style::from_cell(back.cells[index], self.color_mode);
                emit_style(&mut self.queue, &mut current_style, style)?;

                let mut run = String::new();
                while index < len
                    && !unchanged(index)
                    && Style::from_cell(back.cells[index], self.color_mode) == style
                {
                    let cell = back.cells[index];
                    if cell.is_continuation() {
                        // The right half of a wide character: printing the
                        // glyph in front of it already moved the terminal's
                        // cursor across this column, so anything printed here
                        // would land a column further right than the surface
                        // says, and take the rest of the row with it. A stray
                        // continuation with no glyph in front of it — which
                        // the surface tries not to keep — still needs the
                        // column painted, so it gets a blank.
                        if !covered_by_wide(back, index) {
                            run.push(' ');
                        }
                        index += 1;
                        continue;
                    }

                    run.push(cell.ch);
                    index += 1;
                }

                queue!(self.queue, Print(run))?;
            }
        }

        out.write_all(&self.queue)
    }
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
    /// of where the surface says it is.
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
        assert!(text.contains("\u{754c}ok"), "wide run not printed once: {text:?}");
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
