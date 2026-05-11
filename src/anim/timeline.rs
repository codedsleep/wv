//! `Timeline` of in-flight tweens.

#![allow(dead_code)]

use std::collections::HashMap;
use std::time::Duration;

use crossterm::style::Color;

use crate::anim::tween::{Easing, Tween};
use crate::backend::PaneId;
use crate::layout::geometry::FRect;
use crate::layout::tree::Node;

const FLOAT_EPSILON: f32 = 0.000_1;

#[derive(Default)]
pub struct Timeline {
    leaf_rects: HashMap<PaneId, Tween<FRect>>,
    internal_ratios: HashMap<usize, Tween<f32>>,
    focus_border: Option<Tween<Color>>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TimelineAdvance {
    pub changed_panes: Vec<PaneId>,
    pub border_color_changed: bool,
}

impl Timeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn active_count(&self) -> usize {
        self.leaf_rects.len()
            + self.internal_ratios.len()
            + usize::from(self.focus_border.is_some())
    }

    pub fn is_idle(&self) -> bool {
        self.active_count() == 0
    }

    pub fn tween_leaf_rect(
        &mut self,
        pane: PaneId,
        from: FRect,
        to: FRect,
        duration: Duration,
        easing: Easing,
    ) {
        self.leaf_rects
            .insert(pane, Tween::new(from, to, duration, easing));
    }

    pub fn retarget_leaf_rect(&mut self, pane: PaneId, to: FRect) {
        if let Some(tween) = self.leaf_rects.get_mut(&pane) {
            tween.retarget(to);
        }
    }

    pub fn tween_internal_ratio(
        &mut self,
        internal_index: usize,
        from: f32,
        to: f32,
        duration: Duration,
        easing: Easing,
    ) {
        self.internal_ratios.insert(
            internal_index,
            Tween::new(normalize_ratio(from), normalize_ratio(to), duration, easing),
        );
    }

    pub fn tween_focus_border(
        &mut self,
        from: Color,
        to: Color,
        duration: Duration,
        easing: Easing,
    ) {
        self.focus_border = Some(Tween::new(from, to, duration, easing));
    }

    pub fn advance(
        &mut self,
        dt: Duration,
        root: Option<&mut Node>,
        focused_border_color: &mut Color,
    ) -> TimelineAdvance {
        let mut advance = TimelineAdvance::default();
        self.advance_focus_border(dt, focused_border_color, &mut advance);

        let Some(root) = root else {
            self.leaf_rects.clear();
            self.internal_ratios.clear();
            return advance;
        };

        self.advance_leaf_rects(dt, root, &mut advance);
        self.advance_internal_ratios(dt, root, &mut advance);
        advance.changed_panes.sort_by_key(|pane| pane.0);
        advance.changed_panes.dedup();
        advance
    }

    fn advance_focus_border(
        &mut self,
        dt: Duration,
        focused_border_color: &mut Color,
        advance: &mut TimelineAdvance,
    ) {
        let Some(tween) = self.focus_border.as_mut() else {
            return;
        };

        let running = tween.advance(dt);
        let value = tween.value();
        if *focused_border_color != value {
            *focused_border_color = value;
            advance.border_color_changed = true;
        }
        if !running {
            self.focus_border = None;
        }
    }

    fn advance_leaf_rects(&mut self, dt: Duration, root: &mut Node, advance: &mut TimelineAdvance) {
        let mut finished = Vec::new();
        let panes = self.leaf_rects.keys().copied().collect::<Vec<_>>();

        for pane in panes {
            let Some(tween) = self.leaf_rects.get_mut(&pane) else {
                continue;
            };
            let running = tween.advance(dt);
            let value = tween.value();

            match root.find_leaf_mut(pane) {
                Some(Node::Leaf { rect_current, .. }) => {
                    if frect_changed(*rect_current, value) {
                        *rect_current = value;
                        advance.changed_panes.push(pane);
                    }
                }
                Some(Node::Internal { .. }) => unreachable!("find_leaf_mut only returns leaves"),
                None => finished.push(pane),
            }

            if !running {
                finished.push(pane);
            }
        }

        for pane in finished {
            self.leaf_rects.remove(&pane);
        }
    }

    fn advance_internal_ratios(
        &mut self,
        dt: Duration,
        root: &mut Node,
        advance: &mut TimelineAdvance,
    ) {
        let mut finished = Vec::new();
        let internal_indices = self.internal_ratios.keys().copied().collect::<Vec<_>>();

        for internal_index in internal_indices {
            let Some(tween) = self.internal_ratios.get_mut(&internal_index) else {
                continue;
            };
            let running = tween.advance(dt);
            let ratio = normalize_ratio(tween.value());
            let mut traversal_index = 0;

            if set_internal_ratio(root, internal_index, ratio, &mut traversal_index) {
                collect_leaf_panes(root, &mut advance.changed_panes);
            }

            if !running || internal_index >= traversal_index {
                finished.push(internal_index);
            }
        }

        for internal_index in finished {
            self.internal_ratios.remove(&internal_index);
        }
    }
}

