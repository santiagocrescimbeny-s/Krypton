//! Process-wide `tracing` subscriber with an `EnvFilter`.
//!
//! # Policy (audit finding #31)
//! - **Default level `info`.** `RUST_LOG` overrides it; the configured
//!   `LOG_LEVEL` is the fallback when `RUST_LOG` is unset.
//! - **Nothing sensitive at any level.** Events carry counts, sequence numbers,
//!   slots, static labels and flags: never public keys, signatures, key material,
//!   token amounts or balances. The filter is a second line of defence, not the
//!   first: a `RUST_LOG=trace` must not leak either. `Config`, `SecureWallet` and
//!   the secret containers redact their `Debug` output, and the monitor logs error
//!   labels ([`crate::errors::ExitError::label`]) instead of value-carrying text.
//! - Plain text on stdout, without ANSI colours and without module paths.
//!
//! Call [`init`] once, from a synchronous `main`, before spawning tasks.

use tracing::Level;
use tracing_subscriber::EnvFilter;

use crate::errors::LoggingError;

/// The environment variable holding the filter directive.
pub const FILTER_ENV_VAR: &str = "RUST_LOG";

/// Builds the filter for `directive` (the value of [`FILTER_ENV_VAR`], if set
/// and non-blank), falling back to `default_level`.
///
/// # Errors
/// [`LoggingError::InvalidFilter`] when `directive` does not parse. The
/// directive is not echoed back.
pub fn build_filter(
    directive: Option<&str>,
    default_level: Level,
) -> Result<EnvFilter, LoggingError> {
    match directive.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => EnvFilter::try_new(text).map_err(|_| LoggingError::InvalidFilter),
        None => EnvFilter::try_new(default_level.as_str().to_ascii_lowercase())
            .map_err(|_| LoggingError::InvalidFilter),
    }
}

/// Installs the global subscriber: `RUST_LOG` if set, else `default_level`.
///
/// # Errors
/// - [`LoggingError::InvalidFilter`] for a malformed `RUST_LOG`;
/// - [`LoggingError::AlreadyInstalled`] if a global subscriber exists.
pub fn init(default_level: Level) -> Result<(), LoggingError> {
    let directive = std::env::var(FILTER_ENV_VAR).ok();
    let filter = build_filter(directive.as_deref(), default_level)?;
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init()
        .map_err(|_| LoggingError::AlreadyInstalled)
}
