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
    const fn from_cell(cell: Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            attrs: cell.attrs,
        }
    }
}

pub struct DiffRenderer {
    queue: Vec<u8>,
}

impl DiffRenderer {
    pub const fn new() -> Self {
        Self { queue: Vec::new() }
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
                let style = Style::from_cell(back.cells[index]);
                emit_style(&mut self.queue, &mut current_style, style)?;

                let mut run = String::new();
                while index < len
                    && front.cells[index] != back.cells[index]
                    && Style::from_cell(back.cells[index]) == style
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
        queue!(queue_buf, SetAttributes(to_crossterm_attrs(next_style.attrs)))?;
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

    use super::DiffRenderer;
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
}
