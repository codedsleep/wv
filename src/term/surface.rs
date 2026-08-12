//! `Surface` cell buffer + blit.

use unicode_width::UnicodeWidthChar;

use crate::term::cell::Cell;

/// How many columns a character takes up, as the renderer counts them.
///
/// Anything the tables call zero-width counts as one: a surface cell holds one
/// character, so a combining mark that should have joined its neighbour gets a
/// cell of its own rather than disappearing.
pub fn char_width(ch: char) -> u16 {
    if ch.is_ascii() {
        return 1;
    }

    u16::try_from(UnicodeWidthChar::width(ch).unwrap_or(1).max(1)).unwrap_or(1)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Surface {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>,
}

impl Surface {
    pub fn new(width: u16, height: u16) -> Self {
        let len = usize::from(width) * usize::from(height);

        Self {
            width,
            height,
            cells: vec![Cell::default(); len],
        }
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::default());
    }

    pub fn set(&mut self, x: u16, y: u16, cell: Cell) {
        let Some(index) = self.index(x, y) else {
            return;
        };

        // Half of a wide character is being written over, so the other half is
        // now stranded — a continuation with nothing in front of it, or a
        // glyph whose second column belongs to something else. Either one
        // walks the rest of the row sideways, so the orphan goes blank as the
        // new cell lands. Borders and status text are drawn over pane content,
        // which is where this happens.
        let old = self.cells[index];
        if old.is_continuation() && !cell.is_continuation() && x > 0 {
            self.cells[index - 1].ch = ' ';
        } else if char_width(old.ch) == 2 && char_width(cell.ch) != 2 && x + 1 < self.width {
            let right = index + 1;
            if self.cells[right].is_continuation() {
                self.cells[right].ch = ' ';
            }
        }

        self.cells[index] = cell;
    }

    /// Write `cell` at `(x, y)` along with the continuation a wide character
    /// needs, and return how many columns it took.
    ///
    /// `limit` is the first column the caller may not draw into — the end of a
    /// status-bar run, or of the surface. A wide character with a single column
    /// left in front of it is written as a blank instead: half a glyph is worse
    /// than none, and drawing it would push the row across by a column.
    pub fn set_char(&mut self, x: u16, y: u16, cell: Cell, limit: u16) -> u16 {
        let limit = limit.min(self.width);
        if x >= limit {
            return 0;
        }

        if char_width(cell.ch) != 2 {
            self.set(x, y, cell);
            return 1;
        }

        if x + 1 >= limit {
            self.set(x, y, Cell { ch: ' ', ..cell });
            return 1;
        }

        self.set(x, y, cell);
        self.set(x + 1, y, Cell::continuation(cell));
        2
    }

    pub fn get(&self, x: u16, y: u16) -> Option<&Cell> {
        self.index(x, y).map(|index| &self.cells[index])
    }

    pub fn blit(&mut self, src: &Self, dst_x: u16, dst_y: u16) {
        let copy_width = src.width.min(self.width.saturating_sub(dst_x));
        let copy_height = src.height.min(self.height.saturating_sub(dst_y));

        for y in 0..copy_height {
            for x in 0..copy_width {
                let src_index = usize::from(y) * usize::from(src.width) + usize::from(x);
                let dst_index =
                    usize::from(dst_y + y) * usize::from(self.width) + usize::from(dst_x + x);

                self.cells[dst_index] = src.cells[src_index];
            }
        }
    }

    fn index(&self, x: u16, y: u16) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }

        Some(usize::from(y) * usize::from(self.width) + usize::from(x))
    }
}

#[cfg(test)]
mod tests {
    use crossterm::style::Color;

    use super::Surface;
    use crate::term::cell::{Cell, CellAttrs};

    fn marker(ch: char) -> Cell {
        Cell::new(ch, Color::Green, Color::Black, CellAttrs::BOLD)
    }

