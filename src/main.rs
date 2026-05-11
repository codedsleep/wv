#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

fn init_tracing() -> anyhow::Result<()> {
    let log_path =
        std::path::PathBuf::from(std::env::var("HOME")?).join(".local/state/weave/weave.log");

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

fn install_panic_hook() {
    let prev = std::panic::take_hook();

    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = crossterm::terminal::disable_raw_mode();
        tracing::error!("panic: {info}");
        prev(info);
    }));
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args = weave::app::Args::parse_env()?;
    init_tracing()?;
    install_panic_hook();
    let (width, height) = crossterm::terminal::size()?;
    let app = weave::app::App::new(width, height, args).await?;
    let _guard = weave::term::TerminalGuard::new()?;
    tracing::info!("weave starting");
    tracing::info!("entered alt screen");
    app.run().await?;

    Ok(())
}
