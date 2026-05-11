//! Fractional edge rendering + alpha blend.

use crossterm::style::Color;

use crate::layout::geometry::FRect;
use crate::term::cell::{Cell, CellAttrs};
use crate::term::surface::Surface;

const EPSILON: f32 = 0.000_1;

#[derive(Copy, Clone)]
struct EdgeStyle {
    glyph: char,
    fg: Color,
    bg: Color,
    alpha: f32,
}

#[derive(Copy, Clone)]
struct EdgeBounds {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[derive(Copy, Clone)]
struct EdgeFractions {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

pub fn draw_edges(surface: &mut Surface, frect: FRect, fg: Color, bg: Color) {
    let Some(bounds) = edge_bounds(frect) else {
        return;
    };
    let fractions = edge_fractions(frect);

    draw_side_edges(surface, bounds, fractions, fg, bg);
    draw_corner_edges(surface, bounds, fractions, fg, bg);
}

fn edge_bounds(frect: FRect) -> Option<EdgeBounds> {
    if frect.w <= 0.0 || frect.h <= 0.0 {
        return None;
    }

    let left = floor_to_i32(frect.x);
    let top = floor_to_i32(frect.y);
    let right_edge = frect.x + frect.w;
    let bottom_edge = frect.y + frect.h;
    let right = ceil_to_i32(right_edge) - 1;
    let bottom = ceil_to_i32(bottom_edge) - 1;

    if right < left || bottom < top {
        None
    } else {
        Some(EdgeBounds {
            left,
            top,
            right,
            bottom,
        })
    }
}

fn edge_fractions(frect: FRect) -> EdgeFractions {
    EdgeFractions {
        left: fraction(frect.x),
        right: fraction(frect.x + frect.w),
        top: fraction(frect.y),
        bottom: fraction(frect.y + frect.h),
    }
}

fn draw_side_edges(
    surface: &mut Surface,
    bounds: EdgeBounds,
    fractions: EdgeFractions,
    fg: Color,
    bg: Color,
) {
    if is_fractional(fractions.left) {
        paint_vertical(
            surface,
            bounds.left,
            bounds.top,
            bounds.bottom,
            EdgeStyle {
                glyph: '▐',
                fg,
                bg,
                alpha: 1.0 - fractions.left,
            },
        );
    }
    if is_fractional(fractions.right) {
        paint_vertical(
            surface,
            bounds.right,
            bounds.top,
            bounds.bottom,
            EdgeStyle {
                glyph: '▌',
                fg,
                bg,
                alpha: fractions.right,
            },
        );
    }
    if is_fractional(fractions.top) {
        paint_horizontal(
            surface,
            bounds.top,
            bounds.left,
            bounds.right,
            EdgeStyle {
                glyph: '▄',
                fg,
                bg,
                alpha: 1.0 - fractions.top,
            },
        );
    }
    if is_fractional(fractions.bottom) {
        paint_horizontal(
            surface,
            bounds.bottom,
            bounds.left,
            bounds.right,
            EdgeStyle {
                glyph: '▀',
                fg,
                bg,
                alpha: fractions.bottom,
            },
        );
    }
}

fn draw_corner_edges(
    surface: &mut Surface,
    bounds: EdgeBounds,
    fractions: EdgeFractions,
    fg: Color,
    bg: Color,
) {
    let left_alpha = 1.0 - fractions.left;
    let top_alpha = 1.0 - fractions.top;

    if is_fractional(fractions.left) && is_fractional(fractions.top) {
        paint_cell(
            surface,
            bounds.left,
            bounds.top,
            EdgeStyle {
                glyph: '▗',
                fg,
                bg,
                alpha: left_alpha * top_alpha,
            },
        );
    }
    if is_fractional(fractions.right) && is_fractional(fractions.top) {
        paint_cell(
            surface,
            bounds.right,
            bounds.top,
            EdgeStyle {
                glyph: '▖',
                fg,
                bg,
                alpha: fractions.right * top_alpha,
            },
        );
    }
    if is_fractional(fractions.left) && is_fractional(fractions.bottom) {
        paint_cell(
            surface,
            bounds.left,
            bounds.bottom,
            EdgeStyle {
                glyph: '▝',
                fg,
                bg,
                alpha: left_alpha * fractions.bottom,
            },
        );
    }
    if is_fractional(fractions.right) && is_fractional(fractions.bottom) {
        paint_cell(
            surface,
            bounds.right,
            bounds.bottom,
            EdgeStyle {
                glyph: '▘',
                fg,
                bg,
                alpha: fractions.right * fractions.bottom,
            },
        );
    }
}

/// Blends two colors by alpha using sRGB transfer functions and returns an RGB
/// color. This keeps v1 color handling small while making 50% black/white
/// blends perceptually closer to mid-grey than raw byte interpolation.
pub fn blend(fg: Color, bg: Color, alpha: f32) -> Color {
    let alpha = normalize_alpha(alpha);
    let (fg_r, fg_g, fg_b) = color_to_rgb(fg);
    let (bg_r, bg_g, bg_b) = color_to_rgb(bg);

    Color::Rgb {
        r: blend_channel(fg_r, bg_r, alpha),
        g: blend_channel(fg_g, bg_g, alpha),
        b: blend_channel(fg_b, bg_b, alpha),
    }
}

fn paint_vertical(surface: &mut Surface, x: i32, top: i32, bottom: i32, style: EdgeStyle) {
    for y in top..=bottom {
        paint_cell(surface, x, y, style);
    }
}

fn paint_horizontal(surface: &mut Surface, y: i32, left: i32, right: i32, style: EdgeStyle) {
    for x in left..=right {
        paint_cell(surface, x, y, style);
    }
}

fn paint_cell(surface: &mut Surface, x: i32, y: i32, style: EdgeStyle) {
    let (Ok(x), Ok(y)) = (u16::try_from(x), u16::try_from(y)) else {
        return;
    };
    surface.set(
        x,
        y,
        Cell::new(
            style.glyph,
            blend(style.bg, style.fg, style.alpha),
            style.bg,
            CellAttrs::empty(),
        ),
    );
}

fn fraction(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }

