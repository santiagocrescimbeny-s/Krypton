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

    /// On-chain transaction construction failure (PDA derivation, instruction
    /// encoding, message compilation, signing).
    #[error("transaction builder error: {0}")]
    Builder(#[from] BuilderError),

    /// Jito Block Engine failure (tip, bundle submission, status queries).
    #[error("Jito error: {0}")]
    Jito(#[from] JitoError),

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

/// Failures of the Jito Block Engine client and of the tip / bundle assembly.
///
/// Variants carry only static labels, numbers and sanitized remote text (see
/// [`sanitize_remote_message`]); never keys, URLs or raw response bodies.
#[derive(Debug, Error)]
pub enum JitoError {
    /// The engine URL is unusable.
    #[error("invalid Jito endpoint: {reason}")]
    InvalidEndpoint {
        /// Value-free explanation.
        reason: String,
    },

    /// The HTTP client could not be constructed.
    #[error("failed to build the HTTP client: {0}")]
    ClientBuild(#[source] reqwest::Error),

    /// `DRY_RUN` is enabled: nothing is submitted to the network.
    #[error("dry-run mode is active: the bundle was not submitted")]
    DryRun,

    /// A bundle needs at least one transaction.
    #[error("a bundle must contain at least one transaction")]
    EmptyBundle,

    /// Jito accepts at most five transactions per bundle.
    #[error("a bundle holds {count} transactions, above the limit of {max}")]
    BundleTooLarge {
        /// Transactions supplied.
        count: usize,
        /// Maximum accepted.
        max: usize,
    },

    /// The tip is below the Block Engine minimum.
    #[error("tip of {lamports} lamports is below the minimum of {min} lamports")]
    TipTooLow {
        /// Requested tip.
        lamports: u64,
        /// Minimum accepted tip.
        min: u64,
    },

    /// The tip is above the configured cap (or the absolute ceiling).
    #[error("tip of {lamports} lamports exceeds the cap of {max} lamports")]
    TipTooHigh {
        /// Requested tip.
        lamports: u64,
        /// Effective cap.
        max: u64,
    },

    /// A transaction in the bundle does not fit a packet.
    #[error("transaction is {size} bytes, above the {max} byte packet limit")]
    TransactionTooLarge {
        /// Serialized size.
        size: usize,
        /// Packet limit.
        max: usize,
    },

    /// The Block Engine rejected the request rate (HTTP 429).
    #[error("the Block Engine is rate limiting this client (HTTP 429)")]
    RateLimited,

    /// The Block Engine rejected the credentials (HTTP 401/403).
    #[error("the Block Engine rejected the request (HTTP {status})")]
    Unauthorized {
        /// HTTP status code received.
        status: u16,
    },

    /// A non-success HTTP status without a JSON-RPC error body.
    #[error("the Block Engine returned HTTP {status}: {message}")]
    Api {
        /// HTTP status code received.
        status: u16,
        /// Remote text, sanitized.
        message: String,
    },

    /// A JSON-RPC error object.
    #[error("the Block Engine returned JSON-RPC error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// Remote text, sanitized.
        message: String,
    },

    /// Connection-level failure (DNS, TCP, TLS, reset, ...).
    #[error("transport error while talking to the Block Engine: {0}")]
    Transport(#[source] reqwest::Error),

    /// The Block Engine did not answer within the deadline.
    #[error("the Block Engine did not answer within the deadline")]
    Timeout,

    /// The response was not the expected JSON-RPC document.
    #[error("unexpected response from the Block Engine: {0}")]
    InvalidResponse(&'static str),

    /// Every attempt of a read-only call failed with a transient error.
    #[error("request failed after {attempts} attempts: {source}")]
    RetriesExhausted {
        /// Number of attempts performed.
        attempts: u32,
        /// The error of the last attempt.
        #[source]
        source: Box<JitoError>,
    },

    /// The launch transaction could not be assembled.
    #[error("transaction builder error: {0}")]
    Builder(#[from] BuilderError),
}

impl JitoError {
    /// `true` for transient failures worth retrying on a read-only call:
    /// timeouts, connection-level errors, HTTP 429, 408, 425 and 5xx.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Timeout | Self::RateLimited => true,
            Self::Transport(source) => {
                !(source.is_builder()
                    || source.is_redirect()
                    || source.is_status()
                    || source.is_decode())
            }
            Self::Api { status, .. } => matches!(*status, 408 | 425 | 500..=599),
            _ => false,
        }
    }
}

/// Failures while deriving addresses, encoding instructions, or assembling and
/// signing the atomic `Create` + `Buy` transaction.
///
/// Variants carry only static labels and numbers (never keys, seeds or remote
/// text), so they are always safe to log.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum BuilderError {
    /// No valid bump exists for the requested seeds (practically unreachable,
    /// but reported instead of panicking).
    #[error("could not derive the program address for `{account}`")]
    PdaDerivationFailed {
        /// Logical name of the account being derived.
        account: &'static str,
    },

    /// A token metadata field (`name`, `symbol`, `uri`) is empty.
    #[error("token field `{field}` must not be empty")]
    EmptyField {
        /// Offending field.
        field: &'static str,
    },

    /// A token metadata field exceeds the on-chain limit.
    #[error("token field `{field}` is too long: {actual} bytes (max {max})")]
    FieldTooLong {
        /// Offending field.
        field: &'static str,
        /// Maximum accepted length in bytes.
        max: usize,
        /// Actual length in bytes.
        actual: usize,
    },

    /// A token metadata field contains a character that would be rejected
    /// downstream (control characters).
    #[error("token field `{field}` contains control characters")]
    InvalidFieldContent {
        /// Offending field.
        field: &'static str,
    },

    /// The requested launch option is not supported by the selected variant.
    #[error("unsupported launch option: {reason}")]
    UnsupportedOption {
        /// Static explanation.
        reason: &'static str,
    },

    /// The initial buy resolves to zero tokens.
    #[error("the initial buy amount resolves to zero tokens")]
    ZeroBuyAmount,

    /// The requested tokens exceed what the bonding curve can still sell.
    #[error("requested {requested} base units but the curve only has {available} left")]
    BuyExceedsReserves {
        /// Tokens requested, in base units.
        requested: u64,
        /// Tokens still available on the curve, in base units.
        available: u64,
    },

    /// Slippage tolerance outside `0..=max`.
    #[error("slippage {bps} bps exceeds the allowed maximum of {max} bps")]
    InvalidSlippage {
        /// Requested slippage.
        bps: u16,
        /// Maximum accepted slippage.
        max: u16,
    },

    /// Assumed total trading fee outside `0..=max`.
    #[error("fee {bps} bps exceeds the allowed maximum of {max} bps")]
    InvalidFee {
        /// Requested fee.
        bps: u16,
        /// Maximum accepted fee.
        max: u16,
    },

    /// Curve parameters are inconsistent (zero reserves, supply mismatch, ...).
    #[error("invalid bonding curve parameters: {reason}")]
    InvalidCurve {
        /// Static explanation.
        reason: &'static str,
    },

    /// A checked arithmetic operation overflowed.
    #[error("arithmetic overflow while computing {context}")]
    ArithmeticOverflow {
        /// What was being computed.
        context: &'static str,
    },

    /// The `v0` message could not be compiled.
    #[error("failed to compile the v0 message: {0}")]
    MessageCompile(String),

    /// An Address Lookup Table is malformed, inactive or does not hold the
    /// addresses the launch needs.
    #[error("invalid address lookup table: {reason}")]
    InvalidLookupTable {
        /// Static, value-free explanation.
        reason: &'static str,
    },

    /// The priority fee (`price x compute unit limit`) is above the configured cap.
    #[error("priority fee of {fee_lamports} lamports exceeds the cap of {max} lamports")]
    PriorityFeeTooHigh {
        /// Fee the transaction would pay, in lamports.
        fee_lamports: u128,
        /// Effective cap, in lamports.
        max: u64,
    },

    /// The fee payer cannot cover the worst-case cost of the launch.
    #[error("insufficient balance: {required} lamports required, {available} available")]
    InsufficientBalance {
        /// Worst-case lamports the launch needs.
        required: u64,
        /// Lamports the payer holds.
        available: u64,
    },

    /// The signed transaction would not fit in a single packet.
    #[error("transaction is {size} bytes, above the {max} byte packet limit")]
    TransactionTooLarge {
        /// Estimated serialized size.
        size: usize,
        /// Packet limit.
        max: usize,
    },

    /// The supplied wallet does not match the fee payer of the message.
    #[error("the wallet does not match the transaction fee payer")]
    PayerMismatch,

    /// The supplied mint signer does not match the mint of the launch.
    #[error("the mint signer does not match the launch mint")]
    MintSignerMismatch,

    /// Signing the message failed.
    #[error("failed to sign the transaction: {0}")]
    Signing(String),
}