fn set_internal_ratio(
    node: &mut Node,
    target_index: usize,
    ratio_value: f32,
    traversal_index: &mut usize,
) -> bool {
    match node {
        Node::Leaf { .. } => false,
        Node::Internal { ratio, a, b, .. } => {
            let current_index = *traversal_index;
            *traversal_index += 1;

            if current_index == target_index {
                let changed = (*ratio - ratio_value).abs() > FLOAT_EPSILON;
                *ratio = ratio_value;
                return changed;
            }

            set_internal_ratio(a, target_index, ratio_value, traversal_index)
                || set_internal_ratio(b, target_index, ratio_value, traversal_index)
        }
    }
}

fn collect_leaf_panes(node: &Node, panes: &mut Vec<PaneId>) {
    match node {
        Node::Leaf { pane, .. } => panes.push(*pane),
        Node::Internal { a, b, .. } => {
            collect_leaf_panes(a, panes);
            collect_leaf_panes(b, panes);
        }
    }
}

fn normalize_ratio(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.5
    }
}

fn frect_changed(a: FRect, b: FRect) -> bool {
    (a.x - b.x).abs() > FLOAT_EPSILON
        || (a.y - b.y).abs() > FLOAT_EPSILON
        || (a.w - b.w).abs() > FLOAT_EPSILON
        || (a.h - b.h).abs() > FLOAT_EPSILON
}

#[cfg(test)]
mod tests {
    use super::Timeline;
    use crate::anim::tween::Easing;
    use crate::backend::PaneId;
    use crate::layout::geometry::{FRect, Rect, Split};
    use crate::layout::tree::Node;
    use crossterm::style::Color;
    use std::time::Duration;

    const ROOT: Rect = Rect {
        x: 0,
        y: 0,
        w: 80,
        h: 24,
    };

    fn leaf(pane: PaneId, rect: Rect) -> Node {
        Node::Leaf {
            pane,
            rect_current: FRect::from(rect),
            rect_target: rect,
        }
    }

    #[test]
    fn leaf_rect_tween_updates_current_rect_and_reports_pane() {
        let mut root = leaf(PaneId(1), ROOT);
        let mut timeline = Timeline::new();
        timeline.tween_leaf_rect(
            PaneId(1),
            FRect::from(ROOT),
            FRect {
                x: 10.0,
                y: 0.0,
                w: 70.0,
                h: 24.0,
            },
            Duration::from_millis(100),
            Easing::Linear,
        );
        let mut border = Color::Cyan;

        let advance = timeline.advance(Duration::from_millis(50), Some(&mut root), &mut border);

        assert_eq!(advance.changed_panes, vec![PaneId(1)]);
        match root {
            Node::Leaf { rect_current, .. } => {
                assert_eq!(rect_current.x, 5.0);
                assert_eq!(rect_current.w, 75.0);
            }
            Node::Internal { .. } => panic!("expected leaf"),
        }
        assert_eq!(timeline.active_count(), 1);
    }

    #[test]
    fn finished_leaf_rect_tween_is_removed() {
        let mut root = leaf(PaneId(1), ROOT);
        let mut timeline = Timeline::new();
        timeline.tween_leaf_rect(
            PaneId(1),
            FRect::from(ROOT),
            FRect {
                x: 10.0,
                y: 0.0,
                w: 70.0,
                h: 24.0,
            },
            Duration::from_millis(100),
            Easing::Linear,
        );
        let mut border = Color::Cyan;

        let advance = timeline.advance(Duration::from_millis(100), Some(&mut root), &mut border);

        assert_eq!(advance.changed_panes, vec![PaneId(1)]);
        assert!(timeline.is_idle());
    }

    #[test]
    fn internal_ratio_tween_updates_preorder_internal_node() {
        let mut root = Node::Internal {
            split: Split::Vertical,
            ratio: 0.5,
            ratio_target: 0.5,
            a: Box::new(leaf(PaneId(1), ROOT)),
            b: Box::new(leaf(PaneId(2), ROOT)),
            rect: ROOT,
        };
        let mut timeline = Timeline::new();
        timeline.tween_internal_ratio(0, 0.5, 0.75, Duration::from_millis(100), Easing::Linear);
        let mut border = Color::Cyan;

        let advance = timeline.advance(Duration::from_millis(50), Some(&mut root), &mut border);

        assert_eq!(advance.changed_panes, vec![PaneId(1), PaneId(2)]);
        match root {
            Node::Internal { ratio, .. } => assert_eq!(ratio, 0.625),
            Node::Leaf { .. } => panic!("expected internal"),
        }
    }

    #[test]
    fn focus_border_tween_updates_color() {
        let mut timeline = Timeline::new();
        timeline.tween_focus_border(
            Color::Rgb { r: 0, g: 0, b: 0 },
            Color::Rgb {
                r: 100,
                g: 50,
                b: 0,
            },
            Duration::from_millis(100),
            Easing::Linear,
        );
        let mut border = Color::Rgb { r: 0, g: 0, b: 0 };

        let advance = timeline.advance(Duration::from_millis(50), None, &mut border);

        assert!(advance.border_color_changed);
        assert_eq!(border, Color::Rgb { r: 50, g: 25, b: 0 });
    }
}
