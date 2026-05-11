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

fn init_tracing() -> anyhow::Result<()> {
    let log_path = std::path::PathBuf::from(std::env::var("HOME")?)
        .join(".local/state/weave/weave.log");

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let level = match std::env::var("WEAVE_LOG").as_deref() {
        Ok("trace") => tracing::Level::TRACE,
        Ok("debug") => tracing::Level::DEBUG,
        _ => tracing::Level::INFO,
    };
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let writer = std::sync::Mutex::new(log_file);

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_max_level(level)
        .with_ansi(false)
        .init();

    Ok(())
}

fn main() -> anyhow::Result<()> {
    init_tracing()?;
    tracing::info!("weave starting");
    println!("weave starting");

    Ok(())
}
