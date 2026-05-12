//! Window/workspace mapping for tmux sessions.

use std::collections::{HashMap, HashSet};

use crate::app::WORKSPACE_COUNT;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    Assigned(usize),
    Overflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    Freed(usize),
    WasOverflow,
    Unknown,
}

#[derive(Debug, Default)]
pub struct WindowMap {
    window_to_workspace: HashMap<u64, usize>,
    workspace_to_window: HashMap<usize, u64>,
    overflow: HashSet<u64>,
}

impl WindowMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on_window_add(&mut self, window_id: u64, window_index: usize) -> AddOutcome {
        let Some(workspace) = window_index.checked_sub(1) else {
            self.mark_overflow(window_id);
            return AddOutcome::Overflow;
        };

        if workspace >= WORKSPACE_COUNT {
            self.mark_overflow(window_id);
            return AddOutcome::Overflow;
        }

        if let Some(existing_window_id) = self.window_for_workspace(workspace) {
            if existing_window_id == window_id {
                return AddOutcome::Assigned(workspace);
            }

            self.mark_overflow(window_id);
            return AddOutcome::Overflow;
        }

        if let Some(previous_workspace) = self.window_to_workspace.insert(window_id, workspace) {
            self.workspace_to_window.remove(&previous_workspace);
        }
        self.workspace_to_window.insert(workspace, window_id);
        self.overflow.remove(&window_id);

        AddOutcome::Assigned(workspace)
    }

    pub fn on_window_close(&mut self, window_id: u64) -> CloseOutcome {
        if let Some(workspace) = self.window_to_workspace.remove(&window_id) {
            self.workspace_to_window.remove(&workspace);
            return CloseOutcome::Freed(workspace);
        }

        if self.overflow.remove(&window_id) {
            return CloseOutcome::WasOverflow;
        }

        CloseOutcome::Unknown
    }

    pub fn workspace_for_window(&self, window_id: u64) -> Option<usize> {
        self.window_to_workspace.get(&window_id).copied()
    }

    pub fn window_for_workspace(&self, workspace: usize) -> Option<u64> {
        self.workspace_to_window.get(&workspace).copied()
    }

    pub fn overflow_windows(&self) -> impl Iterator<Item = u64> + '_ {
        self.overflow.iter().copied()
    }

    fn mark_overflow(&mut self, window_id: u64) {
        if let Some(workspace) = self.window_to_workspace.remove(&window_id) {
            self.workspace_to_window.remove(&workspace);
        }
        self.overflow.insert(window_id);
    }
}

#[cfg(test)]
mod tests {
    use super::{AddOutcome, CloseOutcome, WindowMap};
    use crate::app::WORKSPACE_COUNT;

    #[test]
    fn window_indices_one_through_nine_map_to_workspaces_zero_through_eight() {
        let mut windows = WindowMap::new();

        for window_index in 1..=WORKSPACE_COUNT {
            let window_id = u64::try_from(window_index).unwrap();
            let workspace = window_index - 1;

            assert_eq!(
                windows.on_window_add(window_id, window_index),
                AddOutcome::Assigned(workspace)
            );
            assert_eq!(windows.workspace_for_window(window_id), Some(workspace));
            assert_eq!(windows.window_for_workspace(workspace), Some(window_id));
        }
    }

    #[test]
    fn window_index_ten_or_greater_becomes_overflow() {
        let mut windows = WindowMap::new();

        assert_eq!(windows.on_window_add(10, 10), AddOutcome::Overflow);
        assert_eq!(windows.workspace_for_window(10), None);
        assert_eq!(windows.window_for_workspace(9), None);
        assert_eq!(windows.overflow_windows().collect::<Vec<_>>(), vec![10]);
    }

    #[test]
    fn close_frees_slot_and_clears_inverse_mapping() {
        let mut windows = WindowMap::new();

        assert_eq!(windows.on_window_add(42, 3), AddOutcome::Assigned(2));
        assert_eq!(windows.on_window_close(42), CloseOutcome::Freed(2));
        assert_eq!(windows.workspace_for_window(42), None);
        assert_eq!(windows.window_for_workspace(2), None);
    }

    #[test]
    fn close_on_unknown_window_id_returns_unknown() {
        let mut windows = WindowMap::new();

        assert_eq!(windows.on_window_close(42), CloseOutcome::Unknown);
    }

    #[test]
    fn duplicate_window_index_returns_overflow_for_second_window() {
        let mut windows = WindowMap::new();

        assert_eq!(windows.on_window_add(1, 1), AddOutcome::Assigned(0));
        assert_eq!(windows.on_window_add(2, 1), AddOutcome::Overflow);
        assert_eq!(windows.window_for_workspace(0), Some(1));
        assert_eq!(windows.workspace_for_window(2), None);
        assert_eq!(windows.on_window_close(2), CloseOutcome::WasOverflow);
    }

    #[test]
    fn overflow_windows_iterates_the_right_set() {
        let mut windows = WindowMap::new();

        assert_eq!(windows.on_window_add(1, 10), AddOutcome::Overflow);
        assert_eq!(windows.on_window_add(2, 99), AddOutcome::Overflow);
        assert_eq!(windows.on_window_add(3, 1), AddOutcome::Assigned(0));

        let mut overflow = windows.overflow_windows().collect::<Vec<_>>();
        overflow.sort_unstable();

        assert_eq!(overflow, vec![1, 2]);
    }
}
