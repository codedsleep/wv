//! The goto picker: every session and window in one fuzzy-filtered list.
//!
//! `Alt+;` opens it. Rows are gathered once on open — the current session's
//! windows straight out of memory, everyone else's over their socket — and
//! then filtered locally as the query is typed, so no keystroke costs a round
//! trip. Selecting a row in this session is an ordinary `select-window`;
//! selecting one elsewhere hands the client to that session.
//!
//! The picker is session-global, like the command prompt it sits beside in
//! `App`: every attached client sees it and follows the jump.

pub mod fuzzy;

use std::time::Duration;

use crate::anim::tween::{Easing, Tween};

pub use fuzzy::Match;

/// How long the box takes to grow open, and to fold back up.
pub const PICKER_TWEEN_DURATION: Duration = Duration::from_millis(120);

/// One line in the picker: a whole session, or one window inside one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerRow {
    pub session: String,
    /// The `#{window_index}` this row jumps to, or `None` for a session row —
    /// which jumps to whatever window that session has current.
    pub window: Option<u32>,
    /// What is matched against and drawn: `main` or `main:2 dev-server`.
    pub label: String,
    /// The right-aligned count: `3 windows`, `2 panes`.
    pub detail: String,
    /// The session or window the user is looking at right now.
    pub is_current: bool,
    /// Whether this row belongs to the session running the picker. Local rows
    /// jump with `select-window`; the rest need a re-attach.
    pub is_local: bool,
}

impl PickerRow {
    /// A row naming a whole session.
    pub fn session(name: String, windows: usize, is_current: bool, is_local: bool) -> Self {
        Self {
            session: name.clone(),
            window: None,
            label: name,
            detail: plural(windows, "window"),
            is_current,
            is_local,
        }
    }

    /// A row naming one window inside a session.
    pub fn window(
        session: String,
        index: u32,
        name: &str,
        panes: usize,
        is_current: bool,
        is_local: bool,
    ) -> Self {
        Self {
            label: format!("{session}:{index} {name}"),
            session,
            window: Some(index),
            detail: plural(panes, "pane"),
            is_current,
            is_local,
        }
    }
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// A row that survived the filter, with where it matched.
pub struct Filtered<'a> {
    pub row: &'a PickerRow,
    pub positions: &'a [usize],
}

/// An open goto picker.
pub struct Picker {
    rows: Vec<PickerRow>,
    /// Held as chars, like `Prompt`, so the cursor can index without worrying
    /// about UTF-8 boundaries.
    query: Vec<char>,
    cursor: usize,
    /// Indices into `rows`, best match first, with the positions that matched.
    filtered: Vec<(usize, Match)>,
    selected: usize,
    /// 0 → 1 on open, back to 0 on close. The box grows out of its own centre.
    open: Tween<f32>,
    /// Set once the closing tween starts; the picker is dropped when it lands.
    closing: bool,
}

impl Picker {
    /// Open on `rows`, selecting the current window if there is one.
    pub fn new(rows: Vec<PickerRow>) -> Self {
        let mut picker = Self {
            rows,
            query: Vec::new(),
            cursor: 0,
            filtered: Vec::new(),
            selected: 0,
            open: Tween::new(0.0, 1.0, PICKER_TWEEN_DURATION, Easing::EaseOutCubic),
            closing: false,
        };
        picker.refilter();
        // Land on where you already are, so Enter on an untouched picker is a
        // no-op rather than a jump somewhere arbitrary.
        let current = picker.filtered.iter().position(|(index, _)| {
            let row = &picker.rows[*index];
            row.is_current && row.window.is_some()
        });
        if let Some(at) = current {
            picker.selected = at;
        }

        picker
    }

    pub fn query(&self) -> String {
        self.query.iter().collect()
    }

    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    pub const fn selected(&self) -> usize {
        self.selected
    }

    pub const fn is_closing(&self) -> bool {
        self.closing
    }

    /// How far open the box is, 0 to 1.
    pub fn progress(&self) -> f32 {
        self.open.value().clamp(0.0, 1.0)
    }

