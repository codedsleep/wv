//! Terminal RAII guard + module exports.

#[allow(dead_code)]
pub mod cell;
#[allow(dead_code)]
pub mod pane;
#[allow(dead_code)]
pub mod query;
#[allow(dead_code)]
pub mod surface;

use crossterm::event::{
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};

pub struct TerminalGuard {
    /// Whether the keyboard enhancement push succeeded and needs popping.
    ///
    /// Popping flags that were never pushed pops somebody else's, so the
    /// teardown has to know whether the setup got that far.
    enhanced_keyboard: bool,
}

impl TerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            // Hidden only until the first frame places it. `App` positions the
            // cursor on the focused pane after every flush, so leaving it
            // visible here would show it parked at the top-left in between.
            crossterm::cursor::Hide
        )?;

        // Without this, ESC arrives as a bare `\x1b` byte, indistinguishable
        // from the start of an escape sequence, and every program reading our
        // input has to guess by timing. Vim modes in the agent TUIs are the
        // visible casualty: mode switches land late or not at all.
        //
        // Only DISAMBIGUATE_ESCAPE_CODES is asked for. The louder flags
        // (key release events, report-all-keys) change what a well-behaved
        // program sees for ordinary typing, and weave has no use for them.
        let enhanced_keyboard = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
            && crossterm::execute!(
                std::io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok();

        Ok(Self { enhanced_keyboard })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.enhanced_keyboard {
            let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
