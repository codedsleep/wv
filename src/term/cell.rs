//! `Cell` + `CellAttrs` bitflags.

use crossterm::style::Color;

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CellAttrs: u8 {
        const BOLD = 0b0001;
        const ITALIC = 0b0010;
        const UNDERLINE = 0b0100;
        const REVERSE = 0b1000;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: CellAttrs,
}

impl Cell {
    /// What sits in the cell to the right of a double-width character.
    ///
    /// A wide glyph is one `char` over two columns, and the surface has a cell
    /// per column. The right one holds this instead of a character of its own,
    /// so that the renderer knows the terminal's cursor has already crossed it
    /// and prints nothing there. A blank would be printed, and every column
    /// after it on the row would land one to the right of where it belongs.
    pub const CONTINUATION: char = '\0';

    pub const fn new(ch: char, fg: Color, bg: Color, attrs: CellAttrs) -> Self {
        Self { ch, fg, bg, attrs }
    }

    /// The right half of the wide character in `cell`, styled to match it.
    pub const fn continuation(cell: Self) -> Self {
        Self {
            ch: Self::CONTINUATION,
            ..cell
        }
    }

    pub fn is_continuation(self) -> bool {
        self.ch == Self::CONTINUATION
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Color::Reset,
            bg: Color::Reset,
            attrs: CellAttrs::empty(),
        }
    }
}
