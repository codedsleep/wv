//! tmux -CC backed `TmuxBackend`.

pub mod layout;
pub mod parser;
pub mod process;

#[allow(unused_imports)]
pub use process::TmuxBackend;
