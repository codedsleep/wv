#![forbid(unsafe_code)]
#![warn(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;
const LOG_ARCHIVE_COUNT: u8 = 3;
const LOG_ROTATION_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

fn init_tracing() -> anyhow::Result<()> {
    let log_path = log_path()?;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate_log_if_needed(&log_path, LOG_MAX_BYTES, LOG_ARCHIVE_COUNT)?;

    let level = log_level_from_env();
    let writer = RotatingLogWriter::new(log_path, LOG_MAX_BYTES, LOG_ARCHIVE_COUNT)?;
    spawn_log_rotation_thread(writer.rotation_handle())?;

    tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_max_level(level)
        .with_ansi(false)
        .init();

    Ok(())
}

fn log_path() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME")?).join(".local/state/weave/weave.log"))
}

fn log_level_from_env() -> tracing::Level {
    parse_log_level(std::env::var("WEAVE_LOG").as_deref().ok())
}

fn parse_log_level(value: Option<&str>) -> tracing::Level {
    match value {
        Some("trace") => tracing::Level::TRACE,
        Some("debug") => tracing::Level::DEBUG,
        Some("warn") => tracing::Level::WARN,
        Some("error") => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

#[derive(Clone)]
struct RotatingLogWriter {
    inner: Arc<Mutex<std::fs::File>>,
    path: PathBuf,
    max_bytes: u64,
    archive_count: u8,
}

impl RotatingLogWriter {
    fn new(path: PathBuf, max_bytes: u64, archive_count: u8) -> std::io::Result<Self> {
        Ok(Self {
            inner: Arc::new(Mutex::new(open_log_file(&path)?)),
            path,
            max_bytes,
            archive_count,
        })
    }

    fn rotation_handle(&self) -> LogRotationHandle {
        LogRotationHandle {
            inner: Arc::clone(&self.inner),
            path: self.path.clone(),
            max_bytes: self.max_bytes,
            archive_count: self.archive_count,
        }
    }
}

impl Write for RotatingLogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        lock_log_file(&self.inner).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        lock_log_file(&self.inner).flush()
    }
}

struct LogRotationHandle {
    inner: Arc<Mutex<std::fs::File>>,
    path: PathBuf,
    max_bytes: u64,
    archive_count: u8,
}

impl LogRotationHandle {
    fn rotate_if_needed(&self) -> std::io::Result<bool> {
        let mut file = lock_log_file(&self.inner);
        if file.metadata()?.len() < self.max_bytes {
            return Ok(false);
        }

        file.flush()?;
        rotate_logs(&self.path, self.archive_count)?;
        *file = open_log_file(&self.path)?;
        Ok(true)
    }
}

fn spawn_log_rotation_thread(handle: LogRotationHandle) -> std::io::Result<()> {
    // Rotation runs off the Tokio runtime and shares only the log file mutex
    // with tracing, never render state, so the render tick is not involved.
    std::thread::Builder::new()
        .name("weave-log-rotation".to_owned())
        .spawn(move || loop {
            std::thread::sleep(LOG_ROTATION_CHECK_INTERVAL);
            let _ = handle.rotate_if_needed();
        })?;
    Ok(())
}

fn lock_log_file(file: &Mutex<std::fs::File>) -> MutexGuard<'_, std::fs::File> {
    file.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn open_log_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
}

fn rotate_log_if_needed(path: &Path, max_bytes: u64, archive_count: u8) -> std::io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.len() >= max_bytes => {
            rotate_logs(path, archive_count)?;
            Ok(true)
        }
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn rotate_logs(path: &Path, archive_count: u8) -> std::io::Result<()> {
    if archive_count == 0 {
        remove_file_if_exists(path)?;
        return Ok(());
    }

    remove_file_if_exists(&archive_path(path, archive_count))?;
    for index in (1..archive_count).rev() {
        rename_file_if_exists(&archive_path(path, index), &archive_path(path, index + 1))?;
    }
    rename_file_if_exists(path, &archive_path(path, 1))?;
    Ok(())
}