    pub fn match_count(&self) -> usize {
        self.filtered.len()
    }

    /// The rows that survived the filter, best first.
    pub fn matches(&self) -> impl Iterator<Item = Filtered<'_>> {
        self.filtered.iter().map(|(index, matched)| Filtered {
            row: &self.rows[*index],
            positions: &matched.positions,
        })
    }

    /// The row `Enter` would activate.
    pub fn selection(&self) -> Option<&PickerRow> {
        self.filtered
            .get(self.selected)
            .map(|(index, _)| &self.rows[*index])
    }

    /// Start folding the box back up. The caller drops the picker once
    /// [`Picker::advance`] reports it has finished.
    pub fn close(&mut self) {
        if self.closing {
            return;
        }
        self.closing = true;
        self.open.retarget(0.0);
    }

    /// Advance the open/close tween. Returns whether it is still running.
    pub fn advance(&mut self, dt: Duration) -> bool {
        self.open.advance(dt)
    }

    /// Whether the picker has finished closing and should be dropped.
    pub fn is_finished(&self) -> bool {
        self.closing && self.open.elapsed >= self.open.duration
    }

    /// Add rows that turned up after the picker was already on screen.
    ///
    /// Peers answer one at a time and at their own speed, so the list fills in
    /// under the query rather than waiting for the slowest of them. Order is
    /// not the caller's problem: [`Picker::refilter`] sorts by session and
    /// window index anyway, so a late session lands in its natural place.
    ///
    /// Unlike typing, this is not something the user did, so the selection
    /// stays on the row it was on — pressing Enter must not jump somewhere
    /// else because a peer answered in the same instant.
    pub fn extend(&mut self, rows: impl IntoIterator<Item = PickerRow>) {
        let selected = self.selection().cloned();
        self.rows.extend(rows);
        self.refilter();
        if let Some(row) = selected {
            if let Some(at) = self
                .filtered
                .iter()
                .position(|(index, _)| self.rows[*index] == row)
            {
                self.selected = at;
            }
        }
    }

    pub fn insert(&mut self, ch: char) {
        self.query.insert(self.cursor.min(self.query.len()), ch);
        self.cursor += 1;
        self.refilter();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.query.remove(self.cursor);
            self.refilter();
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.query.len() {
            self.query.remove(self.cursor);
            self.refilter();
        }
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.cursor = 0;
        self.refilter();
    }

    pub fn cursor_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn cursor_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.query.len());
    }

    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.query.len();
    }

    /// Move the selection, wrapping at both ends the way every other picker does.
    pub fn move_selection(&mut self, delta: isize) {
        let len = self.filtered.len();
        if len == 0 {
            self.selected = 0;
            return;
        }

        let len = isize::try_from(len).unwrap_or(isize::MAX);
        let at = isize::try_from(self.selected).unwrap_or(0);
        self.selected = usize::try_from((at + delta).rem_euclid(len)).unwrap_or(0);
    }

    /// Re-run the filter and keep the selection in range.
    ///
    /// The selection resets to the top rather than trying to follow the row it
    /// was on: after typing a character the best match is what you want, and
    /// chasing the old row would leave the cursor somewhere down the list.
    fn refilter(&mut self) {
        let query = self.query();
        self.filtered = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                fuzzy::match_query(&query, &row.label).map(|matched| (index, matched))
            })
            .collect();
        // Score first, then the natural order — session name, then window
        // index — so an untouched picker reads like `wv ls` rather than like
        // hash order.
        self.filtered.sort_by(|(left, left_match), (right, right_match)| {
            right_match
                .score
                .cmp(&left_match.score)
                .then_with(|| self.rows[*left].session.cmp(&self.rows[*right].session))
                .then_with(|| self.rows[*left].window.cmp(&self.rows[*right].window))
        });
        self.selected = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{Picker, PickerRow, PICKER_TWEEN_DURATION};

    fn rows() -> Vec<PickerRow> {
        vec![
            PickerRow::session("main".to_owned(), 2, true, true),
            PickerRow::window("main".to_owned(), 1, "editor", 2, true, true),
            PickerRow::window("main".to_owned(), 2, "dev-server", 1, false, true),
            PickerRow::session("scratch".to_owned(), 1, false, false),
            PickerRow::window("scratch".to_owned(), 1, "shell", 1, false, false),
        ]
    }

    #[test]
    fn an_untouched_picker_shows_everything_in_natural_order() {
        let picker = Picker::new(rows());

        assert_eq!(picker.match_count(), 5);
        let labels: Vec<String> = picker.matches().map(|row| row.row.label.clone()).collect();
        assert_eq!(
            labels,
            vec![
                "main",
                "main:1 editor",
                "main:2 dev-server",
                "scratch",
                "scratch:1 shell"
            ]
        );
    }

    #[test]
    fn it_opens_on_the_window_you_are_already_in() {
        let picker = Picker::new(rows());

        assert_eq!(
            picker.selection().expect("something is selected").label,
            "main:1 editor",
            "the current window, not the current session's own row"
        );
    }

    #[test]
    fn typing_filters_and_resets_the_selection_to_the_best_match() {
        let mut picker = Picker::new(rows());
        for ch in "dev".chars() {
            picker.insert(ch);
        }

        assert_eq!(picker.query(), "dev");
        assert_eq!(
            picker.selection().expect("a match survived").label,
            "main:2 dev-server"
        );
    }

    #[test]
    fn a_query_that_matches_nothing_leaves_no_selection() {
        let mut picker = Picker::new(rows());
        for ch in "zzz".chars() {
            picker.insert(ch);
        }

        assert_eq!(picker.match_count(), 0);
        assert!(picker.selection().is_none());
    }

    #[test]
    fn editing_the_query_puts_the_rows_back() {
        let mut picker = Picker::new(rows());
        for ch in "zzz".chars() {
            picker.insert(ch);
        }
        picker.backspace();
        picker.backspace();
        picker.backspace();

        assert_eq!(picker.query(), "");
        assert_eq!(picker.match_count(), 5);
    }

    #[test]
    fn clearing_the_query_is_not_the_same_as_closing() {
        let mut picker = Picker::new(rows());
        picker.insert('d');
        picker.clear();

        assert_eq!(picker.query(), "");
        assert_eq!(picker.cursor(), 0);
        assert!(!picker.is_closing());
    }

    #[test]
    fn the_selection_wraps_at_both_ends() {
        let mut picker = Picker::new(rows());
        picker.clear();
        assert_eq!(picker.selected(), 0);

        picker.move_selection(-1);
        assert_eq!(picker.selected(), 4, "up from the top lands at the bottom");

        picker.move_selection(1);
        assert_eq!(picker.selected(), 0, "and down from the bottom comes back");
    }

    #[test]
    fn moving_the_selection_with_nothing_matched_is_harmless() {
        let mut picker = Picker::new(rows());
        for ch in "zzz".chars() {
            picker.insert(ch);
        }
        picker.move_selection(1);

        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn the_box_grows_open_and_folds_shut() {
        let mut picker = Picker::new(rows());
        assert!(picker.progress() < 1.0, "it starts closed");

        picker.advance(PICKER_TWEEN_DURATION);
        assert!((picker.progress() - 1.0).abs() < f32::EPSILON, "then open");
        assert!(!picker.is_finished(), "an open picker is not finished");

        picker.close();
        assert!(picker.is_closing());
        picker.advance(PICKER_TWEEN_DURATION);
        assert!(picker.is_finished(), "and closed again once the tween lands");
    }

    #[test]
    fn cursor_movement_stays_inside_the_query() {
        let mut picker = Picker::new(rows());
        picker.insert('a');
        picker.insert('b');

        picker.cursor_right();
        assert_eq!(picker.cursor(), 2, "already at the end");
        picker.cursor_home();
        assert_eq!(picker.cursor(), 0);
        picker.cursor_left();
        assert_eq!(picker.cursor(), 0, "and cannot go further");
        picker.cursor_end();
        assert_eq!(picker.cursor(), 2);
    }
}
