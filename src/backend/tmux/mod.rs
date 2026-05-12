//! tmux -CC backed `TmuxBackend`.

pub(crate) mod format;
pub mod layout;
pub mod parser;
pub mod process;

#[allow(unused_imports)]
pub use process::TmuxBackend;
