//! Logging: a daily log file in the data directory, plus stderr. The log
//! level comes from `RUST_LOG` and defaults to `info`.
//!
//! Never log secret keys, ticket secrets, file contents or message bodies.

use std::backtrace::Backtrace;
use std::io;
use std::panic;
use std::path::Path;

use thiserror::Error;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
use tracing_subscriber::util::TryInitError;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

const LOG_FILE_PREFIX: &str = "groovy";
const LOG_FILE_SUFFIX: &str = "log";
const KEEP_LOG_FILES: usize = 7;
const DEFAULT_FILTER: &str = "info";

/// Errors from starting logging.
#[derive(Debug, Error)]
pub enum LoggingError {
    #[error("failed to create the log file")]
    Appender(#[source] InitError),
    #[error("logging has already been started")]
    AlreadyStarted(#[source] TryInitError),
}

/// Keeps the log file writer running. Dropping it flushes any pending lines,
/// so hold it until the app exits.
pub struct LogGuard {
    _worker: WorkerGuard,
}

/// Starts logging into `logs_dir`. Keeps the most recent seven daily files.
pub fn init(logs_dir: &Path) -> Result<LogGuard, LoggingError> {
    let file = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(KEEP_LOG_FILES)
        .build(logs_dir)
        .map_err(LoggingError::Appender)?;
    let (file, worker) = tracing_appender::non_blocking(file);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_ansi(false).with_writer(file))
        .with(fmt::layer().with_writer(io::stderr))
        .try_init()
        .map_err(LoggingError::AlreadyStarted)?;

    Ok(LogGuard { _worker: worker })
}

/// Logs each panic with a backtrace, then runs the previous hook so the
/// panic still prints as usual.
pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::force_capture();
        tracing::error!(%info, %backtrace, "thread panicked");
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A writer that appends to a shared buffer, so a test can read what was logged.
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn panics_are_logged_with_a_backtrace() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = fmt::Subscriber::builder()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            install_panic_hook();
            let _ = panic::catch_unwind(|| panic!("deliberate test panic"));
        });

        let output = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains("thread panicked"), "{output}");
        assert!(output.contains("deliberate test panic"), "{output}");
        assert!(output.contains("backtrace"), "{output}");
    }
}
