use std::ffi::OsString;
use std::sync::Mutex;

use crossterm::style::Color;
use weave::anim::timeline::Timeline;
use weave::backend::PaneId;
use weave::config::ThemeConfig;
use weave::layout::geometry::{FRect, Rect, Split};
use weave::layout::tree::Node;
use weave::render::compositor::{compose, ComposeOptions};
use weave::render::diff::DiffRenderer;
use weave::term::pane::Pane;
use weave::term::surface::Surface;

const WIDTH: u16 = 80;
const HEIGHT: u16 = 24;
static ANSI_ENV_LOCK: Mutex<()> = Mutex::new(());

const TEST_THEME: ThemeConfig = ThemeConfig {
    border_focused: Color::Cyan,
    border_unfocused: Color::DarkGrey,
    status_fg: Color::White,
    status_bg: Color::DarkBlue,
    status_segment: Color::DarkBlue,
    status_session: Color::DarkBlue,
    accent: Color::Red,
    agent_working: Color::Green,
    agent_waiting: Color::Yellow,
    agent_idle: Color::DarkGrey,
};

#[test]
fn single_fullscreen_pane() {
    let rect = screen_rect();
    let root = leaf(PaneId(1), rect);
    let panes = vec![pane(PaneId(1), rect, "alpha fullscreen")];

    insta::assert_snapshot!("single_fullscreen_pane", render_ansi(&root, &panes));
}

#[test]
fn single_pane_with_title() {
    let rect = screen_rect();
    let root = leaf(PaneId(1), rect);
    let panes = vec![pane(PaneId(1), rect, "\x1b]2;build logs\x07alpha")];

    insta::assert_snapshot!("single_pane_with_title", render_ansi(&root, &panes));
}

#[test]
fn two_leaf_horizontal_split() {
    let root_rect = screen_rect();
    let (top, bottom) = root_rect.split(Split::Horizontal, 0.5);
    let root = internal(
        Split::Horizontal,
        root_rect,
        leaf(PaneId(1), top),
        leaf(PaneId(2), bottom),
    );
    let panes = vec![
        pane(PaneId(1), top, "top pane"),
        pane(PaneId(2), bottom, "bottom pane"),
    ];

    insta::assert_snapshot!("two_leaf_horizontal_split", render_ansi(&root, &panes));
}

#[test]
fn four_leaf_nested_splits() {
    let root_rect = screen_rect();
    let (left, right) = root_rect.split(Split::Vertical, 0.5);
    let (left_top, left_bottom) = left.split(Split::Horizontal, 0.5);
    let (right_top, right_bottom) = right.split(Split::Horizontal, 0.5);
    let left_node = internal(
        Split::Horizontal,
        left,
        leaf(PaneId(1), left_top),
        leaf(PaneId(2), left_bottom),
    );
    let right_node = internal(
        Split::Horizontal,
        right,
        leaf(PaneId(3), right_top),
        leaf(PaneId(4), right_bottom),
    );
    let root = internal(Split::Vertical, root_rect, left_node, right_node);
    let panes = vec![
        pane(PaneId(1), left_top, "left top"),
        pane(PaneId(2), left_bottom, "left bottom"),
        pane(PaneId(3), right_top, "right top"),
        pane(PaneId(4), right_bottom, "right bottom"),
    ];

    insta::assert_snapshot!("four_leaf_nested_splits", render_ansi(&root, &panes));
}

fn screen_rect() -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: WIDTH,
        h: HEIGHT,
    }
}

fn leaf(id: PaneId, rect: Rect) -> Node {
    Node::Leaf {
        pane: id,
        rect_current: FRect::from(rect),
        rect_target: rect,
    }
}

fn internal(split: Split, rect: Rect, a: Node, b: Node) -> Node {
    Node::Internal {
        split,
        ratio: 0.5,
        ratio_target: 0.5,
        a: Box::new(a),
        b: Box::new(b),
        rect,
    }
}

fn pane(id: PaneId, rect: Rect, text: &str) -> Pane {
    let mut pane = Pane::new(
        id,
        rect.w.saturating_sub(2).max(1),
        rect.h.saturating_sub(2).max(1),
    );
    pane.process(text.as_bytes());
    pane
}

fn render_ansi(root: &Node, panes: &[Pane]) -> String {
    let _env_lock = ANSI_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _no_color = NoColorEnvGuard::remove();

    let front = Surface::new(WIDTH, HEIGHT);
    let mut back = Surface::new(WIDTH, HEIGHT);
    let mut renderer = DiffRenderer::new();
    let mut bytes = Vec::new();

    compose(
        Some(root),
        panes,
        Some(PaneId(1)),
        TEST_THEME,
        &Timeline::new(),
        &mut back,
        ComposeOptions { pane_titles: true, zoomed: None },
    );
    renderer
        .flush(&front, &back, &mut bytes)
        .expect("diff flush should succeed");

    String::from_utf8(bytes).expect("crossterm output should be utf8")
}

struct NoColorEnvGuard {
    previous: Option<OsString>,
}

impl NoColorEnvGuard {
    fn remove() -> Self {
        let previous = std::env::var_os("NO_COLOR");
        std::env::remove_var("NO_COLOR");
        Self { previous }
    }
}

impl Drop for NoColorEnvGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var("NO_COLOR", previous);
        } else {
            std::env::remove_var("NO_COLOR");
        }
    }
}
