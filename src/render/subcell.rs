//! Fractional edge rendering + alpha blend.

use crossterm::style::Color;

use crate::layout::geometry::FRect;
use crate::term::surface::Surface;

pub fn draw_edges(_surface: &mut Surface, _frect: FRect, _fg: Color, _bg: Color) {
    // TODO(3.5): paint fractional pane edges with sub-cell glyphs and blended colors.
}
