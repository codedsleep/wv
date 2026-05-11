//! `Pane` wrapping vt100 parser.

use crossterm::style::Color as TermColor;

use crate::backend::PaneId;
use crate::term::cell::{Cell, CellAttrs};
use crate::term::surface::Surface;

pub struct Pane {
    id: PaneId,
    parser: vt100::Parser,
    dirty: bool,
}

impl Pane {
    pub fn new(id: PaneId, cols: u16, rows: u16) -> Self {
        Self {
            id,
            parser: vt100::Parser::new(rows, cols, 0),
            dirty: true,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
        self.dirty = true;
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn cells_into(&self, surface: &mut Surface, dst_x: u16, dst_y: u16) {
        let (rows, cols) = self.screen().size();

        for row in 0..rows {
            let Some(y) = dst_y.checked_add(row) else {
                continue;
            };

            for col in 0..cols {
                let Some(x) = dst_x.checked_add(col) else {
                    continue;
                };

                if let Some(cell) = self.screen().cell(row, col) {
                    surface.set(x, y, map_cell(cell));
                }
            }
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(rows, cols);
        self.dirty = true;
    }

    pub const fn id(&self) -> PaneId {
        self.id
    }

    pub const fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }
}

fn map_cell(cell: &vt100::Cell) -> Cell {
    let ch = cell.contents().chars().next().unwrap_or(' ');

    Cell::new(
        ch,
        map_color(cell.fgcolor()),
        map_color(cell.bgcolor()),
        map_attrs(cell),
    )
}

fn map_color(color: vt100::Color) -> TermColor {
    match color {
        vt100::Color::Default => TermColor::Reset,
        vt100::Color::Idx(index) => TermColor::AnsiValue(index),
        vt100::Color::Rgb(r, g, b) => TermColor::Rgb { r, g, b },
    }
}

fn map_attrs(cell: &vt100::Cell) -> CellAttrs {
    let mut attrs = CellAttrs::empty();

    attrs.set(CellAttrs::BOLD, cell.bold());
    attrs.set(CellAttrs::ITALIC, cell.italic());
    attrs.set(CellAttrs::UNDERLINE, cell.underline());
    attrs.set(CellAttrs::REVERSE, cell.inverse());

    attrs
}

#[cfg(test)]
mod tests {
    use crossterm::style::Color;

    use super::Pane;
    use crate::backend::PaneId;
    use crate::term::surface::Surface;

    #[test]
    fn cells_into_maps_text_and_ansi_red() {
        let mut pane = Pane::new(PaneId(1), 80, 24);
        let mut surface = Surface::new(80, 24);

        pane.process(b"hello\x1b[31mworld");
        pane.cells_into(&mut surface, 0, 0);

        for (x, ch) in "hello".chars().enumerate() {
            let cell = surface.get(u16::try_from(x).expect("test index fits u16"), 0);
            let cell = cell.expect("cell should be in surface");
            assert_eq!(cell.ch, ch);
            assert_eq!(cell.fg, Color::Reset);
        }

        for (offset, ch) in "world".chars().enumerate() {
            let x = u16::try_from(offset + 5).expect("test index fits u16");
            let cell = surface.get(x, 0).expect("cell should be in surface");
            assert_eq!(cell.ch, ch);
            assert_eq!(cell.fg, Color::AnsiValue(1));
        }
    }
}