fn archive_path(path: &Path, index: u8) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("weave.log");
    path.with_file_name(format!("{file_name}.{index}"))
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn rename_file_if_exists(from: &Path, to: &Path) -> std::io::Result<()> {
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
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
    let launch_args = weave::app::LaunchArgs::parse_env()?;
    init_tracing()?;
    let app = match launch_args {
        weave::app::LaunchArgs::ListSessions => return weave::app::print_weave_sessions(),
        weave::app::LaunchArgs::Run(args) => {
            install_panic_hook();
            let (width, height) = crossterm::terminal::size()?;
            weave::app::App::new(width, height, args).await?
        }
        weave::app::LaunchArgs::Attach(args) => {
            install_panic_hook();
            let (width, height) = crossterm::terminal::size()?;
            weave::app::App::attach(width, height, args).await?
        }
    };
    let _guard = weave::term::TerminalGuard::new()?;
    tracing::info!("weave starting");
    tracing::info!("entered alt screen");
    app.run().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::path::{Path, PathBuf};

    use super::{archive_path, parse_log_level, rotate_log_if_needed, LogRotationHandle};

    #[test]
    fn parse_log_level_accepts_warn_and_error() {
        assert_eq!(parse_log_level(Some("trace")), tracing::Level::TRACE);
        assert_eq!(parse_log_level(Some("debug")), tracing::Level::DEBUG);
        assert_eq!(parse_log_level(Some("warn")), tracing::Level::WARN);
        assert_eq!(parse_log_level(Some("error")), tracing::Level::ERROR);
        assert_eq!(parse_log_level(Some("unknown")), tracing::Level::INFO);
        assert_eq!(parse_log_level(None), tracing::Level::INFO);
    }

    #[test]
    fn rotate_log_if_needed_shifts_archives_and_caps_oldest() {
        let dir = test_dir("startup-rotation");
        std::fs::create_dir_all(&dir).expect("test dir created");
        let log = dir.join("weave.log");
        write_file(&log, b"0123456789");
        write_file(&archive_path(&log, 1), b"one");
        write_file(&archive_path(&log, 2), b"two");
        write_file(&archive_path(&log, 3), b"three");

        let rotated = rotate_log_if_needed(&log, 10, 3).expect("rotation succeeds");

        assert!(rotated);
        assert_eq!(read_file(&archive_path(&log, 1)), b"0123456789");
        assert_eq!(read_file(&archive_path(&log, 2)), b"one");
        assert_eq!(read_file(&archive_path(&log, 3)), b"two");
        assert!(!log.exists());
        std::fs::remove_dir_all(dir).expect("test dir removed");
    }

    #[test]
    fn rotation_handle_reopens_current_log_after_rotation() {
        let dir = test_dir("runtime-rotation");
        std::fs::create_dir_all(&dir).expect("test dir created");
        let log = dir.join("weave.log");
        let file = super::open_log_file(&log).expect("log opens");
        let handle = LogRotationHandle {
            inner: std::sync::Arc::new(std::sync::Mutex::new(file)),
            path: log.clone(),
            max_bytes: 4,
            archive_count: 3,
        };

        {
            let mut file = handle
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            file.write_all(b"hello").expect("log write succeeds");
            file.flush().expect("log flush succeeds");
        }

        assert!(handle.rotate_if_needed().expect("rotation succeeds"));
        {
            let mut file = handle
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            file.write_all(b"new").expect("new log write succeeds");
            file.flush().expect("new log flush succeeds");
        }

        assert_eq!(read_file(&archive_path(&log, 1)), b"hello");
        assert_eq!(read_file(&log), b"new");
        std::fs::remove_dir_all(dir).expect("test dir removed");
    }

    fn write_file(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).expect("test file written");
    }

    fn read_file(path: &Path) -> Vec<u8> {
        std::fs::read(path).expect("test file read")
    }

    fn test_dir(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("weave-{name}-{}-{unique}", std::process::id()))
    }
}
