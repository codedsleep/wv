//! Which panes are running coding agents, and whether they need you.
//!
//! An agent pane is worth watching for one reason: you want to know when it
//! has stopped without having to look at it. Three states carry that:
//! `Working` while it is producing output, `Waiting` when it has stopped at a
//! question, and `Idle` when it has stopped and wants nothing.
//!
//! The signal is the pane's own screen, which weave already renders. Nothing
//! is asked of the agent, so this works for any tool that prints while it
//! thinks — which is all of them.
//!
//! Specifically the screen, not the bytes behind it. An agent's PTY never
//! really goes quiet: idle Claude Code writes eight bytes a second to blink
//! its cursor and idle Codex around fifty, so "has it produced output lately"
//! answers yes forever and no agent is ever seen to stop. What those bytes do
//! not do is change any text on screen, so that is what gets watched instead.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::backend::PaneId;

/// What an agent pane is doing, as far as its output shows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    /// Producing output right now.
    Working,
    /// Stopped at something that reads like a question.
    Waiting,
    /// Stopped, with nothing on screen asking for you.
    Idle,
}

/// Per-pane facts the state is derived from.
///
/// The IO — reading `/proc`, scraping the screen — happens in the caller and
/// arrives here as plain data, so the rules stay testable without a PTY.
#[derive(Default)]
pub struct AgentTracker {
    last_output: HashMap<PaneId, Instant>,
    last_input: HashMap<PaneId, Instant>,
    foreground: HashMap<PaneId, String>,
}

impl AgentTracker {
    /// Record that a pane's screen just changed.
    pub fn note_output(&mut self, pane: PaneId, at: Instant) {
        self.last_output.insert(pane, at);
    }

    /// Record that keys were sent to a pane.
    pub fn note_input(&mut self, pane: PaneId, at: Instant) {
        self.last_input.insert(pane, at);
    }

    /// Whether the last thing to move a pane's screen was your own typing.
    ///
    /// Keystrokes echo, so a message being typed to an agent reads exactly
    /// like the agent working, and pausing reads like it finishing. The two
    /// are told apart by what came last: an agent that actually did something
    /// keeps painting long after the keystroke that set it off, so its final
    /// screen change lands well beyond it. `grace` covers the gap between a
    /// key and the poll that notices its echo, so a pane is only "yours" if
    /// nothing but the echo has happened since.
    pub fn ended_on_your_typing(&self, pane: PaneId, grace: Duration) -> bool {
        let Some(typed) = self.last_input.get(&pane) else {
            return false;
        };

        !self
            .last_output
            .get(&pane)
            .is_some_and(|printed| *printed > *typed + grace)
    }

    /// Record the command a pane is currently running, as of the last poll.
    pub fn set_foreground(&mut self, pane: PaneId, command: Option<String>) {
        match command {
            Some(command) => self.foreground.insert(pane, command),
            None => self.foreground.remove(&pane),
        };
    }

    /// The command a pane is running, if it was polled.
    pub fn foreground(&self, pane: PaneId) -> Option<&str> {
        self.foreground.get(&pane).map(String::as_str)
    }

    /// Drop a pane's history when it closes, so its id can be reused cleanly.
    pub fn forget(&mut self, pane: PaneId) {
        self.last_output.remove(&pane);
        self.last_input.remove(&pane);
        self.foreground.remove(&pane);
    }

    /// Whether a pane produced output recently enough to count as working.
    pub fn is_active(&self, pane: PaneId, now: Instant, window: Duration) -> bool {
        self.last_output
            .get(&pane)
            .is_some_and(|last| now.duration_since(*last) < window)
    }

    /// Resolve a pane's state.
    ///
    /// Output beats everything: an agent printing its way through a question
    /// is working, not waiting. `asking` and `busy` are the caller's read of
    /// the screen.
    ///
    /// `busy` is the agent's own word for it — the footer it shows while a
    /// turn is running. A turn is not a stream of output: an agent that hands
    /// a long command to a tool prints nothing until it comes back, and a
    /// screen that has not moved for a couple of seconds is otherwise
    /// indistinguishable from one whose turn is over. Believing the footer
    /// keeps a pause inside a turn from reading as the end of one.
    pub fn state(
        &self,
        pane: PaneId,
        now: Instant,
        window: Duration,
        asking: bool,
        busy: bool,
    ) -> AgentState {
        if busy || self.is_active(pane, now, window) {
            AgentState::Working
        } else if asking {
            AgentState::Waiting
        } else {
            AgentState::Idle
        }
    }
}

