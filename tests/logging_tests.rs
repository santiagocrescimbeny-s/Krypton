//! Logging policy tests (audit finding #31): `info` by default, `RUST_LOG`
//! override, malformed directives rejected without echoing them.
//!
//! Conventions
//! - Tests return `Result` and use `?` / `matches!`; no `unwrap()` / `expect()`.
//! - Output goes to an in-memory sink through a thread-local subscriber
//!   (`with_default`), never to the global one, so tests do not interfere.

use std::error::Error;
use std::io::Write;
use std::sync::{Arc, Mutex};

use krypton::errors::{LauncherError, LoggingError};
use krypton::logging::{build_filter, FILTER_ENV_VAR};
use tracing::{debug, error, info, trace, warn, Level};
use tracing_subscriber::EnvFilter;

type TestResult = Result<(), Box<dyn Error>>;

struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut inner) = self.0.lock() {
            inner.extend_from_slice(buf);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Emits one event per level under `filter` and returns what was printed.
fn emitted_with(filter: EnvFilter) -> Result<String, Box<dyn Error>> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&buffer);
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(move || Sink(Arc::clone(&sink)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        trace!("level-trace");
        debug!("level-debug");
        info!("level-info");
        warn!("level-warn");
        error!("level-error");
    });
    let bytes = buffer.lock().map_err(|_| "poisoned")?.clone();
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn the_default_filter_is_info() -> TestResult {
    let output = emitted_with(build_filter(None, Level::INFO)?)?;
    assert!(output.contains("level-info"));
    assert!(output.contains("level-warn"));
    assert!(output.contains("level-error"));
    assert!(!output.contains("level-debug"));
    assert!(!output.contains("level-trace"));
    Ok(())
}

#[test]
fn a_blank_directive_falls_back_to_the_default_level() -> TestResult {
    for blank in ["", "   ", "\t"] {
        let output = emitted_with(build_filter(Some(blank), Level::WARN)?)?;
        assert!(!output.contains("level-info"), "{blank:?}");
        assert!(output.contains("level-warn"), "{blank:?}");
    }
    Ok(())
}

#[test]
fn the_environment_directive_overrides_the_default() -> TestResult {
    assert_eq!(FILTER_ENV_VAR, "RUST_LOG");
    let verbose = emitted_with(build_filter(Some("debug"), Level::INFO)?)?;
    assert!(verbose.contains("level-debug"));
    assert!(!verbose.contains("level-trace"));

    let quiet = emitted_with(build_filter(Some("error"), Level::INFO)?)?;
    assert!(quiet.contains("level-error"));
    assert!(!quiet.contains("level-warn"));
    assert!(!quiet.contains("level-info"));

    let off = emitted_with(build_filter(Some("off"), Level::INFO)?)?;
    assert!(off.is_empty());
    Ok(())
}

#[test]
fn output_is_plain_text_without_module_paths() -> TestResult {
    let output = emitted_with(build_filter(None, Level::INFO)?)?;
    assert!(!output.contains('\u{1b}'), "no ANSI escape sequences");
    assert!(!output.contains("logging_tests"), "no target / module path");
    Ok(())
}

#[test]
fn a_malformed_directive_is_rejected_without_being_echoed() {
    let secret_looking = "krypton=not_a_level_SECRET123";
    let result = build_filter(Some(secret_looking), Level::INFO);
    assert!(matches!(result, Err(LoggingError::InvalidFilter)));
    let text = LoggingError::InvalidFilter.to_string();
    assert!(!text.contains("SECRET123"));
}

#[test]
fn logging_errors_convert_into_the_launcher_error() {
    let error: LauncherError = LoggingError::AlreadyInstalled.into();
    assert!(matches!(
        error,
        LauncherError::Logging(LoggingError::AlreadyInstalled)
    ));
    assert!(error.to_string().contains("already installed"));
}
