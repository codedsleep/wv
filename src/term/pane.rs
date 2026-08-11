//! `Pane` wrapping vt100 parser.

use crossterm::style::Color as TermColor;

use crate::backend::PaneId;
use crate::layout::geometry::Rect;
use crate::term::cell::{Cell, CellAttrs};
use crate::term::query::{self, QueryScanner, Segment};
use crate::term::surface::Surface;

pub struct Pane {
    id: PaneId,
    parser: vt100::Parser,
    scanner: QueryScanner,
    dirty: bool,
}

impl Pane {
    pub fn new(id: PaneId, cols: u16, rows: u16) -> Self {
        Self {
            id,
            parser: vt100::Parser::new(rows, cols, 0),
            scanner: QueryScanner::default(),
            dirty: true,
        }
    }

    /// Feed pane output to the emulator, returning any bytes owed back to it.
    ///
    /// Programs block on terminal queries — a shell holds its prompt until its
    /// DA1 probe is answered — so the caller must write the returned bytes to
    /// the pane's PTY.
    pub fn process(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut replies = Vec::new();

        for segment in self.scanner.feed(bytes) {
            match segment {
                Segment::Data(data) => self.parser.process(&data),
                Segment::Query(kind) => {
                    // Answered against the cursor as of this point in the
                    // stream, which is what the program asked about.
                    let (row, col) = self.parser.screen().cursor_position();
                    replies.extend_from_slice(&query::reply(kind, row, col));
                }
            }
        }

        self.dirty = true;

        replies
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn title(&self) -> Option<&str> {
        let title = self.screen().title();
        if title.is_empty() {
            None
        } else {
            Some(title)
        }
    }

    /// Draw the pane's grid into `surface` at `rect`, clipped to it.
    ///
    /// The clip is what keeps a pane whose emulator grid is still the old size
    /// — mid-tween, before the resize lands — from spilling over its
    /// neighbours. Compared with rendering into a scratch surface and blitting,
    /// this saves an allocation and a full copy of the grid on every frame.
    pub fn blit_into(&self, surface: &mut Surface, rect: Rect) {
        let (rows, cols) = self.screen().size();
        let screen = self.screen();
        let width = cols.min(rect.w);
        let height = rows.min(rect.h);

        for row in 0..height {
            let Some(y) = rect.y.checked_add(row) else {
                continue;
            };

            for col in 0..width {
                let Some(x) = rect.x.checked_add(col) else {
                    continue;
                };

                if let Some(cell) = screen.cell(row, col) {
                    surface.set(x, y, map_cell(cell));
                }
            }
        }
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

    /// The visible screen as lines of plain text.
    ///
    /// Trailing blank lines are dropped, so capturing a pane running one
    /// command returns that command's output rather than it plus twenty empty
    /// rows. There is no scrollback, so this is the whole of what can be read.
    pub fn capture_lines(&self) -> Vec<String> {
        let (_, cols) = self.screen().size();
        let mut lines: Vec<String> = self
            .screen()
            .rows(0, cols)
            .map(|row| row.trim_end().to_owned())
            .collect();

        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }

        lines
    }

    /// The emulator grid's `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.screen().size();
        (cols, rows)
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
    // `vt100::Cell::contents` allocates a `String` on every call, blank cells
    // included. Most of a screen is blank, and this runs once per cell per
    // frame, so the empty case has to stay off the allocator.
    let ch = if cell.has_contents() {
        cell.contents().chars().next().unwrap_or(' ')
    } else {
        ' '
    };

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

    /// btop positions with HVP and never with CUP. Losing those moves makes it
    /// paint its whole interface from wherever the cursor sat, so the boxes
    /// pile up on one another and wrap.
    #[test]
    fn hvp_positions_the_cursor_the_same_as_cup() {
        let mut hvp = Pane::new(PaneId(1), 12, 4);
        let mut cup = Pane::new(PaneId(2), 12, 4);

        hvp.process(b"\x1b[3;5fX");
        cup.process(b"\x1b[3;5HX");

        assert_eq!(hvp.screen().cursor_position(), (2, 5));
        assert_eq!(
            hvp.screen().cell(2, 4).expect("cell exists").contents(),
            "X"
        );
        assert_eq!(
            hvp.screen().contents_formatted(),
            cup.screen().contents_formatted()
        );
    }

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

    /// A shell holds its prompt until its DA1 probe is answered, so the pane
    /// must hand the reply back rather than swallowing the query.
    #[test]
    fn process_answers_a_device_attributes_query() {
        let mut pane = Pane::new(PaneId(1), 80, 24);

        assert_eq!(pane.process(b"hello"), Vec::<u8>::new());
        assert_eq!(pane.process(b"\x1b[0c"), b"\x1b[?1;2c".to_vec());
    }

    #[test]
    fn process_answers_a_cursor_position_query_with_the_live_cursor() {
        let mut pane = Pane::new(PaneId(1), 80, 24);

        // Three columns of text, then ask where the cursor ended up.
        assert_eq!(pane.process(b"abc\x1b[6n"), b"\x1b[1;4R".to_vec());
    }

    #[test]
    fn title_returns_osc_title_when_set() {
        let mut pane = Pane::new(PaneId(1), 80, 24);

        assert_eq!(pane.title(), None);

        pane.process(b"\x1b]2;hello\x07");

        assert_eq!(pane.title(), Some("hello"));
    }
}
