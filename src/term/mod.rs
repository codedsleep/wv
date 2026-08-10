//! Terminal RAII guard + module exports.

#[allow(dead_code)]
pub mod cell;
#[allow(dead_code)]
pub mod pane;
#[allow(dead_code)]
pub mod query;
#[allow(dead_code)]
pub mod surface;

pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    pub fn new() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::cursor::Hide
        )?;

        Ok(Self { _private: () })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
    }
}
