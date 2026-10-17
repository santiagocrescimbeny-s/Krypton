//! Unified error hierarchy for Krypton.
//!
//! Design rules:
//! - No error message ever embeds secret material (private keys, raw `.env`
//!   lines, URLs that may carry API keys). Inner errors that could echo input
//!   are deliberately dropped or reduced to non-sensitive metadata.
//! - Every fallible production path returns one of these types; `unwrap()` and
//!   `expect()` are denied crate-wide (see `lib.rs`).

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, LauncherError>;

/// Top-level error returned by Krypton's public entry points.
#[derive(Debug, Error)]
pub enum LauncherError {
    /// Environment / configuration failure.
    #[error("configuration error: {0}")]
    Config(#[from] ConfigError),

    /// Keypair / cryptographic material failure.
    #[error("wallet error: {0}")]
    Wallet(#[from] WalletError),

    /// Filesystem or other I/O failure.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// HTTP transport failure (Pinata/IPFS, Jito JSON-RPC, ...).
    ///
    /// The request URL is stripped on conversion (see the manual `From`
    /// implementation) because RPC URLs frequently embed API keys.
    #[error("HTTP error: {0}")]
    Http(#[source] reqwest::Error),

    /// JSON (de)serialization failure.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Solana RPC or Jito Block Engine returned an error.
    #[error("RPC error during {context}: {message}")]
    Rpc {
        /// Short description of the operation that failed.
        context: &'static str,
        /// Error message reported by the remote endpoint.
        message: String,
    },

    /// An operation exceeded its deadline.
    #[error("operation `{operation}` timed out after {millis} ms")]
    Timeout {
        /// Name of the operation that timed out.
        operation: &'static str,
        /// Deadline that was exceeded, in milliseconds.
        millis: u64,
    },
}

impl From<reqwest::Error> for LauncherError {
    fn from(err: reqwest::Error) -> Self {
        Self::Http(err.without_url())
    }
}

/// Failures while loading or validating configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A mandatory variable is absent (or blank).
    #[error("required environment variable `{name}` is not set")]
    MissingVar {
        /// Name of the missing variable.
        name: &'static str,
    },

    /// A variable is present but its value is not acceptable.
    ///
    /// `reason` never contains the offending value.
    #[error("environment variable `{name}` is invalid: {reason}")]
    InvalidVar {
        /// Name of the offending variable.
        name: &'static str,
        /// Human readable explanation (value-free).
        reason: String,
    },

    /// The collected variables could not be mapped onto the raw config schema.
    #[error("failed to deserialize environment variables: {0}")]
    Deserialize(String),

    /// The `.env` file has a syntax error. The offending line is *not* included
    /// because it may contain a private key.
    #[error("failed to parse .env file near entry {position}")]
    DotEnvSyntax {
        /// Position reported by the parser.
        position: usize,
    },

    /// The `.env` file exists but could not be read.
    #[error("failed to read .env file: {0:?}")]
    DotEnvIo(std::io::ErrorKind),

    /// Any other `.env` loading failure.
    #[error("failed to load .env file")]
    DotEnvUnknown,
}

/// Failures while parsing or validating keypair material.
///
/// Variants intentionally carry no input-derived text: even a single invalid
/// character position from a Base58 decoder leaks information about a key.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WalletError {
    /// The secret string is empty or whitespace only.
    #[error("the secret key input is empty")]
    EmptySecret,

    /// The input (or its decoded form) exceeds the accepted size.
    #[error("the secret key input exceeds the maximum accepted size")]
    InputTooLarge,

    /// The input is not valid Base58.
    #[error("the secret key is not valid Base58")]
    InvalidBase58,

    /// The input starts with `[` but is not a valid JSON array of bytes.
    #[error("the secret key is not a valid JSON byte array (expected `[0-255, ...]`)")]
    InvalidJsonArray,

    /// The decoded key does not have the expected number of bytes.
    #[error("invalid keypair length: expected {expected} bytes, got {actual}")]
    InvalidLength {
        /// Required number of bytes.
        expected: usize,
        /// Number of bytes actually decoded.
        actual: usize,
    },

    /// The public half of the keypair does not match the secret half.
    #[error("the public key does not match the secret key")]
    PublicKeyMismatch,
}
