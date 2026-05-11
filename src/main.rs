#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

mod app;
mod backend;
mod command;
mod config;
mod anim;
mod input;
mod layout;
mod render;
mod term;

fn main() {
    println!("weave starting");
}
