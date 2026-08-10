//! Diff front vs back, flush.

use std::io::{self, Write};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::{
    Attribute, Attributes, Color, Print, SetAttribute, SetAttributes, SetBackgroundColor,
    SetForegroundColor,
};

use crate::term::cell::{Cell, CellAttrs};
use crate::term::surface::Surface;

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
        self.queue.clear();

        if back.width == 0 {
            out.write_all(&self.queue)?;
            return Ok(());
        }

        let mut current_style = None;
        let mut index = 0;
        let len = front.cells.len().min(back.cells.len());

        while index < len {
            if front.cells[index] == back.cells[index] {
                index += 1;
                continue;
            }

            let x = u16::try_from(index % usize::from(back.width)).unwrap_or(u16::MAX);
            let y = u16::try_from(index / usize::from(back.width)).unwrap_or(u16::MAX);
            queue!(self.queue, MoveTo(x, y))?;

            while index < len && front.cells[index] != back.cells[index] {
                let style = Style::from_cell(back.cells[index], self.color_mode);
                emit_style(&mut self.queue, &mut current_style, style)?;

                let mut run = String::new();
                while index < len
                    && front.cells[index] != back.cells[index]
                    && Style::from_cell(back.cells[index], self.color_mode) == style
                {
                    run.push(back.cells[index].ch);
                    index += 1;
                }

                queue!(self.queue, Print(run))?;
            }
        }

        out.write_all(&self.queue)
    }
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
