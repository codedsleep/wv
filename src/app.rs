//! `App`: event loop + state owner.

pub struct App;

impl App {
    pub fn new() -> Self {
        Self
    }

    #[allow(clippy::needless_continue)]
    pub async fn run(self) -> anyhow::Result<()> {
        use tokio::signal::ctrl_c;
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = signal(SignalKind::terminate())?;
        let mut sigwinch = signal(SignalKind::window_change())?;

        loop {
            tokio::select! {
                result = ctrl_c() => {
                    result?;
                    tracing::info!("ctrl-c received");
                    break;
                }
                _ = sigterm.recv() => {
                    tracing::info!("SIGTERM received");
                    break;
                }
                _ = sigwinch.recv() => {
                    tracing::info!("SIGWINCH received");
                    continue;
                }
            }
        }

        Ok(())
    }
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}
