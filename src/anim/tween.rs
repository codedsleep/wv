//! `Tween`, `Easing`, `Lerp`.

#![allow(dead_code)]

use std::time::Duration;

use crossterm::style::Color;

const BAKED_POINTS: usize = 255;
const INV_BAKED_POINTS: f32 = 1.0 / 255.0;

pub trait Lerp: Copy {
    fn lerp(&self, other: &Self, t: f32) -> Self;
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct FRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Easing {
    Linear,
    EaseOutCubic,
    EaseInOutCubic,
    EaseOutBack,
    EaseOutExpo,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Tween<T: Lerp> {
    pub from: T,
    pub to: T,
    pub elapsed: Duration,
    pub duration: Duration,
    pub easing: Easing,
}

impl Lerp for f32 {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        self + ((other - self) * t)
    }
}

impl Lerp for FRect {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        Self {
            x: self.x.lerp(&other.x, t),
            y: self.y.lerp(&other.y, t),
            w: self.w.lerp(&other.w, t),
            h: self.h.lerp(&other.h, t),
        }
    }
}

impl Lerp for Color {
    fn lerp(&self, other: &Self, t: f32) -> Self {
        if t <= 0.0 {
            return *self;
        }

        if t >= 1.0 {
            return *other;
        }

        let (from_r, from_g, from_b) = color_to_rgb(*self);
        let (to_r, to_g, to_b) = color_to_rgb(*other);

        Self::Rgb {
            r: lerp_channel(from_r, to_r, t),
            g: lerp_channel(from_g, to_g, t),
            b: lerp_channel(from_b, to_b, t),
        }
    }
}

impl Easing {
    pub fn apply(self, t: f32) -> f32 {
        let t = normalize_t(t);

        match self {
            Self::Linear => t,
            Self::EaseOutCubic => bezier_y_for_point(t, (0.33, 1.0), (0.68, 1.0)),
            Self::EaseInOutCubic => bezier_y_for_point(t, (0.65, 0.0), (0.35, 1.0)),
            Self::EaseOutBack => bezier_y_for_point(t, (0.34, 1.56), (0.64, 1.0)),
            Self::EaseOutExpo => bezier_y_for_point(t, (0.16, 1.0), (0.30, 1.0)),
        }
    }
}

impl<T: Lerp> Tween<T> {
    pub const fn new(from: T, to: T, duration: Duration, easing: Easing) -> Self {
        Self {
            from,
            to,
            elapsed: Duration::ZERO,
            duration,
            easing,
        }
    }

    pub fn value(&self) -> T {
        let t = if self.duration.is_zero() {
            1.0
        } else {
            self.elapsed.as_secs_f32() / self.duration.as_secs_f32()
        };

        self.from.lerp(&self.to, self.easing.apply(t))
    }

    pub fn advance(&mut self, dt: Duration) -> bool {
        self.elapsed = self.elapsed.saturating_add(dt).min(self.duration);
        self.elapsed < self.duration
    }

    pub fn retarget(&mut self, to: T) {
        self.from = self.value();
        self.to = to;
        self.elapsed = Duration::ZERO;
    }
}

fn normalize_t(t: f32) -> f32 {
    if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

fn bezier_y_for_point(x: f32, p1: (f32, f32), p2: (f32, f32)) -> f32 {
    if x >= 1.0 {
        return 1.0;
    }

    if x <= 0.0 {
        return 0.0;
    }

    let baked = bake_bezier(p1, p2);
    let mut index = 0usize;
    let mut below = true;
    let mut step = BAKED_POINTS.div_ceil(2);

    while step > 0 {
        if below {
            index = (index + step).min(BAKED_POINTS - 1);
        } else {
            index = index.saturating_sub(step);
        }

        below = baked[index].0 < x;
        step /= 2;
    }

    let mut lower_index = if !below || index == BAKED_POINTS - 1 {
        index.saturating_sub(1)
    } else {
        index
    };
    lower_index = lower_index.min(BAKED_POINTS - 2);

    let (lower_x, lower_y) = baked[lower_index];
    let (upper_x, upper_y) = baked[lower_index + 1];
    let dx = upper_x - lower_x;

    if dx <= 1e-6 {
        return lower_y;
    }

    let percent_in_delta = (x - lower_x) / dx;

    if !percent_in_delta.is_finite() {
        return lower_y;
    }

    lower_y + ((upper_y - lower_y) * percent_in_delta)
}

fn bake_bezier(p1: (f32, f32), p2: (f32, f32)) -> [(f32, f32); BAKED_POINTS] {
    std::array::from_fn(|i| {
        #[allow(clippy::cast_precision_loss)]
        let t = (i + 1) as f32 * INV_BAKED_POINTS;
        (
            bezier_axis_for_t(t, p1.0, p2.0),
            bezier_axis_for_t(t, p1.1, p2.1),
        )
    })
}

fn bezier_axis_for_t(t: f32, p1: f32, p2: f32) -> f32 {
    let one_minus_t = 1.0 - t;
    let t2 = t * t;
    let t3 = t2 * t;

    (3.0 * t * one_minus_t * one_minus_t * p1) + (3.0 * t2 * one_minus_t * p2) + t3
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

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn lerp_channel(from: u8, to: u8, t: f32) -> u8 {
    let value = f32::from(from).lerp(&f32::from(to), t).round();

    value.clamp(0.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::{Easing, FRect, Lerp, Tween};
    use crossterm::style::Color;
    use std::time::Duration;

    #[test]
    fn linear_tween_returns_midpoint_at_half_duration() {
        let mut tween = Tween::new(10.0, 20.0, Duration::from_millis(100), Easing::Linear);

        assert!(tween.advance(Duration::from_millis(50)));

        assert_eq!(tween.value(), 15.0);
    }

    #[test]
    fn ease_out_cubic_is_monotonic() {
        let mut previous = Easing::EaseOutCubic.apply(0.0);

        for i in 1..=100 {
            #[allow(clippy::cast_precision_loss)]
            let next = Easing::EaseOutCubic.apply(i as f32 / 100.0);
            assert!(next >= previous, "{next} regressed below {previous}");
            previous = next;
        }
    }

    #[test]
    fn frect_lerp_interpolates_each_component() {
        let from = FRect {
            x: 0.0,
            y: 10.0,
            w: 20.0,
            h: 30.0,
        };
        let to = FRect {
            x: 10.0,
            y: 20.0,
            w: 40.0,
            h: 70.0,
        };

        assert_eq!(
            from.lerp(&to, 0.25),
            FRect {
                x: 2.5,
                y: 12.5,
                w: 25.0,
                h: 40.0,
            }
        );
    }

    #[test]
    fn color_lerp_interpolates_rgb_channels() {
        let from = Color::Rgb {
            r: 0,
            g: 64,
            b: 255,
        };
        let to = Color::Rgb {
            r: 255,
            g: 128,
            b: 0,
        };

        assert_eq!(
            from.lerp(&to, 0.5),
            Color::Rgb {
                r: 128,
                g: 96,
                b: 128,
            }
        );
    }

    #[test]
    fn retarget_uses_current_value_as_new_from() {
        let mut tween = Tween::new(0.0, 100.0, Duration::from_millis(100), Easing::Linear);
        assert!(tween.advance(Duration::from_millis(25)));

        tween.retarget(200.0);

        assert_eq!(tween.from, 25.0);
        assert_eq!(tween.to, 200.0);
        assert_eq!(tween.elapsed, Duration::ZERO);
        assert_eq!(tween.easing, Easing::Linear);
    }
}