/// Whether a state change means an agent just finished a turn.
///
/// Finishing is leaving `Working`, whether it stopped at a question or with
/// nothing to say — both mean the pane is yours again, which is the moment
/// worth a sound. A pane first seen already stopped has not finished anything;
/// otherwise every agent idle at startup would ring at once.
pub fn just_finished(previous: Option<AgentState>, current: AgentState) -> bool {
    previous == Some(AgentState::Working) && current != AgentState::Working
}

/// Where `command` sits in the configured agent list, if it is one.
///
/// The position doubles as the kind's sort rank, so the bar groups agents in
/// the order `agent-commands` names them and keeps that order as panes come
/// and go. Compared against the file name so a command found by absolute path
/// still matches, and case-insensitively because argv[0] casing is not worth
/// caring about.
pub fn agent_rank(command: &str, agents: &[String]) -> Option<usize> {
    let name = command.rsplit('/').next().unwrap_or(command).trim();

    agents
        .iter()
        .position(|agent| agent.trim().eq_ignore_ascii_case(name))
}

/// Split an option's comma-separated list into trimmed, non-empty entries.
pub fn parse_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether a pane's visible text ends at something asking for input.
///
/// Only the last few non-blank lines are considered: an agent's transcript is
/// full of questions it has already answered, and it is the bottom of the
/// screen that says what it is waiting on now.
pub fn looks_like_a_question(lines: &[String], patterns: &[String]) -> bool {
    tail_matches(lines, patterns)
}

/// Whether a pane's visible text says the agent is still in the middle of a
/// turn — `esc to interrupt` and its equivalents.
///
/// The same bottom-of-the-screen scan as [`looks_like_a_question`], because
/// the footer that offers to interrupt sits in the same place as the prompt
/// that asks a question, and only one of them is ever showing.
pub fn looks_busy(lines: &[String], patterns: &[String]) -> bool {
    tail_matches(lines, patterns)
}

/// Whether any of the last few non-blank lines contains any of `patterns`,
/// case-insensitively.
fn tail_matches(lines: &[String], patterns: &[String]) -> bool {
    const TAIL: usize = 6;

    if patterns.is_empty() {
        return false;
    }

    lines
        .iter()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(TAIL)
        .any(|line| {
            let line = line.to_lowercase();
            patterns
                .iter()
                .any(|pattern| line.contains(&pattern.to_lowercase()))
        })
}

#[cfg(test)]
mod tests {
    use super::{
        agent_rank, just_finished, looks_busy, looks_like_a_question, parse_list, AgentState,
        AgentTracker,
    };
    use crate::backend::PaneId;
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_secs(2);

    fn patterns() -> Vec<String> {
        parse_list("do you want,(y/n)")
    }

    #[test]
    fn a_pane_that_just_printed_is_working() {
        let mut tracker = AgentTracker::default();
        let now = Instant::now();
        tracker.note_output(PaneId(1), now);

        assert_eq!(
            tracker.state(PaneId(1), now, WINDOW, false, false),
            AgentState::Working
        );
    }

    /// Output wins over a question on screen: an agent printing its way past a
    /// prompt it already answered has not stopped.
    #[test]
    fn output_beats_a_question_on_screen() {
        let mut tracker = AgentTracker::default();
        let now = Instant::now();
        tracker.note_output(PaneId(1), now);

        assert_eq!(
            tracker.state(PaneId(1), now, WINDOW, true, false),
            AgentState::Working
        );
    }

    #[test]
    fn a_quiet_pane_at_a_question_is_waiting() {
        let mut tracker = AgentTracker::default();
        let start = Instant::now();
        tracker.note_output(PaneId(1), start);
        let later = start + Duration::from_secs(5);

        assert_eq!(
            tracker.state(PaneId(1), later, WINDOW, true, false),
            AgentState::Waiting
        );
        assert_eq!(
            tracker.state(PaneId(1), later, WINDOW, false, false),
            AgentState::Idle
        );
    }

    /// The case the activity window alone gets wrong: a tool call long enough
    /// to age the pane out of it, while the turn it belongs to is still going.
    #[test]
    fn a_pane_whose_footer_says_it_is_working_is_working() {
        let mut tracker = AgentTracker::default();
        let start = Instant::now();
        tracker.note_output(PaneId(1), start);
        let later = start + Duration::from_secs(60);

        assert_eq!(
            tracker.state(PaneId(1), later, WINDOW, false, true),
            AgentState::Working
        );
        // And it outranks a question: an agent that printed one and carried on
        // is not sitting at it.
        assert_eq!(
            tracker.state(PaneId(1), later, WINDOW, true, true),
            AgentState::Working
        );
    }