    #[test]
    fn get_set_roundtrip() {
        let mut surface = Surface::new(3, 2);
        let cell = Cell::new('x', Color::Red, Color::Blue, CellAttrs::UNDERLINE);

        surface.set(2, 1, cell);

        assert_eq!(surface.get(2, 1), Some(&cell));
        assert_eq!(surface.get(3, 1), None);
        assert_eq!(surface.get(2, 2), None);
    }

    #[test]
    fn clear_resets_every_cell() {
        let mut surface = Surface::new(4, 3);
        surface.set(0, 0, marker('a'));
        surface.set(3, 2, marker('b'));

        surface.clear();

        assert!(surface.cells.iter().all(|cell| *cell == Cell::default()));
    }

    /// A wide glyph is two columns and one character, so the surface has to
    /// hold the second column as a continuation. Without it the renderer prints
    /// a blank there, and the terminal — already past both columns — puts the
    /// rest of the row one cell to the right.
    #[test]
    fn set_char_reserves_the_second_column_of_a_wide_character() {
        let mut surface = Surface::new(4, 1);

        let step = surface.set_char(0, 0, marker('\u{754c}'), 4);

        assert_eq!(step, 2);
        assert_eq!(surface.get(0, 0).map(|cell| cell.ch), Some('\u{754c}'));
        assert_eq!(
            surface.get(1, 0).map(|cell| cell.ch),
            Some(Cell::CONTINUATION)
        );
        // The continuation carries the glyph's colours, so a run of them is one
        // style rather than two.
        assert_eq!(surface.get(1, 0).map(|cell| cell.bg), Some(Color::Black));
    }

    /// Half a glyph would be drawn over whatever is past the limit — a border,
    /// or a neighbouring pane — and shift the row across by a column.
    #[test]
    fn set_char_blanks_a_wide_character_with_one_column_left() {
        let mut surface = Surface::new(4, 1);

        let step = surface.set_char(3, 0, marker('\u{754c}'), 4);

        assert_eq!(step, 1);
        assert_eq!(surface.get(3, 0).map(|cell| cell.ch), Some(' '));
    }

    /// Borders and status text are drawn over pane content, so either half of
    /// a wide character can be overwritten. The half left behind has to go.
    #[test]
    fn writing_over_half_a_wide_character_blanks_the_other_half() {
        let mut over_the_left = Surface::new(4, 1);
        over_the_left.set_char(1, 0, marker('\u{754c}'), 4);
        over_the_left.set(1, 0, marker('x'));
        assert_eq!(over_the_left.get(2, 0).map(|cell| cell.ch), Some(' '));

        let mut over_the_right = Surface::new(4, 1);
        over_the_right.set_char(1, 0, marker('\u{754c}'), 4);
        over_the_right.set(2, 0, marker('x'));
        assert_eq!(over_the_right.get(1, 0).map(|cell| cell.ch), Some(' '));
    }

    #[test]
    fn blit_clips_at_edges() {
        let mut dst = Surface::new(3, 2);
        let mut src = Surface::new(3, 3);

        src.set(0, 0, marker('a'));
        src.set(1, 0, marker('b'));
        src.set(2, 0, marker('c'));
        src.set(0, 1, marker('d'));
        src.set(1, 1, marker('e'));
        src.set(2, 1, marker('f'));
        src.set(0, 2, marker('g'));
        src.set(1, 2, marker('h'));
        src.set(2, 2, marker('i'));

        dst.blit(&src, 1, 1);

        assert_eq!(dst.get(0, 0), Some(&Cell::default()));
        assert_eq!(dst.get(1, 0), Some(&Cell::default()));
        assert_eq!(dst.get(2, 0), Some(&Cell::default()));
        assert_eq!(dst.get(0, 1), Some(&Cell::default()));
        assert_eq!(dst.get(1, 1), Some(&marker('a')));
        assert_eq!(dst.get(2, 1), Some(&marker('b')));
    }
}
