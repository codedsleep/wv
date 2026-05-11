#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]

pub mod anim;
pub mod app;
pub mod backend;
pub mod command;
pub mod config;
pub mod input;
pub mod layout;
pub mod render;
pub mod term;