    let fraction = value - value.floor();
    if fraction <= EPSILON || (1.0 - fraction) <= EPSILON {
        0.0
    } else {
        fraction
    }
}

fn is_fractional(value: f32) -> bool {
    value > EPSILON
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn floor_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }

    value.floor().clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn ceil_to_i32(value: f32) -> i32 {
    if !value.is_finite() {
        return 0;
    }

    value.ceil().clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

fn normalize_alpha(alpha: f32) -> f32 {
    if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn color_to_rgb(color: Color) -> (u8, u8, u8) {
    match color {
        Color::Reset | Color::Black => (0, 0, 0),
        Color::DarkGrey => (128, 128, 128),
        Color::Red => (255, 0, 0),
        Color::DarkRed => (128, 0, 0),
        Color::Green => (0, 255, 0),
        Color::DarkGreen => (0, 128, 0),
        Color::Yellow => (255, 255, 0),
        Color::DarkYellow => (128, 128, 0),
        Color::Blue => (0, 0, 255),
        Color::DarkBlue => (0, 0, 128),
        Color::Magenta => (255, 0, 255),
        Color::DarkMagenta => (128, 0, 128),
        Color::Cyan => (0, 255, 255),
        Color::DarkCyan => (0, 128, 128),
        Color::White => (255, 255, 255),
        Color::Grey => (192, 192, 192),
        Color::Rgb { r, g, b } => (r, g, b),
        Color::AnsiValue(value) => ansi_to_rgb(value),
    }
}

fn ansi_to_rgb(value: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];

    match value {
        0..=15 => BASIC[usize::from(value)],
        16..=231 => {
            let value = value - 16;
            let r = value / 36;
            let g = (value % 36) / 6;
            let b = value % 6;
            (
                ansi_cube_channel(r),
                ansi_cube_channel(g),
                ansi_cube_channel(b),
            )
        }
        232..=255 => {
            let level = 8 + ((value - 232) * 10);
            (level, level, level)
        }
    }
}

fn ansi_cube_channel(value: u8) -> u8 {
    if value == 0 {
        0
    } else {
        55 + (value * 40)
    }
}

fn blend_channel(fg: u8, bg: u8, alpha: f32) -> u8 {
    let fg = srgb_to_linear(f32::from(fg) / 255.0);
    let bg = srgb_to_linear(f32::from(bg) / 255.0);
    let linear = fg + ((bg - fg) * alpha);
    channel_from_unit(linear_to_srgb(linear))
}

fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f32) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn channel_from_unit(value: f32) -> u8 {
    (value * 255.0).round().clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::{blend, draw_edges};
    use crate::layout::geometry::FRect;
    use crate::term::surface::Surface;
    use crossterm::style::Color;

    const FG: Color = Color::White;
    const BG: Color = Color::Black;

    #[test]
    fn blend_endpoints_return_inputs() {
        assert_eq!(
            blend(Color::Black, Color::White, 0.0),
            Color::Rgb { r: 0, g: 0, b: 0 }
        );
        assert_eq!(
            blend(Color::Black, Color::White, 1.0),
            Color::Rgb {
                r: 255,
                g: 255,
                b: 255,
            }
        );
    }

    #[test]
    fn blend_half_black_white_is_mid_grey_in_srgb_transfer_space() {
        assert_eq!(
            blend(Color::Black, Color::White, 0.5),
            Color::Rgb {
                r: 188,
                g: 188,
                b: 188,
            }
        );
    }

    #[test]
    fn integer_aligned_rect_draws_no_edges() {
        let mut surface = Surface::new(4, 4);

        draw_edges(
            &mut surface,
            FRect {
                x: 1.0,
                y: 1.0,
                w: 2.0,
                h: 2.0,
            },
            FG,
            BG,
        );

        assert!(surface.cells.iter().all(|cell| cell.ch == ' '));
    }

    #[test]
    fn left_fractional_offset_draws_right_half_with_coverage_alpha() {
        let mut surface = Surface::new(5, 4);

        draw_edges(
            &mut surface,
            FRect {
                x: 1.25,
                y: 1.0,
                w: 2.75,
                h: 2.0,
            },
            FG,
            BG,
        );

        let cell = surface.get(1, 1).expect("cell exists");
        assert_eq!(cell.ch, '▐');
        assert_eq!(cell.fg, blend(BG, FG, 0.75));
    }

    #[test]
    fn half_fractional_offset_draws_half_brightness_edge() {
        let mut surface = Surface::new(5, 4);

        draw_edges(
            &mut surface,
            FRect {
                x: 1.5,
                y: 1.0,
                w: 2.5,
                h: 2.0,
            },
            FG,
            BG,
        );

        let cell = surface.get(1, 1).expect("cell exists");
        assert_eq!(cell.ch, '▐');
        assert_eq!(
            cell.fg,
            Color::Rgb {
                r: 188,
                g: 188,
                b: 188
            }
        );
    }

    #[test]
    fn three_quarter_fractional_offset_draws_dim_right_half() {
        let mut surface = Surface::new(5, 4);

        draw_edges(
            &mut surface,
            FRect {
                x: 1.75,
                y: 1.0,
                w: 2.25,
                h: 2.0,
            },
            FG,
            BG,
        );

        let cell = surface.get(1, 1).expect("cell exists");
        assert_eq!(cell.ch, '▐');
        assert_eq!(cell.fg, blend(BG, FG, 0.25));
    }

    #[test]
    fn right_top_bottom_and_corner_glyphs_match_covered_sides() {
        let mut surface = Surface::new(5, 5);

        draw_edges(
            &mut surface,
            FRect {
                x: 1.25,
                y: 1.25,
                w: 2.5,
                h: 2.5,
            },
            FG,
            BG,
        );

        assert_eq!(surface.get(1, 2).expect("left edge").ch, '▐');
        assert_eq!(surface.get(3, 2).expect("right edge").ch, '▌');
        assert_eq!(surface.get(2, 1).expect("top edge").ch, '▄');
        assert_eq!(surface.get(2, 3).expect("bottom edge").ch, '▀');
        assert_eq!(surface.get(1, 1).expect("top-left corner").ch, '▗');
        assert_eq!(surface.get(3, 1).expect("top-right corner").ch, '▖');
        assert_eq!(surface.get(1, 3).expect("bottom-left corner").ch, '▝');
        assert_eq!(surface.get(3, 3).expect("bottom-right corner").ch, '▘');
    }
}
