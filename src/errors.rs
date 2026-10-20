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

    /// Off-chain metadata engine failure (Pinata / IPFS).
    #[error("IPFS error: {0}")]
    Ipfs(#[from] IpfsError),

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

/// Failures in the off-chain metadata engine (image + JSON pinning).
///
/// Like every error in this crate, no variant echoes credentials, request URLs
/// or unsanitized remote text.
#[derive(Debug, Error)]
pub enum IpfsError {
    /// A token metadata field violates Pump.fun / Metaplex constraints.
    #[error("invalid token metadata field `{field}`: {reason}")]
    InvalidMetadata {
        /// Offending field name.
        field: &'static str,
        /// Value-free explanation.
        reason: String,
    },

    /// The image payload has zero bytes.
    #[error("the image is empty")]
    EmptyImage,

    /// The image is not PNG, JPEG, GIF or WebP (checked via magic bytes).
    #[error("unsupported image format (expected PNG, JPEG, GIF or WebP)")]
    UnsupportedImageFormat,

    /// The image exceeds the configured size limit.
    #[error("image is {size} bytes, exceeding the {max} byte limit")]
    ImageTooLarge {
        /// Actual size in bytes.
        size: u64,
        /// Maximum accepted size in bytes.
        max: u64,
    },

    /// The image file could not be read from disk.
    #[error("failed to read the image file: {0:?}")]
    ImageRead(std::io::ErrorKind),

    /// The Pinata JWT is absent or blank.
    #[error("the Pinata JWT is missing or blank")]
    MissingCredentials,

    /// An endpoint (upload or gateway) is unusable.
    #[error("invalid IPFS endpoint `{name}`: {reason}")]
    InvalidEndpoint {
        /// Which endpoint setting is wrong.
        name: &'static str,
        /// Value-free explanation.
        reason: String,
    },

    /// The HTTP client could not be constructed.
    #[error("failed to build the HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// The pinning service rejected the credentials (HTTP 401/403).
    #[error("the pinning service rejected the credentials (HTTP {status})")]
    Unauthorized {
        /// HTTP status code received.
        status: u16,
    },

    /// The pinning service answered with a non-success status.
    #[error("the pinning service returned HTTP {status}: {message}")]
    Api {
        /// HTTP status code received.
        status: u16,
        /// Remote text, sanitized by [`sanitize_remote_message`].
        message: String,
    },

    /// Connection-level failure (DNS, TCP, TLS, reset, ...).
    #[error("transport error while talking to the pinning service: {0}")]
    Transport(#[source] reqwest::Error),

    /// The pinning service did not answer within the deadline.
    #[error("the pinning service did not answer within the deadline")]
    Timeout,

    /// The response was not the expected document.
    #[error("unexpected response from the pinning service: {0}")]
    InvalidResponse(&'static str),

    /// The returned CID is empty, too short/long or contains illegal characters.
    #[error("the pinning service returned a malformed CID")]
    InvalidCid,

    /// The final URI would not fit the on-chain `uri` field.
    #[error("the resulting URI is {len} bytes, exceeding the {max} byte on-chain limit")]
    UriTooLong {
        /// Length of the URI in bytes.
        len: usize,
        /// Maximum allowed length in bytes.
        max: usize,
    },

    /// The metadata document could not be serialized.
    #[error("failed to serialize the metadata JSON: {0}")]
    Serialize(#[source] serde_json::Error),

    /// Every retry attempt failed with a transient error.
    #[error("upload failed after {attempts} attempts: {source}")]
    RetriesExhausted {
        /// Number of attempts performed.
        attempts: u32,
        /// The error of the last attempt.
        #[source]
        source: Box<IpfsError>,
    },
}

impl IpfsError {
    /// `true` for transient failures worth retrying: timeouts, connection-level
    /// errors, HTTP 408/425/429 and 5xx. Credentials, validation, parsing and
    /// other 4xx errors are permanent.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout => true,
            Self::Transport(source) => {
                !(source.is_builder()
                    || source.is_redirect()
                    || source.is_status()
                    || source.is_decode())
            }
            Self::Api { status, .. } => matches!(*status, 408 | 425 | 429 | 500..=599),
            _ => false,
        }
    }
}

/// Maximum number of characters of remote text kept in an error message.
pub const MAX_REMOTE_MESSAGE_CHARS: usize = 256;

/// Makes text received from a remote service safe to store in an error:
/// control characters become spaces, whitespace is collapsed, anything that
/// looks like a URL (`scheme://...`) or a JWT (`eyJ...`) is replaced by a
/// placeholder, and the result is truncated to [`MAX_REMOTE_MESSAGE_CHARS`].
#[must_use]
pub fn sanitize_remote_message(raw: &str) -> String {
    let spaced: String = raw
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let joined = spaced
        .split_whitespace()
        .map(|token| {
            if token.contains("://") {
                "<url>"
            } else if token.starts_with("eyJ") {
                "<redacted>"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    joined.chars().take(MAX_REMOTE_MESSAGE_CHARS).collect()
}
