//! `Surface` cell buffer + blit.

use crate::term::cell::Cell;

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
        if let Some(index) = self.index(x, y) {
            self.cells[index] = cell;
        }
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