    #[test]
    fn a_footer_offering_to_interrupt_reads_as_busy() {
        let patterns = parse_list("to interrupt,esc to stop");
        let busy = vec![
            "· Running bash…".to_owned(),
            "  (esc to interrupt · 42s)".to_owned(),
        ];
        let done = vec!["all done".to_owned(), "> ".to_owned()];

        assert!(looks_busy(&busy, &patterns));
        assert!(!looks_busy(&done, &patterns));
        // No patterns configured is the old behaviour: nothing is ever busy.
        assert!(!looks_busy(&busy, &[]));
    }

    #[test]
    fn a_pane_that_never_printed_is_idle() {
        let tracker = AgentTracker::default();

        assert_eq!(
            tracker.state(PaneId(9), Instant::now(), WINDOW, false, false),
            AgentState::Idle
        );
    }

    #[test]
    fn forgetting_a_pane_drops_its_history() {
        let mut tracker = AgentTracker::default();
        let now = Instant::now();
        tracker.note_output(PaneId(1), now);
        tracker.set_foreground(PaneId(1), Some("claude".to_owned()));

        tracker.forget(PaneId(1));

        assert_eq!(tracker.foreground(PaneId(1)), None);
        assert_eq!(
            tracker.state(PaneId(1), now, WINDOW, false, false),
            AgentState::Idle
        );
    }

    #[test]
    fn setting_a_foreground_of_none_clears_it() {
        let mut tracker = AgentTracker::default();
        tracker.set_foreground(PaneId(1), Some("codex".to_owned()));
        tracker.set_foreground(PaneId(1), None);

        assert_eq!(tracker.foreground(PaneId(1)), None);
    }

    /// The position is the kind's sort rank, so it has to be the index in the
    /// configured list rather than merely "yes, an agent".
    #[test]
    fn agents_rank_by_file_name_and_ignore_case() {
        let agents = parse_list("claude, codex");

        assert_eq!(agent_rank("claude", &agents), Some(0));
        assert_eq!(agent_rank("/usr/local/bin/codex", &agents), Some(1));
        assert_eq!(agent_rank("Claude", &agents), Some(0));
        assert_eq!(agent_rank("fish", &agents), None);
        assert_eq!(agent_rank("claudette", &agents), None);
    }

    #[test]
    fn an_empty_agent_list_matches_nothing() {
        assert_eq!(agent_rank("claude", &[]), None);
    }

    #[test]
    fn a_question_near_the_bottom_counts() {
        let lines = vec![
            "building".to_owned(),
            "Do you want to proceed?".to_owned(),
            "  1. Yes".to_owned(),
        ];

        assert!(looks_like_a_question(&lines, &patterns()));
    }

    /// Questions scroll away. One far enough up the transcript has been
    /// answered already and must not pin the pane to `Waiting` forever.
    #[test]
    fn a_question_further_up_the_transcript_does_not_count() {
        let mut lines = vec!["Do you want to proceed?".to_owned()];
        lines.extend((0..20).map(|n| format!("line {n}")));

        assert!(!looks_like_a_question(&lines, &patterns()));
    }

    #[test]
    fn blank_lines_below_a_question_do_not_hide_it() {
        let lines = vec![
            "Do you want to proceed?".to_owned(),
            String::new(),
            "   ".to_owned(),
        ];

        assert!(looks_like_a_question(&lines, &patterns()));
    }

    #[test]
    fn no_patterns_means_nothing_is_ever_waiting() {
        let lines = vec!["Do you want to proceed?".to_owned()];

        assert!(!looks_like_a_question(&lines, &[]));
    }

    /// Both ways of stopping count: the sound says "it is your turn", not
    /// "it succeeded".
    #[test]
    fn leaving_work_behind_is_finishing() {
        assert!(just_finished(Some(AgentState::Working), AgentState::Idle));
        assert!(just_finished(Some(AgentState::Working), AgentState::Waiting));
    }

    /// A pane that was already stopped, or is still going, has not finished
    /// anything now — including the first time it is ever looked at.
    #[test]
    fn staying_put_is_not_finishing() {
        assert!(!just_finished(Some(AgentState::Working), AgentState::Working));
        assert!(!just_finished(Some(AgentState::Idle), AgentState::Idle));
        assert!(!just_finished(Some(AgentState::Idle), AgentState::Waiting));
        assert!(!just_finished(None, AgentState::Idle));
        assert!(!just_finished(None, AgentState::Working));
    }

    #[test]
    fn parse_list_trims_and_drops_empties() {
        assert_eq!(parse_list(" a , ,b "), vec!["a".to_owned(), "b".to_owned()]);
        assert!(parse_list("  ").is_empty());
    }
}
