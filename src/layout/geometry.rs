//! Integer geometry primitives for BSP layout.

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Split {
    Horizontal,
    Vertical,
}

impl Rect {
    pub fn split(self, split: Split, ratio: f32) -> (Self, Self) {
        match split {
            Split::Horizontal => {
                let first_h = split_extent(self.h, ratio);
                let second_h = self.h.saturating_sub(first_h).max(1);

                (
                    Self { h: first_h, ..self },
                    Self {
                        y: self.y.saturating_add(first_h),
                        h: second_h,
                        ..self
                    },
                )
            }
            Split::Vertical => {
                let first_w = split_extent(self.w, ratio);
                let second_w = self.w.saturating_sub(first_w).max(1);

                (
                    Self { w: first_w, ..self },
                    Self {
                        x: self.x.saturating_add(first_w),
                        w: second_w,
                        ..self
                    },
                )
            }
        }
    }
}

fn split_extent(extent: u16, ratio: f32) -> u16 {
    if extent <= 1 {
        return 1;
    }

    let max_first = extent - 1;
    let ratio = if ratio.is_finite() {
        ratio.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let raw = (f32::from(extent) * ratio).round();

    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (raw as u16).clamp(1, max_first)
}

#[cfg(test)]
mod tests {
    use super::{Rect, Split};

    #[test]
    fn one_cell_width_split_does_not_panic_or_zero_children() {
        let rect = Rect {
            x: 3,
            y: 5,
            w: 1,
            h: 4,
        };

        let (left, right) = rect.split(Split::Vertical, 0.5);

        assert!(left.w >= 1);
        assert!(right.w >= 1);
        assert_eq!(left.h, 4);
        assert_eq!(right.h, 4);
    }

    #[test]
    fn one_cell_height_split_does_not_panic_or_zero_children() {
        let rect = Rect {
            x: 3,
            y: 5,
            w: 4,
            h: 1,
        };

        let (top, bottom) = rect.split(Split::Horizontal, 0.5);

        assert!(top.h >= 1);
        assert!(bottom.h >= 1);
        assert_eq!(top.w, 4);
        assert_eq!(bottom.w, 4);
    }

    #[test]
    fn two_cell_vertical_split_at_half() {
        let rect = Rect {
            x: 10,
            y: 20,
            w: 2,
            h: 8,
        };

        let (left, right) = rect.split(Split::Vertical, 0.5);

        assert_eq!(
            left,
            Rect {
                x: 10,
                y: 20,
                w: 1,
                h: 8,
            }
        );
        assert_eq!(
            right,
            Rect {
                x: 11,
                y: 20,
                w: 1,
                h: 8,
            }
        );
    }

    #[test]
    fn two_cell_horizontal_split_at_half() {
        let rect = Rect {
            x: 10,
            y: 20,
            w: 8,
            h: 2,
        };

        let (top, bottom) = rect.split(Split::Horizontal, 0.5);

        assert_eq!(
            top,
            Rect {
                x: 10,
                y: 20,
                w: 8,
                h: 1,
            }
        );
        assert_eq!(
            bottom,
            Rect {
                x: 10,
                y: 21,
                w: 8,
                h: 1,
            }
        );
    }

    #[test]
    fn ratios_near_zero_and_one_yield_non_zero_vertical_children() {
        let rect = Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 5,
        };

        let (left_near_zero, right_near_zero) = rect.split(Split::Vertical, 0.001);
        let (left_near_one, right_near_one) = rect.split(Split::Vertical, 0.999);

        assert_eq!(left_near_zero.w, 1);
        assert_eq!(right_near_zero.w, 9);
        assert_eq!(left_near_one.w, 9);
        assert_eq!(right_near_one.w, 1);
    }

    #[test]
    fn ratios_near_zero_and_one_yield_non_zero_horizontal_children() {
        let rect = Rect {
            x: 0,
            y: 0,
            w: 5,
            h: 10,
        };

        let (top_near_zero, bottom_near_zero) = rect.split(Split::Horizontal, 0.001);
        let (top_near_one, bottom_near_one) = rect.split(Split::Horizontal, 0.999);

        assert_eq!(top_near_zero.h, 1);
        assert_eq!(bottom_near_zero.h, 9);
        assert_eq!(top_near_one.h, 9);
        assert_eq!(bottom_near_one.h, 1);
    }

    #[test]
    fn horizontal_split_returns_top_then_bottom() {
        let rect = Rect {
            x: 2,
            y: 4,
            w: 8,
            h: 6,
        };

        let (top, bottom) = rect.split(Split::Horizontal, 0.5);

        assert_eq!(top.y, 4);
        assert_eq!(top.h, 3);
        assert_eq!(bottom.y, 7);
        assert_eq!(bottom.h, 3);
    }

    #[test]
    fn vertical_split_returns_left_then_right() {
        let rect = Rect {
            x: 2,
            y: 4,
            w: 8,
            h: 6,
        };

        let (left, right) = rect.split(Split::Vertical, 0.5);

        assert_eq!(left.x, 2);
        assert_eq!(left.w, 4);
        assert_eq!(right.x, 6);
        assert_eq!(right.w, 4);
    }
}
